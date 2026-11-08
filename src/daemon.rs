//! Daemon event loop and Idle -> Blanked -> Locked state machine. Owner: Opus F.
//!
//! Single-threaded: `X11::wait_event_timeout` is the only blocking point. Every timer
//! (idle poll, lock, cycle, frame, config reload, grab retry, locker deadline) is an explicit
//! `Instant` and the wait is bounded by the nearest one (and by 1 s, so signal flags are seen
//! promptly; 100 ms while the unlock dialog is up or PAM is running on its worker thread).
//!
//! Lock invariants (AGENTS.md rule 3): the only path out of `SaverState::Locked` is
//! `LockEvent::Unlocked` from the locker; EXIT/RESTART/SIGTERM/SIGINT are refused while locked;
//! keys and pointer wake-ups reach the locker only while we hold the keyboard+pointer grab.
//!
//! Grabs: keyboard and pointer are grabbed on the root window (`grab.rs`), so recreating the
//! saver windows on a RANDR change never drops them. `SaverState::Locked` (status LOCK, reply
//! `+locking`) is entered only while both grabs are held; otherwise the lock stays pending
//! (`lock_pending`, status BLANK) and the grab is retried every second; activity does not cancel
//! a pending lock. A broken grab (FocusOut with mode Ungrab, our window unmapped, or a failed
//! re-assert every 2 s while locked) hides the dialog and re-grabs at once.
//!
//! Robustness: each loop iteration runs under `catch_unwind`; a panic switches to the blank
//! pattern, hides the dialog and continues. Only a failing `sync()` (dead connection) ends the
//! loop with an error.

use std::error::Error;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::dpms;
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, CreateWindowAux, EventMask, NotifyMode,
    Place, PropMode, Visibility, Window, WindowClass,
};
use x11rb::wrapper::ConnectionExt as _;

use crate::cli::Args;
use crate::compat::protocol::{self, Command, Status};
use crate::config::{Config, Mode};
use crate::patterns::{self, Pattern, Rng};
use crate::render::Framebuffer;
use crate::render::oled::OledGuard;
use crate::x11::conn::X11;
use crate::x11::present::Presenter;
use crate::x11::saver_window::{self, SaverWindows};
use crate::x11::{Monitor, X11Error, grab, idle, monitors};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaverState {
    Idle,
    Blanked,
    Locked,
}

/// Program name published in WM_CLASS res_name / WM_COMMAND of the rendezvous window.
const PROGNAME: &str = "nightsaver";
const IDLE_POLL: Duration = Duration::from_secs(1);
const CONFIG_CHECK: Duration = Duration::from_secs(5);
const GRAB_RETRY: Duration = Duration::from_secs(1);
/// How long activation waits for the grab (xscreensaver tries 4 x 1 s). Command replies are
/// sent after activation, and xscreensaver-command gives up after about 10 s.
const GRAB_TIMEOUT: Duration = Duration::from_secs(3);
/// Re-assert the grab this often while locked (detects a grab broken behind our back).
const GRAB_REASSERT: Duration = Duration::from_secs(2);
/// Periodic re-raise of the saver windows while active (backstop for missed restacks).
const RAISE_PERIOD: Duration = Duration::from_secs(5);
/// Minimum spacing of raises, so two always-on-top clients cannot start a raise storm.
const RAISE_MIN_GAP: Duration = Duration::from_millis(250);
/// Retry delay after a failed relayout.
const RELAYOUT_RETRY: Duration = Duration::from_secs(1);
/// Back-off after an error from polling the X connection while `sync()` still works.
const POLL_ERROR_PAUSE: Duration = Duration::from_millis(10);
/// Upper bound for one wait, so signal flags are noticed promptly.
const MAX_WAIT: Duration = Duration::from_secs(1);
/// Wait bound while the dialog is visible or PAM runs (worker thread does not wake poll()).
const LOCK_UI_WAIT: Duration = Duration::from_millis(100);
const MIN_DIALOG_FPS: u32 = 10;

// ---------------------------------------------------------------------------
// Lock / keyboard adapter: the only place that touches the lock and keyboard APIs
// (kept as one seam so API changes in lock/ or x11/keyboard.rs stay local).
// ---------------------------------------------------------------------------
mod lockapi {
    use std::time::{Duration, Instant};

    use x11rb::protocol::Event;
    use x11rb::protocol::xproto::KeyPressEvent;

    use crate::lock::{LockEvent, Locker};
    use crate::render::Framebuffer;
    use crate::x11::X11Error;
    use crate::x11::conn::X11;
    use crate::x11::keyboard::{KeyInput, Keyboard};

