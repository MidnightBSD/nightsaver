//! xkbcommon keymap/state for translating KeyPress during the lock dialog. Owner: Opus E.
//!
//! Contract: `Keyboard::new(&X11)`, `.handle_event(&Event) -> bool` (alias `handle_xkb_event`),
//! `.translate(&KeyPressEvent) -> KeyInput`, `is_xkb_event(&Event)`.
//!
//! Keymap reload: `handle_event` has no connection, so on `XkbNewKeyboardNotify` /
//! `XkbMapNotify` it only marks the keymap stale (the old keymap keeps working). The daemon
//! should call `kb.reload_if_stale(&x)` after handling events; `XkbStateNotify` (modifier and
//! group changes, e.g. Shift/Caps Lock) is applied immediately.

use x11rb::protocol::Event;
use x11rb::protocol::xkb::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{KeyPressEvent, ModMask};
use xkbcommon::xkb as xk;

use super::X11Error;
use super::conn::X11;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyInput {
    Text(String),
    Backspace,
    Enter,
    Escape,
    /// Clear whole buffer (Ctrl-U).
    Kill,
    Other,
}

/// Is this an XKB extension event (the kind `Keyboard::handle_event` consumes)?
pub fn is_xkb_event(ev: &Event) -> bool {
    matches!(
        ev,
        Event::XkbAccessXNotify(_)
            | Event::XkbActionMessage(_)
            | Event::XkbBellNotify(_)
            | Event::XkbCompatMapNotify(_)
            | Event::XkbControlsNotify(_)
            | Event::XkbExtensionDeviceNotify(_)
            | Event::XkbIndicatorMapNotify(_)
            | Event::XkbIndicatorStateNotify(_)
            | Event::XkbMapNotify(_)
            | Event::XkbNamesNotify(_)
            | Event::XkbNewKeyboardNotify(_)
            | Event::XkbStateNotify(_)
    )
}

/// Classify a key from its keysym, UTF-8 text and whether Control is held.
/// Pure function so it is unit-testable without an X server.
pub fn classify(sym: u32, utf8: &str, ctrl: bool) -> KeyInput {
    use xk::keysyms as k;
    match sym {
        k::KEY_Return | k::KEY_KP_Enter => return KeyInput::Enter,
        k::KEY_BackSpace if ctrl => return KeyInput::Kill,
        k::KEY_BackSpace => return KeyInput::Backspace,
        k::KEY_Escape => return KeyInput::Escape,
        k::KEY_u | k::KEY_U if ctrl => return KeyInput::Kill,
        _ => {}
    }
    if ctrl {
        // Ctrl+<letter> yields a control character; never insert it as text.
        return KeyInput::Other;
    }
    if !utf8.is_empty() && !utf8.chars().any(char::is_control) {
        KeyInput::Text(utf8.to_string())
    } else {
        KeyInput::Other
    }
}

pub struct Keyboard {
    context: xk::Context,
    device_id: i32,
    keymap: xk::Keymap,
    state: xk::State,
    stale: bool,
    /// Set after a failed reload: do not retry (or log) before this instant.
    retry_after: Option<std::time::Instant>,
}

impl std::fmt::Debug for Keyboard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Keyboard")
            .field("device_id", &self.device_id)
            .field("stale", &self.stale)
            .finish_non_exhaustive()
    }
}

fn load(context: &xk::Context, x: &X11, device_id: i32) -> Result<(xk::Keymap, xk::State), X11Error> {
    let keymap =
        xk::x11::keymap_new_from_device(context, &x.conn, device_id, xk::KEYMAP_COMPILE_NO_FLAGS);
    if keymap.get_raw_ptr().is_null() {
        return Err(X11Error::Protocol("xkb: cannot load keymap from X server".into()));
    }
    let state = xk::x11::state_new_from_device(&keymap, &x.conn, device_id);
    if state.get_raw_ptr().is_null() {
        return Err(X11Error::Protocol("xkb: cannot load keyboard state".into()));
    }
    Ok((keymap, state))
}

