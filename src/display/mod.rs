//! Display-independent types shared by the display backends: monitor geometry and the
//! sanitising of monitor lists, which every backend must apply to what the display server
//! reports before creating per-monitor surfaces and framebuffers.
//!
//! It also defines the seam between the daemon (all policy: the Idle -> Blanked -> Locked
//! state machine, timers, commands, rendering) and a display backend (all mechanics: the
//! connection, surfaces, exclusive input, monitor power, the control channel). The daemon
//! only sees `DisplayEvent`s and calls `Backend` methods.

use std::error::Error;
use std::os::fd::RawFd;
use std::time::{Duration, Instant, SystemTime};

use crate::compat::protocol::{Command, Status};
use crate::config::DpmsConfig;
use crate::input::KeyInput;
use crate::render::Framebuffer;

pub type BackendResult<T> = Result<T, Box<dyn Error>>;

/// One blanking surface (an X window id on X11). Only meaningful to the backend that
/// issued it, and only until the next `blank`/`relayout`/`unblank`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SurfaceId(pub u32);

/// What the display can do; fixed for the lifetime of a backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// False when exclusive input here would not cover every client (XWayland): nothing
    /// may ever be reported as locked.
    pub can_lock: bool,
    /// The monitor can really be powered down (DPMS). Without it the daemon shows a black
    /// screen instead.
    pub power_control: bool,
    /// Exclusive input taken for blanking is not good enough to lock with: locking needs
    /// `acquire_exclusive` with `for_lock` (Wayland: a confirmed session lock). False
    /// where one grab serves both (X11).
    pub lock_is_separate: bool,
}

/// What a backend reports to the daemon. Input events never carry raw key data beyond
/// `KeyInput`, and must never be Debug-formatted into a log (AGENTS.md rule 3).
pub enum DisplayEvent {
    /// A raw event was consumed inside the backend. Reported so the daemon's per-iteration
    /// event cap also counts events that need no policy decision.
    Internal,
    /// A control command; the daemon answers it with `Backend::respond`.
    Command(Command),
    /// A key press delivered through our exclusive input. `None` when no keyboard state is
    /// available to translate it.
    Key(Option<KeyInput>),
    /// A pointer button press delivered through our exclusive input.
    Button,
    /// Pointer motion delivered through our exclusive input, in root coordinates.
    Motion { x: i32, y: i32 },
    /// A key or button press seen outside exclusive input (activity only, no content).
    RawActivity,
    /// The monitor layout changed.
    OutputsChanged,
    /// A blanking surface was moved, resized or destroyed behind our back.
    SurfacesDisturbed,
    /// A surface lost its contents and must be presented again.
    Redraw(SurfaceId),
    /// Exclusive input was (or may have been) taken away.
    ExclusiveLost(&'static str),
}

/// The display mechanics the daemon drives. Methods that return nothing are best effort
/// and log their own failures.
pub trait Backend {
    fn capabilities(&self) -> Capabilities;

    /// Wait up to `timeout` for (`Some`) or poll (`None`) one event. A wait also ends
    /// early, with `Ok(None)`, as soon as one of the `wake` descriptors is readable (the
    /// daemon's other input, such as the control socket). `Err` means the event could not
    /// be read; whether that is fatal is decided with `check_connection`.
    fn wait_event(
        &mut self,
        timeout: Option<Duration>,
        wake: &[RawFd],
    ) -> BackendResult<Option<DisplayEvent>>;
    /// Called once after each batch of events (deferred work such as keymap reloads).
    fn end_of_batch(&mut self);
    /// Round trip to the display server; `Err` when the connection is gone.
    fn check_connection(&self) -> BackendResult<()>;
    /// Earliest time `tick` has work to do.
    fn next_deadline(&self) -> Option<Instant>;
    /// Run the backend's own timers (keeping the surfaces on top).
    fn tick(&mut self, now: Instant);

