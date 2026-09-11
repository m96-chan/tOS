//! Reading input from a host terminal.
//!
//! When tOS runs nested inside another terminal for development, its input
//! arrives as an escape sequence stream rather than from evdev. This decoder
//! turns that stream back into [`InputEvent`]s, which is the inverse of what
//! [`crate::encode`] does.

use crate::event::{
    InputEvent, KeyCode, KeyEvent, KeyState, ModifierKey, Modifiers, MouseAction, MouseButton,
    MouseEvent,
};

/// Incremental decoder for a host terminal's input stream.
#[derive(Debug, Default)]
pub struct HostInput {
    buf: Vec<u8>,
    /// Collected text while inside a bracketed paste.
    paste: Option<Vec<u8>>,
}

impl HostInput {
    pub fn new() -> Self {
        HostInput::default()
    }

    /// Feed bytes and take whatever complete events they produced.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<InputEvent> {
        self.buf.extend_from_slice(bytes);
        let mut events = Vec::new();
        loop {
            match self.step() {
                Step::Event(event) => events.push(event),
                Step::Consumed => {}
                Step::NeedMore => break,
            }
        }
        events
    }

    /// Bytes held back because they are an incomplete sequence.
    pub fn pending(&self) -> usize {
        self.buf.len()
    }

    fn step(&mut self) -> Step {
        if self.buf.is_empty() {
            return Step::NeedMore;
        }

        if self.paste.is_some() {
            return self.step_paste();
        }

        match self.buf[0] {
            0x1b => self.step_escape(),
            byte => {
                let (event, used) = decode_plain(&self.buf);
                if used == 0 {
                    return Step::NeedMore;
                }
                self.buf.drain(..used);
                let _ = byte;
                match event {
                    Some(event) => Step::Event(event),
                    None => Step::Consumed,
                }
            }
        }
    }

    fn step_paste(&mut self) -> Step {
        const END: &[u8] = b"\x1b[201~";
        let Some(at) = find(&self.buf, END) else {
            // Keep everything that cannot yet be part of the terminator.
            let keep = END.len() - 1;
            if self.buf.len() > keep {
                let take = self.buf.len() - keep;
                let chunk: Vec<u8> = self.buf.drain(..take).collect();
                self.paste.as_mut().unwrap().extend_from_slice(&chunk);
            }
            return Step::NeedMore;
        };
        let chunk: Vec<u8> = self.buf.drain(..at).collect();
        self.buf.drain(..END.len());
        let mut text = self.paste.take().unwrap();
        text.extend_from_slice(&chunk);
        Step::Event(InputEvent::Paste(
            String::from_utf8_lossy(&text).into_owned(),
        ))
    }

    fn step_escape(&mut self) -> Step {
        if self.buf.len() < 2 {
            return Step::NeedMore;
        }
        match self.buf[1] {
            b'[' => self.step_csi(),
            b'O' => {
                if self.buf.len() < 3 {
                    return Step::NeedMore;
                }
                let final_byte = self.buf[2];
                self.buf.drain(..3);
                match ss3_key(final_byte) {
                    Some(code) => Step::Event(InputEvent::Key(KeyEvent::new(
                        code,
                        Modifiers::NONE,
                    ))),
                    None => Step::Consumed,
                }
            }
            0x1b => {
                // A lone escape followed by another escape: report the first.
                self.buf.drain(..1);
                Step::Event(InputEvent::Key(KeyEvent::new(
                    KeyCode::Escape,
                    Modifiers::NONE,
                )))
            }
            _ => {
                // Alt plus whatever follows.
                let rest = self.buf[1..].to_vec();
                let (event, used) = decode_plain(&rest);
                if used == 0 {
                    return Step::NeedMore;
                }
                self.buf.drain(..used + 1);
                match event {
                    Some(InputEvent::Key(mut key)) => {
                        key.modifiers.insert(Modifiers::ALT);
                        Step::Event(InputEvent::Key(key))
                    }
                    other => other.map(Step::Event).unwrap_or(Step::Consumed),
                }
            }
        }
    }

    fn step_csi(&mut self) -> Step {
        // Find the final byte of the sequence.
        let Some(end) = self.buf[2..]
            .iter()
            .position(|&b| (0x40..=0x7e).contains(&b))
            .map(|i| i + 2)
        else {
            return Step::NeedMore;
        };
        let body: Vec<u8> = self.buf[2..end].to_vec();
        let final_byte = self.buf[end];
        self.buf.drain(..end + 1);

        // Mouse reports in SGR encoding.
        if body.first() == Some(&b'<') {
            return match decode_sgr_mouse(&body[1..], final_byte) {
                Some(event) => Step::Event(InputEvent::Mouse(event)),
                None => Step::Consumed,
            };
        }

        let params = parse_params(&body);
        match final_byte {
            b'I' => return Step::Event(InputEvent::FocusGained),
            b'O' => return Step::Event(InputEvent::FocusLost),
            b'~' => {
                let number = params.first().copied().unwrap_or(0);
                if number == 200 {
                    self.paste = Some(Vec::new());
                    return Step::Consumed;
                }
                let mods = modifiers_from_param(params.get(1).copied());
                return match tilde_key(number) {
                    Some(code) => Step::Event(InputEvent::Key(KeyEvent::new(code, mods))),
                    None => Step::Consumed,
                };
            }
            b'u' => {
                // The Kitty protocol's own encoding, which a host terminal may
                // send back if tOS asked it to.
                let number = params.first().copied().unwrap_or(0);
                let mods = modifiers_from_param(params.get(1).copied());
                let code = char::from_u32(number)
                    .map(KeyCode::Char)
                    .unwrap_or(KeyCode::Unknown(number));
                return Step::Event(InputEvent::Key(KeyEvent::new(code, mods)));
            }
            _ => {}
        }

        let mut mods = modifiers_from_param(params.get(1).copied());
        // CSI Z is back tab: the shift is in the final byte, not a parameter.
        if final_byte == b'Z' {
            mods.insert(Modifiers::SHIFT);
        }
        match csi_key(final_byte) {
            Some(code) => Step::Event(InputEvent::Key(KeyEvent::new(code, mods))),
            None => Step::Consumed,
        }
    }
}

