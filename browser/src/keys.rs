//! What a key is called on the other side.
//!
//! A terminal names a key by the character it produced, or by a number the
//! Kitty protocol handed out. A web page names it by three things at once: a
//! `key` (what it means: `"a"`, `"Enter"`, `"ArrowLeft"`), a `code` (where it
//! is on the board: `"KeyA"`, `"Enter"`, `"ArrowLeft"`) and a
//! `windowsVirtualKeyCode`, which is a number from 1995 that pages still
//! branch on. Nothing in the terminal's report says which physical key was
//! pressed, so the third and second have to be *inferred* from the first, and
//! that inference is a layout assumption.
//!
//! The assumption is US QWERTY, and it is written here in one table so that it
//! is a decision rather than a scattering of special cases. It is right for
//! `code` on a US board and wrong for `code` on any other, which costs a page
//! that reads `event.code` to mean a position — a game's WASD — on a Dvorak
//! machine. It is right for `key` and for typed text everywhere, because those
//! come from the character the terminal reported and never from the table.
//! Trading `code` for `key` this way is the only trade available: the protocol
//! does not carry a scancode, and inventing one from the character is exactly
//! what this does.
//!
//! This is the module that is always wrong, so it is the module with a test
//! per row.

use crate::input::{Key, KeyAction, KeyInput};
use crate::json::Json;

/// The three names a browser wants for a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Named {
    /// DOM `event.key`.
    pub key: String,
    /// DOM `event.code`.
    pub code: String,
    /// `windowsVirtualKeyCode`, which is also `nativeVirtualKeyCode`.
    pub vk: u32,
}

/// Name a key, or say that it has no name in this layout.
///
/// `None` means the character is real but has no place on the assumed board —
/// a Japanese character from an input method, an emoji pasted in — and the
/// caller should use `Input.insertText` instead of pretending a key was
/// pressed. A page that listens for keystrokes will not see one, which is
/// correct: none happened.
pub fn name(input: &KeyInput) -> Option<Named> {
    let named = |key: &str, code: &str, vk: u32| Named {
        key: key.to_string(),
        code: code.to_string(),
        vk,
    };
    Some(match input.key {
        Key::Enter => named("Enter", "Enter", 13),
        Key::Tab => named("Tab", "Tab", 9),
        Key::Backspace => named("Backspace", "Backspace", 8),
        Key::Escape => named("Escape", "Escape", 27),
        Key::Insert => named("Insert", "Insert", 45),
        Key::Delete => named("Delete", "Delete", 46),
        Key::Up => named("ArrowUp", "ArrowUp", 38),
        Key::Down => named("ArrowDown", "ArrowDown", 40),
        Key::Left => named("ArrowLeft", "ArrowLeft", 37),
        Key::Right => named("ArrowRight", "ArrowRight", 39),
        Key::Home => named("Home", "Home", 36),
        Key::End => named("End", "End", 35),
        Key::PageUp => named("PageUp", "PageUp", 33),
        Key::PageDown => named("PageDown", "PageDown", 34),
        Key::Function(n) if (1..=24).contains(&n) => {
            named(&format!("F{n}"), &format!("F{n}"), 111 + n as u32)
        }
        Key::Char(c) => return character(c, input),
        _ => return None,
    })
}

/// The `key`, `code` and virtual key code of a character key.
fn character(c: char, input: &KeyInput) -> Option<Named> {
    // What the page sees as `key` is what was typed, not what the table says:
    // the terminal already applied shift, the keyboard layout and any dead
    // key, and the result is more authoritative than any inference here.
    let key = match input.text {
        Some(text) => text.to_string(),
        None => c.to_string(),
    };

    let lower = c.to_ascii_lowercase();
    if lower.is_ascii_alphabetic() {
        return Some(Named {
            key,
            code: format!("Key{}", lower.to_ascii_uppercase()),
            vk: lower.to_ascii_uppercase() as u32,
        });
    }
    if c.is_ascii_digit() {
        return Some(Named {
            key,
            code: format!("Digit{c}"),
            vk: c as u32,
        });
    }
    if c == ' ' {
        return Some(Named {
            key: " ".to_string(),
            code: "Space".to_string(),
            vk: 32,
        });
    }

    // Punctuation, by the character it is or the character shift makes of it.
    let (code, vk) = PUNCTUATION
        .iter()
        .find(|(plain, shifted, _, _)| *plain == c || *shifted == c)
        .map(|(_, _, code, vk)| (*code, *vk))
        .or_else(|| {
            SHIFTED_DIGITS
                .iter()
                .position(|&shifted| shifted == c)
                .map(|index| {
                    let digit = if index == 9 { 0 } else { index + 1 };
                    (DIGIT_CODES[digit], b'0' as u32 + digit as u32)
                })
        })?;
    Some(Named {
        key,
        code: code.to_string(),
        vk,
    })
}