    /// How long the user has been idle according to the display server. `None` when the
    /// backend cannot be polled for it.
    fn idle_time(&mut self) -> BackendResult<Option<Duration>>;
    /// Pointer position in root coordinates, if known.
    fn pointer_position(&self) -> Option<(i32, i32)>;

    /// Cover every monitor with a black surface. Returns the number of monitors.
    fn blank(&mut self) -> BackendResult<usize>;
    /// Recreate the surfaces for the current monitor layout. On `Err` the old surfaces are
    /// kept and stretched so nothing is left uncovered. Returns the number of monitors.
    fn relayout(&mut self) -> BackendResult<usize>;
    /// Put the surfaces back on screen and on top after exclusive input was lost.
    fn restore_cover(&mut self);
    /// Remove the surfaces (and everything attached to them).
    fn unblank(&mut self);
    /// The current surfaces and the monitor each one covers.
    fn surfaces(&self) -> Vec<(SurfaceId, Monitor)>;
    /// Prepare `surface` for `present` with frames of `width` x `height`.
    fn attach(&mut self, surface: SurfaceId, width: u32, height: u32) -> BackendResult<()>;
    /// Undo `attach` for one surface / for all of them.
    fn detach(&mut self, surface: SurfaceId);
    fn detach_all(&mut self);
    /// Show `fb` on an attached surface (a no-op for one that is not attached).
    fn present(&mut self, surface: SurfaceId, fb: &Framebuffer) -> BackendResult<()>;

    /// Take exclusive keyboard and pointer input, trying for up to `timeout`. With
    /// `for_lock` it must be good enough to lock the screen with: `Ok` then means no other
    /// client can receive input or show anything until `release_exclusive`.
    fn acquire_exclusive(&mut self, timeout: Duration, for_lock: bool) -> BackendResult<()>;
    /// Confirm exclusive input is still ours without releasing anything on failure.
    fn check_exclusive(&mut self) -> BackendResult<bool>;
    /// Give exclusive input up. This is what unlocks a locked display, so the daemon only
    /// calls it while not locked, or after a successful authentication.
    fn release_exclusive(&mut self);
    /// False when key presses cannot be translated (`DisplayEvent::Key(None)`).
    fn has_keyboard(&self) -> bool;
    /// Last chance to set the keyboard up, called when the screen locks.
    fn prepare_keyboard(&mut self);

    /// Install the blanked-screen power timeouts, remembering the previous settings.
    fn power_install(&mut self, cfg: &DpmsConfig);
    fn power_off(&mut self);
    fn power_on(&mut self);
    /// Wake the monitor and put the remembered settings back.
    fn power_restore(&mut self);

    /// Advertise the saver state (`hacks`: 1-based pattern number per surface, 0 = blank).
    fn publish_status(&mut self, status: Status, since: SystemTime, hacks: &[u32]) -> BackendResult<()>;
    /// Answer the command being handled.
    fn respond(&mut self, ok: bool, msg: &str);
    /// Give the client up to `limit` to read the answer before we go away.
    fn flush_response(&mut self, limit: Duration);
}

/// Geometry of one monitor in root-window coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Monitor {
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
    pub primary: bool,
}

/// Upper bound on the number of monitors we create windows/framebuffers for.
pub const MAX_MONITORS: usize = 16;

