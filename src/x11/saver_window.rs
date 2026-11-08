//! Override-redirect black saver windows (one per monitor), cursor hiding, map/raise/unmap. Owner: Opus A.
//! Contract: `SaverWindows::create(&X11, &[Monitor])`, `.map_and_raise(&X11)`, `.raise(&X11)`, `.unmap(&X11)`,
//! `.destroy(&X11)`, `.windows() -> &[(Window, Monitor)]`, `.contains(Window)`, `.relayout(&X11, &[Monitor])`,
//! `.is_foreign_map(&Event)`, `.overlapping()`, `.cover_root(&X11, Monitor)`, `select_root_events(&X11)`.
//!
//! Cursor hiding uses a per-window invisible cursor (1x1 pixmap with an all-zero mask) passed as
//! the window's `cursor` attribute. XFIXES `HideCursor` is deliberately *not* used: it hides the
//! pointer for the whole screen until `ShowCursor`/disconnect, not just over our windows, so a
//! long-lived daemon could leave the desktop cursor hidden after unblanking.
//!
//! `select_root_events` (also called by `create`) adds SUBSTRUCTURE_NOTIFY, FOCUS_CHANGE and
//! PROPERTY_CHANGE to this client's event mask on the root window (read-modify-write, so masks
//! selected on root by other code on the same connection are preserved). SUBSTRUCTURE_NOTIFY
//! reports Map/Configure/Circulate/Unmap of every top-level window (ours included: classify by
//! the event's `window`, not its `event`), FOCUS_CHANGE reports the FocusOut(Ungrab) that marks
//! a broken root grab, PROPERTY_CHANGE reports writes to `_SCREENSAVER_STATUS`.
//!
//! Key/pointer input is not read from these windows: the daemon grabs on root, so input events
//! carry `event == root`. KEY_RELEASE is not selected (nothing uses it, and it must never be
//! logged).

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ConfigureWindowAux, ConnectionExt as _, CreateGCAux,
    CreateWindowAux, Cursor, EventMask, PropMode, Rectangle, StackMode, Window, WindowClass,
};
use x11rb::wrapper::ConnectionExt as _;

use super::conn::X11;
use super::{Monitor, X11Error};

const NAME: &str = "nightsaver";

/// The set of saver windows, one per monitor.
#[derive(Debug, Default)]
pub struct SaverWindows {
    wins: Vec<(Window, Monitor)>,
    cursor: Cursor,
    mapped: bool,
}

/// Build an invisible 1x1 cursor.
fn invisible_cursor(x: &X11) -> Result<Cursor, X11Error> {
    let c = &x.conn;
    let pix = c.generate_id()?;
    c.create_pixmap(1, pix, x.root, 1, 1)?;
    // Pixmap contents are undefined after creation: clear it so the mask is fully transparent.
    let gc = c.generate_id()?;
    c.create_gc(gc, pix, &CreateGCAux::new().foreground(0))?;
    c.poly_fill_rectangle(
        pix,
        gc,
        &[Rectangle {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        }],
    )?;
    c.free_gc(gc)?;
    let cursor = c.generate_id()?;
    c.create_cursor(cursor, pix, pix, 0, 0, 0, 0, 0, 0, 0, 0)?
        .check()?;
    c.free_pixmap(pix)?;
    Ok(cursor)
}

fn saver_event_mask() -> EventMask {
    EventMask::KEY_PRESS
        | EventMask::BUTTON_PRESS
        | EventMask::BUTTON_RELEASE
        | EventMask::POINTER_MOTION
        | EventMask::EXPOSURE
        | EventMask::STRUCTURE_NOTIFY
        | EventMask::FOCUS_CHANGE
        | EventMask::VISIBILITY_CHANGE
}

fn create_one(x: &X11, m: &Monitor, cursor: Cursor) -> Result<Window, X11Error> {
    let c = &x.conn;
    let win = c.generate_id()?;
    let aux = CreateWindowAux::new()
        .background_pixel(0)
        .border_pixel(0)
        .override_redirect(1)
        .event_mask(saver_event_mask())
        .cursor(cursor);
    c.create_window(
        x.depth,
        win,
        x.root,
        m.x,
        m.y,
        m.width.max(1),
        m.height.max(1),
        0,
        WindowClass::INPUT_OUTPUT,
        x.visual,
        &aux,
    )?
    .check()?;
    let utf8 = x.atoms.get("UTF8_STRING");
    let net_wm_name = x.atoms.get("_NET_WM_NAME");
    if utf8 != x11rb::NONE && net_wm_name != x11rb::NONE {
        c.change_property8(PropMode::REPLACE, win, net_wm_name, utf8, NAME.as_bytes())?;
    }
    c.change_property8(
        PropMode::REPLACE,
        win,
        AtomEnum::WM_NAME,
        AtomEnum::STRING,
        NAME.as_bytes(),
    )?;
    let class = format!("{NAME}\0{NAME}\0");
    c.change_property8(
        PropMode::REPLACE,
        win,
        AtomEnum::WM_CLASS,
        AtomEnum::STRING,
        class.as_bytes(),
    )?;
    Ok(win)
}

