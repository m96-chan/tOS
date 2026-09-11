//! Turning input events into the bytes applications expect.
//!
//! Two schemes are supported: the legacy xterm encoding that every terminal
//! application understands, and the Kitty keyboard protocol, which an
//! application opts into and which can express things the legacy encoding
//! cannot, such as key releases and ctrl+shift combinations.

use tos_term::modes::{KeyboardFlags, MouseEncoding, MouseState, MouseTracking};
use tos_term::Terminal;

use crate::event::{
    KeyCode, KeyEvent, KeyState, Keypad, ModifierKey, Modifiers, MouseAction, MouseButton,
    MouseEvent,
};

/// The terminal state that input encoding depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncodeContext {
    /// DECCKM: cursor keys send SS3 rather than CSI.
    pub cursor_keys_application: bool,
    /// DECNKM: the keypad sends application sequences.
    pub keypad_application: bool,
    /// Kitty keyboard protocol flags currently in force.
    pub kitty: KeyboardFlags,
    /// LNM: Enter sends carriage return and line feed.
    pub newline_mode: bool,
    /// Alt prefixes an escape rather than setting the high bit.
    pub alt_sends_escape: bool,
}

impl Default for EncodeContext {
    fn default() -> Self {
        EncodeContext {
            cursor_keys_application: false,
            keypad_application: false,
            kitty: KeyboardFlags::default(),
            newline_mode: false,
            alt_sends_escape: true,
        }
    }
}

impl EncodeContext {
    /// Read the current state out of a terminal.
    pub fn from_terminal(term: &Terminal) -> Self {
        EncodeContext {
            cursor_keys_application: term.modes.application_cursor_keys,
            keypad_application: term.modes.application_keypad,
            kitty: term.keyboard_flags(),
            newline_mode: term.modes.linefeed_newline,
            alt_sends_escape: true,
        }
    }
}

/// Encode a key event. An empty result means the key sends nothing.
pub fn encode_key(event: &KeyEvent, ctx: &EncodeContext) -> Vec<u8> {
    if !ctx.kitty.is_empty() {
        return encode_kitty(event, ctx);
    }
    if event.state == KeyState::Release {
        return Vec::new();
    }
    encode_legacy(event, ctx)
}

// ---------------------------------------------------------------------------
// Legacy xterm encoding
// ---------------------------------------------------------------------------

fn encode_legacy(event: &KeyEvent, ctx: &EncodeContext) -> Vec<u8> {
    let mods = event.modifiers.effective();
    let mut out = Vec::new();

    match event.code {
        KeyCode::Char(_) => {
            let Some(text) = event.text else {
                return out;
            };
            if mods.ctrl() {
                if let Some(byte) = control_byte(text) {
                    if mods.alt() && ctx.alt_sends_escape {
                        out.push(0x1b);
                    }
                    out.push(byte);
                    return out;
                }
            }
            if mods.alt() && ctx.alt_sends_escape {
                out.push(0x1b);
            }
            let mut buf = [0u8; 4];
            out.extend_from_slice(text.encode_utf8(&mut buf).as_bytes());
        }
        KeyCode::Enter => {
            if mods.alt() && ctx.alt_sends_escape {
                out.push(0x1b);
            }
            out.push(b'\r');
            if ctx.newline_mode {
                out.push(b'\n');
            }
        }
        KeyCode::Tab => {
            if mods.shift() {
                out.extend_from_slice(b"\x1b[Z");
            } else {
                if mods.alt() && ctx.alt_sends_escape {
                    out.push(0x1b);
                }
                out.push(b'\t');
            }
        }
        KeyCode::Backspace => {
            if mods.alt() && ctx.alt_sends_escape {
                out.push(0x1b);
            }
            // The terminal convention: backspace sends DEL, ctrl makes it BS.
            out.push(if mods.ctrl() { 0x08 } else { 0x7f });
        }
        KeyCode::Escape => {
            if mods.alt() && ctx.alt_sends_escape {
                out.push(0x1b);
            }
            out.push(0x1b);
        }
        KeyCode::Up
        | KeyCode::Down
        | KeyCode::Right
        | KeyCode::Left
        | KeyCode::Home
        | KeyCode::End => {
            let final_byte = cursor_final(event.code);
            if mods.is_empty() {
                if ctx.cursor_keys_application {
                    out.extend_from_slice(b"\x1bO");
                } else {
                    out.extend_from_slice(b"\x1b[");
                }
                out.push(final_byte);
            } else {
                out.extend_from_slice(format!("\x1b[1;{}", mods.xterm_param()).as_bytes());
                out.push(final_byte);
            }
        }
        KeyCode::Insert | KeyCode::Delete | KeyCode::PageUp | KeyCode::PageDown => {
            let number = tilde_number(event.code);
            out.extend_from_slice(&tilde_sequence(number, mods));
        }
        KeyCode::Function(n) => match n {
            1..=4 => {
                let final_byte = b'P' + (n - 1);
                if mods.is_empty() {
                    out.extend_from_slice(b"\x1bO");
                    out.push(final_byte);
                } else {
                    out.extend_from_slice(format!("\x1b[1;{}", mods.xterm_param()).as_bytes());
                    out.push(final_byte);
                }
            }
            _ => {
                if let Some(number) = function_tilde_number(n) {
                    out.extend_from_slice(&tilde_sequence(number, mods));
                }
            }
        },
        KeyCode::Keypad(key) => out.extend_from_slice(&encode_keypad(key, ctx, mods)),
        // Keys with no legacy representation send nothing at all. The
        // Japanese conversion keys are in that company on purpose: no terminal
        // convention assigns them bytes, so anything invented here would land
        // in a program that never agreed to read it. They travel as events for
        // the compositor to route and stop at the encoder.
        KeyCode::ModifierKey(_)
        | KeyCode::Ime(_)
        | KeyCode::CapsLock
        | KeyCode::NumLock
        | KeyCode::ScrollLock
        | KeyCode::PrintScreen
        | KeyCode::Pause
        | KeyCode::Menu
        | KeyCode::Unknown(_) => {}
    }
    out
}

