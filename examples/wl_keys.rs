//! Test helper: a virtual keyboard for a headless Wayland compositor (virtual-keyboard-v1)
//! with the "us" layout. The device exists while this program runs. Commands are read from
//! standard input, one per line: `type TEXT` (characters the layout can produce, with or
//! without Shift) or `key enter|escape|backspace`.

use std::io::{BufRead, Write};
use std::os::fd::AsFd;

use wayland_client::protocol::{wl_registry, wl_seat::WlSeat};
use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use xkbcommon::xkb;

/// `wl_keyboard.keymap_format.xkb_v1` and the key states, as the protocol numbers them.
const XKB_V1: u32 = 1;
const RELEASED: u32 = 0;
const PRESSED: u32 = 1;

#[derive(Default)]
struct App {
    manager: Option<ZwpVirtualKeyboardManagerV1>,
    seat: Option<WlSeat>,
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
        if let wl_registry::Event::Global { name, interface, .. } = event {
            match interface.as_str() {
                "zwp_virtual_keyboard_manager_v1" => app.manager = Some(registry.bind(name, 1, qh, ())),
                "wl_seat" if app.seat.is_none() => app.seat = Some(registry.bind(name, 1, qh, ())),
                _ => {}
            }
        }
    }
}

delegate_noop!(App: ZwpVirtualKeyboardManagerV1);
delegate_noop!(App: ZwpVirtualKeyboardV1);
delegate_noop!(App: ignore WlSeat);

/// The evdev key code and whether Shift is needed to produce `ch` in `keymap`.
fn key_for(keymap: &xkb::Keymap, ch: char) -> Option<(u32, bool)> {
    for raw in keymap.min_keycode().raw()..=keymap.max_keycode().raw() {
        for level in 0..2 {
            let syms = keymap.key_get_syms_by_level(xkb::Keycode::new(raw), 0, level);
            if syms.len() == 1 && xkb::keysym_to_utf32(syms[0]) == u32::from(ch) {
                return Some((raw.checked_sub(8)?, level == 1));
            }
        }
    }
    None
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let conn = Connection::connect_to_env()?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let mut app = App::default();
    conn.display().get_registry(&qh, ());
    queue.roundtrip(&mut app)?;
    let manager = app.manager.clone().ok_or("the compositor has no virtual-keyboard-v1")?;
    let seat = app.seat.clone().ok_or("the compositor has no seat")?;

    let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap = xkb::Keymap::new_from_names(&ctx, "evdev", "pc105", "us", "", None, xkb::KEYMAP_COMPILE_NO_FLAGS)
        .ok_or("no xkb data for the us layout")?;
    let shift = 1u32 << keymap.mod_get_index(xkb::MOD_NAME_SHIFT);
    let mut text = keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1).into_bytes();
    text.push(0);
    // The compositor maps the keymap from a descriptor: an unlinked file in the runtime dir.
    let dir = std::env::var_os("XDG_RUNTIME_DIR").ok_or("XDG_RUNTIME_DIR is not set")?;
    let path = std::path::Path::new(&dir).join(format!("wl-keys-{}.xkb", std::process::id()));
    let mut file = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&path)?;
    std::fs::remove_file(&path)?;
    file.write_all(&text)?;

    let keyboard = manager.create_virtual_keyboard(&seat, &qh, ());
    keyboard.keymap(XKB_V1, file.as_fd(), u32::try_from(text.len())?);
    queue.roundtrip(&mut app)?;
    println!("ready");

    let mut time = 0u32;
    let mut tap = |code: u32, shifted: bool| {
        if shifted {
            keyboard.modifiers(shift, 0, 0, 0);
        }
        time += 10;
        keyboard.key(time, code, PRESSED);
        time += 10;
        keyboard.key(time, code, RELEASED);
        if shifted {
            keyboard.modifiers(0, 0, 0, 0);
        }
    };
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        match line.split_once(' ') {
            Some(("type", s)) => {
                for ch in s.chars() {
                    match key_for(&keymap, ch) {
                        Some((code, shifted)) => tap(code, shifted),
                        None => eprintln!("no key for {ch:?}"),
                    }
                }
            }
            // evdev codes from <linux/input-event-codes.h>.
            Some(("key", "escape")) => tap(1, false),
            Some(("key", "backspace")) => tap(14, false),
            Some(("key", "enter")) => tap(28, false),
            _ => eprintln!("unknown command: {line}"),
        }
        queue.roundtrip(&mut app)?;
    }
    Ok(())
}