enum Step {
    Event(InputEvent),
    Consumed,
    NeedMore,
}

/// Decode a non-escape byte sequence, returning the event and bytes used.
fn decode_plain(buf: &[u8]) -> (Option<InputEvent>, usize) {
    let byte = buf[0];
    let key = |code: KeyCode, mods: Modifiers| {
        (
            Some(InputEvent::Key(KeyEvent::new(code, mods))),
            1usize,
        )
    };
    match byte {
        b'\r' | b'\n' => key(KeyCode::Enter, Modifiers::NONE),
        b'\t' => key(KeyCode::Tab, Modifiers::NONE),
        0x7f | 0x08 => key(KeyCode::Backspace, Modifiers::NONE),
        0x00 => key(KeyCode::Char(' '), Modifiers::CTRL),
        0x01..=0x1a => {
            let c = (b'a' + byte - 1) as char;
            key(KeyCode::Char(c), Modifiers::CTRL)
        }
        0x1c..=0x1f => {
            let c = (b'\\' + byte - 0x1c) as char;
            key(KeyCode::Char(c), Modifiers::CTRL)
        }
        0x20..=0x7e => {
            let c = byte as char;
            // The base key is what the layout produces unshifted; without it
            // a binding on shift plus a digit could never match, because the
            // host only ever sends the shifted character.
            let base = unshift_us(c);
            let mut event = KeyEvent::new(KeyCode::Char(base), Modifiers::NONE);
            if base != c {
                event.modifiers.insert(Modifiers::SHIFT);
            }
            event.text = Some(c);
            event.base = Some(base);
            (Some(InputEvent::Key(event)), 1)
        }
        _ => {
            // Multi byte UTF-8.
            let needed = utf8_length(byte);
            if buf.len() < needed {
                return (None, 0);
            }
            match std::str::from_utf8(&buf[..needed]) {
                Ok(text) => {
                    let c = text.chars().next().unwrap();
                    let mut event = KeyEvent::new(KeyCode::Char(c), Modifiers::NONE);
                    event.text = Some(c);
                    (Some(InputEvent::Key(event)), needed)
                }
                Err(_) => (None, 1),
            }
        }
    }
}

/// The unshifted character a US layout key produces.
///
/// A host terminal reports only the resulting character, so the base key has
/// to be inferred to tell shifted keys apart from unshifted ones.
fn unshift_us(c: char) -> char {
    match c {
        'A'..='Z' => c.to_ascii_lowercase(),
        '!' => '1',
        '@' => '2',
        '#' => '3',
        '$' => '4',
        '%' => '5',
        '^' => '6',
        '&' => '7',
        '*' => '8',
        '(' => '9',
        ')' => '0',
        '_' => '-',
        '+' => '=',
        '{' => '[',
        '}' => ']',
        '|' => '\\',
        ':' => ';',
        '"' => '\'',
        '<' => ',',
        '>' => '.',
        '?' => '/',
        '~' => '`',
        other => other,
    }
}