/// Add SUBSTRUCTURE_NOTIFY, FOCUS_CHANGE and PROPERTY_CHANGE to our existing root event mask
/// without clobbering it.
pub fn select_root_events(x: &X11) -> Result<(), X11Error> {
    let attrs = x.conn.get_window_attributes(x.root)?.reply()?;
    let want = attrs.your_event_mask
        | EventMask::SUBSTRUCTURE_NOTIFY
        | EventMask::FOCUS_CHANGE
        | EventMask::PROPERTY_CHANGE;
    if want != attrs.your_event_mask {
        x.conn
            .change_window_attributes(x.root, &ChangeWindowAttributesAux::new().event_mask(want))?
            .check()?;
    }
    Ok(())
}

impl SaverWindows {
    /// Create (unmapped) one black override-redirect window per monitor.
    pub fn create(x: &X11, monitors: &[Monitor]) -> Result<SaverWindows, X11Error> {
        select_root_events(x)?;
        let cursor = invisible_cursor(x)?;
        let mut sw = SaverWindows {
            wins: Vec::with_capacity(monitors.len()),
            cursor,
            mapped: false,
        };
        for m in monitors {
            match create_one(x, m, cursor) {
                Ok(w) => sw.wins.push((w, *m)),
                Err(e) => {
                    sw.destroy(x);
                    return Err(e);
                }
            }
        }
        x.flush();
        Ok(sw)
    }

    /// Map all windows, raise them to the top, and wait (one round trip) until the server has
    /// processed the requests, so grabs and drawing can follow immediately.
    pub fn map_and_raise(&mut self, x: &X11) -> Result<(), X11Error> {
        for &(w, _) in &self.wins {
            x.conn.map_window(w)?;
        }
        self.mapped = true;
        self.raise(x)?;
        x.sync()?;
        Ok(())
    }

    /// Raise all windows to the top of the stacking order.
    pub fn raise(&self, x: &X11) -> Result<(), X11Error> {
        let aux = ConfigureWindowAux::new().stack_mode(StackMode::ABOVE);
        for &(w, _) in &self.wins {
            x.conn.configure_window(w, &aux)?;
        }
        x.flush();
        Ok(())
    }

    /// Unmap all windows (best effort).
    pub fn unmap(&mut self, x: &X11) {
        for &(w, _) in &self.wins {
            let _ = x.conn.unmap_window(w);
        }
        self.mapped = false;
        x.flush();
    }

    /// Destroy all windows and the cursor (best effort). The value is left empty and reusable.
    pub fn destroy(&mut self, x: &X11) {
        for (w, _) in self.wins.drain(..) {
            let _ = x.conn.destroy_window(w);
        }
        if self.cursor != x11rb::NONE {
            let _ = x.conn.free_cursor(self.cursor);
            self.cursor = x11rb::NONE;
        }
        self.mapped = false;
        x.flush();
    }

    pub fn windows(&self) -> &[(Window, Monitor)] {
        &self.wins
    }

    pub fn is_mapped(&self) -> bool {
        self.mapped
    }

    pub fn contains(&self, w: Window) -> bool {
        self.wins.iter().any(|&(ww, _)| ww == w)
    }

    /// Recreate windows for a new monitor layout. If the windows were mapped, the new ones are
    /// mapped and raised too before the old ones are destroyed (callers redraw afterwards; a
    /// grab on root is not affected). On error the old windows are kept unchanged.
    pub fn relayout(&mut self, x: &X11, monitors: &[Monitor]) -> Result<(), X11Error> {
        let was_mapped = self.mapped;
        // Create the new set before destroying the old one so the screen is never uncovered.
        let mut fresh = SaverWindows::create(x, monitors)?;
        if was_mapped && let Err(e) = fresh.map_and_raise(x) {
            fresh.destroy(x);
            return Err(e);
        }
        self.destroy(x);
        *self = fresh;
        Ok(())
    }

    /// Fallback when a relayout fails: stretch the first window over `root` (the current root
    /// geometry) and raise everything, so the whole screen stays covered.
    pub fn cover_root(&mut self, x: &X11, root: Monitor) -> Result<(), X11Error> {
        let Some((w, m)) = self.wins.first_mut() else {
            return Err(X11Error::Protocol("no saver window to stretch".into()));
        };
        x.conn.configure_window(
            *w,
            &ConfigureWindowAux::new()
                .x(i32::from(root.x))
                .y(i32::from(root.y))
                .width(u32::from(root.width.max(1)))
                .height(u32::from(root.height.max(1)))
                .stack_mode(StackMode::ABOVE),
        )?;
        *m = root;
        if self.mapped {
            x.conn.map_window(*w)?;
        }
        self.raise(x)
    }

    /// True when two of our windows intersect (mirrored or overlapping monitors). They then
    /// obscure each other, so VisibilityNotify says nothing about foreign windows.
    pub fn overlapping(&self) -> bool {
        let r = |m: &Monitor| {
            let (x0, y0) = (i32::from(m.x), i32::from(m.y));
            (x0, y0, x0 + i32::from(m.width.max(1)), y0 + i32::from(m.height.max(1)))
        };
        self.wins.iter().enumerate().any(|(i, (_, a))| {
            let a = r(a);
            self.wins[i + 1..].iter().any(|(_, b)| {
                let b = r(b);
                a.0 < b.2 && b.0 < a.2 && a.1 < b.3 && b.1 < a.3
            })
        })
    }

    /// True for a MapNotify (from root SUBSTRUCTURE_NOTIFY) about a window that is not ours.
    pub fn is_foreign_map(&self, ev: &Event) -> bool {
        match ev {
            Event::MapNotify(m) => !self.contains(m.window),
            _ => false,
        }
    }
}
