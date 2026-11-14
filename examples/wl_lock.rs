//! Test helper: hold the Wayland session lock for a while, as another screen locker
//! would, so the daemon's "lock pending" path can be exercised (the Wayland counterpart
//! of `hold_grab`).
//!
//! Usage: wl_lock SECONDS

use wayland_client::protocol::wl_registry;
use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
use wayland_protocols::ext::session_lock::v1::client::{
    ext_session_lock_manager_v1::ExtSessionLockManagerV1,
    ext_session_lock_v1::{self, ExtSessionLockV1},
};

#[derive(Default)]
struct App {
    manager: Option<ExtSessionLockManagerV1>,
    held: Option<bool>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for App {
    fn event(
        app: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, .. } = event
            && interface == "ext_session_lock_manager_v1"
        {
            app.manager = Some(registry.bind(name, 1, qh, ()));
        }
    }
}

impl Dispatch<ExtSessionLockV1, ()> for App {
    fn event(
        app: &mut Self,
        _: &ExtSessionLockV1,
        event: ext_session_lock_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_session_lock_v1::Event::Locked => app.held = Some(true),
            ext_session_lock_v1::Event::Finished => app.held = Some(false),
            _ => {}
        }
    }
}

delegate_noop!(App: ExtSessionLockManagerV1);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let secs: f64 = std::env::args().nth(1).ok_or("usage: wl_lock SECONDS")?.parse()?;
    let conn = Connection::connect_to_env()?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let mut app = App::default();
    conn.display().get_registry(&qh, ());
    queue.roundtrip(&mut app)?;
    let manager = app.manager.clone().ok_or("the compositor has no ext-session-lock-v1")?;
    let lock = manager.lock(&qh, ());
    while app.held.is_none() {
        queue.blocking_dispatch(&mut app)?;
    }
    if app.held != Some(true) {
        lock.destroy();
        queue.roundtrip(&mut app)?;
        return Err("the compositor refused the lock".into());
    }
    println!("locked");
    std::thread::sleep(std::time::Duration::from_secs_f64(secs));
    lock.unlock_and_destroy();
    queue.roundtrip(&mut app)?;
    println!("unlocked");
    Ok(())
}
