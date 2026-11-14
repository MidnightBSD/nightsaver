//! RANDR monitor enumeration with root fallback, hotplug notifications. Owner: Opus A.
//! Contract: `fn monitors(x: &X11) -> Result<Vec<Monitor>, X11Error>` (never empty: falls back to root geometry),
//! `fn select_hotplug(x: &X11) -> Result<(), X11Error>`, `fn is_hotplug_event(ev: &Event) -> bool`.
//!
//! Xinerama is not queried: RANDR >= 1.5 `GetMonitors` covers the same information on any
//! server we support, and the root geometry is the last-resort fallback.
//!
//! RANDR monitors are client-settable (`xrandr --setmonitor`), so they are untrusted: every
//! rectangle is clipped to the *current* root geometry (a fresh `GetGeometry`, not the stale
//! connection-setup values) by `crate::display::sanitize`, which also drops empty results,
//! merges identical rectangles and caps the count. This bounds per-monitor framebuffer
//! allocations by the root size.

use x11rb::protocol::Event;
use x11rb::protocol::randr;
use x11rb::protocol::xproto::ConnectionExt as _;

use super::X11Error;
use super::conn::X11;
use crate::display::{Monitor, sanitize};

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
