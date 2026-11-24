//! egui physical keys -> USB HID usages (what the host injects).

use egui::Key;
use rconn_core::hid::usage as u;

pub fn to_hid(key: Key) -> Option<u16> {
    use Key::*;
    let letter = |i: u16| Some(u::A + i);
    Some(match key {
        Key::A => return letter(0),
        Key::B => return letter(1),
        C => return letter(2),
        D => return letter(3),
        E => return letter(4),
        F => return letter(5),
        G => return letter(6),
        H => return letter(7),
        I => return letter(8),
        J => return letter(9),
        K => return letter(10),
        L => return letter(11),
        M => return letter(12),
        N => return letter(13),
        O => return letter(14),
        P => return letter(15),
        Q => return letter(16),
        R => return letter(17),
        S => return letter(18),
        T => return letter(19),
        U => return letter(20),
        V => return letter(21),
        W => return letter(22),
        X => return letter(23),
        Y => return letter(24),
        Z => return letter(25),
        Num1 => u::DIGIT1,
        Num2 => u::DIGIT1 + 1,
        Num3 => u::DIGIT1 + 2,
        Num4 => u::DIGIT1 + 3,
        Num5 => u::DIGIT1 + 4,
        Num6 => u::DIGIT1 + 5,
        Num7 => u::DIGIT1 + 6,
        Num8 => u::DIGIT1 + 7,
        Num9 => u::DIGIT1 + 8,
        Num0 => u::DIGIT0,
        Enter => u::ENTER,
        Escape => u::ESCAPE,
        Backspace => u::BACKSPACE,
        Tab => u::TAB,
        Space => u::SPACE,
        Minus => u::MINUS,
        Equals | Plus => u::EQUAL,
        OpenBracket | OpenCurlyBracket => u::BRACKET_LEFT,
        CloseBracket | CloseCurlyBracket => u::BRACKET_RIGHT,
        Backslash | Pipe => u::BACKSLASH,
        Semicolon | Colon => u::SEMICOLON,
        Quote => u::QUOTE,
        Backtick => u::BACKQUOTE,
        Comma => u::COMMA,
        Period => u::PERIOD,
        Slash | Questionmark => u::SLASH,
        CapsLock => u::CAPS_LOCK,
        F1 => F1_,
        F2 => F1_ + 1,
        F3 => F1_ + 2,
        F4 => F1_ + 3,
        F5 => F1_ + 4,
        F6 => F1_ + 5,
        F7 => F1_ + 6,
        F8 => F1_ + 7,
        F9 => F1_ + 8,
        F10 => F1_ + 9,
        F11 => F1_ + 10,
        F12 => F1_ + 11,
        PrintScreen => u::PRINT_SCREEN,
        ScrollLock => u::SCROLL_LOCK,
        Pause => u::PAUSE,
        Insert => u::INSERT,
        Home => u::HOME,
        PageUp => u::PAGE_UP,
        Delete => u::DELETE,
        End => u::END,
        PageDown => u::PAGE_DOWN,
        ArrowRight => u::ARROW_RIGHT,
        ArrowLeft => u::ARROW_LEFT,
        ArrowDown => u::ARROW_DOWN,
        ArrowUp => u::ARROW_UP,
        NumLock => u::NUM_LOCK,
        IntlBackslash => u::INTL_BACKSLASH,
        Menu => u::CONTEXT_MENU,
        ShiftLeft => u::SHIFT_LEFT,
        ShiftRight => u::SHIFT_RIGHT,
        ControlLeft => u::CONTROL_LEFT,
        ControlRight => u::CONTROL_RIGHT,
        AltLeft => u::ALT_LEFT,
        AltRight => u::ALT_RIGHT,
        SuperLeft => u::META_LEFT,
        SuperRight => u::META_RIGHT,
        _ => return None,
    })
}

const F1_: u16 = u::F1;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_digits_and_modifiers_map() {
        assert_eq!(to_hid(Key::A), Some(u::A));
        assert_eq!(to_hid(Key::Z), Some(u::A + 25));
        assert_eq!(to_hid(Key::Num0), Some(u::DIGIT0));
        assert_eq!(to_hid(Key::F12), Some(F1_ + 11));
        assert_eq!(to_hid(Key::ControlLeft), Some(u::CONTROL_LEFT));
        // Every mapped key must be injectable on Windows.
        for k in Key::ALL {
            if let Some(h) = to_hid(*k) {
                assert!(
                    rconn_core::hid::to_windows_scancode(h).is_some(),
                    "{k:?} -> {h:#x} has no scancode"
                );
            }
        }
    }
}
