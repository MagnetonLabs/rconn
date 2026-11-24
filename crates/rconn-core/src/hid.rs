//! USB HID keyboard usages (page 0x07): the platform-neutral key identity on the wire.
//! Viewers translate their native key events to these; hosts translate them to native
//! codes. Physical keys, not characters: the host's keyboard layout decides what a key types.

/// HID usage IDs for the keys a remote-desktop viewer sends.
pub mod usage {
    pub const A: u16 = 0x04; // ..Z = 0x1D
    pub const DIGIT1: u16 = 0x1E; // ..9 = 0x26, 0 = 0x27
    pub const DIGIT0: u16 = 0x27;
    pub const ENTER: u16 = 0x28;
    pub const ESCAPE: u16 = 0x29;
    pub const BACKSPACE: u16 = 0x2A;
    pub const TAB: u16 = 0x2B;
    pub const SPACE: u16 = 0x2C;
    pub const MINUS: u16 = 0x2D;
    pub const EQUAL: u16 = 0x2E;
    pub const BRACKET_LEFT: u16 = 0x2F;
    pub const BRACKET_RIGHT: u16 = 0x30;
    pub const BACKSLASH: u16 = 0x31;
    pub const SEMICOLON: u16 = 0x33;
    pub const QUOTE: u16 = 0x34;
    pub const BACKQUOTE: u16 = 0x35;
    pub const COMMA: u16 = 0x36;
    pub const PERIOD: u16 = 0x37;
    pub const SLASH: u16 = 0x38;
    pub const CAPS_LOCK: u16 = 0x39;
    pub const F1: u16 = 0x3A; // ..F12 = 0x45
    pub const PRINT_SCREEN: u16 = 0x46;
    pub const SCROLL_LOCK: u16 = 0x47;
    pub const PAUSE: u16 = 0x48;
    pub const INSERT: u16 = 0x49;
    pub const HOME: u16 = 0x4A;
    pub const PAGE_UP: u16 = 0x4B;
    pub const DELETE: u16 = 0x4C;
    pub const END: u16 = 0x4D;
    pub const PAGE_DOWN: u16 = 0x4E;
    pub const ARROW_RIGHT: u16 = 0x4F;
    pub const ARROW_LEFT: u16 = 0x50;
    pub const ARROW_DOWN: u16 = 0x51;
    pub const ARROW_UP: u16 = 0x52;
    pub const NUM_LOCK: u16 = 0x53;
    pub const INTL_BACKSLASH: u16 = 0x64;
    pub const CONTEXT_MENU: u16 = 0x65;
    pub const F13: u16 = 0x68; // ..F24 = 0x73
    pub const CONTROL_LEFT: u16 = 0xE0;
    pub const SHIFT_LEFT: u16 = 0xE1;
    pub const ALT_LEFT: u16 = 0xE2;
    pub const META_LEFT: u16 = 0xE3;
    pub const CONTROL_RIGHT: u16 = 0xE4;
    pub const SHIFT_RIGHT: u16 = 0xE5;
    pub const ALT_RIGHT: u16 = 0xE6;
    pub const META_RIGHT: u16 = 0xE7;
}

/// A PC/AT (set 1) scan code, as Windows `SendInput` takes with `KEYEVENTF_SCANCODE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanCode {
    pub code: u16,
    /// Needs `KEYEVENTF_EXTENDEDKEY` (the 0xE0 prefix).
    pub extended: bool,
}

const fn sc(code: u16) -> Option<ScanCode> {
    Some(ScanCode {
        code,
        extended: false,
    })
}
const fn ext(code: u16) -> Option<ScanCode> {
    Some(ScanCode {
        code,
        extended: true,
    })
}