    pub fn new_locker(user: String, service: String, passwd_timeout: Duration) -> Locker {
        Locker::new(user, service, passwd_timeout)
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
    pub fn keyboard_new(x: &X11) -> Result<Keyboard, X11Error> {
        Keyboard::new(x)
    }
    pub fn keyboard_event(kb: &mut Keyboard, ev: &Event) -> bool {
        kb.handle_event(ev)
    }
    pub fn keyboard_refresh(kb: &mut Keyboard, x: &X11) {
        if let Err(e) = kb.reload_if_stale(x) {
            log::warn!("keymap reload failed: {e}");
        }
    }
    pub fn translate(kb: &Keyboard, ev: &KeyPressEvent) -> KeyInput {
        kb.translate(ev)
    }
}

use crate::lock::{LockEvent, Locker};
use crate::x11::keyboard::{KeyInput, Keyboard};

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
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------
fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: `buf` is a valid writable buffer of the length we pass; gethostname writes at
    // most that many bytes and we only read up to the first NUL (or the whole buffer).
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return "localhost".into();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let s = String::from_utf8_lossy(&buf[..end]).into_owned();
    if s.is_empty() { "localhost".into() } else { s }
}

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

fn secs_u16(d: Duration) -> u16 {
    d.as_secs().min(u64::from(u16::MAX)) as u16
}

/// Everything drawn on one monitor.
struct Screen {
    win: Window,
    mon: Monitor,
    fb: Framebuffer,
    presenter: Presenter,
    pattern: Box<dyn Pattern>,
    oled: OledGuard,
    rng: Rng,
}

/// Server DPMS settings saved at activation, restored at deactivation.
#[derive(Debug, Clone, Copy)]
struct SavedDpms {
    standby: u16,
    suspend: u16,
    off: u16,
    enabled: bool,
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
    x: X11,
    /// Screen 0 root: `_SCREENSAVER_STATUS` lives here.
    status_root: Window,
    /// Hidden rendezvous window carrying `_SCREENSAVER_VERSION`.
    ident: Window,
    state: SaverState,
    windows: SaverWindows,
    screens: Vec<Screen>,
    grabbed: bool,
    /// A lock was requested (LOCK command or lock timeout) but the grab is not held yet.
    lock_pending: bool,
    /// Index into `cfg.patterns` of the running pattern (`None` for blank mode).
    current: Option<usize>,
    rng: Rng,
    user: Option<String>,
    keyboard: Option<Keyboard>,
    locker: Option<Locker>,
    status: Status,
    /// Time published with the status; kept across LOCK <-> AUTH like xscreensaver's locked_at.
    status_since: SystemTime,
    /// Low 16 bits of a request sequence number sent right after our last successful grab.
    /// Focus events older than it were caused by an earlier grab break we already handled.
    grab_seq: u16,
    /// Force the next `update_status` to write even if the status is unchanged.
    status_dirty: bool,
    /// Baseline for idle detection: idle = min(server idle, now - activity_reset).
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
    next_periodic_raise: Option<Instant>,
    /// A raise requested while rate-limited.
    pending_raise: Option<Instant>,
    last_raise: Option<Instant>,
    /// Exact items last written to `_SCREENSAVER_STATUS` (to undo foreign writes).
    status_items: Vec<u32>,
    watch: Vec<PathBuf>,
    watch_mtimes: Vec<Option<SystemTime>>,
    /// Pointer position (root coords) and accumulated motion since activation.
    motion_last: Option<(i32, i32)>,
    motion_accum: f64,
    dpms_saved: Option<SavedDpms>,
}

