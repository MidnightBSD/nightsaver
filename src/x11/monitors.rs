//! RANDR monitor enumeration with root fallback, hotplug notifications. Owner: Opus A.
//! Contract: `fn monitors(x: &X11) -> Result<Vec<Monitor>, X11Error>` (never empty: falls back to root geometry),
//! `fn select_hotplug(x: &X11) -> Result<(), X11Error>`, `fn is_hotplug_event(ev: &Event) -> bool`.
//!
//! Xinerama is not queried: RANDR >= 1.5 `GetMonitors` covers the same information on any
//! server we support, and the root geometry is the last-resort fallback.
//!
//! RANDR monitors are client-settable (`xrandr --setmonitor`), so they are untrusted: every
//! rectangle is clipped to the *current* root geometry (a fresh `GetGeometry`, not the stale
//! connection-setup values), empty results are dropped, identical rectangles are merged and the
//! count is capped at `MAX_MONITORS`. This bounds per-monitor framebuffer allocations by the
//! root size.

use x11rb::protocol::Event;
use x11rb::protocol::randr;
use x11rb::protocol::xproto::ConnectionExt as _;

use super::conn::X11;
use super::{Monitor, X11Error};

/// Upper bound on the number of monitors we create windows/framebuffers for.
pub const MAX_MONITORS: usize = 16;

/// Current monitors in root coordinates. Never empty.
pub fn monitors(x: &X11) -> Result<Vec<Monitor>, X11Error> {
    let (root_w, root_h) = root_size(x)?;
    let raw: Vec<Monitor> = match randr::get_monitors(&x.conn, x.root, true)?.reply() {
        Ok(r) => r
            .monitors
            .iter()
            .map(|m| Monitor {
                x: m.x,
                y: m.y,
                width: m.width,
                height: m.height,
                primary: m.primary,
            })
            .collect(),
        // RANDR < 1.5 servers answer GetMonitors with BadRequest; fall back to root geometry.
        Err(x11rb::errors::ReplyError::X11Error(e)) => {
            log::debug!(
                "RANDR GetMonitors failed ({:?}); using root geometry",
                e.error_kind
            );
            Vec::new()
        }
        Err(e) => return Err(e.into()),
    };
    Ok(sanitize(raw, root_w, root_h))
}

/// Current root window size from a fresh `GetGeometry` (the setup values go stale after a
/// RANDR resize). Falls back to the setup values if the server reports a 0-sized root.
fn root_size(x: &X11) -> Result<(u16, u16), X11Error> {
    let g = x.conn.get_geometry(x.root)?.reply()?;
    if g.width > 0 && g.height > 0 {
        Ok((g.width, g.height))
    } else {
        let s = x.screen();
        log::warn!(
            "GetGeometry(root) reported {}x{}; using setup size {}x{}",
            g.width,
            g.height,
            s.width_in_pixels,
            s.height_in_pixels
        );
        Ok((s.width_in_pixels.max(1), s.height_in_pixels.max(1)))
    }
}

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

/// Ask for RANDR screen/CRTC/output change notifications on the root window.
pub fn select_hotplug(x: &X11) -> Result<(), X11Error> {
    randr::select_input(
        &x.conn,
        x.root,
        randr::NotifyMask::SCREEN_CHANGE
            | randr::NotifyMask::CRTC_CHANGE
            | randr::NotifyMask::OUTPUT_CHANGE,
    )?;
    x.flush();
    Ok(())
}

/// True for RANDR events that mean the monitor layout may have changed.
pub fn is_hotplug_event(ev: &Event) -> bool {
    matches!(
        ev,
        Event::RandrScreenChangeNotify(_) | Event::RandrNotify(_)
    )
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
