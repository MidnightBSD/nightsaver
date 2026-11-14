//! Daemon event loop and Idle -> Blanked -> Locked state machine. Owner: Opus F.
//!
//! All display mechanics are behind `display::Backend` (X11: `x11::backend`); this file is
//! policy only and never talks to the display server itself.
//!
//! Single-threaded: `Backend::wait_event` is the only blocking point. Every timer
//! (idle poll, lock, cycle, frame, config reload, grab retry, locker deadline) is an explicit
//! `Instant` and the wait is bounded by the nearest one (and by 1 s, so signal flags are seen
//! promptly; 100 ms while the unlock dialog is up or PAM is running on its worker thread).
//!
//! Lock invariants (AGENTS.md rule 3): the only path out of `SaverState::Locked` is
//! `LockEvent::Unlocked` from the locker; EXIT/RESTART/SIGTERM/SIGINT are refused while locked;
//! keys and pointer wake-ups reach the locker only while we hold the keyboard+pointer grab.
//!
//! Grabs: "grabbed" means the backend holds exclusive input (on X11 a keyboard and pointer
//! grab). `SaverState::Locked` (status LOCK, reply `+locking`) is entered only while it is
//! held; otherwise the lock stays pending (`lock_pending`, status BLANK) and the grab is
//! retried every second; activity does not cancel a pending lock. A broken grab (reported by
//! the backend, or a failed re-assert every 2 s while locked) hides the dialog and re-grabs
//! at once.
//!
//! Robustness: each loop iteration runs under `catch_unwind`; a panic switches to the blank
//! pattern, hides the dialog and continues. Only a failing `check_connection()` (dead
//! connection) ends the loop with an error.

use std::error::Error;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use crate::cli::Args;
use crate::compat::protocol::{Command, Status};
use crate::control::{self, ControlServer, Request};
use crate::config::{Config, Mode, ResolvedBackend, resolve_backend, wayland_session};
use crate::patterns::{self, Pattern, Rng};
use crate::render::Framebuffer;
use crate::render::oled::OledGuard;
use crate::display::{Backend, DisplayEvent, Monitor, SurfaceId};
use crate::wayland::WaylandBackend;
use crate::x11::backend::X11Backend;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaverState {
    Idle,
    Blanked,
    Locked,
}

const PROGNAME: &str = "nightsaver";
const IDLE_POLL: Duration = Duration::from_secs(1);
const CONFIG_CHECK: Duration = Duration::from_secs(5);
const GRAB_RETRY: Duration = Duration::from_secs(1);
/// Upper bound on display events handled per loop iteration before timers run.
const MAX_EVENTS_PER_ITERATION: usize = 256;
/// How long activation waits for the grab (xscreensaver tries 4 x 1 s). Command replies are
/// sent after activation, and xscreensaver-command gives up after about 10 s.
const GRAB_TIMEOUT: Duration = Duration::from_secs(3);
/// Re-assert the grab this often while locked (detects a grab broken behind our back).
const GRAB_REASSERT: Duration = Duration::from_secs(2);
/// Retry delay after a failed relayout.
const RELAYOUT_RETRY: Duration = Duration::from_secs(1);
/// Back-off after an error from reading display events while the connection still works.
const POLL_ERROR_PAUSE: Duration = Duration::from_millis(10);
/// Upper bound for one wait, so signal flags are noticed promptly.
const MAX_WAIT: Duration = Duration::from_secs(1);
/// Wait bound while the dialog is visible or PAM runs (worker thread does not wake poll()).
const LOCK_UI_WAIT: Duration = Duration::from_millis(100);
const MIN_DIALOG_FPS: u32 = 10;

// ---------------------------------------------------------------------------
// Lock adapter: the only place that touches the lock API (kept as one seam so API
// changes in lock/ stay local).
// ---------------------------------------------------------------------------
mod lockapi {
    use std::time::{Duration, Instant};

    use crate::input::KeyInput;
    use crate::lock::{LockEvent, Locker};
    use crate::render::Framebuffer;

    pub fn new_locker(user: String, service: String, passwd_timeout: Duration) -> Locker {
        Locker::new(user, service, passwd_timeout)
    }
    /// A locker whose every attempt is denied (used when the user name is unknown).
    pub fn new_denying_locker(passwd_timeout: Duration) -> Locker {
        use crate::lock::pam::PamError;
        use std::sync::Arc;
        Locker::with_auth(
            String::new(),
            String::new(),
            passwd_timeout,
            Arc::new(|_, _, _| Err(PamError::System("user name unknown".into()))),
        )
    }
    pub fn key(l: &mut Locker, k: KeyInput, now: Instant) -> LockEvent {
        l.key(k, now)
    }
    pub fn poll(l: &mut Locker, now: Instant) -> LockEvent {
        l.poll(now)
    }
    pub fn is_prompt_visible(l: &Locker) -> bool {
        l.is_prompt_visible()
    }
    pub fn next_deadline(l: &Locker) -> Option<Instant> {
        l.next_deadline()
    }
    pub fn is_checking(l: &Locker) -> bool {
        l.is_checking()
    }
    pub fn draw(fb: &mut Framebuffer, l: &Locker, now: Instant) {
        crate::lock::ui::draw(fb, l, now)
    }
    pub fn current_username() -> Option<String> {
        crate::lock::pam::current_username()
    }
}

use crate::input::KeyInput;
use crate::lock::{LockEvent, Locker};

// ---------------------------------------------------------------------------
// Signals
// ---------------------------------------------------------------------------
static SIG_TERM: AtomicBool = AtomicBool::new(false);
static SIG_HUP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_term(_: libc::c_int) {
    SIG_TERM.store(true, Ordering::SeqCst);
}
extern "C" fn on_hup(_: libc::c_int) {
    SIG_HUP.store(true, Ordering::SeqCst);
}

fn install_signal_handlers() {
    use nix::sys::signal::{SigHandler, Signal, signal};
    // SIGCHLD is inherited; if a parent left it at SIG_IGN, pam_exec's waitpid() fails and
    // every unlock attempt is denied. Restore the default disposition.
    let set = |sig: Signal, h: SigHandler| {
        // SAFETY: the handlers only store to a static AtomicBool, which is async-signal-safe;
        // SIG_IGN for SIGPIPE has no handler code at all.
        if let Err(e) = unsafe { signal(sig, h) } {
            log::warn!("cannot install handler for {sig}: {e}");
        }
    };
    set(Signal::SIGTERM, SigHandler::Handler(on_term));
    set(Signal::SIGINT, SigHandler::Handler(on_term));
    set(Signal::SIGHUP, SigHandler::Handler(on_hup));
    set(Signal::SIGPIPE, SigHandler::SigIgn);
    set(Signal::SIGCHLD, SigHandler::SigDfl);
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------
fn mtimes(paths: &[PathBuf]) -> Vec<Option<SystemTime>> {
    paths
        .iter()
        .map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
        .collect()
}

fn time_seed() -> u64 {
    let t = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    t ^ (u64::from(std::process::id()) << 32)
}

fn min_opt(a: Option<Instant>, b: Option<Instant>) -> Option<Instant> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, None) => a,
        (None, b) => b,
    }
}

/// `t + d` without the overflow panic of `Instant + Duration` (saturates at a far deadline).
fn later(t: Instant, d: Duration) -> Instant {
    t.checked_add(d)
        .or_else(|| t.checked_add(Duration::from_secs(86_400 * 365)))
        .unwrap_or(t)
}

/// Everything drawn on one monitor.
struct Screen {
    surface: SurfaceId,
    mon: Monitor,
    fb: Framebuffer,
    pattern: Box<dyn Pattern>,
    oled: OledGuard,
    rng: Rng,
}

/// Why `deactivate` is called; `Unlocked` is the only reason accepted while locked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Why {
    Activity,
    Command,
    Unlocked,
    Exit,
}