fn utf8_length(lead: u8) -> usize {
    match lead {
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    }
}

fn parse_params(body: &[u8]) -> Vec<u32> {
    body.split(|&b| b == b';' || b == b':')
        .map(|part| {
            std::str::from_utf8(part)
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0)
        })
        .collect()
}

fn modifiers_from_param(param: Option<u32>) -> Modifiers {
    let Some(param) = param else {
        return Modifiers::NONE;
    };
    if param == 0 {
        return Modifiers::NONE;
    }
    let bits = param - 1;
    let mut mods = Modifiers::NONE;
    mods.set(Modifiers::SHIFT, bits & 1 != 0);
    mods.set(Modifiers::ALT, bits & 2 != 0);
    mods.set(Modifiers::CTRL, bits & 4 != 0);
    mods.set(Modifiers::SUPER, bits & 8 != 0);
    mods
}

fn csi_key(final_byte: u8) -> Option<KeyCode> {
    Some(match final_byte {
        b'A' => KeyCode::Up,
        b'B' => KeyCode::Down,
        b'C' => KeyCode::Right,
        b'D' => KeyCode::Left,
        b'H' => KeyCode::Home,
        b'F' => KeyCode::End,
        b'P' => KeyCode::Function(1),
        b'Q' => KeyCode::Function(2),
        b'R' => KeyCode::Function(3),
        b'S' => KeyCode::Function(4),
        b'Z' => return Some(KeyCode::Tab),
        _ => return None,
    })
}

fn ss3_key(final_byte: u8) -> Option<KeyCode> {
    Some(match final_byte {
        b'A' => KeyCode::Up,
        b'B' => KeyCode::Down,
        b'C' => KeyCode::Right,
        b'D' => KeyCode::Left,
        b'H' => KeyCode::Home,
        b'F' => KeyCode::End,
        b'P' => KeyCode::Function(1),
        b'Q' => KeyCode::Function(2),
        b'R' => KeyCode::Function(3),
        b'S' => KeyCode::Function(4),
        b'M' => KeyCode::Enter,
        _ => return None,
    })
}

fn tilde_key(number: u32) -> Option<KeyCode> {
    Some(match number {
        1 | 7 => KeyCode::Home,
        2 => KeyCode::Insert,
        3 => KeyCode::Delete,
        4 | 8 => KeyCode::End,
        5 => KeyCode::PageUp,
        6 => KeyCode::PageDown,
        15 => KeyCode::Function(5),
        17 => KeyCode::Function(6),
        18 => KeyCode::Function(7),
        19 => KeyCode::Function(8),
        20 => KeyCode::Function(9),
        21 => KeyCode::Function(10),
        23 => KeyCode::Function(11),
        24 => KeyCode::Function(12),
        _ => return None,
    })
}

