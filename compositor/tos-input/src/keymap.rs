//! Keyboard layout: evdev scancodes to logical keys.
//!
//! Only the US layout is built in. Layout files belong to a later milestone;
//! the shape of this module is what matters, because everything above it works
//! in [`KeyCode`] rather than in scancodes.

use crate::event::{ImeKey, KeyCode, Keypad, ModifierKey};

/// What one physical key produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyMapping {
    pub code: KeyCode,
    /// Character produced with no modifiers.
    pub plain: Option<char>,
    /// Character produced with shift.
    pub shifted: Option<char>,
}

impl KeyMapping {
    const fn key(code: KeyCode) -> Self {
        KeyMapping {
            code,
            plain: None,
            shifted: None,
        }
    }

    const fn text(plain: char, shifted: char) -> Self {
        KeyMapping {
            code: KeyCode::Char(plain),
            plain: Some(plain),
            shifted: Some(shifted),
        }
    }

    /// The character this key produces given the shift and caps lock state.
    pub fn character(&self, shift: bool, caps_lock: bool) -> Option<char> {
        let base = if shift { self.shifted } else { self.plain }?;
        if caps_lock {
            // Caps lock only affects letters, and inverts whatever shift did.
            let letter = if shift { self.plain } else { self.shifted };
            if base.is_alphabetic() {
                return letter;
            }
        }
        Some(base)
    }
}