/// Clip `raw` monitors to the root `[0, root_w) x [0, root_h)`, drop empty ones, merge
/// identical rectangles (OR-ing `primary`, keeping first-seen order), cap at `MAX_MONITORS`,
/// and fall back to the whole root when nothing survives. Never returns an empty vector.
pub fn sanitize(raw: Vec<Monitor>, root_w: u16, root_h: u16) -> Vec<Monitor> {
    let (rw, rh) = (i32::from(root_w), i32::from(root_h));
    let mut out: Vec<Monitor> = Vec::new();
    let mut capped = false;
    for m in raw {
        // i32 arithmetic: i16 + u16 can exceed i16.
        let x0 = i32::from(m.x).max(0);
        let y0 = i32::from(m.y).max(0);
        let x1 = (i32::from(m.x) + i32::from(m.width)).min(rw);
        let y1 = (i32::from(m.y) + i32::from(m.height)).min(rh);
        if x1 <= x0 || y1 <= y0 {
            log::debug!("dropping monitor {m:?}: outside root {root_w}x{root_h} or empty");
            continue;
        }
        // All four values are within [0, 65535] and x0/y0 < root size <= u16::MAX, but the
        // origin must fit i16 for the Monitor type; skip anything that does not.
        let (Ok(cx), Ok(cy), Ok(cw), Ok(ch)) = (
            i16::try_from(x0),
            i16::try_from(y0),
            u16::try_from(x1 - x0),
            u16::try_from(y1 - y0),
        ) else {
            log::debug!("dropping monitor {m:?}: clipped geometry out of range");
            continue;
        };
        let c = Monitor { x: cx, y: cy, width: cw, height: ch, primary: m.primary };
        if c != m {
            log::debug!("clipped monitor {m:?} to {c:?}");
        }
        if let Some(d) = out
            .iter_mut()
            .find(|d| (d.x, d.y, d.width, d.height) == (c.x, c.y, c.width, c.height))
        {
            d.primary |= c.primary;
            continue;
        }
        if out.len() >= MAX_MONITORS {
            capped = true;
            continue;
        }
        out.push(c);
    }
    if capped {
        log::warn!("more than {MAX_MONITORS} distinct monitors reported; using the first {MAX_MONITORS}");
    }
    // Drop monitors fully contained in another (xscreensaver does the same): they would only
    // duplicate framebuffers, and a tiny "primary" could otherwise capture the dialog.
    let area = |m: &Monitor| i64::from(m.width) * i64::from(m.height);
    let contains = |a: &Monitor, b: &Monitor| {
        let (ax0, ay0) = (i32::from(a.x), i32::from(a.y));
        let (bx0, by0) = (i32::from(b.x), i32::from(b.y));
        bx0 >= ax0
            && by0 >= ay0
            && bx0 + i32::from(b.width) <= ax0 + i32::from(a.width)
            && by0 + i32::from(b.height) <= ay0 + i32::from(a.height)
    };
    let mut kept: Vec<Monitor> = Vec::with_capacity(out.len());
    let mut dropped_primary: Vec<Monitor> = Vec::new();
    for (i, m) in out.iter().enumerate() {
        // Identical rectangles were merged above, so a strictly larger (or earlier, if equal
        // area) container is unambiguous.
        let inside = out
            .iter()
            .enumerate()
            .any(|(j, o)| j != i && contains(o, m) && (area(o) > area(m) || j < i));
        if inside {
            log::debug!("dropping monitor {m:?}: contained in another");
            if m.primary {
                dropped_primary.push(*m);
            }
            continue;
        }
        kept.push(*m);
    }
    for d in dropped_primary {
        if let Some(k) = kept.iter_mut().find(|k| contains(k, &d)) {
            k.primary = true;
        }
    }
    out = kept;
    // Bound the total framebuffer area: overlapping client-defined monitors could otherwise
    // multiply allocations until the process aborts, which would drop the lock.
    let root_area = i64::from(root_w.max(1)) * i64::from(root_h.max(1));
    let total: i64 = out.iter().map(area).sum();
    if total > root_area.saturating_mul(2) {
        log::warn!("monitors cover {total} px, more than twice the root ({root_area} px); using the root");
        out.clear();
    }
    if out.is_empty() {
        out.push(Monitor { x: 0, y: 0, width: root_w.max(1), height: root_h.max(1), primary: true });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(x: i16, y: i16, width: u16, height: u16) -> Monitor {
        Monitor { x, y, width, height, primary: false }
    }

    fn root() -> Monitor {
        Monitor { x: 0, y: 0, width: 1280, height: 800, primary: true }
    }

    #[test]
    fn in_bounds_monitors_kept_in_order() {
        let raw = vec![m(0, 0, 640, 800), m(640, 0, 640, 800)];
        assert_eq!(sanitize(raw.clone(), 1280, 800), raw);
    }

    #[test]
    fn huge_monitor_is_clipped_to_root() {
        let out = sanitize(vec![m(0, 0, 30000, 30000)], 1280, 800);
        assert_eq!(out, vec![m(0, 0, 1280, 800)]);
        let out = sanitize(vec![m(i16::MAX, i16::MAX, u16::MAX, u16::MAX)], 1280, 800);
        assert_eq!(out, vec![root()], "fully outside -> root fallback");
    }

    #[test]
    fn partially_off_root_is_clipped() {
        let out = sanitize(vec![m(-100, -50, 400, 300), m(1200, 700, 500, 500)], 1280, 800);
        assert_eq!(out, vec![m(0, 0, 300, 250), m(1200, 700, 80, 100)]);
    }

    #[test]
    fn off_root_and_empty_dropped() {
        let raw = vec![
            m(2000, 0, 100, 100),
            m(0, 900, 100, 100),
            m(-500, 0, 400, 100),
            m(10, 10, 0, 100),
            m(10, 10, 100, 0),
            m(100, 100, 50, 50),
        ];
        assert_eq!(sanitize(raw, 1280, 800), vec![m(100, 100, 50, 50)]);
    }

    #[test]
    fn identical_deduped_overlapping_kept() {
        let mut p = m(0, 0, 1280, 800);
        p.primary = true;
        // Two rectangles that become identical after clipping, plus an overlapping one.
        let raw = vec![m(0, 0, 1280, 800), p, m(0, 0, 5000, 5000), m(100, 100, 640, 480)];
        let out = sanitize(raw, 1280, 800);
        // The overlapping one lies inside the root-sized monitor and is dropped.
        assert_eq!(out, vec![p], "primary flag merged into first occurrence");
    }

    #[test]
    fn capped_at_max_after_dedupe() {
        let mut raw: Vec<Monitor> = (0..20).map(|i| m(i * 10, 0, 10, 10)).collect();
        // Duplicates do not count towards the cap.
        raw.insert(1, m(0, 0, 10, 10));
        let out = sanitize(raw, 1280, 800);
        assert_eq!(out.len(), MAX_MONITORS);
        assert_eq!(out[MAX_MONITORS - 1], m((MAX_MONITORS as i16 - 1) * 10, 0, 10, 10));
    }

    #[test]
    fn nothing_survives_falls_back_to_root() {
        assert_eq!(sanitize(Vec::new(), 1280, 800), vec![root()]);
        assert_eq!(sanitize(vec![m(0, 0, 0, 0)], 1280, 800), vec![root()]);
        // Degenerate root still yields a non-empty, non-zero rectangle.
        assert_eq!(sanitize(Vec::new(), 0, 0)[0].width, 1);
    }

    #[test]
    fn contained_monitors_are_dropped_and_primary_propagates() {
        let mut tiny = m(10, 10, 4, 4);
        tiny.primary = true;
        let out = sanitize(vec![m(0, 0, 1280, 800), tiny, m(100, 100, 200, 200)], 1280, 800);
        assert_eq!(out.len(), 1);
        assert_eq!((out[0].width, out[0].height), (1280, 800));
        assert!(out[0].primary, "primary flag moves to the containing monitor");
    }

    #[test]
    fn overlapping_but_not_contained_survive() {
        let out = sanitize(vec![m(0, 0, 1000, 800), m(500, 0, 780, 800)], 1280, 800);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn excessive_total_area_falls_back_to_root() {
        // Sixteen near-root monitors each offset by one pixel: none contains another.
        let raw: Vec<Monitor> = (0..16).map(|i| m(i, 0, 1264, 800)).collect();
        let out = sanitize(raw, 1280, 800);
        assert_eq!(out, vec![root()]);
    }
}
