//! Keyboard/pointer grab with retry. Owner: Opus A.
//! Contract: `fn grab_all(x: &X11, window: Window, timeout: Duration) -> Result<(), X11Error>`,
//! `fn reassert(x: &X11, window: Window) -> Result<bool, X11Error>`, `fn ungrab_all(x: &X11)`.
//!
//! The daemon grabs on the **root window** (`owner_events = false`, `confine_to = None`): root
//! is always viewable, so destroying or recreating saver windows (RANDR relayout) never breaks
//! the grab, and every key/pointer event is reported with `event == root`. With cursor `None`
//! the server shows the cursor of the window under the pointer, i.e. the saver windows'
//! invisible cursor.
//!
//! `grab_all` retries every 50 ms until both grabs succeed or `timeout` elapses. On failure
//! no grab is left behind (a keyboard grab obtained earlier is released). The window must be
//! viewable, otherwise the server answers NOT_VIEWABLE until it is.
//!
//! `reassert` re-issues both grabs once without rolling anything back. A GrabKeyboard /
//! GrabPointer from the client that already holds the grab simply succeeds (it is idempotent and,
//! on the same window, generates no focus/crossing events), so this is a cheap periodic check that
//! the grab is still ours.

use std::time::{Duration, Instant};

use x11rb::CURRENT_TIME;
use x11rb::NONE;
use x11rb::protocol::xproto::{ConnectionExt as _, EventMask, GrabMode, GrabStatus, Window};

use super::X11Error;
use super::conn::X11;

const RETRY: Duration = Duration::from_millis(50);

fn try_keyboard(x: &X11, window: Window) -> Result<GrabStatus, X11Error> {
    Ok(x.conn
        .grab_keyboard(
            false,
            window,
            CURRENT_TIME,
            GrabMode::ASYNC,
            GrabMode::ASYNC,
        )?
        .reply()?
        .status)
}

fn try_pointer(x: &X11, window: Window) -> Result<GrabStatus, X11Error> {
    let mask = EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION;
    Ok(x.conn
        .grab_pointer(
            false,
            window,
            mask,
            GrabMode::ASYNC,
            GrabMode::ASYNC,
            NONE,
            NONE,
            CURRENT_TIME,
        )?
        .reply()?
        .status)
}

/// Grab keyboard then pointer on `window`, retrying until `timeout`.
pub fn grab_all(x: &X11, window: Window, timeout: Duration) -> Result<(), X11Error> {
    let deadline = Instant::now() + timeout;
    let mut have_kbd = false;
    let mut last = (GrabStatus::SUCCESS, GrabStatus::SUCCESS);
    loop {
        if !have_kbd {
            let s = try_keyboard(x, window)?;
            last.0 = s;
            have_kbd = s == GrabStatus::SUCCESS;
        }
        if have_kbd {
            let s = try_pointer(x, window)?;
            last.1 = s;
            if s == GrabStatus::SUCCESS {
                return Ok(());
            }
        }
        if Instant::now() + RETRY > deadline {
            break;
        }
        std::thread::sleep(RETRY);
    }
    log::warn!("grab failed: keyboard={:?} pointer={:?}", last.0, last.1);
    if have_kbd {
        let _ = x.conn.ungrab_keyboard(CURRENT_TIME);
        x.flush();
    }
    Err(X11Error::GrabFailed)
}

/// Re-issue both grabs once, without releasing anything on failure. True when both are held.
pub fn reassert(x: &X11, window: Window) -> Result<bool, X11Error> {
    let k = try_keyboard(x, window)?;
    let p = try_pointer(x, window)?;
    if k != GrabStatus::SUCCESS || p != GrabStatus::SUCCESS {
        log::warn!("grab re-assert failed: keyboard={k:?} pointer={p:?}");
        return Ok(false);
    }
    Ok(true)
}

/// Release keyboard and pointer grabs (best effort) and flush.
pub fn ungrab_all(x: &X11) {
    let _ = x.conn.ungrab_keyboard(CURRENT_TIME);
    let _ = x.conn.ungrab_pointer(CURRENT_TIME);
    x.flush();
}