/// Outcome of the event loop.
enum Exit {
    Quit,
    Restart,
}

struct Daemon {
    args: Args,
    cfg: Config,
    backend: Box<dyn Backend>,
    state: SaverState,
    screens: Vec<Screen>,
    /// The backend holds exclusive input (keyboard and pointer), as far as we know.
    grabbed: bool,
    /// The exclusive input held is good enough to lock with (always, where the backend
    /// has only one kind; on Wayland only once the compositor confirmed a session lock).
    grab_secure: bool,
    /// `Capabilities::lock_is_separate`.
    lock_is_separate: bool,
    /// A lock was requested (LOCK command or lock timeout) but the grab is not held yet.
    lock_pending: bool,
    /// Index into `cfg.patterns` of the running pattern (`None` for blank mode).
    current: Option<usize>,
    rng: Rng,
    user: Option<String>,
    locker: Option<Locker>,
    status: Status,
    /// Time published with the status; kept across LOCK <-> AUTH like xscreensaver's locked_at.
    status_since: SystemTime,
    /// Force the next `update_status` to write even if the status is unchanged.
    status_dirty: bool,
    /// Baseline for idle detection: idle = min(display idle, now - activity_reset).
    activity_reset: Instant,
    next_idle_poll: Instant,
    lock_deadline: Option<Instant>,
    cycle_deadline: Option<Instant>,
    next_frame: Option<Instant>,
    last_frame: Instant,
    next_config_check: Instant,
    next_grab_retry: Option<Instant>,
    next_grab_reassert: Option<Instant>,
    next_relayout: Option<Instant>,
    watch: Vec<PathBuf>,
    watch_mtimes: Vec<Option<SystemTime>>,
    /// Pointer position (root coords) and accumulated motion since activation.
    motion_last: Option<(i32, i32)>,
    motion_accum: f64,
    /// The backend can power the monitor down itself (`Capabilities::power_control`).
    has_power: bool,
    /// Software stand-in for DPMS "off" on displays without power control: frames are pure
    /// black instead of the pattern (see `soft_off_delay`). Cleared by waking and unblanking.
    soft_off: bool,
    /// When the stand-in engages; only armed while blanked without power control.
    soft_off_deadline: Option<Instant>,
    /// False when the display cannot be locked (`Capabilities::can_lock`; XWayland): grabs
    /// there do not cover native Wayland clients, so nothing may ever be reported as locked.
    lockable: bool,
    /// The control socket; `None` when it could not be set up (no runtime directory).
    control: Option<ControlServer>,
    /// The socket client whose command is being handled; its reply goes there instead of
    /// to the backend's control channel.
    reply_to: Option<control::Client>,
}

/// `--demo-blank` is an X11 debugging aid.
const DEMO_NEEDS_X11: &str = "--demo-blank only works on an Xorg display, not in a Wayland session";

/// Run the daemon until EXIT (returns Ok) or a fatal X error. RESTART re-execs and only
/// returns on failure.
pub fn run(args: Args, cfg: Config) -> Result<(), Box<dyn Error>> {
    install_signal_handlers();
    install_panic_hook();
    let user = lockapi::current_username().or_else(|| {
        nix::unistd::User::from_uid(nix::unistd::getuid())
            .ok()
            .flatten()
            .map(|u| u.name)
    });
    let (backend, display_name): (Box<dyn Backend>, String) =
        match resolve_backend(cfg.backend, wayland_session()) {
            ResolvedBackend::Wayland => {
                let name = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".into());
                // The control socket is the only place a running daemon can be seen here
                // (X has the rendezvous window for this).
                if control::daemon_listening(&name) {
                    eprintln!("{PROGNAME}: already running on display {name}");
                    std::process::exit(1);
                }
                (Box::new(WaylandBackend::open(cfg.lock)?), name)
            }
            ResolvedBackend::X11 { lockable } => {
                if !lockable {
                    log::warn!(
                        "WAYLAND_DISPLAY is set: this X server is XWayland, which cannot lock \
                         native Wayland clients; locking is disabled and the screen will only \
                         be blanked"
                    );
                }
                let name = cfg
                    .display
                    .clone()
                    .or_else(|| std::env::var("DISPLAY").ok())
                    .unwrap_or_default();
                let backend = X11Backend::open(
                    cfg.display.as_deref(),
                    &name,
                    user.as_deref(),
                    cfg.lock,
                    lockable,
                )?;
                (Box::new(backend), name)
            }
        };
    let caps = backend.capabilities();
    let control = match ControlServer::bind(&display_name) {
        Ok(c) => {
            log::info!("control socket: {}", c.path().display());
            Some(c)
        }
        Err(e) => {
            log::info!("no control socket ({e})");
            None
        }
    };

    log::info!(
        "{PROGNAME} {} (xscreensaver {} compatible) on display {display_name}: timeout {}s, \
         cycle {}s, lock {} (after {}s), mode {:?}, patterns {:?}",
        crate::VERSION,
        crate::COMPAT_XSCREENSAVER_VERSION,
        cfg.timeout.as_secs(),
        cfg.cycle.as_secs(),
        cfg.lock,
        cfg.lock_timeout.as_secs(),
        cfg.mode,
        cfg.patterns
    );
    if cfg.splash {
        log::info!("splash disabled in nightsaver");
    }
    if !caps.power_control && cfg.dpms.enabled {
        log::info!("no DPMS extension: the screen goes black instead of powering off");
    }

    let now = Instant::now();
    let watch = cfg.watch_paths();
    let watch_mtimes = mtimes(&watch);
    let mut d = Daemon {
        args,
        cfg,
        backend,
        state: SaverState::Idle,
        screens: Vec::new(),
        grabbed: false,
        grab_secure: false,
        lock_is_separate: caps.lock_is_separate,
        lock_pending: false,
        current: None,
        rng: Rng::new(time_seed()),
        user,
        locker: None,
        status: Status::Off,
        status_since: SystemTime::now(),
        status_dirty: false,
        activity_reset: now,
        next_idle_poll: later(now, IDLE_POLL),
        lock_deadline: None,
        cycle_deadline: None,
        next_frame: None,
        last_frame: now,
        next_config_check: later(now, CONFIG_CHECK),
        next_grab_retry: None,
        next_grab_reassert: None,
        next_relayout: None,
        watch,
        watch_mtimes,
        motion_last: None,
        motion_accum: 0.0,
        has_power: caps.power_control,
        soft_off: false,
        soft_off_deadline: None,
        lockable: caps.can_lock,
        control,
        reply_to: None,
    };

    match d.event_loop()? {
        Exit::Quit => {
            log::info!("exiting");
            Ok(())
        }
        Exit::Restart => {
            drop(d);
            restart()
        }
    }
}

/// Panics must reach the log (the daemon may run detached with stderr closed).
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        if log::log_enabled!(log::Level::Error) {
            log::error!("{info}");
        } else {
            eprintln!("{PROGNAME}: {info}");
        }
    }));
}

/// Replace this process with a fresh copy of itself (same argv).
fn restart() -> Result<(), Box<dyn Error>> {
    use std::os::unix::process::CommandExt;
    let mut argv = std::env::args_os();
    let arg0 = argv.next().unwrap_or_else(|| PROGNAME.into());
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from(&arg0));
    log::info!("restarting: exec {}", exe.display());
    let err = std::process::Command::new(&exe).arg0(&arg0).args(argv).exec();
    Err(format!("restart: exec {} failed: {err}", exe.display()).into())
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // Best-effort teardown; the backend withdraws our identity and status and closes
        // the connection when it is dropped right after this.
        //
        // Not while locked or while a lock is pending (we only get here then when the
        // display connection died or the process is unwinding): teardown releases exclusive
        // input, which on Wayland is what unlocks the session. Closing the connection
        // without it leaves the compositor's session lock in place, and on X the server
        // removes our windows and grabs either way.
        if self.state != SaverState::Locked && !self.lock_pending {
            self.teardown_windows();
        }
        self.restore_dpms();
    }
}