fn cursor_final(code: KeyCode) -> u8 {
    match code {
        KeyCode::Up => b'A',
        KeyCode::Down => b'B',
        KeyCode::Right => b'C',
        KeyCode::Left => b'D',
        KeyCode::Home => b'H',
        KeyCode::End => b'F',
        _ => b'A',
    }
}

fn tilde_number(code: KeyCode) -> u32 {
    match code {
        KeyCode::Insert => 2,
        KeyCode::Delete => 3,
        KeyCode::PageUp => 5,
        KeyCode::PageDown => 6,
        _ => 1,
    }
}

fn function_tilde_number(n: u8) -> Option<u32> {
    Some(match n {
        5 => 15,
        6 => 17,
        7 => 18,
        8 => 19,
        9 => 20,
        10 => 21,
        11 => 23,
        12 => 24,
        // F13 and beyond are shifted function keys in the xterm scheme.
        13 => 25,
        14 => 26,
        15 => 28,
        16 => 29,
        17 => 31,
        18 => 32,
        19 => 33,
        20 => 34,
        _ => return None,
    })
}

fn tilde_sequence(number: u32, mods: Modifiers) -> Vec<u8> {
    if mods.is_empty() {
        format!("\x1b[{number}~").into_bytes()
    } else {
        format!("\x1b[{number};{}~", mods.xterm_param()).into_bytes()
    }
}