impl Keyboard {
    /// Set up XKB on the connection, load the core keyboard's keymap and state, and select
    /// XKB new-keyboard / map / state notifications for it.
    pub fn new(x: &X11) -> Result<Keyboard, X11Error> {
        let (mut maj, mut min, mut base_ev, mut base_err) = (0u16, 0u16, 0u8, 0u8);
        let ok = xk::x11::setup_xkb_extension(
            &x.conn,
            xk::x11::MIN_MAJOR_XKB_VERSION,
            xk::x11::MIN_MINOR_XKB_VERSION,
            xk::x11::SetupXkbExtensionFlags::NoFlags,
            &mut maj,
            &mut min,
            &mut base_ev,
            &mut base_err,
        );
        if !ok {
            return Err(X11Error::MissingExtension("XKEYBOARD"));
        }
        let device_id = xk::x11::get_core_keyboard_device_id(&x.conn);
        if device_id < 0 {
            return Err(X11Error::MissingExtension("XKEYBOARD"));
        }
        let context = xk::Context::new(xk::CONTEXT_NO_FLAGS);
        let (keymap, state) = load(&context, x, device_id)?;

        // Selecting through x11rb also fills x11rb's extension cache for XKEYBOARD, which it
        // needs to parse XKB events into `Event::Xkb*` (xkbcommon talked to raw xcb).
        let events = xkb::EventType::NEW_KEYBOARD_NOTIFY
            | xkb::EventType::MAP_NOTIFY
            | xkb::EventType::STATE_NOTIFY;
        let map_parts = xkb::MapPart::KEY_TYPES
            | xkb::MapPart::KEY_SYMS
            | xkb::MapPart::MODIFIER_MAP
            | xkb::MapPart::EXPLICIT_COMPONENTS
            | xkb::MapPart::KEY_ACTIONS
            | xkb::MapPart::VIRTUAL_MODS
            | xkb::MapPart::VIRTUAL_MOD_MAP;
        x.conn
            .xkb_select_events(
                device_id as xkb::DeviceSpec,
                xkb::EventType::from(0u16),
                events,
                map_parts,
                map_parts,
                &xkb::SelectEventsAux::new(),
            )?
            .check()?;

        Ok(Keyboard { context, device_id, keymap, state, stale: false, retry_after: None })
    }

    /// Feed an X event. Applies XKB state changes immediately and marks the keymap stale on
    /// keyboard/map changes. Returns true if the event was an XKB event for us (consumed).
    pub fn handle_event(&mut self, ev: &Event) -> bool {
        match ev {
            Event::XkbStateNotify(s) => {
                if i32::from(s.device_id) == self.device_id {
                    self.state.update_mask(
                        u32::from(s.base_mods),
                        u32::from(s.latched_mods),
                        u32::from(s.locked_mods),
                        s.base_group as u32,
                        s.latched_group as u32,
                        u32::from(s.locked_group),
                    );
                }
                true
            }
            Event::XkbNewKeyboardNotify(n) => {
                if i32::from(n.device_id) == self.device_id {
                    self.stale = true;
                }
                true
            }
            Event::XkbMapNotify(m) => {
                if i32::from(m.device_id) == self.device_id {
                    self.stale = true;
                }
                true
            }
            other => is_xkb_event(other),
        }
    }

    /// Alias of `handle_event` (name used in the original stub contract).
    pub fn handle_xkb_event(&mut self, ev: &Event) -> bool {
        self.handle_event(ev)
    }

    /// Same as the free function `is_xkb_event`.
    pub fn is_xkb_event(ev: &Event) -> bool {
        is_xkb_event(ev)
    }

    /// True after a keyboard/map change until `reload_if_stale` runs.
    pub fn is_stale(&self) -> bool {
        self.stale
    }

    /// Re-read keymap and state from the server if a change was signalled. On failure the old
    /// keymap stays in use and the error is returned.
    pub fn reload_if_stale(&mut self, x: &X11) -> Result<(), X11Error> {
        if !self.stale {
            return Ok(());
        }
        let now = std::time::Instant::now();
        if self.retry_after.is_some_and(|t| now < t) {
            return Ok(());
        }
        match self.reload(x) {
            Ok(()) => {
                self.retry_after = None;
                Ok(())
            }
            Err(e) => {
                // The old keymap keeps working; retry (and warn) at most every 5 s instead of
                // on every loop iteration.
                self.retry_after = now.checked_add(std::time::Duration::from_secs(5));
                Err(e)
            }
        }
    }

