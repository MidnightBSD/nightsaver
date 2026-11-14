//! The X11 implementation of `display::Backend`: everything the daemon needs from the X
//! server, with no policy of its own.
//!
//! Covering: the saver windows are kept mapped and on top (raises are rate-limited and
//! repeated periodically). Exclusive input is a keyboard and pointer grab on the root
//! window (`grab.rs`), so recreating the saver windows on a RANDR change never drops it. A
//! broken grab (FocusOut with mode Ungrab, or one of our windows unmapped) is reported as
//! `DisplayEvent::ExclusiveLost`.
//!
//! Control: the xscreensaver ClientMessage protocol on a hidden rendezvous window, and
//! `_SCREENSAVER_STATUS` on the root of screen 0, which is written back if another client
//! changes it.

use std::error::Error;
use std::time::{Duration, Instant, SystemTime};

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::dpms;
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, CreateWindowAux, EventMask, NotifyMode,
    Place, PropMode, Visibility, Window, WindowClass,
};
use x11rb::wrapper::ConnectionExt as _;

use crate::compat::protocol::{self, Command, Status};
use crate::config::DpmsConfig;
use crate::display::{Backend, BackendResult, Capabilities, DisplayEvent, Monitor, SurfaceId};
use crate::render::Framebuffer;
use crate::x11::conn::X11;
use crate::x11::keyboard::Keyboard;
use crate::x11::present::Presenter;
use crate::x11::saver_window::{self, SaverWindows};
use crate::x11::{X11Error, grab, idle, monitors};

/// Program name published in WM_CLASS res_name / WM_COMMAND of the rendezvous window.
const PROGNAME: &str = "nightsaver";
/// Periodic re-raise of the saver windows while active (backstop for missed restacks).
const RAISE_PERIOD: Duration = Duration::from_secs(5);
/// Minimum spacing of raises, so two always-on-top clients cannot start a raise storm.
const RAISE_MIN_GAP: Duration = Duration::from_millis(250);

/// Server DPMS settings saved at activation, restored at deactivation.
#[derive(Debug, Clone, Copy)]
struct SavedDpms {
    standby: u16,
    suspend: u16,
    off: u16,
    enabled: bool,
}

pub struct X11Backend {
    x: X11,
    /// Screen 0 root: `_SCREENSAVER_STATUS` lives here.
    status_root: Window,
    /// Hidden rendezvous window carrying `_SCREENSAVER_VERSION`.
    ident: Window,
    windows: SaverWindows,
    /// Drawing state of the attached surfaces.
    presenters: Vec<(Window, Presenter)>,
    keyboard: Option<Keyboard>,
    /// The saver windows are up (between `blank` and `unblank`).
    active: bool,
    /// The keyboard and pointer grab is held, as far as we know.
    grabbed: bool,
    /// Low 16 bits of a request sequence number sent right after our last successful grab.
    /// Focus events older than it were caused by an earlier grab break we already handled.
    grab_seq: u16,
    /// Exact items last written to `_SCREENSAVER_STATUS` (to undo foreign writes).
    status_items: Vec<u32>,
    dpms_saved: Option<SavedDpms>,
    next_periodic_raise: Option<Instant>,
    /// A raise requested while rate-limited.
    pending_raise: Option<Instant>,
    last_raise: Option<Instant>,
    /// False when this X server is XWayland (see `Capabilities::can_lock`).
    lockable: bool,
}

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