/// Run the daemon until EXIT (returns Ok) or a fatal X error. RESTART re-execs and only
/// returns on failure.
pub fn run(args: Args, cfg: Config) -> Result<(), Box<dyn Error>> {
    let x = X11::open(cfg.display.as_deref())?;
    let display_name = cfg
        .display
        .clone()
        .or_else(|| std::env::var("DISPLAY").ok())
        .unwrap_or_default();

    // Refuse to start when another saver (xscreensaver or us) owns a rendezvous window.
    if let Some(w) = protocol::find_saver_window_any_screen(&x.conn, &x.atoms)? {
        let id = protocol::read_id(&x.conn, w, &x.atoms)
            .ok()
            .flatten()
            .unwrap_or_else(|| "???".into());
        eprintln!(
            "{PROGNAME}: already running on display {display_name} (window 0x{w:x})\n from process {id}"
        );
        std::process::exit(1);
    }

    install_signal_handlers();
    install_panic_hook();
    idle::disable_server_blanker(&x);
    idle::select_activity_events(&x)?;
    monitors::select_hotplug(&x)?;
    saver_window::select_root_events(&x)?;

    let status_root = x.conn.setup().roots[0].root;
    if status_root != x.root {
        let a = x.conn.get_window_attributes(status_root)?.reply()?;
        x.conn
            .change_window_attributes(
                status_root,
                &ChangeWindowAttributesAux::new()
                    .event_mask(a.your_event_mask | EventMask::PROPERTY_CHANGE),
            )?
            .check()?;
    }
    let ident = create_ident_window(&x, status_root)?;
    let user = lockapi::current_username().or_else(|| {
        nix::unistd::User::from_uid(nix::unistd::getuid())
            .ok()
            .flatten()
            .map(|u| u.name)
    });
    let id = protocol::format_id(
        std::process::id(),
        user.as_deref().unwrap_or("???"),
        &hostname(),
    );
    protocol::publish_identity(
        &x.conn,
        ident,
        &x.atoms,
        PROGNAME,
        crate::COMPAT_XSCREENSAVER_VERSION,
        &id,
    )?;
    let status_items = write_status(&x, status_root, Status::Off, SystemTime::now(), &[])?;

    let keyboard = match lockapi::keyboard_new(&x) {
        Ok(k) => Some(k),
        Err(e) => {
            if cfg.lock {
                log::error!(
                    "keyboard setup failed ({e}); it is retried at lock time, and if it still \
                     fails the screen locks WITHOUT any way to type a password"
                );
            } else {
                log::error!("keyboard setup failed ({e}); it is retried at lock time");
            }
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
    if !x.has_dpms {
        log::info!("no DPMS extension: display power management is not available");
    }

    let now = Instant::now();
    let watch = cfg.watch_paths();
    let watch_mtimes = mtimes(&watch);
    let mut d = Daemon {
        args,
        cfg,
        x,
        status_root,
        ident,
        state: SaverState::Idle,
        windows: SaverWindows::default(),
        screens: Vec::new(),
        grabbed: false,
        lock_pending: false,
        current: None,
        rng: Rng::new(time_seed()),
        user,
        keyboard,
        locker: None,
        status: Status::Off,
        status_since: SystemTime::now(),
        grab_seq: 0,
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
        next_periodic_raise: None,
        pending_raise: None,
        last_raise: None,
        status_items,
        watch,
        watch_mtimes,
        motion_last: None,
        motion_accum: 0.0,
        dpms_saved: None,
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

/// Write `_SCREENSAVER_STATUS` and return the exact items written.
fn write_status(
    x: &X11,
    root: Window,
    status: Status,
    since: SystemTime,
    hacks: &[u32],
) -> Result<Vec<u32>, Box<dyn Error>> {
    let items = protocol::encode_status(&x.atoms, status, since, hacks)?;
    protocol::set_status(&x.conn, root, &x.atoms, status, since, hacks)?;
    Ok(items)
}

fn create_ident_window(x: &X11, root: Window) -> Result<Window, X11Error> {
    let w = x.conn.generate_id()?;
    let screen = &x.conn.setup().roots[0];
    x.conn
        .create_window(
            screen.root_depth,
            w,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_OUTPUT,
            screen.root_visual,
            &CreateWindowAux::new()
                .override_redirect(1)
                .event_mask(EventMask::PROPERTY_CHANGE),
        )?
        .check()?;
    Ok(w)
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
        // Best-effort teardown; `X11` closes the connection afterwards, which destroys the
        // rendezvous window anyway.
        self.teardown_windows();
        self.restore_dpms();
        let _ = self.x.conn.destroy_window(self.ident);
        let _ = protocol::clear_status(&self.x.conn, self.status_root, &self.x.atoms);
        let _ = self.x.sync();
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
        while let Some(e) = ev {
            match self.handle_event(e) {
                Ok(Some(exit)) => return Ok(Some(exit)),
                Ok(None) => {}
                Err(e) => self.recoverable(e)?,
            }
            ev = self.next_event(None)?;
        }
        if let Some(kb) = self.keyboard.as_mut() {
            lockapi::keyboard_refresh(kb, &self.x);
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

    /// Wait for (`Some(timeout)`) or poll (`None`) the next event. A ConnectionError (for
    /// example a ParseError) is fatal only when the connection is really gone (`sync` fails);
    /// otherwise the bad event is dropped and the loop continues.
    fn next_event(&self, timeout: Option<Duration>) -> Result<Option<Event>, Box<dyn Error>> {
        let r = match timeout {
            Some(t) => self.x.wait_event_timeout(t),
            None => self.x.poll_event(),
        };
        match r {
            Ok(ev) => Ok(ev),
            Err(e) => {
                self.recoverable(format!("reading X events: {e}").into())?;
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

    /// An error from event handling or a timer. Errors must not end the daemon while the X
    /// connection works (exiting would drop a lock): log and continue. Only a dead
    /// connection (where the server has already removed our windows) is fatal.
    fn recoverable(&self, e: Box<dyn Error>) -> Result<(), Box<dyn Error>> {
        if let Err(conn) = self.x.sync() {
            log::error!("X connection lost ({e}; {conn})");
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
        t = min_opt(t, self.next_frame);
        t = min_opt(t, self.next_grab_retry);
        t = min_opt(t, self.next_grab_reassert);
        t = min_opt(t, self.next_relayout);
        t = min_opt(t, self.next_periodic_raise);
        t = min_opt(t, self.pending_raise);
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
            if self.state == SaverState::Locked {
                log::warn!("SIGTERM/SIGINT received while locked: ignored");
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
                    let server = Duration::from_millis(u64::from(idle::idle_ms(&self.x)?));
                    let idle = server.min(now.saturating_duration_since(self.activity_reset));
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
                if self.next_periodic_raise.is_some_and(|t| now >= t)
                    || self.pending_raise.is_some_and(|t| now >= t)
                {
                    self.raise_now(now);
                }
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
    fn handle_event(&mut self, ev: Event) -> Result<Option<Exit>, Box<dyn Error>> {
        if let Some(kb) = self.keyboard.as_mut()
            && lockapi::keyboard_event(kb, &ev)
        {
            return Ok(None);
        }
        let active = self.state != SaverState::Idle;
        match &ev {
            Event::ClientMessage(cm) => {
                if cm.window != self.ident {
                    log::debug!("ClientMessage for foreign window 0x{:x} ignored", cm.window);
                    return Ok(None);
                }
                match protocol::parse_client_message(cm, &self.x.atoms) {
                    Some(cmd) => {
                        if cmd == Command::Deactivate
                            && let Some(pid) = protocol::deactivate_caller_pid(cm)
                        {
                            log::debug!("DEACTIVATE sent by pid {pid}");
                        }
                        return self.command(cmd);
                    }
                    None => log::debug!("unrecognised ClientMessage ignored"),
                }
            }
            Event::Error(e) => log::debug!("X error: {e:?}"),
            Event::Expose(e) => {
                if e.count == 0 {
                    self.present_window(e.window)?;
                }
            }
            Event::PropertyNotify(p) => {
                if p.window == self.status_root
                    && Some(p.atom) == self.x.atoms.by_name(protocol::ATOM_STATUS)
                {
                    self.verify_status();
                }
            }
            // ---- covering: keep the saver windows on top and mapped ----
            Event::MapNotify(_) if self.windows.is_foreign_map(&ev) => {
                if active {
                    log::debug!("foreign window mapped: raising saver windows");
                    self.request_raise();
                }
            }
            Event::ConfigureNotify(c) if active && c.window != self.x.root => {
                if let Some(&(_, mon)) =
                    self.windows.windows().iter().find(|(w, _)| *w == c.window)
                {
                    let moved = (c.x, c.y, c.width, c.height)
                        != (mon.x, mon.y, mon.width.max(1), mon.height.max(1));
                    if moved && c.event == c.window {
                        log::warn!("saver window 0x{:x} moved/resized by another client", c.window);
                        self.next_relayout = Some(Instant::now());
                    }
                } else {
                    log::trace!("foreign window restacked/configured: raising");
                    self.request_raise();
                }
            }
            Event::CirculateNotify(c) => {
                if active && !self.windows.contains(c.window) && c.place == Place::ON_TOP {
                    log::debug!("foreign window circulated to the top: raising");
                    self.request_raise();
                }
            }
            Event::VisibilityNotify(v) => {
                if active
                    && self.windows.contains(v.window)
                    && v.state != Visibility::UNOBSCURED
                    && !self.windows.overlapping()
                {
                    log::debug!("saver window obscured ({:?}): raising", v.state);
                    self.request_raise();
                }
            }
            Event::UnmapNotify(u) => {
                // Our windows report this twice (own STRUCTURE_NOTIFY and root
                // SUBSTRUCTURE_NOTIFY); act once. Old windows of a relayout are no longer ours.
                if active && u.event == u.window && self.windows.contains(u.window) {
                    self.grab_lost("saver window unmapped by another client");
                }
            }
            Event::DestroyNotify(d) => {
                if active && d.event == d.window && self.windows.contains(d.window) {
                    log::warn!("saver window 0x{:x} destroyed by another client", d.window);
                    self.next_relayout = Some(Instant::now());
                }
            }
            Event::FocusIn(f) | Event::FocusOut(f) => {
                if active
                    && self.grabbed
                    && f.mode == NotifyMode::UNGRAB
                    && (f.sequence.wrapping_sub(self.grab_seq) as i16) >= 0
                    && (f.event == self.x.root || self.windows.contains(f.event))
                {
                    self.grab_lost("focus change with mode Ungrab");
                }
            }
            e if monitors::is_hotplug_event(e) => {
                log::debug!("monitor layout change");
                if active {
                    self.relayout()?;
                }
            }
            Event::ScreensaverNotify(_) => {}
            // ---- input: never Debug-format these (AGENTS.md rule 3) ----
            Event::KeyRelease(_) => log::trace!("KeyRelease ignored"),
            Event::ButtonRelease(_) => log::trace!("ButtonRelease ignored"),
            Event::KeyPress(k) => {
                if !self.is_input_window(k.event) || k.response_type & 0x80 != 0 {
                    log::debug!("KeyPress not from our grab (foreign or synthetic): ignored");
                    return Ok(None);
                }
                match self.state {
                    SaverState::Idle => {}
                    SaverState::Blanked => self.blanked_activity("key press"),
                    SaverState::Locked => {
                        if !self.grabbed {
                            log::trace!("KeyPress without grab ignored");
                        } else if let (Some(kb), Some(l)) =
                            (self.keyboard.as_mut(), self.locker.as_mut())
                        {
                            let input = lockapi::translate(kb, k);
                            let le = lockapi::key(l, input, Instant::now());
                            self.handle_lock_event(le)?;
                        } else {
                            // No keyboard (L6): keys can only reveal the dialog.
                            self.wake_locked()?;
                        }
                    }
                }
            }
            Event::ButtonPress(b) => {
                if !self.is_input_window(b.event) || b.response_type & 0x80 != 0 {
                    log::debug!("ButtonPress not from our grab (foreign or synthetic): ignored");
                    return Ok(None);
                }
                match self.state {
                    SaverState::Idle => {}
                    SaverState::Blanked => self.blanked_activity("button press"),
                    SaverState::Locked => self.wake_locked()?,
                }
            }
            Event::XinputRawKeyPress(_) | Event::XinputRawButtonPress(_) => {
                // Raw presses also arrive for the core events handled above; while locked
                // only the core (grabbed) events count.
                if self.state == SaverState::Blanked {
                    self.blanked_activity("raw key/button");
                }
            }
            Event::MotionNotify(m) => {
                if self.state != SaverState::Idle
                    && self.is_input_window(m.event)
                    && self.motion(i32::from(m.root_x), i32::from(m.root_y))
                {
                    if self.state == SaverState::Blanked {
                        self.blanked_activity("pointer moved past threshold");
                    } else {
                        self.wake_locked()?;
                    }
                }
            }
            Event::XinputRawMotion(_) => {}
            other => {
                if idle::is_activity_event(other) {
                    log::trace!("activity event");
                } else {
                    log::trace!("event ignored: {other:?}");
                }
            }
        }
        Ok(None)
    }

    /// Key/pointer events are ours when reported to the grab window (root) or a saver window.
    fn is_input_window(&self, w: Window) -> bool {
        w == self.x.root || self.windows.contains(w)
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
        self.motion_last = self
            .x
            .conn
            .query_pointer(self.x.root)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| (i32::from(r.root_x), i32::from(r.root_y)));
    }

    /// Pointer activity or DEACTIVATE while locked: wake the display and show the prompt.
    fn wake_locked(&mut self) -> Result<(), Box<dyn Error>> {
        self.dpms_on();
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
    fn respond(&self, ok: bool, msg: &str) {
        if let Err(e) = protocol::send_response(&self.x.conn, &self.x.atoms, self.ident, ok, msg) {
            log::warn!("cannot send response {msg:?}: {e}");
        }
    }

    /// Before EXIT/RESTART destroys the rendezvous window: give the client up to `limit` to read
    /// (and delete) `_SCREENSAVER_RESPONSE`. Otherwise the real xscreensaver-command can hit
    /// BadWindow while reading and report "xscreensaver window unexpectedly deleted" (fatal
    /// for RESTART). Other events arriving meanwhile are dropped: we are shutting down.
    fn await_response_read(&self, limit: Duration) {
        let Some(resp) = self.x.atoms.by_name(protocol::ATOM_RESPONSE) else {
            return;
        };
        let deadline = later(Instant::now(), limit);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                log::debug!("client did not read the response in time");
                return;
            }
            match self.x.wait_event_timeout(left) {
                Ok(Some(Event::PropertyNotify(p)))
                    if p.window == self.ident
                        && p.atom == resp
                        && p.state == x11rb::protocol::xproto::Property::DELETE =>
                {
                    return;
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => return,
            }
        }
    }

    /// Locking needs a user name for PAM. A missing keyboard does not disable locking (L6):
    /// it is retried at lock time and, failing that, the screen locks anyway.
    fn can_lock(&self) -> bool {
        self.user.is_some()
    }

    /// Reply to a command once its action has finished: `+ok_msg`, or `-reason` on error (the
    /// error is passed on to be logged).
    fn reply_after(&self, ok_msg: &str, r: Result<(), Box<dyn Error>>) -> Result<(), Box<dyn Error>> {
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
                    self.respond(false, "locking disabled");
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
                        self.respond(false, "unable to grab keyboard");
                    }
                }
            }
            Command::Suspend => {
                let r = if active { Ok(()) } else { self.activate(false) };
                self.reply_after("suspending", r)?;
                self.dpms_off();
            }
            Command::Exit | Command::Restart => {
                if self.state == SaverState::Locked {
                    log::warn!("{cmd:?} refused: screen is locked");
                    self.respond(false, "screen is locked");
                } else {
                    let restart = cmd == Command::Restart;
                    self.respond(true, if restart { "restarting" } else { "exiting" });
                    self.await_response_read(Duration::from_secs(1));
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
        let mons = self.current_monitors()?;
        let mut wins = SaverWindows::create(&self.x, &mons)?;
        if let Err(e) = wins.map_and_raise(&self.x) {
            wins.destroy(&self.x);
            return Err(e.into());
        }
        self.windows = wins;
        self.state = SaverState::Blanked;
        self.current = pattern;
        self.grabbed = false;
        self.lock_pending = false;
        self.try_grab(GRAB_TIMEOUT);

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
        self.last_raise = Some(now);
        self.next_periodic_raise = Some(later(now, RAISE_PERIOD));

        self.build_screens();
        self.reset_motion();
        log::info!(
            "blanked: pattern {} on {} monitor(s){}",
            self.pattern_name(self.current),
            mons.len(),
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

    /// RANDR monitors; if that fails, one monitor covering the current root geometry.
    fn current_monitors(&self) -> Result<Vec<Monitor>, Box<dyn Error>> {
        match monitors::monitors(&self.x) {
            Ok(m) => Ok(m),
            Err(e) => {
                log::error!("cannot enumerate monitors ({e}); covering the whole root window");
                Ok(vec![self.root_monitor()?])
            }
        }
    }

    /// The current root geometry (from the server, not the stale connection setup).
    fn root_monitor(&self) -> Result<Monitor, Box<dyn Error>> {
        let g = self.x.conn.get_geometry(self.x.root)?.reply()?;
        Ok(Monitor {
            x: 0,
            y: 0,
            width: g.width,
            height: g.height,
            primary: true,
        })
    }

    /// Try to grab keyboard+pointer on the root window. On failure schedule a retry.
    fn try_grab(&mut self, timeout: Duration) {
        if self.state == SaverState::Idle {
            return;
        }
        let now = Instant::now();
        match grab::grab_all(&self.x, self.x.root, timeout) {
            Ok(()) => {
                if !self.grabbed {
                    log::debug!("keyboard and pointer grabbed");
                }
                self.grabbed = true;
                // Sequence boundary for stale FocusIn/Out(Ungrab) events (see `grab_seq`).
                if let Ok(c) = self.x.conn.get_input_focus() {
                    self.grab_seq = c.sequence_number() as u16;
                    let _ = c.reply();
                }
                self.next_grab_retry = None;
                self.next_grab_reassert = Some(later(now, GRAB_REASSERT));
            }
            Err(e) => {
                self.grabbed = false;
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
        self.try_grab(Duration::from_millis(100));
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
        match grab::reassert(&self.x, self.x.root) {
            Ok(true) => self.next_grab_reassert = Some(later(Instant::now(), GRAB_REASSERT)),
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
        if let Err(e) = self.windows.map_and_raise(&self.x) {
            log::warn!("re-map of saver windows failed: {e}");
        }
        self.last_raise = Some(Instant::now());
        self.try_grab(Duration::from_millis(200));
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

    /// Raise now, or once the rate limit allows.
    fn request_raise(&mut self) {
        let now = Instant::now();
        match self.last_raise {
            Some(t) if now < later(t, RAISE_MIN_GAP) => {
                self.pending_raise.get_or_insert(later(t, RAISE_MIN_GAP));
            }
            _ => self.raise_now(now),
        }
    }

    fn raise_now(&mut self, now: Instant) {
        self.pending_raise = None;
        self.last_raise = Some(now);
        self.next_periodic_raise = Some(later(now, RAISE_PERIOD));
        if let Err(e) = self.windows.raise(&self.x) {
            log::warn!("raise failed: {e}");
        }
    }

    /// (Re)create the drawing state for every saver window. Failures are not fatal: a monitor
    /// whose presenter cannot be set up gets no `Screen` and stays black (window background).
    fn build_screens(&mut self) {
        for s in self.screens.drain(..) {
            s.presenter.destroy(&self.x);
        }
        let name = self.pattern_name(self.current).to_owned();
        for &(win, mon) in self.windows.windows() {
            let (w, h) = (u32::from(mon.width), u32::from(mon.height));
            let presenter = match Presenter::new(&self.x, w, h) {
                Ok(p) => p,
                Err(e) => {
                    log::error!("cannot set up drawing for monitor {mon:?} ({e}); it stays black");
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
                win,
                mon,
                fb: Framebuffer::new(w, h),
                presenter,
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
        let Some(user) = self.user.clone() else {
            log::error!("cannot lock: user name unknown");
            return false;
        };
        if !self.grabbed {
            self.try_grab(Duration::from_millis(500));
        }
        if !self.grabbed {
            if !self.lock_pending {
                log::warn!(
                    "cannot grab keyboard and pointer: lock pending, staying blanked until the grab succeeds"
                );
            }
            self.lock_pending = true;
            self.lock_deadline = None;
            return false;
        }
        if self.keyboard.is_none() {
            match lockapi::keyboard_new(&self.x) {
                Ok(k) => {
                    log::info!("keyboard set up at lock time");
                    self.keyboard = Some(k);
                }
                Err(e) => log::error!(
                    "keyboard unavailable ({e}): locking anyway; no password can be typed, so \
                     this lock cannot be undone from this display (kill the daemon from another \
                     session)"
                ),
            }
        }
        self.locker = Some(lockapi::new_locker(
            user,
            self.cfg.pam_service.clone(),
            self.cfg.passwd_timeout,
        ));
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
        self.state = SaverState::Idle;
        self.locker = None;
        self.lock_deadline = None;
        self.cycle_deadline = None;
        self.next_frame = None;
        self.next_grab_retry = None;
        self.next_grab_reassert = None;
        self.next_relayout = None;
        self.next_periodic_raise = None;
        self.pending_raise = None;
        self.lock_pending = false;
        let now = Instant::now();
        self.activity_reset = now;
        self.next_idle_poll = later(now, IDLE_POLL);
        self.update_status();
        log::info!("unblanked ({why:?})");
    }

    fn teardown_windows(&mut self) {
        if self.grabbed || !self.windows.windows().is_empty() {
            grab::ungrab_all(&self.x);
        }
        self.grabbed = false;
        for s in self.screens.drain(..) {
            s.presenter.destroy(&self.x);
        }
        self.windows.unmap(&self.x);
        self.windows.destroy(&self.x);
    }

    /// Recreate the saver windows for the current monitor layout. The grab is on root and is
    /// not affected. On failure the old windows are kept, the first one is stretched over the
    /// current root geometry so nothing is left uncovered, and a retry is scheduled.
    fn relayout(&mut self) -> Result<(), Box<dyn Error>> {
        self.next_relayout = None;
        let now = Instant::now();
        let result = self
            .current_monitors()
            .and_then(|mons| match self.windows.relayout(&self.x, &mons) {
                Ok(()) => Ok(mons.len()),
                Err(e) => Err(e.into()),
            });
        match result {
            Ok(n) => log::info!("monitor layout changed: {n} monitor(s)"),
            Err(e) => {
                log::error!("relayout failed ({e}); keeping old windows, retrying in 1 s");
                self.next_relayout = Some(later(now, RELAYOUT_RETRY));
                match self.root_monitor() {
                    Ok(root) => {
                        if let Err(e) = self.windows.cover_root(&self.x, root) {
                            log::error!("cannot stretch saver window over the root: {e}");
                        }
                    }
                    Err(e) => log::error!("cannot read root geometry: {e}"),
                }
            }
        }
        self.last_raise = Some(now);
        if self.grabbed {
            // Cheap confirmation that the root grab survived.
            match grab::reassert(&self.x, self.x.root) {
                Ok(true) => {}
                Ok(false) => self.grab_lost("re-grab after relayout failed"),
                Err(e) => self.grab_lost(&format!("re-grab after relayout failed: {e}")),
            }
        } else {
            self.try_grab(Duration::from_millis(500));
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

    /// Write `_SCREENSAVER_STATUS` when the advertised state changed.
    fn update_status(&mut self) {
        let want = self.desired_status();
        if want == self.status && !self.status_dirty {
            return;
        }
        let hack = self.current.map_or(0, |i| i as u32 + 1);
        let hacks = vec![hack; self.windows.windows().len().max(1)];
        let locked = |s: Status| matches!(s, Status::Lock | Status::Auth);
        let since = if want == self.status || (locked(want) && locked(self.status)) {
            self.status_since
        } else {
            SystemTime::now()
        };
        match write_status(&self.x, self.status_root, want, since, &hacks) {
            Ok(items) => {
                self.status_since = since;
                self.status_dirty = false;
                log::debug!("status {want:?} hacks {hacks:?}");
                self.status = want;
                self.status_items = items;
            }
            Err(e) => log::warn!("cannot set status: {e}"),
        }
    }

    /// `_SCREENSAVER_STATUS` changed on root: if it no longer holds exactly what we wrote last
    /// (another client overwrote or deleted it), write our value back.
    fn verify_status(&mut self) {
        if self.status_items.is_empty() {
            return;
        }
        let ours = match protocol::read_status(&self.x.conn, self.status_root, &self.x.atoms) {
            Ok(protocol::RawStatus::Items(v)) => v == self.status_items,
            Ok(_) => false,
            Err(e) => {
                log::debug!("cannot read status: {e}");
                return;
            }
        };
        if ours {
            return;
        }
        log::warn!("_SCREENSAVER_STATUS changed by another client: restoring it");
        let Some(atom) = self.x.atoms.by_name(protocol::ATOM_STATUS) else {
            return;
        };
        if let Err(e) = self.x.conn.change_property32(
            PropMode::REPLACE,
            self.status_root,
            atom,
            AtomEnum::INTEGER,
            &self.status_items,
        ) {
            log::warn!("cannot restore status: {e}");
        }
        self.x.flush();
    }

    fn dialog_visible(&self) -> bool {
        self.state == SaverState::Locked
            && self.locker.as_ref().is_some_and(|l| {
                lockapi::is_prompt_visible(l) || lockapi::is_checking(l)
            })
    }

    fn frame_interval(&self) -> Option<Duration> {
        let wants = self.screens.first().map_or(0, |s| s.pattern.wants_fps());
        let mut fps = if wants == 0 { 0 } else { wants.min(self.cfg.fps).max(1) };
        if self.dialog_visible() {
            fps = fps.max(MIN_DIALOG_FPS);
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
        let dialog_on = self
            .screens
            .iter()
            .position(|s| s.mon.primary)
            .unwrap_or(0);
        let show_dialog = self.state == SaverState::Locked && self.grabbed;
        let no_keyboard = self.keyboard.is_none();
        for (i, s) in self.screens.iter_mut().enumerate() {
            s.fb.clear();
            s.pattern.step(dt, &mut s.fb, &mut s.rng);
            s.oled.tick(dt);
            s.oled.apply(&mut s.fb);
            // The dialog is drawn after the OLED pass, so the rest interlude never blanks it
            // while the user is typing (it is short-lived and dim already).
            if show_dialog && i == dialog_on {
                if let Some(l) = &self.locker {
                    lockapi::draw(&mut s.fb, l, now);
                }
                if no_keyboard {
                    draw_notice(&mut s.fb, "keyboard unavailable: cannot unlock");
                }
            }
            s.presenter.present(&self.x, s.win, &s.fb)?;
        }
        log::trace!("frame dt={}ms", dt.as_millis());
        self.next_frame = self.frame_interval().map(|iv| later(now, iv));
        Ok(())
    }

    fn present_window(&mut self, win: Window) -> Result<(), Box<dyn Error>> {
        if let Some(s) = self.screens.iter_mut().find(|s| s.win == win) {
            s.presenter.present(&self.x, s.win, &s.fb)?;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // DPMS (untestable on Xvfb: it has no DPMS extension)
    // -----------------------------------------------------------------------
    fn dpms_activate(&mut self) {
        if !self.cfg.dpms.enabled || !self.x.has_dpms {
            return;
        }
        let c = &self.x.conn;
        let saved = (|| -> Option<SavedDpms> {
            let t = dpms::get_timeouts(c).ok()?.reply().ok()?;
            let i = dpms::info(c).ok()?.reply().ok()?;
            Some(SavedDpms {
                standby: t.standby_timeout,
                suspend: t.suspend_timeout,
                off: t.off_timeout,
                enabled: i.state,
            })
        })();
        if saved.is_none() {
            log::warn!("cannot read DPMS settings; leaving DPMS alone");
            return;
        }
        if self.dpms_saved.is_none() {
            self.dpms_saved = saved;
        }
        let d = &self.cfg.dpms;
        let _ = dpms::set_timeouts(c, secs_u16(d.standby), secs_u16(d.suspend), secs_u16(d.off));
        let _ = dpms::enable(c);
        if d.quick_off {
            let _ = dpms::force_level(c, dpms::DPMSMode::OFF);
        }
        self.x.flush();
        log::debug!("DPMS timeouts set, saved {:?}", self.dpms_saved);
    }

    fn dpms_enabled_on_server(&self) -> bool {
        self.x.has_dpms
            && dpms::info(&self.x.conn)
                .ok()
                .and_then(|c| c.reply().ok())
                .is_some_and(|i| i.state)
    }

    fn dpms_on(&self) {
        if self.dpms_enabled_on_server() {
            let _ = dpms::force_level(&self.x.conn, dpms::DPMSMode::ON);
            self.x.flush();
        }
    }

    fn dpms_off(&self) {
        if !self.x.has_dpms {
            log::info!("SUSPEND: no DPMS extension, display stays blanked");
            return;
        }
        let _ = dpms::enable(&self.x.conn);
        let _ = dpms::force_level(&self.x.conn, dpms::DPMSMode::OFF);
        self.x.flush();
    }

    fn restore_dpms(&mut self) {
        if !self.x.has_dpms {
            return;
        }
        self.dpms_on();
        if let Some(s) = self.dpms_saved.take() {
            let c = &self.x.conn;
            let _ = dpms::set_timeouts(c, s.standby, s.suspend, s.off);
            if !s.enabled {
                let _ = dpms::disable(c);
            }
            self.x.flush();
            log::debug!("DPMS settings restored");
        }
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
        }
    }
}

/// One line of dim text centred near the bottom of `fb`.
fn draw_notice(fb: &mut Framebuffer, text: &str) {
    use crate::render::font;
    let scale = 2;
    let w = font::text_width(text, scale);
    let x = (fb.width as i32 - w) / 2;
    let y = fb.height as i32 - 40;
    font::draw_text(fb, x.max(0), y.max(0), scale, crate::render::rgb(0x80, 0x30, 0x30), text);
}

/// `--demo-blank`: blank every monitor for 3 s with a small grey marker in each monitor's
/// top-left corner, then restore. Exercises connection, monitors, saver windows, grabs and
/// presenting. Owner: Opus A. Fails if the server reports any X error during setup.
pub fn demo_blank(display: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    use crate::render::Framebuffer;
    use crate::x11::{X11Error, conn::X11, grab, monitors, present::Presenter, saver_window::SaverWindows};
    use x11rb::protocol::Event;

    fn drain_errors(x: &X11) -> Result<(), X11Error> {
        while let Some(ev) = x.poll_event()? {
            if let Event::Error(e) = ev {
                return Err(X11Error::Protocol(format!("{e:?}")));
            }
        }
        Ok(())
    }

    let x = X11::open(display)?;
    let mons = monitors::monitors(&x)?;
    log::info!("demo-blank: {} monitor(s): {mons:?}", mons.len());
    let mut wins = SaverWindows::create(&x, &mons)?;
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        wins.map_and_raise(&x)?;
        drain_errors(&x)?;
        let first = wins.windows().first().map(|&(w, _)| w).ok_or("no saver windows")?;
        grab::grab_all(&x, first, std::time::Duration::from_secs(5))?;
        let paint = (|| -> Result<(), Box<dyn std::error::Error>> {
            let mut presenter: Option<Presenter> = None;
            for &(w, m) in wins.windows() {
                let (fw, fh) = (u32::from(m.width), u32::from(m.height));
                let mut fb = Framebuffer::new(fw, fh);
                fb.fill_rect(8, 8, 32, 32, crate::render::rgb(0x40, 0x40, 0x40));
                let p = match presenter.as_mut() {
                    Some(p) => {
                        p.ensure_size(&x, fw, fh)?;
                        p
                    }
                    None => presenter.insert(Presenter::new(&x, fw, fh)?),
                };
                p.present(&x, w, &fb)?;
            }
            x.sync()?;
            drain_errors(&x)?;
            std::thread::sleep(std::time::Duration::from_secs(3));
            if let Some(p) = presenter {
                p.destroy(&x);
            }
            Ok(())
        })();
        grab::ungrab_all(&x);
        paint
    })();
    wins.destroy(&x);
    result
}