fn decode_sgr_mouse(body: &[u8], final_byte: u8) -> Option<MouseEvent> {
    let params = parse_params(body);
    if params.len() < 3 {
        return None;
    }
    let (code, col, row) = (params[0], params[1], params[2]);
    let motion = code & 32 != 0;
    let mut modifiers = Modifiers::NONE;
    modifiers.set(Modifiers::SHIFT, code & 4 != 0);
    modifiers.set(Modifiers::ALT, code & 8 != 0);
    modifiers.set(Modifiers::CTRL, code & 16 != 0);

    let base = code & !(4 | 8 | 16 | 32);
    let button = match base {
        0 => Some(MouseButton::Left),
        1 => Some(MouseButton::Middle),
        2 => Some(MouseButton::Right),
        64 => Some(MouseButton::WheelUp),
        65 => Some(MouseButton::WheelDown),
        66 => Some(MouseButton::WheelLeft),
        67 => Some(MouseButton::WheelRight),
        3 => None,
        other => Some(MouseButton::Other(other as u8)),
    };

    let action = if final_byte == b'm' {
        MouseAction::Release
    } else if motion {
        if button.is_some() {
            MouseAction::Drag
        } else {
            MouseAction::Motion
        }
    } else {
        MouseAction::Press
    };

    Some(MouseEvent {
        button,
        action,
        col: col.saturating_sub(1) as usize,
        row: row.saturating_sub(1) as usize,
        modifiers,
    })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Convenience for tests and for code that only cares about key presses.
pub fn key_press(code: KeyCode, modifiers: Modifiers) -> InputEvent {
    InputEvent::Key(KeyEvent::new(code, modifiers).with_state(KeyState::Press))
}

/// Modifier keys never arrive on a host terminal stream, but the type is part
/// of the shared model, so this makes the mapping explicit.
pub fn modifier_key_event(key: ModifierKey, pressed: bool) -> InputEvent {
    let state = if pressed {
        KeyState::Press
    } else {
        KeyState::Release
    };
    InputEvent::Key(
        KeyEvent::new(KeyCode::ModifierKey(key), key.modifier()).with_state(state),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events(bytes: &[u8]) -> Vec<InputEvent> {
        HostInput::new().feed(bytes)
    }

    fn key_of(event: &InputEvent) -> (KeyCode, Modifiers) {
        match event {
            InputEvent::Key(k) => (k.code, k.modifiers),
            other => panic!("expected a key event, got {other:?}"),
        }
    }

    #[test]
    fn plain_characters() {
        let events = events(b"ab");
        assert_eq!(events.len(), 2);
        assert_eq!(key_of(&events[0]).0, KeyCode::Char('a'));
    }

    #[test]
    fn uppercase_carries_shift() {
        let events = events(b"A");
        let (code, mods) = key_of(&events[0]);
        assert_eq!(code, KeyCode::Char('a'));
        assert!(mods.shift());
        match &events[0] {
            InputEvent::Key(k) => assert_eq!(k.text, Some('A')),
            _ => unreachable!(),
        }
    }

    #[test]
    fn shifted_punctuation_reports_the_base_key() {
        // Compositor bindings are written against the unshifted key, so the
        // decoder has to say which key was pressed, not just what it produced.
        let events = events(b"!");
        let (code, mods) = key_of(&events[0]);
        assert_eq!(code, KeyCode::Char('1'));
        assert!(mods.shift());
        match &events[0] {
            InputEvent::Key(k) => assert_eq!(k.text, Some('!')),
            _ => unreachable!(),
        }
    }

    #[test]
    fn unshifted_punctuation_has_no_modifier() {
        let (code, mods) = key_of(&events(b"1")[0]);
        assert_eq!(code, KeyCode::Char('1'));
        assert!(!mods.shift());
    }

    #[test]
    fn back_tab_keeps_its_shift() {
        let (code, mods) = key_of(&events(b"\x1b[Z")[0]);
        assert_eq!(code, KeyCode::Tab);
        assert!(mods.shift(), "CSI Z is shift plus tab");
    }

    #[test]
    fn control_bytes_become_ctrl_keys() {
        let (code, mods) = key_of(&events(b"\x03")[0]);
        assert_eq!(code, KeyCode::Char('c'));
        assert!(mods.ctrl());
    }

    #[test]
    fn enter_tab_and_backspace() {
        assert_eq!(key_of(&events(b"\r")[0]).0, KeyCode::Enter);
        assert_eq!(key_of(&events(b"\t")[0]).0, KeyCode::Tab);
        assert_eq!(key_of(&events(b"\x7f")[0]).0, KeyCode::Backspace);
    }

    #[test]
    fn arrow_keys() {
        assert_eq!(key_of(&events(b"\x1b[A")[0]).0, KeyCode::Up);
        assert_eq!(key_of(&events(b"\x1bOB")[0]).0, KeyCode::Down);
    }

    #[test]
    fn modified_arrow_keys() {
        let (code, mods) = key_of(&events(b"\x1b[1;5C")[0]);
        assert_eq!(code, KeyCode::Right);
        assert!(mods.ctrl());
    }

    #[test]
    fn navigation_and_function_keys() {
        assert_eq!(key_of(&events(b"\x1b[3~")[0]).0, KeyCode::Delete);
        assert_eq!(key_of(&events(b"\x1b[5~")[0]).0, KeyCode::PageUp);
        assert_eq!(key_of(&events(b"\x1b[15~")[0]).0, KeyCode::Function(5));
        assert_eq!(key_of(&events(b"\x1bOP")[0]).0, KeyCode::Function(1));
    }

    #[test]
    fn alt_prefix_sets_the_modifier() {
        let (code, mods) = key_of(&events(b"\x1bb")[0]);
        assert_eq!(code, KeyCode::Char('b'));
        assert!(mods.alt());
    }

    #[test]
    fn lone_escape_is_reported_once_a_second_byte_arrives() {
        let mut input = HostInput::new();
        assert!(input.feed(b"\x1b").is_empty());
        let events = input.feed(b"\x1b");
        assert_eq!(key_of(&events[0]).0, KeyCode::Escape);
    }

    #[test]
    fn sequences_split_across_reads_still_decode() {
        let mut input = HostInput::new();
        assert!(input.feed(b"\x1b[").is_empty());
        assert!(input.feed(b"1;").is_empty());
        let events = input.feed(b"5A");
        let (code, mods) = key_of(&events[0]);
        assert_eq!(code, KeyCode::Up);
        assert!(mods.ctrl());
    }

    #[test]
    fn multibyte_utf8_split_across_reads() {
        let mut input = HostInput::new();
        let bytes = "漢".as_bytes();
        assert!(input.feed(&bytes[..2]).is_empty());
        let events = input.feed(&bytes[2..]);
        assert_eq!(key_of(&events[0]).0, KeyCode::Char('漢'));
    }

    #[test]
    fn focus_events() {
        assert_eq!(events(b"\x1b[I"), vec![InputEvent::FocusGained]);
        assert_eq!(events(b"\x1b[O"), vec![InputEvent::FocusLost]);
    }

    #[test]
    fn sgr_mouse_press_and_release() {
        let press = events(b"\x1b[<0;5;10M");
        match press[0] {
            InputEvent::Mouse(m) => {
                assert_eq!(m.button, Some(MouseButton::Left));
                assert_eq!(m.action, MouseAction::Press);
                assert_eq!((m.col, m.row), (4, 9));
            }
            _ => panic!("expected a mouse event"),
        }
        let release = events(b"\x1b[<0;5;10m");
        match release[0] {
            InputEvent::Mouse(m) => assert_eq!(m.action, MouseAction::Release),
            _ => panic!("expected a mouse event"),
        }
    }

    #[test]
    fn sgr_mouse_drag_and_wheel() {
        match events(b"\x1b[<32;5;10M")[0] {
            InputEvent::Mouse(m) => assert_eq!(m.action, MouseAction::Drag),
            _ => panic!("expected a mouse event"),
        }
        match events(b"\x1b[<64;1;1M")[0] {
            InputEvent::Mouse(m) => assert_eq!(m.button, Some(MouseButton::WheelUp)),
            _ => panic!("expected a mouse event"),
        }
    }

    #[test]
    fn bracketed_paste_is_collected() {
        let events = events(b"\x1b[200~hello\nworld\x1b[201~");
        assert_eq!(events, vec![InputEvent::Paste("hello\nworld".into())]);
    }

    #[test]
    fn bracketed_paste_across_reads() {
        let mut input = HostInput::new();
        assert!(input.feed(b"\x1b[200~he").is_empty());
        assert!(input.feed(b"llo").is_empty());
        let events = input.feed(b"\x1b[201~");
        assert_eq!(events, vec![InputEvent::Paste("hello".into())]);
    }

    #[test]
    fn shifted_text_round_trips_through_its_character() {
        // The legacy encoding has no way to say "shift" for a text key, so
        // the round trip goes through the character the key produced.
        use crate::encode::{encode_key, EncodeContext};
        let mut event = KeyEvent::new(KeyCode::Char('1'), Modifiers::SHIFT);
        event.text = Some('!');
        let encoded = encode_key(&event, &EncodeContext::default());
        assert_eq!(encoded, b"!");
        let decoded = events(&encoded);
        assert_eq!(key_of(&decoded[0]), (KeyCode::Char('1'), Modifiers::SHIFT));
    }

    #[test]
    fn round_trips_with_the_encoder() {
        use crate::encode::{encode_key, EncodeContext};
        let ctx = EncodeContext::default();
        for (code, mods) in [
            (KeyCode::Char('a'), Modifiers::NONE),
            (KeyCode::Char('c'), Modifiers::CTRL),
            (KeyCode::Up, Modifiers::NONE),
            (KeyCode::Right, Modifiers::CTRL),
            (KeyCode::Delete, Modifiers::NONE),
            (KeyCode::Function(5), Modifiers::NONE),
            (KeyCode::Enter, Modifiers::NONE),
            (KeyCode::Tab, Modifiers::NONE),
            (KeyCode::Tab, Modifiers::SHIFT),
        ] {
            let encoded = encode_key(&KeyEvent::new(code, mods), &ctx);
            let decoded = events(&encoded);
            assert_eq!(decoded.len(), 1, "for {code:?} {mods:?}: {encoded:?}");
            assert_eq!(key_of(&decoded[0]), (code, mods), "for {code:?}");
        }
    }
}