impl Daemon {
    // -----------------------------------------------------------------------
    // Main loop
    // -----------------------------------------------------------------------
    fn event_loop(&mut self) -> Result<Exit, Box<dyn Error>> {
        loop {
            // A panic must never end the process (that would drop a lock): catch it per
            // iteration, fall back to the blank pattern and carry on.
            match catch_unwind(AssertUnwindSafe(|| self.iteration())) {
                Ok(Ok(Some(exit))) => return Ok(exit),
                Ok(Ok(None)) => {}
                Ok(Err(e)) => return Err(e),
                Err(_) => self.recover_from_panic(),
            }
        }
    }

    /// One wait + event batch + signals + timers. `Err` only for a dead connection.
    fn iteration(&mut self) -> Result<Option<Exit>, Box<dyn Error>> {
        let now = Instant::now();
        let timeout = self.next_deadline(now).saturating_duration_since(now);
        let mut ev = self.next_event(Some(timeout))?;
        let mut handled = 0usize;
        while let Some(e) = ev {
            match self.handle_event(e) {
                Ok(Some(exit)) => return Ok(Some(exit)),
                Ok(None) => {}
                Err(e) => self.recoverable(e)?,
            }
            handled += 1;
            if handled >= MAX_EVENTS_PER_ITERATION {
                // A client flooding us with events must not starve the timers (PAM results,
                // dialog timeouts, rendering). The rest is picked up next iteration.
                break;
            }
            ev = self.next_event(None)?;
        }
        self.backend.end_of_batch();
        match self.serve_control() {
            Ok(Some(exit)) => return Ok(Some(exit)),
            Ok(None) => {}
            Err(e) => self.recoverable(e)?,
        }
        match self.handle_signals() {
            Ok(Some(exit)) => return Ok(Some(exit)),
            Ok(None) => {}
            Err(e) => self.recoverable(e)?,
        }
        if let Err(e) = self.run_timers() {
            self.recoverable(e)?;
        }
        Ok(None)
    }

    /// Wait for (`Some(timeout)`) or poll (`None`) the next event. A read error (for
    /// example an unparsable event) is fatal only when the connection is really gone;
    /// otherwise the bad event is dropped and the loop continues.
    fn next_event(&mut self, timeout: Option<Duration>) -> Result<Option<DisplayEvent>, Box<dyn Error>> {
        let wake = self.control.as_ref().map(ControlServer::fds).unwrap_or_default();
        match self.backend.wait_event(timeout, &wake) {
            Ok(ev) => Ok(ev),
            Err(e) => {
                self.recoverable(e)?;
                std::thread::sleep(POLL_ERROR_PAUSE);
                Ok(None)
            }
        }
    }

    /// After a caught panic: hide the dialog, switch every screen to the blank pattern and
    /// continue. If recovery itself panics, stop drawing (the black windows still cover).
    fn recover_from_panic(&mut self) {
        log::error!(
            "panic in the event loop (state {:?}); switching to the blank pattern and continuing",
            self.state
        );
        let r = catch_unwind(AssertUnwindSafe(|| {
            self.hide_prompt();
            if self.state != SaverState::Idle {
                self.set_pattern(None);
            }
        }));
        if r.is_err() {
            log::error!("panic during panic recovery; drawing disabled until the next relayout");
            self.screens.clear();
            self.next_frame = None;
        }
    }

    /// An error from event handling or a timer. Errors must not end the daemon while the
    /// display connection works (exiting would drop a lock): log and continue. Only a dead
    /// connection (where the server has already removed our windows) is fatal.
    fn recoverable(&self, e: Box<dyn Error>) -> Result<(), Box<dyn Error>> {
        if let Err(conn) = self.backend.check_connection() {
            log::error!("display connection lost ({e}; {conn})");
            return Err(e);
        }
        log::error!("{e} (state {:?}); continuing", self.state);
        Ok(())
    }

    fn next_deadline(&self, now: Instant) -> Instant {
        let mut t = Some(self.next_config_check);
        if self.state == SaverState::Idle {
            t = min_opt(t, Some(self.next_idle_poll));
        }
        t = min_opt(t, self.lock_deadline);
        t = min_opt(t, self.cycle_deadline);
        t = min_opt(t, self.soft_off_deadline);
        t = min_opt(t, self.next_frame);
        t = min_opt(t, self.next_grab_retry);
        t = min_opt(t, self.next_grab_reassert);
        t = min_opt(t, self.next_relayout);
        t = min_opt(t, self.backend.next_deadline());
        let mut cap = MAX_WAIT;
        if self.state == SaverState::Locked
            && let Some(l) = &self.locker
        {
            t = min_opt(t, lockapi::next_deadline(l));
            if lockapi::is_prompt_visible(l) || lockapi::is_checking(l) {
                cap = LOCK_UI_WAIT;
            }
        }
        t.unwrap_or(later(now, cap)).min(later(now, cap))
    }

    fn handle_signals(&mut self) -> Result<Option<Exit>, Box<dyn Error>> {
        if SIG_HUP.swap(false, Ordering::SeqCst) {
            log::info!("SIGHUP: reloading configuration");
            self.reload_config();
        }
        if SIG_TERM.swap(false, Ordering::SeqCst) {
            if self.state == SaverState::Locked || self.lock_pending {
                log::warn!("SIGTERM/SIGINT received while locked (or lock pending): ignored");
            } else {
                log::info!("SIGTERM/SIGINT received: shutting down");
                self.deactivate(Why::Exit);
                return Ok(Some(Exit::Quit));
            }
        }
        Ok(None)
    }