/// HID usage -> Windows scan code. `None` for keys we don't map.
pub fn to_windows_scancode(hid: u16) -> Option<ScanCode> {
    use usage::*;
    // Letters A..Z in HID order.
    const LETTERS: [u16; 26] = [
        0x1E, 0x30, 0x2E, 0x20, 0x12, 0x21, 0x22, 0x23, 0x17, 0x24, 0x25, 0x26, 0x32, 0x31, 0x18,
        0x19, 0x10, 0x13, 0x1F, 0x14, 0x16, 0x2F, 0x11, 0x2D, 0x15, 0x2C,
    ];
    // F13..F24 are not contiguous in set 1.
    const F13_24: [u16; 12] = [
        0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6A, 0x6B, 0x6C, 0x6D, 0x6E, 0x76,
    ];
    match hid {
        h if (A..A + 26).contains(&h) => sc(LETTERS[usize::from(h - A)]),
        h if (DIGIT1..=DIGIT0).contains(&h) => sc(h - DIGIT1 + 0x02), // 1..9 -> 0x02..0x0A, 0 -> 0x0B
        ENTER => sc(0x1C),
        ESCAPE => sc(0x01),
        BACKSPACE => sc(0x0E),
        TAB => sc(0x0F),
        SPACE => sc(0x39),
        MINUS => sc(0x0C),
        EQUAL => sc(0x0D),
        BRACKET_LEFT => sc(0x1A),
        BRACKET_RIGHT => sc(0x1B),
        BACKSLASH => sc(0x2B),
        SEMICOLON => sc(0x27),
        QUOTE => sc(0x28),
        BACKQUOTE => sc(0x29),
        COMMA => sc(0x33),
        PERIOD => sc(0x34),
        SLASH => sc(0x35),
        CAPS_LOCK => sc(0x3A),
        h if (F1..F1 + 10).contains(&h) => sc(h - F1 + 0x3B), // F1..F10 -> 0x3B..0x44
        0x44 => sc(0x57),                                     // F11
        0x45 => sc(0x58),                                     // F12
        PRINT_SCREEN => ext(0x37),
        SCROLL_LOCK => sc(0x46),
        PAUSE => sc(0x45), // approximated; the real Pause is an E1 sequence
        INSERT => ext(0x52),
        HOME => ext(0x47),
        PAGE_UP => ext(0x49),
        DELETE => ext(0x53),
        END => ext(0x4F),
        PAGE_DOWN => ext(0x51),
        ARROW_RIGHT => ext(0x4D),
        ARROW_LEFT => ext(0x4B),
        ARROW_DOWN => ext(0x50),
        ARROW_UP => ext(0x48),
        NUM_LOCK => ext(0x45),
        INTL_BACKSLASH => sc(0x56),
        CONTEXT_MENU => ext(0x5D),
        h if (F13..F13 + 12).contains(&h) => sc(F13_24[usize::from(h - F13)]),
        CONTROL_LEFT => sc(0x1D),
        SHIFT_LEFT => sc(0x2A),
        ALT_LEFT => sc(0x38),
        META_LEFT => ext(0x5B),
        CONTROL_RIGHT => ext(0x1D),
        SHIFT_RIGHT => sc(0x36),
        ALT_RIGHT => ext(0x38),
        META_RIGHT => ext(0x5C),
        _ => None,
    }
}

