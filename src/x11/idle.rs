//! Idle time via MIT-SCREEN-SAVER, activity via XInput2 raw events, server blanker disable. Owner: Opus A.
//! Contract: `fn idle_ms(x: &X11) -> Result<u32, X11Error>`, `fn select_activity_events(x: &X11) -> Result<(), X11Error>`,
//! `fn disable_server_blanker(x: &X11)`, `fn is_activity_event(ev: &Event) -> bool`.

use x11rb::protocol::Event;
use x11rb::protocol::screensaver;
use x11rb::protocol::xinput;
use x11rb::protocol::xproto::{Blanking, ConnectionExt as _, Exposures};

use super::X11Error;
use super::conn::X11;

/// Milliseconds since the last user input, as tracked by the server.
pub fn idle_ms(x: &X11) -> Result<u32, X11Error> {
    Ok(screensaver::query_info(&x.conn, x.root)?
        .reply()?
        .ms_since_user_input)
}

/// Select XI2 raw motion/key/button events for all master devices on root, plus
/// MIT-SCREEN-SAVER notify events. Raw events are delivered regardless of focus, and (because
/// `X11::open` announces XI 2.2) also while another client holds a grab.
pub fn select_activity_events(x: &X11) -> Result<(), X11Error> {
    let mask = xinput::EventMask {
        deviceid: u16::from(xinput::Device::ALL_MASTER),
        mask: vec![
            xinput::XIEventMask::RAW_MOTION
                | xinput::XIEventMask::RAW_KEY_PRESS
                | xinput::XIEventMask::RAW_BUTTON_PRESS,
        ],
    };
    xinput::xi_select_events(&x.conn, x.root, &[mask])?.check()?;
    screensaver::select_input(&x.conn, x.root, screensaver::Event::NOTIFY_MASK)?.check()?;
    Ok(())
}

/// Turn off the X server's own blanking so it doesn't fight with us.
/// Errors are logged, not returned (best effort).
pub fn disable_server_blanker(x: &X11) {
    match x
        .conn
        .set_screen_saver(0, 0, Blanking::NOT_PREFERRED, Exposures::NOT_ALLOWED)
    {
        Ok(c) => {
            if let Err(e) = c.check() {
                log::warn!("SetScreenSaver failed: {e}");
            }
        }
        Err(e) => log::warn!("SetScreenSaver failed: {e}"),
    }
}

/// True for events that indicate user activity (XI2 raw or core input events).
pub fn is_activity_event(ev: &Event) -> bool {
    matches!(
        ev,
        Event::XinputRawMotion(_)
            | Event::XinputRawKeyPress(_)
            | Event::XinputRawButtonPress(_)
            | Event::KeyPress(_)
            | Event::ButtonPress(_)
            | Event::MotionNotify(_)
    )
}
