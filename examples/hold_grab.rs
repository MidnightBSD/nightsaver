//! Test helper for tests/xvfb_smoke.sh: grab the keyboard on the root window of $DISPLAY for N
//! seconds (default 5), printing "grabbed" once the grab is held. Run against Xvfb only.
use std::io::Write;
use std::time::Duration;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt as _, GrabMode, GrabStatus};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let secs: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(5);
    let (conn, screen) = x11rb::connect(None)?;
    let root = conn.setup().roots[screen].root;
    let r = conn
        .grab_keyboard(false, root, x11rb::CURRENT_TIME, GrabMode::ASYNC, GrabMode::ASYNC)?
        .reply()?;
    if r.status != GrabStatus::SUCCESS {
        return Err(format!("grab failed: {:?}", r.status).into());
    }
    println!("grabbed");
    std::io::stdout().flush()?;
    std::thread::sleep(Duration::from_secs(secs));
    Ok(())
}