/// Map an evdev key code to a logical key on the US layout, together with the
/// keys a JIS keyboard adds, which the US layout has no scancodes for.
pub fn lookup(code: u16) -> Option<KeyMapping> {
    use KeyCode as K;
    Some(match code {
        1 => KeyMapping::key(K::Escape),
        2 => KeyMapping::text('1', '!'),
        3 => KeyMapping::text('2', '@'),
        4 => KeyMapping::text('3', '#'),
        5 => KeyMapping::text('4', '$'),
        6 => KeyMapping::text('5', '%'),
        7 => KeyMapping::text('6', '^'),
        8 => KeyMapping::text('7', '&'),
        9 => KeyMapping::text('8', '*'),
        10 => KeyMapping::text('9', '('),
        11 => KeyMapping::text('0', ')'),
        12 => KeyMapping::text('-', '_'),
        13 => KeyMapping::text('=', '+'),
        14 => KeyMapping::key(K::Backspace),
        15 => KeyMapping::key(K::Tab),
        16 => KeyMapping::text('q', 'Q'),
        17 => KeyMapping::text('w', 'W'),
        18 => KeyMapping::text('e', 'E'),
        19 => KeyMapping::text('r', 'R'),
        20 => KeyMapping::text('t', 'T'),
        21 => KeyMapping::text('y', 'Y'),
        22 => KeyMapping::text('u', 'U'),
        23 => KeyMapping::text('i', 'I'),
        24 => KeyMapping::text('o', 'O'),
        25 => KeyMapping::text('p', 'P'),
        26 => KeyMapping::text('[', '{'),
        27 => KeyMapping::text(']', '}'),
        28 => KeyMapping::key(K::Enter),
        29 => KeyMapping::key(K::ModifierKey(ModifierKey::LeftCtrl)),
        30 => KeyMapping::text('a', 'A'),
        31 => KeyMapping::text('s', 'S'),
        32 => KeyMapping::text('d', 'D'),
        33 => KeyMapping::text('f', 'F'),
        34 => KeyMapping::text('g', 'G'),
        35 => KeyMapping::text('h', 'H'),
        36 => KeyMapping::text('j', 'J'),
        37 => KeyMapping::text('k', 'K'),
        38 => KeyMapping::text('l', 'L'),
        39 => KeyMapping::text(';', ':'),
        40 => KeyMapping::text('\'', '"'),
        41 => KeyMapping::text('`', '~'),
        42 => KeyMapping::key(K::ModifierKey(ModifierKey::LeftShift)),
        43 => KeyMapping::text('\\', '|'),
        44 => KeyMapping::text('z', 'Z'),
        45 => KeyMapping::text('x', 'X'),
        46 => KeyMapping::text('c', 'C'),
        47 => KeyMapping::text('v', 'V'),
        48 => KeyMapping::text('b', 'B'),
        49 => KeyMapping::text('n', 'N'),
        50 => KeyMapping::text('m', 'M'),
        51 => KeyMapping::text(',', '<'),
        52 => KeyMapping::text('.', '>'),
        53 => KeyMapping::text('/', '?'),
        54 => KeyMapping::key(K::ModifierKey(ModifierKey::RightShift)),
        55 => KeyMapping::key(K::Keypad(Keypad::Multiply)),
        56 => KeyMapping::key(K::ModifierKey(ModifierKey::LeftAlt)),
        57 => KeyMapping::text(' ', ' '),
        58 => KeyMapping::key(K::CapsLock),
        59..=68 => KeyMapping::key(K::Function((code - 58) as u8)),
        69 => KeyMapping::key(K::NumLock),
        70 => KeyMapping::key(K::ScrollLock),
        71 => KeyMapping::key(K::Keypad(Keypad::Digit(7))),
        72 => KeyMapping::key(K::Keypad(Keypad::Digit(8))),
        73 => KeyMapping::key(K::Keypad(Keypad::Digit(9))),
        74 => KeyMapping::key(K::Keypad(Keypad::Subtract)),
        75 => KeyMapping::key(K::Keypad(Keypad::Digit(4))),
        76 => KeyMapping::key(K::Keypad(Keypad::Digit(5))),
        77 => KeyMapping::key(K::Keypad(Keypad::Digit(6))),
        78 => KeyMapping::key(K::Keypad(Keypad::Add)),
        79 => KeyMapping::key(K::Keypad(Keypad::Digit(1))),
        80 => KeyMapping::key(K::Keypad(Keypad::Digit(2))),
        81 => KeyMapping::key(K::Keypad(Keypad::Digit(3))),
        82 => KeyMapping::key(K::Keypad(Keypad::Digit(0))),
        83 => KeyMapping::key(K::Keypad(Keypad::Decimal)),
        87 => KeyMapping::key(K::Function(11)),
        88 => KeyMapping::key(K::Function(12)),
        // The keys only a JIS keyboard has. A US keyboard never sends these
        // scancodes, so giving them their Japanese meaning here takes nothing
        // away from the built in layout, and it beats dropping the events
        // until layout files exist. The characters are the ones the kernel's
        // own jp106 layout produces.
        89 => KeyMapping::text('\\', '_'),
        92 => KeyMapping::key(K::Ime(ImeKey::Convert)),
        93 => KeyMapping::key(K::Ime(ImeKey::KanaMode)),
        94 => KeyMapping::key(K::Ime(ImeKey::NonConvert)),
        96 => KeyMapping::key(K::Keypad(Keypad::Enter)),
        97 => KeyMapping::key(K::ModifierKey(ModifierKey::RightCtrl)),
        98 => KeyMapping::key(K::Keypad(Keypad::Divide)),
        99 => KeyMapping::key(K::PrintScreen),
        100 => KeyMapping::key(K::ModifierKey(ModifierKey::RightAlt)),
        102 => KeyMapping::key(K::Home),
        103 => KeyMapping::key(K::Up),
        104 => KeyMapping::key(K::PageUp),
        105 => KeyMapping::key(K::Left),
        106 => KeyMapping::key(K::Right),
        107 => KeyMapping::key(K::End),
        108 => KeyMapping::key(K::Down),
        109 => KeyMapping::key(K::PageDown),
        110 => KeyMapping::key(K::Insert),
        111 => KeyMapping::key(K::Delete),
        117 => KeyMapping::key(K::Keypad(Keypad::Equal)),
        119 => KeyMapping::key(K::Pause),
        // The yen key, which sits where a US keyboard has nothing at all.
        124 => KeyMapping::text('¥', '|'),
        125 => KeyMapping::key(K::ModifierKey(ModifierKey::LeftSuper)),
        126 => KeyMapping::key(K::ModifierKey(ModifierKey::RightSuper)),
        127 => KeyMapping::key(K::Menu),
        183..=194 => KeyMapping::key(K::Function((code - 183 + 13) as u8)),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_map_to_characters() {
        let a = lookup(30).unwrap();
        assert_eq!(a.code, KeyCode::Char('a'));
        assert_eq!(a.character(false, false), Some('a'));
        assert_eq!(a.character(true, false), Some('A'));
    }

    #[test]
    fn caps_lock_only_affects_letters() {
        let a = lookup(30).unwrap();
        assert_eq!(a.character(false, true), Some('A'));
        assert_eq!(a.character(true, true), Some('a'));

        let one = lookup(2).unwrap();
        assert_eq!(one.character(false, true), Some('1'));
        assert_eq!(one.character(true, true), Some('!'));
    }

    #[test]
    fn function_keys_are_contiguous() {
        assert_eq!(lookup(59).unwrap().code, KeyCode::Function(1));
        assert_eq!(lookup(68).unwrap().code, KeyCode::Function(10));
        assert_eq!(lookup(87).unwrap().code, KeyCode::Function(11));
        assert_eq!(lookup(88).unwrap().code, KeyCode::Function(12));
        assert_eq!(lookup(183).unwrap().code, KeyCode::Function(13));
    }

    #[test]
    fn navigation_keys() {
        assert_eq!(lookup(103).unwrap().code, KeyCode::Up);
        assert_eq!(lookup(111).unwrap().code, KeyCode::Delete);
    }

    #[test]
    fn modifiers_are_recognised() {
        assert_eq!(
            lookup(29).unwrap().code,
            KeyCode::ModifierKey(ModifierKey::LeftCtrl)
        );
        assert_eq!(
            lookup(125).unwrap().code,
            KeyCode::ModifierKey(ModifierKey::LeftSuper)
        );
    }

    #[test]
    fn jis_typing_keys_produce_their_characters() {
        let ro = lookup(89).unwrap();
        assert_eq!(ro.code, KeyCode::Char('\\'));
        assert_eq!(ro.character(false, false), Some('\\'));
        assert_eq!(ro.character(true, false), Some('_'));

        let yen = lookup(124).unwrap();
        assert_eq!(yen.code, KeyCode::Char('¥'));
        assert_eq!(yen.character(false, false), Some('¥'));
        assert_eq!(yen.character(true, false), Some('|'));

        // Neither is a letter, so caps lock leaves both alone.
        assert_eq!(ro.character(false, true), Some('\\'));
        assert_eq!(yen.character(false, true), Some('¥'));
    }

    #[test]
    fn conversion_keys_are_named_rather_than_typed() {
        assert_eq!(lookup(92).unwrap().code, KeyCode::Ime(ImeKey::Convert));
        assert_eq!(lookup(93).unwrap().code, KeyCode::Ime(ImeKey::KanaMode));
        assert_eq!(lookup(94).unwrap().code, KeyCode::Ime(ImeKey::NonConvert));
        for code in [92, 93, 94] {
            let key = lookup(code).unwrap();
            assert_eq!(key.character(false, false), None);
            assert_eq!(key.character(true, false), None);
        }
    }

    #[test]
    fn unmapped_codes_return_nothing() {
        assert!(lookup(0).is_none());
        assert!(lookup(1000).is_none());
    }
}
