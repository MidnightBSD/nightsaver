//! Display-independent keyboard input for the lock dialog: what a key press means to the
//! locker, whichever display backend delivered it. Backends translate their own key events
//! to a keysym and text with xkbcommon and then call [`classify`].
use xkbcommon::xkb as xk;

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
}