/// Plain character, shifted character, DOM code, virtual key code.
///
/// The virtual key codes are the `VK_OEM_*` numbers, which are the ones a page
/// that still reads `keyCode` expects.
const PUNCTUATION: [(char, char, &str, u32); 11] = [
    ('`', '~', "Backquote", 192),
    ('-', '_', "Minus", 189),
    ('=', '+', "Equal", 187),
    ('[', '{', "BracketLeft", 219),
    (']', '}', "BracketRight", 221),
    ('\\', '|', "Backslash", 220),
    (';', ':', "Semicolon", 186),
    ('\'', '"', "Quote", 222),
    (',', '<', "Comma", 188),
    ('.', '>', "Period", 190),
    ('/', '?', "Slash", 191),
];

/// What shift makes of the digit row, in order 1 to 9 then 0.
const SHIFTED_DIGITS: [char; 10] = ['!', '@', '#', '$', '%', '^', '&', '*', '(', ')'];

const DIGIT_CODES: [&str; 10] = [
    "Digit0", "Digit1", "Digit2", "Digit3", "Digit4", "Digit5", "Digit6", "Digit7", "Digit8",
    "Digit9",
];

/// The parameters of an `Input.dispatchKeyEvent` for this event.
///
/// `None` means the key has no place on the board and the text, if any, should
/// go through `Input.insertText`.
pub fn dispatch(input: &KeyInput) -> Option<Json> {
    let named = name(input)?;
    let kind = match input.action {
        KeyAction::Release => "keyUp",
        _ => "keyDown",
    };

    let mut fields = vec![
        ("type", Json::string(kind)),
        ("modifiers", Json::number(input.mods.cdp())),
        ("key", Json::string(named.key)),
        ("code", Json::string(named.code)),
        ("windowsVirtualKeyCode", Json::number(named.vk)),
        ("nativeVirtualKeyCode", Json::number(named.vk)),
    ];
    if input.action == KeyAction::Repeat {
        fields.push(("autoRepeat", Json::Bool(true)));
    }
    // Text on a key *up* would make Chromium insert the character twice, and
    // text with ctrl or meta held would turn a shortcut into typing. The
    // terminal has already decided the second question — it reports no
    // associated text for `ctrl+l` — so this only has to hold the first.
    if input.action != KeyAction::Release {
        if let Some(text) = input.text {
            fields.push(("text", Json::string(text.to_string())));
            fields.push(("unmodifiedText", Json::string(text.to_string())));
        }
    }
    Some(Json::object(fields))
}

