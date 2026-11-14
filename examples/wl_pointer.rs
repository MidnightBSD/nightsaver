//! Test helper: a virtual pointer for a headless Wayland compositor (wlr-virtual-pointer).
//! The device exists while this program runs. Commands are read from standard input,
//! one per line: `move DX DY` (relative motion) or `click`.

use std::io::BufRead;

use wayland_client::protocol::{wl_pointer, wl_registry};
use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

#[derive(Default)]
struct App {
    manager: Option<ZwlrVirtualPointerManagerV1>,
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
            && interface == "zwlr_virtual_pointer_manager_v1"
        {
            app.manager = Some(registry.bind(name, 1, qh, ()));
        }
    }
}

delegate_noop!(App: ZwlrVirtualPointerManagerV1);
delegate_noop!(App: ZwlrVirtualPointerV1);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let conn = Connection::connect_to_env()?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let mut app = App::default();
    conn.display().get_registry(&qh, ());
    queue.roundtrip(&mut app)?;
    let manager = app.manager.clone().ok_or("the compositor has no wlr-virtual-pointer")?;
    let pointer = manager.create_virtual_pointer(None, &qh, ());
    queue.roundtrip(&mut app)?;
    println!("ready");
    let mut time = 0u32;
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        let words: Vec<&str> = line.split_whitespace().collect();
        time += 10;
        match words.as_slice() {
            ["move", dx, dy] => pointer.motion(time, dx.parse()?, dy.parse()?),
            ["click"] => {
                pointer.button(time, 0x110, wl_pointer::ButtonState::Pressed);
                pointer.frame();
                time += 10;
                pointer.button(time, 0x110, wl_pointer::ButtonState::Released);
            }
            _ => eprintln!("unknown command: {line}"),
        }
        pointer.frame();
        queue.roundtrip(&mut app)?;
    }
    Ok(())
}