impl X11Backend {
    /// Connect to `display` (default `$DISPLAY`), claim it as the screen saver and publish
    /// our identity. Exits the process with status 1 when another saver (xscreensaver or
    /// us) already owns the display, like xscreensaver does.
    ///
    /// `display_name` is only used in messages, `user` in the published identity;
    /// `lock_configured` selects how loudly a missing keyboard is reported.
    pub fn open(
        display: Option<&str>,
        display_name: &str,
        user: Option<&str>,
        lock_configured: bool,
        lockable: bool,
    ) -> Result<X11Backend, Box<dyn Error>> {
        let x = X11::open(display)?;

        // Refuse to start when another saver (xscreensaver or us) owns a rendezvous window.
        if let Some(w) = protocol::find_saver_window_any_screen(&x.conn, &x.atoms)? {
            let id = protocol::read_id(&x.conn, w, &x.atoms)
                .ok()
                .flatten()
                .unwrap_or_else(|| "???".into());
            let id: String = id.chars().map(|c| if c.is_control() { '?' } else { c }).collect();
            eprintln!(
                "{PROGNAME}: already running on display {display_name} (window 0x{w:x})\n from process {id}"
            );
            std::process::exit(1);
        }

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
        let id = protocol::format_id(std::process::id(), user.unwrap_or("???"), &hostname());
        protocol::publish_identity(
            &x.conn,
            ident,
            &x.atoms,
            PROGNAME,
            crate::COMPAT_XSCREENSAVER_VERSION,
            &id,
        )?;
        let status_items = write_status(&x, status_root, Status::Off, SystemTime::now(), &[])?;

        let keyboard = match Keyboard::new(&x) {
            Ok(k) => Some(k),
            Err(e) => {
                if lock_configured {
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

        Ok(X11Backend {
            x,
            status_root,
            ident,
            windows: SaverWindows::default(),
            presenters: Vec::new(),
            keyboard,
            active: false,
            grabbed: false,
            grab_seq: 0,
            status_items,
            dpms_saved: None,
            next_periodic_raise: None,
            pending_raise: None,
            last_raise: None,
            lockable,
        })
    }

    /// Turn one X event into what the daemon needs to know about it.
    fn translate(&mut self, ev: Event) -> DisplayEvent {
        // Any client can SendEvent arbitrary events to our windows. Only ClientMessage (the
        // control protocol) is legitimately synthetic; forged Unmap/Focus/XKB/Configure events
        // would otherwise hide the dialog, alter modifier state or force relayouts.
        if ev.sent_event() && !matches!(ev, Event::ClientMessage(_)) {
            log::trace!("synthetic event ignored: {}", event_name(&ev));
            return DisplayEvent::Internal;
        }
        if let Some(kb) = self.keyboard.as_mut()
            && kb.handle_event(&ev)
        {
            return DisplayEvent::Internal;
        }
        let active = self.active;
        match &ev {
            Event::ClientMessage(cm) => {
                if cm.window != self.ident {
                    log::debug!("ClientMessage for foreign window 0x{:x} ignored", cm.window);
                    return DisplayEvent::Internal;
                }
                match protocol::parse_client_message(cm, &self.x.atoms) {
                    Some(cmd) => {
                        if cmd == Command::Deactivate
                            && let Some(pid) = protocol::deactivate_caller_pid(cm)
                        {
                            log::debug!("DEACTIVATE sent by pid {pid}");
                        }
                        return DisplayEvent::Command(cmd);
                    }
                    None => log::debug!("unrecognised ClientMessage ignored"),
                }
            }
            Event::Error(e) => log::debug!("X error: {e:?}"),
            Event::Expose(e) => {
                if e.count == 0 {
                    return DisplayEvent::Redraw(SurfaceId(e.window));
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
                        return DisplayEvent::SurfacesDisturbed;
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
                    return DisplayEvent::ExclusiveLost("saver window unmapped by another client");
                }
            }
            Event::DestroyNotify(d) => {
                if active && d.event == d.window && self.windows.contains(d.window) {
                    log::warn!("saver window 0x{:x} destroyed by another client", d.window);
                    return DisplayEvent::SurfacesDisturbed;
                }
            }
            Event::FocusIn(f) | Event::FocusOut(f) => {
                if active
                    && self.grabbed
                    && f.mode == NotifyMode::UNGRAB
                    && (f.sequence.wrapping_sub(self.grab_seq) as i16) >= 0
                    && (f.event == self.x.root || self.windows.contains(f.event))
                {
                    return DisplayEvent::ExclusiveLost("focus change with mode Ungrab");
                }
            }
            e if monitors::is_hotplug_event(e) => {
                log::debug!("monitor layout change");
                return DisplayEvent::OutputsChanged;
            }
            Event::ScreensaverNotify(_) => {}
            // ---- input: never Debug-format these (AGENTS.md rule 3) ----
            Event::KeyRelease(_) => log::trace!("KeyRelease ignored"),
            Event::ButtonRelease(_) => log::trace!("ButtonRelease ignored"),
            Event::KeyPress(k) => {
                if !self.is_input_window(k.event) || k.response_type & 0x80 != 0 {
                    log::debug!("KeyPress not from our grab (foreign or synthetic): ignored");
                    return DisplayEvent::Internal;
                }
                return DisplayEvent::Key(self.keyboard.as_ref().map(|kb| kb.translate(k)));
            }
            Event::ButtonPress(b) => {
                if !self.is_input_window(b.event) || b.response_type & 0x80 != 0 {
                    log::debug!("ButtonPress not from our grab (foreign or synthetic): ignored");
                    return DisplayEvent::Internal;
                }
                return DisplayEvent::Button;
            }
            // Raw presses also arrive for the core events handled above.
            Event::XinputRawKeyPress(_) | Event::XinputRawButtonPress(_) => {
                return DisplayEvent::RawActivity;
            }
            Event::MotionNotify(m) => {
                if self.is_input_window(m.event) {
                    return DisplayEvent::Motion { x: i32::from(m.root_x), y: i32::from(m.root_y) };
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
        DisplayEvent::Internal
    }

    /// Key/pointer events are ours when reported to the grab window (root) or a saver window.
    fn is_input_window(&self, w: Window) -> bool {
        w == self.x.root || self.windows.contains(w)
    }

    /// Record the sequence boundary for stale FocusIn/Out(Ungrab) events (see `grab_seq`).
    /// Called after every successful grab or re-grab, since the 16-bit window wraps after
    /// 32768 requests (a few minutes of rendering).
    fn mark_grab_seq(&mut self) {
        if let Ok(c) = self.x.conn.get_input_focus() {
            self.grab_seq = c.sequence_number() as u16;
            let _ = c.reply();
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

    fn dpms_enabled_on_server(&self) -> bool {
        self.x.has_dpms
            && dpms::info(&self.x.conn)
                .ok()
                .and_then(|c| c.reply().ok())
                .is_some_and(|i| i.state)
    }
}

impl Drop for X11Backend {
    fn drop(&mut self) {
        // Best effort; closing the connection destroys the rendezvous window anyway.
        let _ = self.x.conn.destroy_window(self.ident);
        let _ = protocol::clear_status(&self.x.conn, self.status_root, &self.x.atoms);
        let _ = self.x.sync();
    }
}

impl Backend for X11Backend {
    fn capabilities(&self) -> Capabilities {
        Capabilities { can_lock: self.lockable, power_control: self.x.has_dpms }
    }

    fn wait_event(&mut self, timeout: Option<Duration>) -> BackendResult<Option<DisplayEvent>> {
        let r = match timeout {
            Some(t) => self.x.wait_event_timeout(t),
            None => self.x.poll_event(),
        };
        match r {
            Ok(Some(ev)) => Ok(Some(self.translate(ev))),
            Ok(None) => Ok(None),
            Err(e) => Err(format!("reading X events: {e}").into()),
        }
    }

    fn end_of_batch(&mut self) {
        if let Some(kb) = self.keyboard.as_mut()
            && let Err(e) = kb.reload_if_stale(&self.x)
        {
            log::warn!("keymap reload failed: {e}");
        }
    }

    fn check_connection(&self) -> BackendResult<()> {
        Ok(self.x.sync()?)
    }

    fn next_deadline(&self) -> Option<Instant> {
        min_opt(self.next_periodic_raise, self.pending_raise)
    }

    fn tick(&mut self, now: Instant) {
        if self.next_periodic_raise.is_some_and(|t| now >= t)
            || self.pending_raise.is_some_and(|t| now >= t)
        {
            self.raise_now(now);
        }
    }

    fn idle_time(&mut self) -> BackendResult<Option<Duration>> {
        Ok(Some(Duration::from_millis(u64::from(idle::idle_ms(&self.x)?))))
    }

    fn pointer_position(&self) -> Option<(i32, i32)> {
        self.x
            .conn
            .query_pointer(self.x.root)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| (i32::from(r.root_x), i32::from(r.root_y)))
    }

    fn blank(&mut self) -> BackendResult<usize> {
        let mons = self.current_monitors()?;
        let mut wins = SaverWindows::create(&self.x, &mons)?;
        if let Err(e) = wins.map_and_raise(&self.x) {
            wins.destroy(&self.x);
            return Err(e.into());
        }
        self.windows = wins;
        self.active = true;
        self.grabbed = false;
        let now = Instant::now();
        self.last_raise = Some(now);
        self.next_periodic_raise = Some(later(now, RAISE_PERIOD));
        Ok(mons.len())
    }

    fn relayout(&mut self) -> BackendResult<usize> {
        let result = self
            .current_monitors()
            .and_then(|mons| match self.windows.relayout(&self.x, &mons) {
                Ok(()) => Ok(mons.len()),
                Err(e) => Err(e.into()),
            });
        if result.is_err() {
            match self.root_monitor() {
                Ok(root) => {
                    if let Err(e) = self.windows.cover_root(&self.x, root) {
                        log::error!("cannot stretch saver window over the root: {e}");
                    }
                }
                Err(e) => log::error!("cannot read root geometry: {e}"),
            }
        }
        self.last_raise = Some(Instant::now());
        result
    }

    fn restore_cover(&mut self) {
        if let Err(e) = self.windows.map_and_raise(&self.x) {
            log::warn!("re-map of saver windows failed: {e}");
        }
        self.last_raise = Some(Instant::now());
    }

    fn unblank(&mut self) {
        self.detach_all();
        self.windows.unmap(&self.x);
        self.windows.destroy(&self.x);
        self.active = false;
        self.next_periodic_raise = None;
        self.pending_raise = None;
    }

    fn surfaces(&self) -> Vec<(SurfaceId, Monitor)> {
        self.windows.windows().iter().map(|&(w, m)| (SurfaceId(w), m)).collect()
    }

    fn attach(&mut self, surface: SurfaceId, width: u32, height: u32) -> BackendResult<()> {
        self.detach(surface);
        let p = Presenter::new(&self.x, width, height)?;
        self.presenters.push((surface.0, p));
        Ok(())
    }

    fn detach(&mut self, surface: SurfaceId) {
        if let Some(i) = self.presenters.iter().position(|(w, _)| *w == surface.0) {
            self.presenters.swap_remove(i).1.destroy(&self.x);
        }
    }

    fn detach_all(&mut self) {
        for (_, p) in self.presenters.drain(..) {
            p.destroy(&self.x);
        }
    }

    fn present(&mut self, surface: SurfaceId, fb: &Framebuffer) -> BackendResult<()> {
        if let Some((w, p)) = self.presenters.iter_mut().find(|(w, _)| *w == surface.0) {
            p.present(&self.x, *w, fb)?;
        }
        Ok(())
    }

    fn acquire_exclusive(&mut self, timeout: Duration) -> BackendResult<()> {
        match grab::grab_all(&self.x, self.x.root, timeout) {
            Ok(()) => {
                self.grabbed = true;
                self.mark_grab_seq();
                Ok(())
            }
            Err(e) => {
                self.grabbed = false;
                Err(e.into())
            }
        }
    }

    fn check_exclusive(&mut self) -> BackendResult<bool> {
        let held = grab::reassert(&self.x, self.x.root);
        match held {
            Ok(true) => self.mark_grab_seq(),
            Ok(false) | Err(_) => self.grabbed = false,
        }
        Ok(held?)
    }

    fn release_exclusive(&mut self) {
        if self.grabbed || !self.windows.windows().is_empty() {
            grab::ungrab_all(&self.x);
        }
        self.grabbed = false;
    }

    fn has_keyboard(&self) -> bool {
        self.keyboard.is_some()
    }

    fn prepare_keyboard(&mut self) {
        if self.keyboard.is_some() {
            return;
        }
        match Keyboard::new(&self.x) {
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

    // DPMS is untestable on Xvfb: it has no DPMS extension.
    fn power_install(&mut self, d: &DpmsConfig) {
        if !self.x.has_dpms {
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
        let _ = dpms::set_timeouts(c, secs_u16(d.standby), secs_u16(d.suspend), secs_u16(d.off));
        let _ = dpms::enable(c);
        if d.quick_off {
            let _ = dpms::force_level(c, dpms::DPMSMode::OFF);
        }
        self.x.flush();
        log::debug!("DPMS timeouts set, saved {:?}", self.dpms_saved);
    }

    fn power_off(&mut self) {
        if !self.x.has_dpms {
            return;
        }
        let _ = dpms::enable(&self.x.conn);
        let _ = dpms::force_level(&self.x.conn, dpms::DPMSMode::OFF);
        self.x.flush();
    }

    fn power_on(&mut self) {
        if self.dpms_enabled_on_server() {
            let _ = dpms::force_level(&self.x.conn, dpms::DPMSMode::ON);
            self.x.flush();
        }
    }

    fn power_restore(&mut self) {
        if !self.x.has_dpms {
            return;
        }
        self.power_on();
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

    fn publish_status(&mut self, status: Status, since: SystemTime, hacks: &[u32]) -> BackendResult<()> {
        self.status_items = write_status(&self.x, self.status_root, status, since, hacks)?;
        Ok(())
    }

    fn respond(&mut self, ok: bool, msg: &str) {
        if let Err(e) = protocol::send_response(&self.x.conn, &self.x.atoms, self.ident, ok, msg) {
            log::warn!("cannot send response {msg:?}: {e}");
        }
    }

    /// Before EXIT/RESTART destroys the rendezvous window: give the client up to `limit` to read
    /// (and delete) `_SCREENSAVER_RESPONSE`. Otherwise the real xscreensaver-command can hit
    /// BadWindow while reading and report "xscreensaver window unexpectedly deleted" (fatal
    /// for RESTART). Other events arriving meanwhile are dropped: we are shutting down.
    fn flush_response(&mut self, limit: Duration) {
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
}

/// Short name of an event for trace logs (never formats key data).
fn event_name(ev: &Event) -> &'static str {
    match ev {
        Event::UnmapNotify(_) => "UnmapNotify",
        Event::MapNotify(_) => "MapNotify",
        Event::DestroyNotify(_) => "DestroyNotify",
        Event::ConfigureNotify(_) => "ConfigureNotify",
        Event::FocusIn(_) => "FocusIn",
        Event::FocusOut(_) => "FocusOut",
        Event::KeyPress(_) => "KeyPress",
        Event::ButtonPress(_) => "ButtonPress",
        Event::MotionNotify(_) => "MotionNotify",
        Event::PropertyNotify(_) => "PropertyNotify",
        Event::VisibilityNotify(_) => "VisibilityNotify",
        _ => "other",
    }
}

/// `--demo-blank`: blank every monitor for 3 s with a small grey marker in each monitor's
/// top-left corner, then restore. Exercises connection, monitors, saver windows, grabs and
/// presenting. Fails if the server reports any X error during setup.
pub fn demo_blank(display: Option<&str>) -> Result<(), Box<dyn Error>> {
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
    let result = (|| -> Result<(), Box<dyn Error>> {
        wins.map_and_raise(&x)?;
        drain_errors(&x)?;
        let first = wins.windows().first().map(|&(w, _)| w).ok_or("no saver windows")?;
        grab::grab_all(&x, first, Duration::from_secs(5))?;
        let paint = (|| -> Result<(), Box<dyn Error>> {
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
            std::thread::sleep(Duration::from_secs(3));
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