/// HID usage -> macOS virtual key code (`kVK_*` from Carbon's `Events.h`), positional on
/// an ANSI keyboard. `None` for keys macOS has no code for (F21..F24).
///
/// PC-only keys follow what macOS itself does with a PC keyboard: Print Screen, Scroll
/// Lock and Pause become F13..F15, Insert becomes Help, Num Lock becomes keypad Clear.
pub fn to_mac_keycode(hid: u16) -> Option<u16> {
    use usage::*;
    // Letters A..Z in HID order.
    const LETTERS: [u16; 26] = [
        0x00, 0x0B, 0x08, 0x02, 0x0E, 0x03, 0x05, 0x04, 0x22, 0x26, 0x28, 0x25, 0x2E, 0x2D, 0x1F,
        0x23, 0x0C, 0x0F, 0x01, 0x11, 0x20, 0x09, 0x0D, 0x07, 0x10, 0x06,
    ];
    // 1..9, 0 in HID order.
    const DIGITS: [u16; 10] = [0x12, 0x13, 0x14, 0x15, 0x17, 0x16, 0x1A, 0x1C, 0x19, 0x1D];
    const F1_12: [u16; 12] = [
        0x7A, 0x78, 0x63, 0x76, 0x60, 0x61, 0x62, 0x64, 0x65, 0x6D, 0x67, 0x6F,
    ];
    const F13_20: [u16; 8] = [0x69, 0x6B, 0x71, 0x6A, 0x40, 0x4F, 0x50, 0x5A];
    match hid {
        h if (A..A + 26).contains(&h) => Some(LETTERS[usize::from(h - A)]),
        h if (DIGIT1..=DIGIT0).contains(&h) => Some(DIGITS[usize::from(h - DIGIT1)]),
        ENTER => Some(0x24),
        ESCAPE => Some(0x35),
        BACKSPACE => Some(0x33), // kVK_Delete
        TAB => Some(0x30),
        SPACE => Some(0x31),
        MINUS => Some(0x1B),
        EQUAL => Some(0x18),
        BRACKET_LEFT => Some(0x21),
        BRACKET_RIGHT => Some(0x1E),
        BACKSLASH => Some(0x2A),
        SEMICOLON => Some(0x29),
        QUOTE => Some(0x27),
        BACKQUOTE => Some(0x32),
        COMMA => Some(0x2B),
        PERIOD => Some(0x2F),
        SLASH => Some(0x2C),
        CAPS_LOCK => Some(0x39),
        h if (F1..F1 + 12).contains(&h) => Some(F1_12[usize::from(h - F1)]),
        PRINT_SCREEN => Some(0x69), // F13
        SCROLL_LOCK => Some(0x6B),  // F14
        PAUSE => Some(0x71),        // F15
        INSERT => Some(0x72),       // kVK_Help
        HOME => Some(0x73),
        PAGE_UP => Some(0x74),
        DELETE => Some(0x75), // kVK_ForwardDelete
        END => Some(0x77),
        PAGE_DOWN => Some(0x79),
        ARROW_RIGHT => Some(0x7C),
        ARROW_LEFT => Some(0x7B),
        ARROW_DOWN => Some(0x7D),
        ARROW_UP => Some(0x7E),
        NUM_LOCK => Some(0x47),       // kVK_ANSI_KeypadClear
        INTL_BACKSLASH => Some(0x0A), // kVK_ISO_Section
        CONTEXT_MENU => Some(0x6E),   // kVK_ContextualMenu
        h if (F13..F13 + 8).contains(&h) => Some(F13_20[usize::from(h - F13)]),
        CONTROL_LEFT => Some(0x3B),
        SHIFT_LEFT => Some(0x38),
        ALT_LEFT => Some(0x3A),  // Option
        META_LEFT => Some(0x37), // Command
        CONTROL_RIGHT => Some(0x3E),
        SHIFT_RIGHT => Some(0x3C),
        ALT_RIGHT => Some(0x3D),
        META_RIGHT => Some(0x36),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::usage::*;
    use super::*;

    #[test]
    fn mac_spot_checks_against_kvk() {
        assert_eq!(to_mac_keycode(A), Some(0x00)); // kVK_ANSI_A
        assert_eq!(to_mac_keycode(A + 18), Some(0x01)); // S
        assert_eq!(to_mac_keycode(A + 25), Some(0x06)); // Z
        assert_eq!(to_mac_keycode(DIGIT1), Some(0x12));
        assert_eq!(to_mac_keycode(DIGIT1 + 4), Some(0x17)); // 5
        assert_eq!(to_mac_keycode(DIGIT0), Some(0x1D));
        assert_eq!(to_mac_keycode(ENTER), Some(0x24));
        assert_eq!(to_mac_keycode(F1), Some(0x7A));
        assert_eq!(to_mac_keycode(F1 + 11), Some(0x6F)); // F12
        assert_eq!(to_mac_keycode(F13 + 7), Some(0x5A)); // F20
        assert_eq!(to_mac_keycode(F13 + 8), None); // F21: no macOS code
        assert_eq!(to_mac_keycode(ARROW_LEFT), Some(0x7B));
        assert_eq!(to_mac_keycode(META_LEFT), Some(0x37)); // kVK_Command
        assert_eq!(to_mac_keycode(ALT_RIGHT), Some(0x3D)); // kVK_RightOption
        assert_eq!(to_mac_keycode(0x01), None);
    }

    #[test]
    fn every_mac_keycode_is_unique_except_pc_aliases() {
        // PC keys macOS folds onto F13..F15, as it does for a real PC keyboard.
        let aliases = [PRINT_SCREEN, SCROLL_LOCK, PAUSE];
        let mut seen = std::collections::HashSet::new();
        for hid in 0..=0xFF {
            if aliases.contains(&hid) {
                continue;
            }
            if let Some(code) = to_mac_keycode(hid) {
                assert!(code <= 0x7F, "HID {hid:#x} -> {code:#x} out of kVK range");
                assert!(seen.insert(code), "duplicate keycode for HID {hid:#x}");
            }
        }
        assert!(seen.len() > 90, "only {} keys mapped", seen.len());
        // Every key Windows maps, macOS maps too, except F21..F24.
        for hid in 0..=0xFF {
            if to_windows_scancode(hid).is_some() && !(F13 + 8..F13 + 12).contains(&hid) {
                assert!(to_mac_keycode(hid).is_some(), "HID {hid:#x} unmapped");
            }
        }
    }

    #[test]
    fn spot_checks_against_the_set1_table() {
        assert_eq!(to_windows_scancode(A), sc(0x1E));
        assert_eq!(to_windows_scancode(A + 25), sc(0x2C)); // Z
        assert_eq!(to_windows_scancode(DIGIT1), sc(0x02));
        assert_eq!(to_windows_scancode(DIGIT0), sc(0x0B));
        assert_eq!(to_windows_scancode(F1), sc(0x3B));
        assert_eq!(to_windows_scancode(F1 + 9), sc(0x44)); // F10
        assert_eq!(to_windows_scancode(F1 + 10), sc(0x57)); // F11
        assert_eq!(to_windows_scancode(F13 + 11), sc(0x76)); // F24
        assert_eq!(to_windows_scancode(ARROW_LEFT), ext(0x4B));
        assert_eq!(to_windows_scancode(CONTROL_RIGHT), ext(0x1D));
        assert_eq!(to_windows_scancode(0x01), None);
    }

    #[test]
    fn every_mapped_scancode_is_unique() {
        let mut seen = std::collections::HashSet::new();
        for hid in 0..=0xFF {
            if let Some(code) = to_windows_scancode(hid) {
                assert!(
                    seen.insert((code.code, code.extended)),
                    "duplicate scancode for HID {hid:#x}"
                );
            }
        }
        assert!(seen.len() > 100, "only {} keys mapped", seen.len());
    }
}