/// The parameters of an `Input.insertText`.
pub fn insert_text(text: &str) -> Json {
    Json::object(vec![("text", Json::string(text))])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{KeyAction, Mods};

    fn key(key: Key, text: Option<char>, mods: u32) -> KeyInput {
        KeyInput {
            key,
            mods: Mods(mods),
            action: KeyAction::Press,
            text,
        }
    }

    fn named_for(input: &KeyInput) -> Named {
        name(input).expect("a name")
    }

    #[test]
    fn letters_carry_the_character_that_was_typed() {
        let lower = named_for(&key(Key::Char('a'), Some('a'), 0));
        assert_eq!(
            lower,
            Named {
                key: "a".to_string(),
                code: "KeyA".to_string(),
                vk: 65,
            }
        );
        // Shift is the terminal's business; what arrives is already the
        // capital, and the code and the number stay the key's own.
        let upper = named_for(&key(Key::Char('a'), Some('A'), Mods::SHIFT));
        assert_eq!(upper.key, "A");
        assert_eq!(upper.code, "KeyA");
        assert_eq!(upper.vk, 65);
    }

    #[test]
    fn digits_and_what_shift_makes_of_them() {
        assert_eq!(named_for(&key(Key::Char('7'), Some('7'), 0)).code, "Digit7");
        assert_eq!(named_for(&key(Key::Char('7'), Some('7'), 0)).vk, 55);
        let shifted = named_for(&key(Key::Char('&'), Some('&'), Mods::SHIFT));
        assert_eq!(shifted.code, "Digit7", "& is shift and the 7 key");
        assert_eq!(shifted.vk, 55);
        assert_eq!(shifted.key, "&");
        assert_eq!(
            named_for(&key(Key::Char(')'), Some(')'), Mods::SHIFT)).code,
            "Digit0"
        );
        assert_eq!(
            named_for(&key(Key::Char('!'), Some('!'), Mods::SHIFT)).code,
            "Digit1"
        );
    }

    #[test]
    fn every_punctuation_key_is_found_by_both_its_faces() {
        for (plain, shifted, code, vk) in PUNCTUATION {
            let unshifted = named_for(&key(Key::Char(plain), Some(plain), 0));
            assert_eq!((unshifted.code.as_str(), unshifted.vk), (code, vk));
            let upper = named_for(&key(Key::Char(shifted), Some(shifted), Mods::SHIFT));
            assert_eq!((upper.code.as_str(), upper.vk), (code, vk));
            assert_eq!(upper.key, shifted.to_string());
        }
    }

    #[test]
    fn the_named_keys_a_page_branches_on() {
        let cases: &[(Key, &str, u32)] = &[
            (Key::Enter, "Enter", 13),
            (Key::Tab, "Tab", 9),
            (Key::Backspace, "Backspace", 8),
            (Key::Escape, "Escape", 27),
            (Key::Delete, "Delete", 46),
            (Key::Insert, "Insert", 45),
            (Key::Up, "ArrowUp", 38),
            (Key::Down, "ArrowDown", 40),
            (Key::Left, "ArrowLeft", 37),
            (Key::Right, "ArrowRight", 39),
            (Key::Home, "Home", 36),
            (Key::End, "End", 35),
            (Key::PageUp, "PageUp", 33),
            (Key::PageDown, "PageDown", 34),
            (Key::Function(1), "F1", 112),
            (Key::Function(12), "F12", 123),
            (Key::Char(' '), " ", 32),
        ];
        for (what, expected, vk) in cases {
            let named = named_for(&key(*what, None, 0));
            assert_eq!(named.key, *expected, "{what:?}");
            assert_eq!(named.vk, *vk, "{what:?}");
            // Except for space, a named key's code is its name.
            if *what != Key::Char(' ') {
                assert_eq!(named.code, *expected);
            } else {
                assert_eq!(named.code, "Space");
            }
        }
    }

    #[test]
    fn a_character_with_no_place_on_the_board_has_no_key_event() {
        assert_eq!(name(&key(Key::Char('\u{65e5}'), Some('\u{65e5}'), 0)), None);
        assert_eq!(name(&key(Key::Char('\u{1f600}'), None, 0)), None);
        assert_eq!(name(&key(Key::Other(57363), None, 0)), None);
        // And that is what `Input.insertText` is for.
        assert_eq!(
            insert_text("\u{65e5}\u{672c}").to_string(),
            "{\"text\":\"\u{65e5}\u{672c}\"}"
        );
    }

    #[test]
    fn a_press_carries_its_text_and_a_release_does_not() {
        let press = dispatch(&key(Key::Char('a'), Some('a'), 0)).expect("params");
        let text = press.to_string();
        assert!(text.contains("\"type\":\"keyDown\""), "{text}");
        assert!(text.contains("\"text\":\"a\""), "{text}");
        assert!(text.contains("\"unmodifiedText\":\"a\""), "{text}");
        assert!(text.contains("\"windowsVirtualKeyCode\":65"), "{text}");

        let mut up = key(Key::Char('a'), Some('a'), 0);
        up.action = KeyAction::Release;
        let text = dispatch(&up).expect("params").to_string();
        assert!(text.contains("\"type\":\"keyUp\""), "{text}");
        assert!(!text.contains("\"text\""), "{text}");
    }

    #[test]
    fn a_repeat_is_a_keydown_that_says_it_is_one() {
        let mut repeat = key(Key::Char('a'), Some('a'), 0);
        repeat.action = KeyAction::Repeat;
        let text = dispatch(&repeat).expect("params").to_string();
        assert!(text.contains("\"type\":\"keyDown\""), "{text}");
        assert!(text.contains("\"autoRepeat\":true"), "{text}");
    }

    #[test]
    fn a_shortcut_dispatches_with_its_modifier_and_no_text() {
        // ctrl+a, which a page may take as select-all.
        let input = key(Key::Char('a'), None, Mods::CTRL);
        let text = dispatch(&input).expect("params").to_string();
        assert!(text.contains("\"modifiers\":2"), "{text}");
        assert!(!text.contains("\"text\""), "{text}");

        // The whole mask, in CDP's order: Alt 1, Ctrl 2, Meta 4, Shift 8.
        let all = key(
            Key::Char('a'),
            None,
            Mods::ALT | Mods::CTRL | Mods::SUPER | Mods::SHIFT,
        );
        assert!(dispatch(&all)
            .expect("params")
            .to_string()
            .contains("\"modifiers\":15"));
    }
}