    fn run_timers(&mut self) -> Result<(), Box<dyn Error>> {
        let now = Instant::now();

        if now >= self.next_config_check {
            self.next_config_check = later(now, CONFIG_CHECK);
            let m = mtimes(&self.watch);
            if m != self.watch_mtimes {
                log::info!("configuration file changed: reloading");
                self.reload_config();
            }
        }

        match self.state {
            SaverState::Idle => {
                if now >= self.next_idle_poll {
                    self.next_idle_poll = later(now, IDLE_POLL);
                    let since_reset = now.saturating_duration_since(self.activity_reset);
                    let idle = match self.backend.idle_time()? {
                        Some(display) => display.min(since_reset),
                        None => since_reset,
                    };
                    log::trace!("idle {} ms", idle.as_millis());
                    if self.cfg.mode != Mode::Off
                        && !self.cfg.timeout.is_zero()
                        && idle >= self.cfg.timeout
                    {
                        log::info!("idle for {}s: activating", idle.as_secs());
                        self.activate(false)?;
                    }
                }
            }
            SaverState::Blanked | SaverState::Locked => {
                if self.next_relayout.is_some_and(|t| now >= t) {
                    self.relayout()?;
                }
                self.backend.tick(now);
                if self.next_grab_retry.is_some_and(|t| now >= t) {
                    self.retry_grab();
                }
                if self.next_grab_reassert.is_some_and(|t| now >= t) {
                    self.reassert_grab();
                }
                if self.state == SaverState::Blanked && self.lock_deadline.is_some_and(|t| now >= t)
                {
                    self.lock_deadline = None;
                    log::info!("lock timeout reached: locking");
                    self.lock_now();
                }
                if self.cycle_deadline.is_some_and(|t| now >= t) {
                    self.cycle_deadline = now.checked_add(self.cfg.cycle);
                    self.cycle_pattern();
                }
                if self.soft_off_deadline.is_some_and(|t| now >= t) {
                    self.soft_power_off("DPMS timeout");
                }
                if self.state == SaverState::Locked
                    && let Some(l) = self.locker.as_mut()
                {
                    let ev = lockapi::poll(l, Instant::now());
                    self.handle_lock_event(ev)?;
                }
                if self.next_frame.is_some_and(|t| Instant::now() >= t) {
                    self.render()?;
                }
            }
        }
        self.update_status();
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Events
    // -----------------------------------------------------------------------
    fn handle_event(&mut self, ev: DisplayEvent) -> Result<Option<Exit>, Box<dyn Error>> {
        let active = self.state != SaverState::Idle;
        match ev {
            DisplayEvent::Internal => {}
            DisplayEvent::Command(cmd) => return self.command(cmd),
            DisplayEvent::Redraw(surface) => self.present_surface(surface)?,
            DisplayEvent::SurfacesDisturbed => {
                if active {
                    self.next_relayout = Some(Instant::now());
                }
            }
            DisplayEvent::ExclusiveLost(why) => {
                if active {
                    self.grab_lost(why);
                }
            }
            DisplayEvent::OutputsChanged => {
                if active {
                    self.relayout()?;
                }
            }
            // ---- input: never log these (AGENTS.md rule 3) ----
            DisplayEvent::Key(input) => match self.state {
                SaverState::Idle => {}
                SaverState::Blanked => self.blanked_activity("key press"),
                SaverState::Locked => {
                    if !self.grabbed {
                        log::trace!("KeyPress without grab ignored");
                    } else if let (Some(input), true) = (input, self.locker.is_some()) {
                        // A real DPMS monitor wakes on the key by itself; the software
                        // power-off has to be ended here.
                        self.wake_display();
                        let Some(l) = self.locker.as_mut() else {
                            return Ok(None);
                        };
                        let le = lockapi::key(l, input, Instant::now());
                        self.handle_lock_event(le)?;
                    } else {
                        // No keyboard (L6): keys can only reveal the dialog.
                        self.wake_locked()?;
                    }
                }
            },
            DisplayEvent::Button => match self.state {
                SaverState::Idle => {}
                SaverState::Blanked => self.blanked_activity("button press"),
                SaverState::Locked => self.wake_locked()?,
            },
            DisplayEvent::RawActivity => {
                // Raw presses also arrive for the grabbed events handled above; while locked
                // only the grabbed events count.
                if self.state == SaverState::Blanked {
                    self.blanked_activity("raw key/button");
                }
            }
            DisplayEvent::Motion { x, y } => {
                if self.state != SaverState::Idle && self.motion(x, y) {
                    if self.state == SaverState::Blanked {
                        self.blanked_activity("pointer moved past threshold");
                    } else {
                        self.wake_locked()?;
                    }
                }
            }
        }
        Ok(None)
    }

    /// User activity while blanked: unblank, unless a lock is pending (a pending lock is never
    /// cancelled by input; it completes as soon as the grab succeeds).
    fn blanked_activity(&mut self, what: &str) {
        if self.lock_pending {
            log::trace!("{what} while a lock is pending: ignored");
        } else {
            log::debug!("{what}: deactivating");
            self.deactivate(Why::Activity);
        }
    }

    /// Accumulate pointer travel; true once it exceeds the threshold (then resets).
    fn motion(&mut self, x: i32, y: i32) -> bool {
        if let Some((lx, ly)) = self.motion_last {
            let (dx, dy) = (f64::from(x - lx), f64::from(y - ly));
            self.motion_accum += (dx * dx + dy * dy).sqrt();
        }
        self.motion_last = Some((x, y));
        if self.motion_accum > f64::from(self.cfg.motion_threshold_px) {
            self.motion_accum = 0.0;
            true
        } else {
            false
        }
    }

    fn reset_motion(&mut self) {
        self.motion_accum = 0.0;
        self.motion_last = self.backend.pointer_position();
    }

    /// Pointer activity or DEACTIVATE while locked: wake the display and show the prompt.
    fn wake_locked(&mut self) -> Result<(), Box<dyn Error>> {
        self.wake_display();
        if self.grabbed
            && let Some(l) = self.locker.as_mut()
        {
            let le = lockapi::key(l, KeyInput::Other, Instant::now());
            self.handle_lock_event(le)?;
        }
        Ok(())
    }

    fn handle_lock_event(&mut self, ev: LockEvent) -> Result<(), Box<dyn Error>> {
        match ev {
            LockEvent::None => {}
            LockEvent::Redraw => {
                self.update_status();
                self.schedule_frames();
                self.render()?;
            }
            LockEvent::Unlocked => {
                log::info!("unlocked by PAM");
                self.deactivate(Why::Unlocked);
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Commands
    // -----------------------------------------------------------------------
    /// Answer the command being handled, on the channel it arrived on.
    fn respond(&mut self, ok: bool, msg: &str) {
        match self.reply_to.take() {
            Some(client) => client.reply(ok, msg),
            None => self.backend.respond(ok, msg),
        }
    }

    /// Handle what arrived on the control socket. Commands take the same path as those
    /// from the display's own control channel, so every refusal rule applies unchanged.
    fn serve_control(&mut self) -> Result<Option<Exit>, Box<dyn Error>> {
        let Some(server) = self.control.as_mut() else {
            return Ok(None);
        };
        let mut first_err = None;
        for (request, client) in server.poll(Instant::now()) {
            match request {
                Request::Command(cmd) => {
                    self.reply_to = Some(client);
                    let r = self.command(cmd);
                    if let Some(unanswered) = self.reply_to.take() {
                        unanswered.reply(false, "command failed");
                    }
                    match r {
                        Ok(Some(exit)) => return Ok(Some(exit)),
                        Ok(None) => {}
                        // Keep serving the other clients; report the first failure.
                        Err(e) => {
                            if let Some(earlier) = first_err.replace(e) {
                                first_err = Some(earlier);
                            }
                        }
                    }
                }
                Request::Version => client.reply(
                    true,
                    &format!("{} {}", control::DAEMON_CLASS, crate::COMPAT_XSCREENSAVER_VERSION),
                ),
                Request::Status => {
                    if let Some(server) = self.control.as_ref() {
                        client.reply(true, server.status_line());
                    }
                }
                Request::Watch => {
                    if let Some(server) = self.control.as_mut() {
                        server.watch(client);
                    }
                }
            }
        }
        first_err.map_or(Ok(None), Err)
    }

    /// Locking needs a user name for PAM. A missing keyboard does not disable locking (L6):
    /// it is retried at lock time and, failing that, the screen locks anyway.
    fn can_lock(&self) -> bool {
        // Locking never depends on the user lookup: see `lock_now` (fail closed). It does
        // depend on the display being lockable at all (not XWayland).
        self.lockable
    }

    /// Reply to a command once its action has finished: `+ok_msg`, or `-reason` on error (the
    /// error is passed on to be logged).
    fn reply_after(&mut self, ok_msg: &str, r: Result<(), Box<dyn Error>>) -> Result<(), Box<dyn Error>> {
        match r {
            Ok(()) => {
                self.respond(true, ok_msg);
                Ok(())
            }
            Err(e) => {
                let mut why = e.to_string();
                if why.len() > 120 {
                    let mut cut = 120;
                    while !why.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    why.truncate(cut);
                }
                self.respond(false, &why);
                Err(e)
            }
        }
    }

    fn command(&mut self, cmd: Command) -> Result<Option<Exit>, Box<dyn Error>> {
        log::info!("command {cmd:?} (state {:?})", self.state);
        let active = self.state != SaverState::Idle;
        match cmd {
            Command::Activate => {
                if active {
                    self.respond(false, "already active");
                } else {
                    let r = self.activate(false);
                    self.reply_after("blanking", r)?;
                }
            }
            Command::Deactivate => {
                self.activity_reset = Instant::now();
                match self.state {
                    SaverState::Idle => {
                        self.respond(true, "already inactive, resetting activity time");
                    }
                    SaverState::Blanked if self.lock_pending => {
                        // A lock is waiting for the grab: behave as if locked.
                        log::info!("DEACTIVATE while a lock is pending: keeping the screen blanked");
                        self.respond(true, "deactivating");
                        self.wake_display();
                    }
                    SaverState::Blanked => {
                        self.respond(true, "deactivating");
                        self.deactivate(Why::Command);
                    }
                    SaverState::Locked => {
                        // xscreensaver keeps the lock; it only wakes up and shows the dialog.
                        self.respond(true, "deactivating");
                        self.wake_locked()?;
                    }
                }
            }
            Command::Cycle => {
                if active {
                    self.respond(true, "cycling");
                    self.cycle_pattern();
                } else {
                    self.respond(false, "not blanked");
                }
            }
            Command::Next | Command::Prev => {
                let n = self.cfg.patterns.len();
                let step = |i: usize| {
                    if cmd == Command::Next { (i + 1) % n } else { (i + n - 1) % n }
                };
                let next = step(self.current.unwrap_or(self.cfg.selected));
                if active {
                    self.respond(true, "cycling");
                    self.set_pattern(Some(next));
                } else {
                    self.current = Some(next);
                    let r = self.activate_with(false, Some(next));
                    self.reply_after("blanking", r)?;
                }
            }
            Command::Select(n) => {
                let idx = n as usize;
                if idx == 0 || idx > self.cfg.patterns.len() {
                    log::warn!(
                        "SELECT {n}: only {} pattern(s) configured",
                        self.cfg.patterns.len()
                    );
                    self.respond(false, "no such hack");
                } else if active {
                    self.respond(true, "selecting");
                    self.set_pattern(Some(idx - 1));
                } else {
                    let r = self.activate_with(false, Some(idx - 1));
                    self.reply_after("blanking", r)?;
                }
            }
            Command::Lock => {
                if self.state == SaverState::Locked {
                    self.respond(false, "already locked");
                } else if !self.can_lock() {
                    self.respond(false, "locking is not supported on this display");
                } else {
                    // Reply only once the outcome is known: "+locking" means Locked, which in
                    // turn means both grabs are held. Otherwise the lock stays pending (status
                    // BLANK) and completes when a grab retry succeeds.
                    let r = if active {
                        self.lock_now();
                        Ok(())
                    } else {
                        self.activate(true)
                    };
                    if let Err(e) = r {
                        self.reply_after("locking", Err(e))?;
                    } else if self.state == SaverState::Locked {
                        self.respond(true, "locking");
                    } else {
                        let why = if self.lock_is_separate {
                            "unable to lock the session"
                        } else {
                            "unable to grab keyboard"
                        };
                        self.respond(false, why);
                    }
                }
            }
            Command::Suspend => {
                if suspend_allowed(self.state, self.lock_pending) {
                    let r = if active { Ok(()) } else { self.activate(false) };
                    self.reply_after("suspending", r)?;
                    self.dpms_off();
                } else {
                    log::warn!("SUSPEND refused: screen is locked or a lock is pending");
                    self.respond(false, "screen is locked");
                }
            }
            Command::Exit | Command::Restart => {
                if self.state == SaverState::Locked || self.lock_pending {
                    log::warn!("{cmd:?} refused: screen is locked or a lock is pending");
                    self.respond(false, "screen is locked");
                } else {
                    let restart = cmd == Command::Restart;
                    let via_backend = self.reply_to.is_none();
                    self.respond(true, if restart { "restarting" } else { "exiting" });
                    if via_backend {
                        self.backend.flush_response(Duration::from_secs(1));
                    }
                    self.deactivate(Why::Exit);
                    return Ok(Some(if restart { Exit::Restart } else { Exit::Quit }));
                }
            }
            Command::Demo | Command::Prefs => {
                self.respond(false, "not supported");
            }
        }
        self.update_status();
        Ok(None)
    }

    // -----------------------------------------------------------------------
    // Activation
    // -----------------------------------------------------------------------
    fn initial_pattern(&mut self) -> Option<usize> {
        match self.cfg.mode {
            Mode::Blank => None,
            Mode::One | Mode::Off => Some(self.cfg.selected),
            Mode::Random => Some(self.rng.next_u32() as usize % self.cfg.patterns.len()),
        }
    }

    fn pattern_name(&self, idx: Option<usize>) -> &str {
        idx.and_then(|i| self.cfg.patterns.get(i)).map_or("blank", String::as_str)
    }

    fn activate(&mut self, lock_now: bool) -> Result<(), Box<dyn Error>> {
        let p = self.initial_pattern();
        self.activate_with(lock_now, p)
    }

    fn activate_with(&mut self, lock_now: bool, pattern: Option<usize>) -> Result<(), Box<dyn Error>> {
        if self.state != SaverState::Idle {
            if lock_now {
                self.lock_now();
            }
            return Ok(());
        }
        let monitors = self.backend.blank()?;
        self.state = SaverState::Blanked;
        self.current = pattern;
        self.grabbed = false;
        self.lock_pending = false;
        self.try_grab(GRAB_TIMEOUT, false);

        // Decide the lock state now that the windows are mapped and the grab attempted, before
        // any fallible rendering setup.
        let now = Instant::now();
        self.lock_deadline = None;
        if lock_now {
            self.lock_now();
        } else if self.cfg.lock {
            self.lock_deadline = now.checked_add(self.cfg.lock_timeout);
        }
        self.cycle_deadline = (self.cfg.mode == Mode::Random
            && self.cfg.patterns.len() > 1
            && !self.cfg.cycle.is_zero())
        .then(|| now.checked_add(self.cfg.cycle))
        .flatten();

        self.build_screens();
        self.reset_motion();
        log::info!(
            "blanked: pattern {} on {} monitor(s){}",
            self.pattern_name(self.current),
            monitors,
            if self.grabbed { "" } else { " (no grab)" }
        );

        self.dpms_activate();
        self.update_status();
        self.last_frame = Instant::now();
        self.schedule_frames();
        if let Err(e) = self.render() {
            log::error!("render failed: {e}");
        }
        Ok(())
    }

    /// Try to take exclusive keyboard+pointer input. On failure schedule a retry. Once a
    /// lock is wanted (locked, pending, or `for_lock`) only lock-grade input counts.
    fn try_grab(&mut self, timeout: Duration, for_lock: bool) {
        if self.state == SaverState::Idle {
            return;
        }
        let for_lock = for_lock || self.state == SaverState::Locked || self.lock_pending;
        let now = Instant::now();
        match self.backend.acquire_exclusive(timeout, for_lock) {
            Ok(()) => {
                if !self.grabbed {
                    if self.lock_is_separate {
                        log::debug!("exclusive input held ({})", if for_lock { "session lock" } else { "saver surfaces" });
                    } else {
                        log::debug!("keyboard and pointer grabbed");
                    }
                }
                self.grabbed = true;
                self.grab_secure = for_lock || !self.lock_is_separate;
                self.next_grab_retry = None;
                self.next_grab_reassert = Some(later(now, GRAB_REASSERT));
            }
            Err(e) => {
                self.grabbed = false;
                self.grab_secure = false;
                self.next_grab_reassert = None;
                if self.next_grab_retry.is_none() {
                    if self.state == SaverState::Locked || self.lock_pending {
                        log::warn!("{e}: lock pending, retrying every second");
                    } else {
                        log::warn!("{e}: blanking without grab, retrying every second");
                    }
                }
                self.next_grab_retry = Some(later(now, GRAB_RETRY));
            }
        }
    }

    fn retry_grab(&mut self) {
        self.try_grab(Duration::from_millis(100), false);
        if self.grabbed {
            log::info!("grab acquired");
            if self.lock_pending {
                self.lock_now();
            }
            self.update_status();
            if let Err(e) = self.render() {
                log::warn!("render failed: {e}");
            }
        }
    }

    /// Periodic check (while locked) that the grab is still ours. Grab requests from the
    /// current grabber are idempotent and generate no focus events.
    fn reassert_grab(&mut self) {
        self.next_grab_reassert = None;
        if !self.grabbed {
            return;
        }
        if self.state != SaverState::Locked {
            return;
        }
        match self.backend.check_exclusive() {
            Ok(true) => {
                self.next_grab_reassert = Some(later(Instant::now(), GRAB_REASSERT));
            }
            Ok(false) => self.grab_lost("periodic re-grab failed"),
            Err(e) => self.grab_lost(&format!("periodic re-grab failed: {e}")),
        }
    }

    /// The grab was (or may have been) broken: hide the dialog so nothing more is typed into
    /// it, re-map and raise our windows, and re-grab at once (then every second).
    fn grab_lost(&mut self, why: &str) {
        log::warn!("grab lost ({why}): hiding the unlock dialog and re-grabbing");
        self.grabbed = false;
        self.next_grab_reassert = None;
        self.hide_prompt();
        self.backend.restore_cover();
        self.try_grab(Duration::from_millis(200), false);
        if self.grabbed {
            log::info!("grab re-acquired");
            if self.lock_pending {
                self.lock_now();
            }
        }
        self.update_status();
        if let Err(e) = self.render() {
            log::warn!("render failed: {e}");
        }
    }

    /// Hide the unlock dialog (as Escape would). Never reveals a hidden dialog.
    fn hide_prompt(&mut self) {
        if let Some(l) = self.locker.as_mut()
            && lockapi::is_prompt_visible(l)
            && !lockapi::is_checking(l)
        {
            let _ = lockapi::key(l, KeyInput::Escape, Instant::now());
        }
    }

    /// (Re)create the drawing state for every surface. Failures are not fatal: a monitor
    /// whose surface cannot be set up for drawing gets no `Screen` and stays black.
    fn build_screens(&mut self) {
        self.screens.clear();
        self.backend.detach_all();
        let name = self.pattern_name(self.current).to_owned();
        for (surface, mon) in self.backend.surfaces() {
            let (w, h) = (u32::from(mon.width), u32::from(mon.height));
            if let Err(e) = self.backend.attach(surface, w, h) {
                log::error!("cannot set up drawing for monitor {mon:?} ({e}); it stays black");
                continue;
            }
            let fb = match Framebuffer::try_new(w, h) {
                Ok(fb) => fb,
                Err(e) => {
                    log::error!("cannot allocate a {w}x{h} framebuffer for monitor {mon:?} ({e}); it stays black");
                    self.backend.detach(surface);
                    continue;
                }
            };
            let mut rng = Rng::new(self.rng.next_u32() as u64 | (u64::from(self.rng.next_u32()) << 32));
            let mut pattern = patterns::by_name(&name).unwrap_or_else(|| {
                log::warn!("unknown pattern {name:?}: using clock");
                patterns::by_name("clock").expect("clock pattern exists")
            });
            pattern.init(w, h, self.cfg.oled.lit_budget, &mut rng);
            self.screens.push(Screen {
                surface,
                mon,
                fb,
                pattern,
                oled: OledGuard::new(self.cfg.oled.clone()),
                rng,
            });
        }
    }

    fn set_pattern(&mut self, idx: Option<usize>) {
        self.current = idx;
        log::info!("pattern {}", self.pattern_name(idx));
        if self.state == SaverState::Idle {
            return;
        }
        let name = self.pattern_name(idx).to_owned();
        for s in &mut self.screens {
            let mut p = patterns::by_name(&name).unwrap_or_else(|| {
                log::warn!("unknown pattern {name:?}: using clock");
                patterns::by_name("clock").expect("clock pattern exists")
            });
            p.init(s.fb.width, s.fb.height, self.cfg.oled.lit_budget, &mut s.rng);
            s.pattern = p;
        }
        self.status_dirty = true; // rewrite the status with the new hack index
        self.update_status();
        self.schedule_frames();
        if let Err(e) = self.render() {
            log::warn!("render failed: {e}");
        }
    }

    fn cycle_pattern(&mut self) {
        let n = self.cfg.patterns.len();
        let next = match self.cfg.mode {
            Mode::Blank => None,
            Mode::Random if n > 1 => {
                let cur = self.current.unwrap_or(0);
                let r = self.rng.next_u32() as usize % (n - 1);
                Some(if r >= cur { r + 1 } else { r })
            }
            _ => Some((self.current.unwrap_or(self.cfg.selected) + 1) % n),
        };
        self.set_pattern(next);
    }

    /// Transition Blanked -> Locked. Only happens while both grabs are held; otherwise the
    /// lock becomes pending (status stays BLANK) and `retry_grab` completes it. Returns true
    /// when locked.
    fn lock_now(&mut self) -> bool {
        match self.state {
            SaverState::Locked => return true,
            SaverState::Idle => return false,
            SaverState::Blanked => {}
        }
        if !self.lockable {
            // Single choke point: whatever asked for the lock (command, lock timeout,
            // --lock), an XWayland display is never reported as locked.
            log::warn!("lock requested, but this display cannot be locked: staying blanked");
            self.lock_deadline = None;
            return false;
        }
        if self.user.is_none() {
            // The lookup failed at startup (NIS/LDAP outage?); try once more now.
            self.user = lockapi::current_username();
        }
        if !(self.grabbed && self.grab_secure) {
            self.try_grab(Duration::from_millis(500), true);
        }
        if !(self.grabbed && self.grab_secure) {
            // Input that is not lock-grade does not count as held from here on.
            self.grabbed = false;
            if !self.lock_pending {
                log::warn!(
                    "cannot grab keyboard and pointer: lock pending, staying blanked until the grab succeeds"
                );
            }
            let newly_pending = !std::mem::replace(&mut self.lock_pending, true);
            self.lock_deadline = None;
            if newly_pending {
                // The "NOT LOCKED" notice must appear now, also when nothing else would draw
                // a frame (blank pattern, software power-off).
                self.schedule_frames();
                if let Err(e) = self.render() {
                    log::warn!("render failed: {e}");
                }
            }
            return false;
        }
        self.backend.prepare_keyboard();
        self.locker = Some(match self.user.clone() {
            Some(user) => lockapi::new_locker(user, self.cfg.pam_service.clone(), self.cfg.passwd_timeout),
            None => {
                // Fail closed: lock, but no attempt can succeed because we do not know whom to
                // authenticate. Better than leaving the screen unlocked.
                log::error!(
                    "user name unknown: locking anyway; every unlock attempt will be denied until \
                     the daemon is restarted from another session"
                );
                lockapi::new_denying_locker(self.cfg.passwd_timeout)
            }
        });
        self.state = SaverState::Locked;
        self.lock_pending = false;
        self.lock_deadline = None;
        self.next_grab_reassert = Some(later(Instant::now(), GRAB_REASSERT));
        log::info!("locked (pattern {})", self.pattern_name(self.current));
        self.update_status();
        true
    }

    /// Unblank. While locked this only proceeds for `Why::Unlocked`.
    fn deactivate(&mut self, why: Why) {
        if self.state == SaverState::Locked && why != Why::Unlocked {
            log::warn!("refusing to unblank while locked ({why:?})");
            return;
        }
        if self.state == SaverState::Idle {
            return;
        }
        self.teardown_windows();
        self.restore_dpms();
        self.soft_off = false;
        self.soft_off_deadline = None;
        self.state = SaverState::Idle;
        self.locker = None;
        self.lock_deadline = None;
        self.cycle_deadline = None;
        self.next_frame = None;
        self.next_grab_retry = None;
        self.next_grab_reassert = None;
        self.next_relayout = None;
        self.lock_pending = false;
        let now = Instant::now();
        self.activity_reset = now;
        self.next_idle_poll = later(now, IDLE_POLL);
        self.update_status();
        log::info!("unblanked ({why:?})");
    }

    fn teardown_windows(&mut self) {
        self.backend.release_exclusive();
        self.grabbed = false;
        self.screens.clear();
        self.backend.unblank();
    }

    /// Recreate the surfaces for the current monitor layout; exclusive input is not affected.
    /// On failure the backend keeps the old surfaces, stretched so nothing is left uncovered,
    /// and a retry is scheduled.
    fn relayout(&mut self) -> Result<(), Box<dyn Error>> {
        self.next_relayout = None;
        let now = Instant::now();
        match self.backend.relayout() {
            Ok(n) => log::info!("monitor layout changed: {n} monitor(s)"),
            Err(e) => {
                log::error!("relayout failed ({e}); keeping old windows, retrying in 1 s");
                self.next_relayout = Some(later(now, RELAYOUT_RETRY));
            }
        }
        if self.grabbed {
            // Cheap confirmation that the grab survived.
            match self.backend.check_exclusive() {
                Ok(true) => {}
                Ok(false) => self.grab_lost("re-grab after relayout failed"),
                Err(e) => self.grab_lost(&format!("re-grab after relayout failed: {e}")),
            }
        } else {
            self.try_grab(Duration::from_millis(500), false);
            if self.grabbed && self.lock_pending {
                self.lock_now();
            }
        }
        self.build_screens();
        self.reset_motion();
        self.update_status();
        self.schedule_frames();
        self.render()?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Status, rendering
    // -----------------------------------------------------------------------
    fn desired_status(&self) -> Status {
        match self.state {
            SaverState::Idle => Status::Off,
            SaverState::Blanked => Status::Blank,
            // Like xscreensaver: AUTH whenever the unlock dialog is up (or PAM runs).
            SaverState::Locked => {
                if self
                    .locker
                    .as_ref()
                    .is_some_and(|l| lockapi::is_prompt_visible(l) || lockapi::is_checking(l))
                {
                    Status::Auth
                } else {
                    Status::Lock
                }
            }
        }
    }

    /// Publish the status when the advertised state changed.
    fn update_status(&mut self) {
        let want = self.desired_status();
        if want == self.status && !self.status_dirty {
            return;
        }
        let hack = self.current.map_or(0, |i| i as u32 + 1);
        let hacks = vec![hack; self.backend.surfaces().len().max(1)];
        let locked = |s: Status| matches!(s, Status::Lock | Status::Auth);
        let since = if want == self.status || (locked(want) && locked(self.status)) {
            self.status_since
        } else {
            SystemTime::now()
        };
        if let Some(server) = self.control.as_mut() {
            server.set_status(want, since, &hacks);
        }
        match self.backend.publish_status(want, since, &hacks) {
            Ok(()) => {
                self.status_since = since;
                self.status_dirty = false;
                log::debug!("status {want:?} hacks {hacks:?}");
                self.status = want;
            }
            Err(e) => log::warn!("cannot set status: {e}"),
        }
    }

    fn dialog_visible(&self) -> bool {
        self.state == SaverState::Locked
            && self.locker.as_ref().is_some_and(|l| {
                lockapi::is_prompt_visible(l) || lockapi::is_checking(l)
            })
    }

    fn frame_interval(&self) -> Option<Duration> {
        // A dark screen has nothing to animate: draw it once, like the blank pattern.
        let wants = if self.dark() { 0 } else { self.screens.first().map_or(0, |s| s.pattern.wants_fps()) };
        let mut fps = if wants == 0 { 0 } else { wants.min(self.cfg.fps).max(1) };
        if self.dialog_visible() {
            fps = fps.max(MIN_DIALOG_FPS);
        }
        if self.lock_pending && !self.grabbed {
            // Keep the "NOT LOCKED" notice drawn and drifting even over a still pattern.
            fps = fps.max(1);
        }
        (fps > 0).then(|| Duration::from_secs(1) / fps)
    }

    fn schedule_frames(&mut self) {
        self.next_frame = self.frame_interval().map(|iv| later(Instant::now(), iv));
    }

    fn render(&mut self) -> Result<(), Box<dyn Error>> {
        if self.state == SaverState::Idle {
            return Ok(());
        }
        let now = Instant::now();
        let dt = now.saturating_duration_since(self.last_frame).min(Duration::from_millis(250));
        self.last_frame = now;
        let dialog_on = dialog_screen(&self.screens);
        let show_dialog = self.state == SaverState::Locked && self.grabbed;
        let not_locked = self.lock_pending && !self.grabbed;
        let not_locked_text = if self.lock_is_separate {
            "NOT LOCKED: the compositor has not locked the session"
        } else {
            "NOT LOCKED: another program holds the keyboard or mouse"
        };
        let no_keyboard = !self.backend.has_keyboard();
        let dark = self.dark();
        for (i, s) in self.screens.iter_mut().enumerate() {
            s.fb.clear();
            // Software power-off: leave the frame pure black. The dialog and the notice
            // below are still drawn, so the lock state is never hidden.
            if !dark {
                s.pattern.step(dt, &mut s.fb, &mut s.rng);
                s.oled.tick(dt);
                s.oled.apply(&mut s.fb);
            }
            // The dialog is drawn after the OLED pass, so the rest interlude never blanks it
            // while the user is typing (it is short-lived and dim already).
            if show_dialog && i == dialog_on {
                if let Some(l) = &self.locker {
                    lockapi::draw(&mut s.fb, l, now);
                }
                if no_keyboard {
                    draw_notice(&mut s.fb, "keyboard unavailable: cannot unlock", now);
                }
            }
            if not_locked {
                // Never let a black screen pass for a lock: another client holds a grab and
                // keystrokes may reach it.
                draw_notice(&mut s.fb, not_locked_text, now);
            }
            self.backend.present(s.surface, &s.fb)?;
        }
        log::trace!("frame dt={}ms", dt.as_millis());
        self.next_frame = self.frame_interval().map(|iv| later(now, iv));
        Ok(())
    }

    fn present_surface(&mut self, surface: SurfaceId) -> Result<(), Box<dyn Error>> {
        if let Some(s) = self.screens.iter().find(|s| s.surface == surface) {
            self.backend.present(s.surface, &s.fb)?;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Monitor power: the backend's (DPMS) when it has any, else the software stand-in
    // -----------------------------------------------------------------------
    fn dpms_activate(&mut self) {
        if !self.has_power {
            self.arm_soft_off(true);
            return;
        }
        if !self.cfg.dpms.enabled {
            return;
        }
        self.backend.power_install(&self.cfg.dpms);
    }

    /// True while frames must be pure black. A pending lock keeps the pattern running so
    /// its "NOT LOCKED" notice stays animated and obvious.
    fn dark(&self) -> bool {
        self.soft_off && !self.lock_pending
    }

    /// (Re)start the software power-off timer on a display without power control. `blanking` is true
    /// when the screen has just been blanked, which is the only moment `quick_off` applies
    /// (with real DPMS it forces the monitor off once and the timeouts take over after a
    /// wake-up).
    fn arm_soft_off(&mut self, blanking: bool) {
        self.soft_off_deadline = None;
        if self.has_power || self.state == SaverState::Idle {
            return;
        }
        match soft_off_delay(&self.cfg.dpms, blanking) {
            Some(d) if d.is_zero() => self.soft_off = true,
            Some(d) => self.soft_off_deadline = Instant::now().checked_add(d),
            None => {}
        }
    }

    /// Stand-in for DPMS "off": stop drawing the pattern and show pure black.
    fn soft_power_off(&mut self, why: &str) {
        self.soft_off_deadline = None;
        if self.state == SaverState::Idle || self.soft_off {
            return;
        }
        self.soft_off = true;
        log::info!("{why}: no DPMS extension, showing a black screen instead");
        if let Err(e) = self.render() {
            log::warn!("render failed: {e}");
        }
    }

    /// Wake the display for the user: force a DPMS monitor on, or end the software
    /// power-off and start its timer again, as the server's idle timeouts would.
    fn wake_display(&mut self) {
        self.backend.power_on();
        if self.has_power || self.state == SaverState::Idle {
            return;
        }
        let was_off = std::mem::replace(&mut self.soft_off, false);
        self.arm_soft_off(false);
        if was_off {
            self.last_frame = Instant::now();
            if let Err(e) = self.render() {
                log::warn!("render failed: {e}");
            }
        }
    }

    fn dpms_off(&mut self) {
        if !self.has_power {
            self.soft_power_off("SUSPEND");
            return;
        }
        self.backend.power_off();
    }

    fn restore_dpms(&mut self) {
        if !self.has_power {
            return;
        }
        self.backend.power_restore();
    }

    // -----------------------------------------------------------------------
    // Config reload
    // -----------------------------------------------------------------------
    fn reload_config(&mut self) {
        self.watch_mtimes = mtimes(&self.watch);
        let mut new = match Config::load(&self.args) {
            Ok(c) => c,
            Err(e) => {
                log::warn!("config reload failed, keeping old settings: {e}");
                return;
            }
        };
        // The display and logging setup cannot change at runtime.
        new.display = self.cfg.display.clone();
        new.backend = self.cfg.backend;
        if new == self.cfg {
            log::debug!("configuration unchanged");
            return;
        }
        // Keep `current` valid for the new pattern list.
        if let Some(i) = self.current
            && i >= new.patterns.len()
        {
            self.current = Some(0);
        }
        log::info!(
            "configuration reloaded: timeout {}s, cycle {}s, lock {} (after {}s), mode {:?}, patterns {:?}",
            new.timeout.as_secs(),
            new.cycle.as_secs(),
            new.lock,
            new.lock_timeout.as_secs(),
            new.mode,
            new.patterns
        );
        self.cfg = new;
        self.watch = self.cfg.watch_paths();
        self.watch_mtimes = mtimes(&self.watch);
        // Never alter the lock state on reload; only refresh timers that are still pending.
        if self.state != SaverState::Idle {
            let now = Instant::now();
            if self.cycle_deadline.is_some() {
                self.cycle_deadline = (!self.cfg.cycle.is_zero()).then(|| now.checked_add(self.cfg.cycle)).flatten();
            }
            if !self.soft_off {
                self.arm_soft_off(false);
            }
        }
    }
}

/// One line of dim text centred near the bottom of `fb`, drifting a little (OLED).
fn draw_notice(fb: &mut Framebuffer, text: &str, now: Instant) {
    use crate::render::font;
    let scale = 2;
    let w = font::text_width(text, scale);
    let t = now.duration_since(*EPOCH).as_secs() as i32;
    let drift = (t / 7) % 24 - 12;
    let x = (fb.width as i32 - w) / 2 + drift;
    let y = fb.height as i32 - 40 + ((t / 11) % 10 - 5);
    font::draw_text(fb, x.max(0), y.max(0), scale, crate::render::rgb(0x80, 0x30, 0x30), text);
}

static EPOCH: std::sync::LazyLock<Instant> = std::sync::LazyLock::new(Instant::now);

/// Which screen gets the unlock dialog: the primary monitor if it is big enough to show the
/// panel, else the largest. The primary flag is client-settable (RRSetMonitor), so a tiny
/// "primary" must not be able to hide the dialog.
fn dialog_screen(screens: &[Screen]) -> usize {
    let geo: Vec<(u32, u32, bool)> = screens.iter().map(|s| (s.fb.width, s.fb.height, s.mon.primary)).collect();
    pick_dialog(&geo)
}

/// `(width, height, primary)` per screen -> index that shows the dialog.
fn pick_dialog(screens: &[(u32, u32, bool)]) -> usize {
    const MIN_W: u32 = 320;
    const MIN_H: u32 = 200;
    let big = |s: &(u32, u32, bool)| s.0 >= MIN_W && s.1 >= MIN_H;
    if let Some(i) = screens.iter().position(|s| s.2 && big(s)) {
        return i;
    }
    screens
        .iter()
        .enumerate()
        .max_by_key(|(_, s)| u64::from(s.0) * u64::from(s.1))
        .map_or(0, |(i, _)| i)
}

/// How long after blanking (or after a wake-up) a server without DPMS should go black, as
/// the stand-in for powering the monitor off: `None` when DPMS management is disabled,
/// zero for `quick_off` at the moment of blanking, else the shortest non-zero timeout
/// (on a real monitor the first DPMS stage already removes the picture; zero means
/// "never" for a DPMS timeout).
fn soft_off_delay(dpms: &crate::config::DpmsConfig, blanking: bool) -> Option<Duration> {
    if !dpms.enabled {
        return None;
    }
    if blanking && dpms.quick_off {
        return Some(Duration::ZERO);
    }
    [dpms.standby, dpms.suspend, dpms.off].into_iter().filter(|d| !d.is_zero()).min()
}

/// SUSPEND may not power the display off while the user must authenticate or while a lock
/// is pending (audit finding F4): the "NOT LOCKED" notice or the unlock dialog would vanish
/// with the screen over the user's head. Same rule as EXIT/RESTART: locked or pending
/// counts as locked.
fn suspend_allowed(state: SaverState, lock_pending: bool) -> bool {
    !(state == SaverState::Locked || lock_pending)
}


/// `--demo-blank` (debug): black windows on every monitor for 3 seconds, then exit.
pub fn demo_blank(display: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    if wayland_session() {
        return Err(DEMO_NEEDS_X11.into());
    }
    crate::x11::backend::demo_blank(display)
}

#[cfg(test)]
mod dialog_tests {
    use super::{SaverState, pick_dialog, soft_off_delay, suspend_allowed};
    use crate::config::DpmsConfig;
    use std::time::Duration;

    #[test]
    fn primary_wins_only_when_big_enough() {
        assert_eq!(pick_dialog(&[(1920, 1080, false), (1280, 800, true)]), 1);
        // A 4x4 "primary" planted via RRSetMonitor cannot hide the dialog.
        assert_eq!(pick_dialog(&[(1920, 1080, false), (4, 4, true)]), 0);
        assert_eq!(pick_dialog(&[(100, 100, false), (200, 150, false)]), 1);
        assert_eq!(pick_dialog(&[]), 0);
    }

    #[test]
    fn soft_off_delay_follows_the_dpms_settings() {
        let s = Duration::from_secs;
        let mut d = DpmsConfig { enabled: false, standby: s(600), suspend: s(900), off: s(1200), quick_off: true };
        assert_eq!(soft_off_delay(&d, true), None, "disabled: never");
        d.enabled = true;
        assert_eq!(soft_off_delay(&d, true), Some(Duration::ZERO), "quick_off applies when blanking");
        assert_eq!(soft_off_delay(&d, false), Some(s(600)), "after a wake-up the timeouts apply");
        d.quick_off = false;
        assert_eq!(soft_off_delay(&d, true), Some(s(600)), "shortest timeout");
        d.standby = Duration::ZERO;
        assert_eq!(soft_off_delay(&d, true), Some(s(900)), "zero means never for that stage");
        (d.suspend, d.off) = (Duration::ZERO, Duration::ZERO);
        assert_eq!(soft_off_delay(&d, true), None, "all stages off");
    }

    #[test]
    fn suspend_refused_while_locked_or_pending() {
        assert!(suspend_allowed(SaverState::Idle, false));
        assert!(suspend_allowed(SaverState::Blanked, false));
        assert!(!suspend_allowed(SaverState::Blanked, true), "lock pending counts as locked");
        assert!(!suspend_allowed(SaverState::Locked, false));
        assert!(!suspend_allowed(SaverState::Locked, true));
    }
}