fn encode_keypad(key: Keypad, ctx: &EncodeContext, mods: Modifiers) -> Vec<u8> {
    if !ctx.keypad_application {
        // In numeric mode the keypad is just the characters it is labelled with.
        let text = match key {
            Keypad::Digit(d) => Some((b'0' + d) as char),
            Keypad::Decimal => Some('.'),
            Keypad::Divide => Some('/'),
            Keypad::Multiply => Some('*'),
            Keypad::Subtract => Some('-'),
            Keypad::Add => Some('+'),
            Keypad::Equal => Some('='),
            Keypad::Separator => Some(','),
            Keypad::Enter => return vec![b'\r'],
            Keypad::Begin => return b"\x1b[E".to_vec(),
        };
        let mut out = Vec::new();
        if mods.alt() && ctx.alt_sends_escape {
            out.push(0x1b);
        }
        if let Some(c) = text {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
        return out;
    }

    // Application keypad mode: SS3 plus a letter.
    let final_byte = match key {
        Keypad::Digit(d) => b'p' + d,
        Keypad::Decimal => b'n',
        Keypad::Divide => b'o',
        Keypad::Multiply => b'j',
        Keypad::Subtract => b'm',
        Keypad::Add => b'k',
        Keypad::Enter => b'M',
        Keypad::Equal => b'X',
        Keypad::Separator => b'l',
        Keypad::Begin => b'E',
    };
    let mut out = b"\x1bO".to_vec();
    out.push(final_byte);
    out
}

/// The control character a key produces when ctrl is held.
fn control_byte(c: char) -> Option<u8> {
    let byte = match c {
        ' ' | '@' | '2' => 0x00,
        'a'..='z' => c as u8 - b'a' + 1,
        'A'..='Z' => c as u8 - b'A' + 1,
        '[' | '3' => 0x1b,
        '\\' | '4' => 0x1c,
        ']' | '5' => 0x1d,
        '^' | '6' => 0x1e,
        '_' | '7' | '/' => 0x1f,
        '?' | '8' => 0x7f,
        _ => return None,
    };
    Some(byte)
}

// ---------------------------------------------------------------------------
// Kitty keyboard protocol
// ---------------------------------------------------------------------------

/// The key number and final byte a key uses in the Kitty protocol.
fn kitty_key(code: KeyCode) -> Option<(u32, u8)> {
    Some(match code {
        KeyCode::Char(c) => (c as u32, b'u'),
        KeyCode::Escape => (27, b'u'),
        KeyCode::Enter => (13, b'u'),
        KeyCode::Tab => (9, b'u'),
        KeyCode::Backspace => (127, b'u'),
        KeyCode::Insert => (2, b'~'),
        KeyCode::Delete => (3, b'~'),
        KeyCode::PageUp => (5, b'~'),
        KeyCode::PageDown => (6, b'~'),
        KeyCode::Up => (1, b'A'),
        KeyCode::Down => (1, b'B'),
        KeyCode::Right => (1, b'C'),
        KeyCode::Left => (1, b'D'),
        KeyCode::Home => (1, b'H'),
        KeyCode::End => (1, b'F'),
        KeyCode::Function(n) => match n {
            1..=4 => (1, b'P' + (n - 1)),
            5..=20 => (function_tilde_number(n)?, b'~'),
            // Beyond F20 the protocol uses its private use numbers.
            _ => (57376 + (n as u32 - 13), b'u'),
        },
        KeyCode::CapsLock => (57358, b'u'),
        KeyCode::ScrollLock => (57359, b'u'),
        KeyCode::NumLock => (57360, b'u'),
        KeyCode::PrintScreen => (57361, b'u'),
        KeyCode::Pause => (57362, b'u'),
        KeyCode::Menu => (57363, b'u'),
        KeyCode::Keypad(key) => (
            match key {
                Keypad::Digit(d) => 57399 + d as u32,
                Keypad::Decimal => 57409,
                Keypad::Divide => 57410,
                Keypad::Multiply => 57411,
                Keypad::Subtract => 57412,
                Keypad::Add => 57413,
                Keypad::Enter => 57414,
                Keypad::Equal => 57415,
                Keypad::Separator => 57416,
                Keypad::Begin => 57427,
            },
            b'u',
        ),
        KeyCode::ModifierKey(key) => (
            match key {
                ModifierKey::LeftShift => 57441,
                ModifierKey::LeftCtrl => 57442,
                ModifierKey::LeftAlt => 57443,
                ModifierKey::LeftSuper => 57444,
                ModifierKey::RightShift => 57447,
                ModifierKey::RightCtrl => 57448,
                ModifierKey::RightAlt => 57449,
                ModifierKey::RightSuper => 57450,
            },
            b'u',
        ),
        // The Kitty protocol numbers every key it knows about, and the
        // Japanese conversion keys are not among them. The private use range
        // is the protocol's to hand out, so a number picked here would mean
        // two terminals disagreeing about what it stood for.
        KeyCode::Ime(_) => return None,
        KeyCode::Unknown(_) => return None,
    })
}

fn encode_kitty(event: &KeyEvent, ctx: &EncodeContext) -> Vec<u8> {
    let flags = ctx.kitty;

    // Releases are only reported when the application asked for them.
    if event.state == KeyState::Release && !flags.contains(KeyboardFlags::REPORT_EVENT_TYPES) {
        return Vec::new();
    }
    // Bare modifier presses are only reported in the escape-everything mode.
    if matches!(event.code, KeyCode::ModifierKey(_))
        && !flags.contains(KeyboardFlags::REPORT_ALL_KEYS_AS_ESCAPE)
    {
        return Vec::new();
    }

    let Some((number, final_byte)) = kitty_key(event.code) else {
        return Vec::new();
    };
    let mods = event.modifiers.effective();

    // Repeats and releases have to be escaped once the application asked to
    // tell them apart, even for keys that would otherwise send plain bytes.
    let plain_event =
        event.state == KeyState::Press || !flags.contains(KeyboardFlags::REPORT_EVENT_TYPES);
    let unmodified = mods.without(Modifiers::SHIFT).is_empty();
    let escape_everything = flags.contains(KeyboardFlags::REPORT_ALL_KEYS_AS_ESCAPE);

    // Below the escape-everything level the protocol requires the legacy
    // bytes for keys that have them. Enter, tab, backspace and escape carry no
    // `text`, so testing `event.text` here would skip them and send CSI-u,
    // which an application at flag level 1 is not required to understand.
    if unmodified && plain_event && !escape_everything {
        if matches!(
            event.code,
            KeyCode::Enter | KeyCode::Tab | KeyCode::Backspace | KeyCode::Escape
        ) {
            return encode_legacy(event, ctx);
        }
        if let Some(text) = event.text {
            let mut buf = [0u8; 4];
            return text.encode_utf8(&mut buf).as_bytes().to_vec();
        }
        // Cursor, navigation and function keys keep their legacy forms too,
        // so that application cursor key mode is still honoured.
        if matches!(
            event.code,
            KeyCode::Up
                | KeyCode::Down
                | KeyCode::Left
                | KeyCode::Right
                | KeyCode::Home
                | KeyCode::End
                | KeyCode::PageUp
                | KeyCode::PageDown
                | KeyCode::Insert
                | KeyCode::Delete
                | KeyCode::Function(_)
                | KeyCode::Keypad(_)
        ) {
            return encode_legacy(event, ctx);
        }
    }

    // CSI number [: shifted [: base]] [; modifiers [: event]] [; text] final
    let mut key_field = number.to_string();
    if flags.contains(KeyboardFlags::REPORT_ALTERNATE_KEYS) {
        let shifted = event
            .text
            .filter(|c| Some(*c) != event.base && mods.shift())
            .map(|c| c as u32);
        let base = event.base.map(|c| c as u32).filter(|&b| b != number);
        if shifted.is_some() || base.is_some() {
            key_field.push(':');
            if let Some(shifted) = shifted {
                key_field.push_str(&shifted.to_string());
            }
            if let Some(base) = base {
                key_field.push(':');
                key_field.push_str(&base.to_string());
            }
        }
    }

    // The Kitty protocol does report caps lock and num lock, unlike the
    // legacy encoding, so the full modifier set is used for the parameter.
    let modifier_param = event.modifiers.xterm_param();
    let report_event =
        flags.contains(KeyboardFlags::REPORT_EVENT_TYPES) && event.state != KeyState::Press;
    let mut modifier_field = String::new();
    if modifier_param != 1 || report_event {
        modifier_field = modifier_param.to_string();
        if report_event {
            modifier_field.push(':');
            modifier_field.push_str(&event.state.kitty_event_type().to_string());
        }
    }

    let mut text_field = String::new();
    if flags.contains(KeyboardFlags::REPORT_ASSOCIATED_TEXT) && event.state != KeyState::Release {
        if let Some(text) = event.text {
            text_field = (text as u32).to_string();
        }
    }

    let mut out = b"\x1b[".to_vec();
    // A leading `1` is implied for the legacy finals, but including it is
    // always valid and keeps the parameter positions unambiguous.
    out.extend_from_slice(key_field.as_bytes());
    if !modifier_field.is_empty() || !text_field.is_empty() {
        out.push(b';');
        out.extend_from_slice(modifier_field.as_bytes());
    }
    if !text_field.is_empty() {
        out.push(b';');
        out.extend_from_slice(text_field.as_bytes());
    }
    out.push(final_byte);
    out
}

// ---------------------------------------------------------------------------
// Mouse
// ---------------------------------------------------------------------------

/// Encode a mouse event, or `None` when the application is not listening for
/// this kind of event.
pub fn encode_mouse(event: &MouseEvent, mouse: MouseState) -> Option<Vec<u8>> {
    let tracking = mouse.tracking;
    if !tracking.is_enabled() {
        return None;
    }
    match event.action {
        MouseAction::Release if !tracking.reports_release() => return None,
        MouseAction::Drag if !tracking.reports_motion() => return None,
        MouseAction::Motion if !tracking.reports_all_motion() => return None,
        _ => {}
    }
    if tracking == MouseTracking::X10 && event.action != MouseAction::Press {
        return None;
    }

    // Three is the protocol's "no button" code, which is what bare motion and
    // a legacy release both report. Inventing a button here made every drag
    // from a real device carry an out of range button number.
    const NO_BUTTON: u32 = 3;
    let mut code = match event.action {
        MouseAction::Release if mouse.encoding != MouseEncoding::Sgr => NO_BUTTON,
        _ => match event.button {
            Some(button) => button.report_code(),
            None => NO_BUTTON,
        },
    };
    if matches!(event.action, MouseAction::Drag | MouseAction::Motion) {
        code += 32;
    }
    // X10 tracking predates modifier reporting.
    if tracking != MouseTracking::X10 {
        let mods = event.modifiers.effective();
        if mods.shift() {
            code += 4;
        }
        if mods.alt() {
            code += 8;
        }
        if mods.ctrl() {
            code += 16;
        }
    }

    let (col, row) = (event.col + 1, event.row + 1);
    let bytes = match mouse.encoding {
        MouseEncoding::Sgr => {
            let final_byte = if event.action == MouseAction::Release {
                'm'
            } else {
                'M'
            };
            format!("\x1b[<{code};{col};{row}{final_byte}").into_bytes()
        }
        MouseEncoding::Urxvt => format!("\x1b[{};{col};{row}M", code + 32).into_bytes(),
        MouseEncoding::Utf8 => {
            let mut out = b"\x1b[M".to_vec();
            push_utf8(&mut out, code + 32);
            push_utf8(&mut out, col as u32 + 32);
            push_utf8(&mut out, row as u32 + 32);
            out
        }
        MouseEncoding::X10 => {
            // Coordinates beyond 223 cannot be expressed, so they are clamped.
            let clamp = |v: usize| (v.min(223) + 32) as u8;
            vec![
                0x1b,
                b'[',
                b'M',
                (code.min(223) + 32) as u8,
                clamp(col),
                clamp(row),
            ]
        }
    };
    Some(bytes)
}

fn push_utf8(out: &mut Vec<u8>, value: u32) {
    let c = char::from_u32(value).unwrap_or('\u{fffd}');
    let mut buf = [0u8; 4];
    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
}

/// Wheel events on the alternate screen can be translated into arrow keys, so
/// that pagers scroll even though they never asked for mouse reporting.
pub fn encode_alternate_scroll(
    button: MouseButton,
    lines: usize,
    ctx: &EncodeContext,
) -> Option<Vec<u8>> {
    let code = match button {
        MouseButton::WheelUp => KeyCode::Up,
        MouseButton::WheelDown => KeyCode::Down,
        _ => return None,
    };
    let event = KeyEvent::new(code, Modifiers::NONE);
    let one = encode_legacy(&event, ctx);
    Some(one.repeat(lines.max(1)))
}

/// Wrap pasted text in the bracketed paste markers when the mode is on.
pub fn encode_paste(text: &str, bracketed: bool) -> Vec<u8> {
    // Escape sequences inside a paste would otherwise be executed; stripping
    // them is what makes bracketed paste a safety feature rather than a
    // convenience.
    let filtered: String = text
        .chars()
        .filter(|c| *c == '\n' || *c == '\t' || !c.is_control())
        .collect();
    let filtered = filtered.replace('\n', "\r");
    if bracketed {
        let mut out = b"\x1b[200~".to_vec();
        out.extend_from_slice(filtered.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        filtered.into_bytes()
    }
}

/// Focus reporting, sent when mode 1004 is on.
pub fn encode_focus(gained: bool) -> &'static [u8] {
    if gained {
        b"\x1b[I"
    } else {
        b"\x1b[O"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::ImeKey;

    fn key(code: KeyCode, mods: Modifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    fn encode(code: KeyCode, mods: Modifiers) -> String {
        String::from_utf8(encode_key(&key(code, mods), &EncodeContext::default())).unwrap()
    }

    #[test]
    fn plain_characters_pass_through() {
        assert_eq!(encode(KeyCode::Char('a'), Modifiers::NONE), "a");
    }

    #[test]
    fn control_characters_follow_the_ascii_table() {
        assert_eq!(encode(KeyCode::Char('c'), Modifiers::CTRL), "\x03");
        assert_eq!(encode(KeyCode::Char('d'), Modifiers::CTRL), "\x04");
        assert_eq!(encode(KeyCode::Char('['), Modifiers::CTRL), "\x1b");
        assert_eq!(encode(KeyCode::Char(' '), Modifiers::CTRL), "\0");
    }

    #[test]
    fn alt_prefixes_an_escape() {
        assert_eq!(encode(KeyCode::Char('b'), Modifiers::ALT), "\x1bb");
        assert_eq!(
            encode(KeyCode::Char('c'), Modifiers::ALT.union(Modifiers::CTRL)),
            "\x1b\x03"
        );
    }

    #[test]
    fn enter_backspace_and_tab() {
        assert_eq!(encode(KeyCode::Enter, Modifiers::NONE), "\r");
        assert_eq!(encode(KeyCode::Backspace, Modifiers::NONE), "\x7f");
        assert_eq!(encode(KeyCode::Backspace, Modifiers::CTRL), "\x08");
        assert_eq!(encode(KeyCode::Tab, Modifiers::NONE), "\t");
        assert_eq!(encode(KeyCode::Tab, Modifiers::SHIFT), "\x1b[Z");
    }

    #[test]
    fn newline_mode_adds_a_line_feed() {
        let ctx = EncodeContext {
            newline_mode: true,
            ..EncodeContext::default()
        };
        let bytes = encode_key(&key(KeyCode::Enter, Modifiers::NONE), &ctx);
        assert_eq!(bytes, b"\r\n");
    }

    #[test]
    fn arrows_respect_application_cursor_keys() {
        assert_eq!(encode(KeyCode::Up, Modifiers::NONE), "\x1b[A");
        let ctx = EncodeContext {
            cursor_keys_application: true,
            ..EncodeContext::default()
        };
        let bytes = encode_key(&key(KeyCode::Up, Modifiers::NONE), &ctx);
        assert_eq!(bytes, b"\x1bOA");
    }

    #[test]
    fn modified_arrows_use_the_parameter_form() {
        assert_eq!(encode(KeyCode::Right, Modifiers::CTRL), "\x1b[1;5C");
        assert_eq!(encode(KeyCode::Left, Modifiers::SHIFT), "\x1b[1;2D");
    }

    #[test]
    fn navigation_keys_use_tilde_sequences() {
        assert_eq!(encode(KeyCode::Home, Modifiers::NONE), "\x1b[H");
        assert_eq!(encode(KeyCode::Delete, Modifiers::NONE), "\x1b[3~");
        assert_eq!(encode(KeyCode::PageUp, Modifiers::CTRL), "\x1b[5;5~");
    }

    #[test]
    fn function_keys_split_at_f5() {
        assert_eq!(encode(KeyCode::Function(1), Modifiers::NONE), "\x1bOP");
        assert_eq!(encode(KeyCode::Function(4), Modifiers::NONE), "\x1bOS");
        assert_eq!(encode(KeyCode::Function(5), Modifiers::NONE), "\x1b[15~");
        assert_eq!(encode(KeyCode::Function(12), Modifiers::NONE), "\x1b[24~");
        assert_eq!(encode(KeyCode::Function(1), Modifiers::SHIFT), "\x1b[1;2P");
    }

    #[test]
    fn keypad_follows_the_application_mode() {
        assert_eq!(
            encode(KeyCode::Keypad(Keypad::Digit(5)), Modifiers::NONE),
            "5"
        );
        let ctx = EncodeContext {
            keypad_application: true,
            ..EncodeContext::default()
        };
        let bytes = encode_key(
            &key(KeyCode::Keypad(Keypad::Digit(5)), Modifiers::NONE),
            &ctx,
        );
        assert_eq!(bytes, b"\x1bOu");
        let enter = encode_key(&key(KeyCode::Keypad(Keypad::Enter), Modifiers::NONE), &ctx);
        assert_eq!(enter, b"\x1bOM");
    }

    #[test]
    fn bare_modifier_keys_send_nothing_in_legacy_mode() {
        assert_eq!(
            encode(KeyCode::ModifierKey(ModifierKey::LeftCtrl), Modifiers::NONE),
            ""
        );
    }

    #[test]
    fn releases_send_nothing_in_legacy_mode() {
        let event = key(KeyCode::Char('a'), Modifiers::NONE).with_state(KeyState::Release);
        assert!(encode_key(&event, &EncodeContext::default()).is_empty());
    }

    // ---- Kitty keyboard protocol ----

    fn kitty_ctx(flags: KeyboardFlags) -> EncodeContext {
        EncodeContext {
            kitty: flags,
            ..EncodeContext::default()
        }
    }

    #[test]
    fn kitty_leaves_plain_typing_alone() {
        let ctx = kitty_ctx(KeyboardFlags::DISAMBIGUATE);
        let bytes = encode_key(&key(KeyCode::Char('a'), Modifiers::NONE), &ctx);
        assert_eq!(bytes, b"a");
    }

    #[test]
    fn kitty_disambiguates_modified_keys() {
        let ctx = kitty_ctx(KeyboardFlags::DISAMBIGUATE);
        let bytes = encode_key(&key(KeyCode::Char('a'), Modifiers::CTRL), &ctx);
        assert_eq!(String::from_utf8(bytes).unwrap(), "\x1b[97;5u");
    }

    #[test]
    fn kitty_can_express_ctrl_shift_combinations() {
        // The pair legacy encoding cannot distinguish from plain ctrl.
        let ctx = kitty_ctx(KeyboardFlags::DISAMBIGUATE);
        let plain = encode_key(&key(KeyCode::Char('i'), Modifiers::CTRL), &ctx);
        let shifted = encode_key(
            &key(KeyCode::Char('i'), Modifiers::CTRL.union(Modifiers::SHIFT)),
            &ctx,
        );
        assert_ne!(plain, shifted);
        assert_eq!(String::from_utf8(shifted).unwrap(), "\x1b[105;6u");
    }

    #[test]
    fn kitty_reports_releases_when_asked() {
        let ctx = kitty_ctx(KeyboardFlags(
            KeyboardFlags::DISAMBIGUATE.0 | KeyboardFlags::REPORT_EVENT_TYPES.0,
        ));
        let event = key(KeyCode::Char('a'), Modifiers::NONE).with_state(KeyState::Release);
        assert_eq!(
            String::from_utf8(encode_key(&event, &ctx)).unwrap(),
            "\x1b[97;1:3u"
        );
        let repeat = key(KeyCode::Char('a'), Modifiers::NONE).with_state(KeyState::Repeat);
        assert_eq!(
            String::from_utf8(encode_key(&repeat, &ctx)).unwrap(),
            "\x1b[97;1:2u"
        );
    }

    #[test]
    fn the_yen_key_reaches_an_application_as_utf8() {
        // Every other key in the built in layout is ASCII, so the yen sign is
        // the first character in it that takes more than one byte on the wire.
        let ctx = EncodeContext::default();
        let bytes = encode_key(&key(KeyCode::Char('¥'), Modifiers::NONE), &ctx);
        assert_eq!(bytes, "¥".as_bytes());
    }

    #[test]
    fn conversion_keys_send_nothing_to_an_application() {
        for ime in [ImeKey::Convert, ImeKey::NonConvert, ImeKey::KanaMode] {
            assert_eq!(encode(KeyCode::Ime(ime), Modifiers::NONE), "");
        }
        // Not even in the escape-everything mode, which reports every key the
        // Kitty protocol has a number for. These have none, and a guessed one
        // would send an application bytes it cannot interpret.
        let ctx = kitty_ctx(KeyboardFlags(
            KeyboardFlags::DISAMBIGUATE.0 | KeyboardFlags::REPORT_ALL_KEYS_AS_ESCAPE.0,
        ));
        let event = key(KeyCode::Ime(ImeKey::Convert), Modifiers::NONE);
        assert!(encode_key(&event, &ctx).is_empty());
    }

    #[test]
    fn kitty_keeps_legacy_finals_for_arrows() {
        let ctx = kitty_ctx(KeyboardFlags::DISAMBIGUATE);
        let bytes = encode_key(&key(KeyCode::Up, Modifiers::CTRL), &ctx);
        assert_eq!(String::from_utf8(bytes).unwrap(), "\x1b[1;5A");
    }

    #[test]
    fn kitty_reports_associated_text_when_asked() {
        let ctx = kitty_ctx(KeyboardFlags(
            KeyboardFlags::DISAMBIGUATE.0
                | KeyboardFlags::REPORT_ALL_KEYS_AS_ESCAPE.0
                | KeyboardFlags::REPORT_ASSOCIATED_TEXT.0,
        ));
        let bytes = encode_key(&key(KeyCode::Char('a'), Modifiers::NONE), &ctx);
        assert_eq!(String::from_utf8(bytes).unwrap(), "\x1b[97;;97u");
    }

    #[test]
    fn kitty_reports_modifier_keys_only_in_escape_everything_mode() {
        let quiet = kitty_ctx(KeyboardFlags::DISAMBIGUATE);
        let event = key(KeyCode::ModifierKey(ModifierKey::LeftCtrl), Modifiers::CTRL);
        assert!(encode_key(&event, &quiet).is_empty());

        let loud = kitty_ctx(KeyboardFlags(
            KeyboardFlags::DISAMBIGUATE.0 | KeyboardFlags::REPORT_ALL_KEYS_AS_ESCAPE.0,
        ));
        assert_eq!(
            String::from_utf8(encode_key(&event, &loud)).unwrap(),
            "\x1b[57442;5u"
        );
    }

    // ---- mouse ----

    fn mouse_state(tracking: MouseTracking, encoding: MouseEncoding) -> MouseState {
        MouseState {
            tracking,
            encoding,
            alternate_scroll: false,
        }
    }

    fn mouse(action: MouseAction, button: Option<MouseButton>) -> MouseEvent {
        MouseEvent {
            button,
            action,
            col: 4,
            row: 9,
            modifiers: Modifiers::NONE,
        }
    }

    #[test]
    fn disabled_tracking_reports_nothing() {
        let state = mouse_state(MouseTracking::None, MouseEncoding::Sgr);
        assert!(encode_mouse(&mouse(MouseAction::Press, Some(MouseButton::Left)), state).is_none());
    }

    #[test]
    fn sgr_encoding_is_one_based() {
        let state = mouse_state(MouseTracking::Normal, MouseEncoding::Sgr);
        let bytes =
            encode_mouse(&mouse(MouseAction::Press, Some(MouseButton::Left)), state).unwrap();
        assert_eq!(String::from_utf8(bytes).unwrap(), "\x1b[<0;5;10M");
    }

    #[test]
    fn sgr_release_uses_a_lowercase_final() {
        let state = mouse_state(MouseTracking::Normal, MouseEncoding::Sgr);
        let bytes =
            encode_mouse(&mouse(MouseAction::Release, Some(MouseButton::Left)), state).unwrap();
        assert_eq!(String::from_utf8(bytes).unwrap(), "\x1b[<0;5;10m");
    }

    #[test]
    fn x10_encoding_offsets_by_32() {
        let state = mouse_state(MouseTracking::Normal, MouseEncoding::X10);
        let bytes =
            encode_mouse(&mouse(MouseAction::Press, Some(MouseButton::Left)), state).unwrap();
        assert_eq!(bytes, vec![0x1b, b'[', b'M', 32, 32 + 5, 32 + 10]);
    }

    #[test]
    fn modifiers_are_added_to_the_button_code() {
        let state = mouse_state(MouseTracking::Normal, MouseEncoding::Sgr);
        let mut event = mouse(MouseAction::Press, Some(MouseButton::Left));
        event.modifiers = Modifiers::CTRL;
        let bytes = encode_mouse(&event, state).unwrap();
        assert_eq!(String::from_utf8(bytes).unwrap(), "\x1b[<16;5;10M");
    }

    #[test]
    fn drags_are_only_reported_in_button_event_tracking() {
        let normal = mouse_state(MouseTracking::Normal, MouseEncoding::Sgr);
        let drag = mouse(MouseAction::Drag, Some(MouseButton::Left));
        assert!(encode_mouse(&drag, normal).is_none());

        let button_event = mouse_state(MouseTracking::ButtonEvent, MouseEncoding::Sgr);
        let bytes = encode_mouse(&drag, button_event).unwrap();
        assert_eq!(String::from_utf8(bytes).unwrap(), "\x1b[<32;5;10M");
    }

    #[test]
    fn bare_motion_needs_any_event_tracking() {
        let button_event = mouse_state(MouseTracking::ButtonEvent, MouseEncoding::Sgr);
        let motion = mouse(MouseAction::Motion, None);
        assert!(encode_mouse(&motion, button_event).is_none());

        let any = mouse_state(MouseTracking::AnyEvent, MouseEncoding::Sgr);
        assert!(encode_mouse(&motion, any).is_some());
    }

    #[test]
    fn x10_tracking_reports_presses_only() {
        let state = mouse_state(MouseTracking::X10, MouseEncoding::X10);
        assert!(encode_mouse(&mouse(MouseAction::Press, Some(MouseButton::Left)), state).is_some());
        assert!(
            encode_mouse(&mouse(MouseAction::Release, Some(MouseButton::Left)), state).is_none()
        );
    }

    #[test]
    fn wheel_buttons_report_in_the_high_range() {
        let state = mouse_state(MouseTracking::Normal, MouseEncoding::Sgr);
        let bytes = encode_mouse(
            &mouse(MouseAction::Press, Some(MouseButton::WheelUp)),
            state,
        )
        .unwrap();
        assert_eq!(String::from_utf8(bytes).unwrap(), "\x1b[<64;5;10M");
    }

    #[test]
    fn alternate_scroll_becomes_arrow_keys() {
        let ctx = EncodeContext::default();
        let bytes = encode_alternate_scroll(MouseButton::WheelUp, 3, &ctx).unwrap();
        assert_eq!(String::from_utf8(bytes).unwrap(), "\x1b[A\x1b[A\x1b[A");
        assert!(encode_alternate_scroll(MouseButton::Left, 1, &ctx).is_none());
    }

    // ---- paste and focus ----

    #[test]
    fn bracketed_paste_wraps_the_text() {
        let bytes = encode_paste("hi", true);
        assert_eq!(String::from_utf8(bytes).unwrap(), "\x1b[200~hi\x1b[201~");
    }

    #[test]
    fn paste_strips_escape_sequences() {
        // Without this an attacker controlled clipboard could run commands.
        let bytes = encode_paste("ls\x1b[200~; rm -rf /", true);
        let text = String::from_utf8(bytes).unwrap();
        assert!(!text[6..text.len() - 6].contains('\x1b'));
    }

    #[test]
    fn paste_converts_newlines_to_carriage_returns() {
        let bytes = encode_paste("a\nb", false);
        assert_eq!(String::from_utf8(bytes).unwrap(), "a\rb");
    }

    #[test]
    fn focus_events_are_distinct() {
        assert_eq!(encode_focus(true), b"\x1b[I");
        assert_eq!(encode_focus(false), b"\x1b[O");
    }
}

#[cfg(test)]
mod review_regressions {
    use super::*;
    use crate::event::{KeyEvent, Modifiers, MouseEvent};

    fn kitty(flags: KeyboardFlags) -> EncodeContext {
        EncodeContext {
            kitty: flags,
            ..EncodeContext::default()
        }
    }

    fn text(bytes: Vec<u8>) -> String {
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn kitty_keeps_the_legacy_bytes_for_control_keys() {
        // At flag level 1 an application is not required to parse CSI-u, so
        // these keys must still send what they always did.
        let ctx = kitty(KeyboardFlags::DISAMBIGUATE);
        for (code, expected) in [
            (KeyCode::Enter, "\r"),
            (KeyCode::Tab, "\t"),
            (KeyCode::Backspace, "\x7f"),
            (KeyCode::Escape, "\x1b"),
        ] {
            let bytes = encode_key(&KeyEvent::new(code, Modifiers::NONE), &ctx);
            assert_eq!(text(bytes), expected, "for {code:?}");
        }
    }

    #[test]
    fn kitty_still_escapes_control_keys_when_modified() {
        let ctx = kitty(KeyboardFlags::DISAMBIGUATE);
        let bytes = encode_key(&KeyEvent::new(KeyCode::Enter, Modifiers::CTRL), &ctx);
        assert_eq!(text(bytes), "\x1b[13;5u");
    }

    #[test]
    fn kitty_escapes_everything_when_asked() {
        let ctx = kitty(KeyboardFlags(
            KeyboardFlags::DISAMBIGUATE.0 | KeyboardFlags::REPORT_ALL_KEYS_AS_ESCAPE.0,
        ));
        let bytes = encode_key(&KeyEvent::new(KeyCode::Enter, Modifiers::NONE), &ctx);
        assert_eq!(text(bytes), "\x1b[13u");
    }

    #[test]
    fn kitty_honours_application_cursor_keys() {
        let ctx = EncodeContext {
            kitty: KeyboardFlags::DISAMBIGUATE,
            cursor_keys_application: true,
            ..EncodeContext::default()
        };
        let bytes = encode_key(&KeyEvent::new(KeyCode::Up, Modifiers::NONE), &ctx);
        assert_eq!(text(bytes), "\x1bOA", "DECCKM must still apply");
    }

    #[test]
    fn kitty_reports_the_lock_modifiers() {
        // Caps lock is modifier bit 64, so the parameter is 1 + 64 + 4.
        let ctx = kitty(KeyboardFlags::DISAMBIGUATE);
        let event = KeyEvent::new(
            KeyCode::Char('a'),
            Modifiers::CTRL.union(Modifiers::CAPS_LOCK),
        );
        assert_eq!(text(encode_key(&event, &ctx)), "\x1b[97;69u");
    }

    #[test]
    fn legacy_encoding_ignores_the_lock_modifiers() {
        let ctx = EncodeContext::default();
        let event = KeyEvent::new(
            KeyCode::Char('c'),
            Modifiers::CTRL.union(Modifiers::CAPS_LOCK),
        );
        assert_eq!(encode_key(&event, &ctx), b"\x03");
    }

    fn mouse_state(tracking: MouseTracking, encoding: MouseEncoding) -> MouseState {
        MouseState {
            tracking,
            encoding,
            alternate_scroll: false,
        }
    }

    #[test]
    fn button_less_motion_reports_the_no_button_code() {
        // Devices report drags with no button; inventing one made every drag
        // report an out of range button number.
        let state = mouse_state(MouseTracking::AnyEvent, MouseEncoding::Sgr);
        let event = MouseEvent {
            button: None,
            action: MouseAction::Motion,
            col: 4,
            row: 9,
            modifiers: Modifiers::NONE,
        };
        assert_eq!(text(encode_mouse(&event, state).unwrap()), "\x1b[<35;5;10M");
    }

    #[test]
    fn button_less_drag_reports_the_no_button_code() {
        let state = mouse_state(MouseTracking::ButtonEvent, MouseEncoding::Sgr);
        let event = MouseEvent {
            button: None,
            action: MouseAction::Drag,
            col: 0,
            row: 0,
            modifiers: Modifiers::NONE,
        };
        assert_eq!(text(encode_mouse(&event, state).unwrap()), "\x1b[<35;1;1M");
    }

    #[test]
    fn extra_buttons_report_in_the_128_range() {
        let state = mouse_state(MouseTracking::Normal, MouseEncoding::Sgr);
        let event = MouseEvent {
            button: Some(MouseButton::Other(8)),
            action: MouseAction::Press,
            col: 0,
            row: 0,
            modifiers: Modifiers::NONE,
        };
        assert_eq!(text(encode_mouse(&event, state).unwrap()), "\x1b[<128;1;1M");
    }
}