    /// Unconditionally re-read keymap and state from the server.
    pub fn reload(&mut self, x: &X11) -> Result<(), X11Error> {
        let (keymap, state) = load(&self.context, x, self.device_id)?;
        self.keymap = keymap;
        self.state = state;
        self.stale = false;
        Ok(())
    }

    /// Translate a KeyPress into lock-dialog input.
    pub fn translate(&self, ev: &KeyPressEvent) -> KeyInput {
        let code = xk::Keycode::from(ev.detail);
        let sym = self.state.key_get_one_sym(code).raw();
        let ctrl = self.state.mod_name_is_active(xk::MOD_NAME_CTRL, xk::STATE_MODS_EFFECTIVE)
            || u16::from(ev.state) & u16::from(ModMask::CONTROL) != 0;
        if ctrl {
            return classify(sym, "", true);
        }
        let mut text = self.state.key_get_utf8(code);
        let r = classify(sym, &text, false);
        zeroize::Zeroize::zeroize(&mut text);
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xk::keysyms as k;

    #[test]
    fn classify_keys() {
        assert_eq!(classify(k::KEY_Return, "\r", false), KeyInput::Enter);
        assert_eq!(classify(k::KEY_KP_Enter, "\r", false), KeyInput::Enter);
        assert_eq!(classify(k::KEY_BackSpace, "\x08", false), KeyInput::Backspace);
        assert_eq!(classify(k::KEY_BackSpace, "", true), KeyInput::Kill);
        assert_eq!(classify(k::KEY_Escape, "\x1b", false), KeyInput::Escape);
        assert_eq!(classify(k::KEY_u, "", true), KeyInput::Kill);
        assert_eq!(classify(k::KEY_U, "", true), KeyInput::Kill);
        assert_eq!(classify(k::KEY_a, "", true), KeyInput::Other);
        assert_eq!(classify(k::KEY_a, "a", false), KeyInput::Text("a".into()));
        assert_eq!(classify(k::KEY_A, "A", false), KeyInput::Text("A".into()));
        assert_eq!(classify(0x00e9, "é", false), KeyInput::Text("é".into()));
        assert_eq!(classify(0xffe1, "", false), KeyInput::Other); // Shift_L
        assert_eq!(classify(0xff09, "\t", false), KeyInput::Other); // Tab
    }

    /// Live check against an X server (e.g. `Xvfb :99`): keymap loads and a Shift press
    /// produces an XkbStateNotify. Runs only with NIGHTSAVER_XKB_TEST=1 and DISPLAY set.
    #[test]
    fn live_xkb_state_notify() {
        if std::env::var("NIGHTSAVER_XKB_TEST").as_deref() != Ok("1") {
            eprintln!("skipping live_xkb_state_notify: set NIGHTSAVER_XKB_TEST=1 (and DISPLAY)");
            return;
        }
        use std::time::{Duration, Instant};
        let x = X11::open(None).expect("open display");
        let mut kb = Keyboard::new(&x).expect("keyboard");
        x.flush();
        let st = std::process::Command::new("xdotool").args(["keydown", "shift"]).status();
        assert!(st.map(|s| s.success()).unwrap_or(false), "xdotool keydown failed");
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut seen = false;
        while Instant::now() < deadline {
            if let Ok(Some(ev)) = x.wait_event_timeout(Duration::from_millis(100))
                && matches!(ev, Event::XkbStateNotify(_))
            {
                assert!(kb.handle_event(&ev));
                seen = true;
                break;
            }
        }
        let shift_active = kb.state.mod_name_is_active(xk::MOD_NAME_SHIFT, xk::STATE_MODS_EFFECTIVE);
        // With Shift held, some keycode must translate to "A" and none to "a".
        let texts: Vec<KeyInput> = (8u8..=255)
            .map(|kc| kb.translate(&KeyPressEvent { detail: kc, ..Default::default() }))
            .collect();
        let _ = std::process::Command::new("xdotool").args(["keyup", "shift"]).status();
        assert!(texts.contains(&KeyInput::Text("A".into())), "no Shift+a -> A");
        assert!(!texts.contains(&KeyInput::Text("a".into())), "lowercase a while Shift held");
        assert!(texts.contains(&KeyInput::Enter));
        assert!(seen, "no XkbStateNotify received");
        assert!(shift_active, "Shift not active in xkb state after StateNotify");
    }
}
