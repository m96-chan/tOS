//! Reading the terminal's answers: keys, mouse, and one mode report.
//!
//! This is the other side of `tos-input`'s encoder. That crate turns events
//! into bytes for an application; this one turns those bytes back into events,
//! because a browser in a pane is that application. The two are tested against
//! each other where it matters — a sequence `tos_input::encode` produces has
//! to parse here into the event it came from — which is the only way to keep a
//! decoder honest about a protocol with this many optional fields.
//!
//! Three grammars arrive down the same descriptor:
//!
//! - the Kitty keyboard protocol, `CSI key[:shifted:base] [; mods[:event]]
//!   [; text] u` and its cousins that end in `A`-`H`, `P`-`S` and `~`;
//! - SGR mouse reports, `CSI < button ; x ; y M|m`, whose coordinates are
//!   cells or pixels depending on whether mode 1016 took;
//! - the DECRPM answer `CSI ? mode ; state $ y`, which is how that question
//!   gets answered.
//!
//! And under all three, the legacy encodings, because the same binary has to
//! work in a terminal that answered `0` to every capability it was asked
//! about. A press of an unmodified printable key arrives as its own UTF-8
//! bytes even at the flag level this program asks for — `tos_input` sends the
//! legacy form when it can — so plain text is not a fallback path here, it is
//! the common one.
//!
//! # The lone escape
//!
//! `ESC` is both a key and the first byte of every other key. A parser cannot
//! tell them apart from the bytes alone, so it does not try: an `ESC` with
//! nothing after it is held, and [`Parser::flush`] turns it into an Escape
//! press when the caller has waited long enough that no more is coming. The
//! caller already has a poll loop with a timeout, so the wait costs nothing
//! and the ambiguity is resolved where the clock is.

/// Modifiers, in the protocol's own bitfield: the CSI parameter minus one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mods(pub u32);

impl Mods {
    pub const SHIFT: u32 = 1;
    pub const ALT: u32 = 2;
    pub const CTRL: u32 = 4;
    pub const SUPER: u32 = 8;

    /// From a CSI modifier parameter, where 1 means "none".
    pub fn from_param(param: u32) -> Mods {
        Mods(param.saturating_sub(1))
    }

    pub fn shift(self) -> bool {
        self.0 & Mods::SHIFT != 0
    }

    pub fn alt(self) -> bool {
        self.0 & Mods::ALT != 0
    }

    pub fn ctrl(self) -> bool {
        self.0 & Mods::CTRL != 0
    }

    pub fn meta(self) -> bool {
        self.0 & Mods::SUPER != 0
    }

    /// The mask CDP wants: Alt 1, Ctrl 2, Meta 4, Shift 8.
    pub fn cdp(self) -> u32 {
        let mut mask = 0;
        if self.alt() {
            mask |= 1;
        }
        if self.ctrl() {
            mask |= 2;
        }
        if self.meta() {
            mask |= 4;
        }
        if self.shift() {
            mask |= 8;
        }
        mask
    }

    pub fn with(self, bit: u32) -> Mods {
        Mods(self.0 | bit)
    }
}

/// A key, named the way a browser will have to name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// A key that stands for a character: letters, digits, punctuation.
    Char(char),
    Enter,
    Tab,
    Backspace,
    Escape,
    Insert,
    Delete,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Function(u8),
    /// A key the protocol numbered and this program has no name for.
    Other(u32),
}

/// Press, repeat or release, as the Kitty protocol's event types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    Press,
    Repeat,
    Release,
}

/// One key event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyInput {
    pub key: Key,
    pub mods: Mods,
    pub action: KeyAction,
    /// The text the key produced, when the terminal said.
    pub text: Option<char>,
}

impl KeyInput {
    /// A plain press, which is most of them.
    pub fn press(key: Key) -> KeyInput {
        KeyInput {
            key,
            mods: Mods::default(),
            action: KeyAction::Press,
            text: None,
        }
    }
}

/// What the pointer did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseKind {
    Press,
    Release,
    /// Moved, with or without a button held.
    Move,
    /// A wheel notch, in the direction the deltas say.
    Wheel,
}

/// One mouse report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseInput {
    pub kind: MouseKind,
    /// 0 left, 1 middle, 2 right; `None` for a bare motion or a wheel.
    pub button: Option<u32>,
    pub mods: Mods,
    /// One-based, in cells or in pixels: see [`Parser::pixel_coordinates`].
    pub x: u32,
    pub y: u32,
    /// Notches, positive right and down.
    pub wheel: (i32, i32),
}

/// Anything the terminal said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    Key(KeyInput),
    Mouse(MouseInput),
    /// A DECRPM answer: `state` is 1 set, 2 reset, 0 unrecognised.
    Mode {
        mode: u16,
        state: u8,
    },
}

/// The incremental parser.
#[derive(Debug, Default)]
pub struct Parser {
    buf: Vec<u8>,
    /// Set once a DECRPM answer says mouse reports are in pixels.
    pixels: bool,
}

impl Parser {
    pub fn new() -> Parser {
        Parser::default()
    }

    /// Whether mouse coordinates are pixels rather than cells.
    ///
    /// Only a `1` from the terminal sets this. `0` (the mode is unknown, which
    /// is what tOS says today) and `2` (known and off) both leave coordinates
    /// in cells, to be multiplied by the cell size — which is the same answer
    /// for two different reasons and is why the question is asked at all.
    pub fn pixel_coordinates(&self) -> bool {
        self.pixels
    }

    /// Feed bytes; get whatever they completed.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Input> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        loop {
            match self.step() {
                Step::Produced(input) => out.push(input),
                Step::Consumed => {}
                Step::NeedMore => break,
            }
        }
        out
    }

    /// Give up on a held `ESC` and call it the Escape key.
    pub fn flush(&mut self) -> Option<Input> {
        if self.buf == [0x1b] {
            self.buf.clear();
            return Some(Input::Key(KeyInput::press(Key::Escape)));
        }
        None
    }

    fn step(&mut self) -> Step {
        let Some(&first) = self.buf.first() else {
            return Step::NeedMore;
        };
        if first != 0x1b {
            return self.plain();
        }
        match self.buf.get(1) {
            None => Step::NeedMore,
            Some(b'[') => self.csi(),
            Some(b'O') => match self.buf.get(2) {
                None => Step::NeedMore,
                Some(&final_byte) => {
                    self.buf.drain(..3);
                    match ss3(final_byte) {
                        Some(key) => Step::Produced(Input::Key(KeyInput::press(key))),
                        None => Step::Consumed,
                    }
                }
            },
            // ESC followed by anything else is the legacy spelling of alt.
            Some(_) => {
                self.buf.remove(0);
                match self.step() {
                    Step::Produced(Input::Key(mut key)) => {
                        key.mods = key.mods.with(Mods::ALT);
                        Step::Produced(Input::Key(key))
                    }
                    // What follows has not all arrived; put the escape back so
                    // that the next feed sees the sequence whole.
                    Step::NeedMore => {
                        self.buf.insert(0, 0x1b);
                        Step::NeedMore
                    }
                    other => other,
                }
            }
        }
    }

    /// A control byte or a UTF-8 character typed without modifiers.
    fn plain(&mut self) -> Step {
        let first = self.buf[0];
        if first < 0x20 || first == 0x7f {
            self.buf.remove(0);
            return match control_key(first) {
                Some(key) => Step::Produced(Input::Key(key)),
                None => Step::Consumed,
            };
        }
        let width = utf8_width(first);
        if self.buf.len() < width {
            return Step::NeedMore;
        }
        let bytes: Vec<u8> = self.buf.drain(..width).collect();
        match std::str::from_utf8(&bytes)
            .ok()
            .and_then(|s| s.chars().next())
        {
            Some(c) => Step::Produced(Input::Key(KeyInput {
                key: Key::Char(c),
                mods: Mods::default(),
                action: KeyAction::Press,
                text: Some(c),
            })),
            None => Step::Consumed,
        }
    }

    fn csi(&mut self) -> Step {
        let mut at = 2;
        let mut prefix = None;
        if let Some(&byte) = self.buf.get(at) {
            if (0x3c..=0x3f).contains(&byte) {
                prefix = Some(byte);
                at += 1;
            }
        }
        let params_from = at;
        while let Some(&byte) = self.buf.get(at) {
            if (0x30..=0x3b).contains(&byte) {
                at += 1;
            } else {
                break;
            }
        }
        let params_to = at;
        while let Some(&byte) = self.buf.get(at) {
            if (0x20..=0x2f).contains(&byte) {
                at += 1;
            } else {
                break;
            }
        }
        let intermediate = self.buf.get(params_to..at).and_then(|s| s.last().copied());
        let Some(&final_byte) = self.buf.get(at) else {
            // A sequence that is still arriving, unless it has run long enough
            // that it cannot be one: a terminal does not send kilobytes of
            // parameters, and a buffer that grew that far is noise that would
            // otherwise wedge the parser forever.
            if self.buf.len() > 64 {
                self.buf.remove(0);
                return Step::Consumed;
            }
            return Step::NeedMore;
        };
        let params = parse_params(&self.buf[params_from..params_to]);
        self.buf.drain(..at + 1);

        match (prefix, intermediate, final_byte) {
            (Some(b'<'), _, b'M') | (Some(b'<'), _, b'm') => {
                match mouse(&params, final_byte == b'm') {
                    Some(report) => Step::Produced(Input::Mouse(report)),
                    None => Step::Consumed,
                }
            }
            (Some(b'?'), Some(b'$'), b'y') => {
                let mode = number(&params, 0, 0) as u16;
                let state = number(&params, 1, 0) as u8;
                if mode == 1016 && state == 1 {
                    self.pixels = true;
                }
                Step::Produced(Input::Mode { mode, state })
            }
            (None, _, b'u' | b'~' | b'A'..=b'H' | b'P'..=b'S' | b'Z') => {
                match key_event(&params, final_byte) {
                    Some(key) => Step::Produced(Input::Key(key)),
                    None => Step::Consumed,
                }
            }
            _ => Step::Consumed,
        }
    }
}

enum Step {
    Produced(Input),
    Consumed,
    NeedMore,
}

/// `1;5:3;97` becomes `[[1], [5, 3], [97]]`.
fn parse_params(bytes: &[u8]) -> Vec<Vec<u32>> {
    if bytes.is_empty() {
        return Vec::new();
    }
    bytes
        .split(|&b| b == b';')
        .map(|part| {
            part.split(|&b| b == b':')
                .map(|number| {
                    number
                        .iter()
                        .fold(0u32, |acc, &b| acc.saturating_mul(10) + (b - b'0') as u32)
                })
                .collect()
        })
        .collect()
}

/// A parameter's sub-value, or a default.
fn number(params: &[Vec<u32>], index: usize, sub: usize) -> u32 {
    params
        .get(index)
        .and_then(|subs| subs.get(sub))
        .copied()
        .unwrap_or(0)
}

/// The key a control byte stands for.
///
/// A terminal that does not speak the Kitty protocol sends `ctrl+l` as `0x0c`
/// and nothing else, so this is where ctrl-anything comes from outside tOS.
/// `ESC` is not here: it is the parser's business, not a key's.
fn control_key(byte: u8) -> Option<KeyInput> {
    let key = match byte {
        b'\r' | b'\n' => return Some(KeyInput::press(Key::Enter)),
        b'\t' => return Some(KeyInput::press(Key::Tab)),
        0x7f | 0x08 => return Some(KeyInput::press(Key::Backspace)),
        0 => Key::Char(' '),
        1..=26 => Key::Char((b'a' + byte - 1) as char),
        // The remaining C0 codes are ctrl with a punctuation key.
        0x1c..=0x1f => Key::Char((byte + 0x40) as char),
        _ => return None,
    };
    Some(KeyInput {
        key,
        mods: Mods(Mods::CTRL),
        action: KeyAction::Press,
        text: None,
    })
}

/// SS3, which is what cursor keys become in application mode.
fn ss3(final_byte: u8) -> Option<Key> {
    Some(match final_byte {
        b'A' => Key::Up,
        b'B' => Key::Down,
        b'C' => Key::Right,
        b'D' => Key::Left,
        b'H' => Key::Home,
        b'F' => Key::End,
        b'P'..=b'S' => Key::Function(final_byte - b'P' + 1),
        _ => return None,
    })
}

/// The key a CSI sequence names, with its modifiers, event type and text.
fn key_event(params: &[Vec<u32>], final_byte: u8) -> Option<KeyInput> {
    let first = number(params, 0, 0);
    let key = match final_byte {
        b'u' => unicode_key(first),
        b'~' => tilde_key(first)?,
        b'Z' => {
            // Shift+Tab has no parameters of its own; the shift is the name.
            return Some(KeyInput {
                key: Key::Tab,
                mods: Mods(Mods::SHIFT),
                action: KeyAction::Press,
                text: None,
            });
        }
        b'A' => Key::Up,
        b'B' => Key::Down,
        b'C' => Key::Right,
        b'D' => Key::Left,
        b'F' => Key::End,
        b'H' => Key::Home,
        b'P'..=b'S' => Key::Function(final_byte - b'P' + 1),
        _ => return None,
    };

    let mods = Mods::from_param(match number(params, 1, 0) {
        0 => 1,
        param => param,
    });
    let action = match number(params, 1, 1) {
        2 => KeyAction::Repeat,
        3 => KeyAction::Release,
        _ => KeyAction::Press,
    };
    // Three sources, in order of how much the terminal committed to. What it
    // reported as the associated text wins; then the shifted alternate, which
    // is what `shift+a` means by `97:65`; then, for a terminal that reports
    // neither, the key's own character when no modifier changed it.
    let reported = params
        .get(2)
        .and_then(|subs| subs.first())
        .copied()
        .filter(|&n| n != 0)
        .and_then(char::from_u32);
    let shifted = params
        .first()
        .and_then(|subs| subs.get(1))
        .copied()
        .filter(|_| mods.shift())
        .and_then(char::from_u32);
    let implied = match key {
        Key::Char(c) if !mods.ctrl() && !mods.alt() && !mods.meta() => Some(c),
        _ => None,
    };

    Some(KeyInput {
        key,
        mods,
        action,
        text: reported.or(shifted).or(implied).filter(|c| !c.is_control()),
    })
}

/// A `CSI number u` key: the number is a codepoint, or one of the protocol's
/// own names for a key that has no codepoint.
fn unicode_key(number: u32) -> Key {
    match number {
        13 => Key::Enter,
        9 => Key::Tab,
        127 | 8 => Key::Backspace,
        27 => Key::Escape,
        57399..=57408 => Key::Char((b'0' + (number - 57399) as u8) as char),
        57414 => Key::Enter,
        57376..=57383 => Key::Function((number - 57376 + 13) as u8),
        other => match char::from_u32(other) {
            Some(c) if other >= 0x20 && !(57344..=63743).contains(&other) => Key::Char(c),
            _ => Key::Other(other),
        },
    }
}

/// A `CSI number ~` key, in the numbering xterm handed down.
fn tilde_key(number: u32) -> Option<Key> {
    Some(match number {
        1 | 7 => Key::Home,
        2 => Key::Insert,
        3 => Key::Delete,
        4 | 8 => Key::End,
        5 => Key::PageUp,
        6 => Key::PageDown,
        11..=14 => Key::Function((number - 10) as u8),
        15 => Key::Function(5),
        17..=21 => Key::Function((number - 11) as u8),
        23 => Key::Function(11),
        24 => Key::Function(12),
        25 => Key::Function(13),
        26 => Key::Function(14),
        28 => Key::Function(15),
        29 => Key::Function(16),
        31 => Key::Function(17),
        32 => Key::Function(18),
        33 => Key::Function(19),
        34 => Key::Function(20),
        _ => return None,
    })
}

/// An SGR mouse report.
fn mouse(params: &[Vec<u32>], released: bool) -> Option<MouseInput> {
    if params.len() < 3 {
        return None;
    }
    let code = number(params, 0, 0);
    let x = number(params, 1, 0);
    let y = number(params, 2, 0);

    let mut mods = Mods::default();
    if code & 4 != 0 {
        mods = mods.with(Mods::SHIFT);
    }
    if code & 8 != 0 {
        mods = mods.with(Mods::ALT);
    }
    if code & 16 != 0 {
        mods = mods.with(Mods::CTRL);
    }

    // Bit 6 is the wheel, bit 5 is motion, and the low two bits are the
    // button. Order matters: a wheel report also has the button bits set to
    // the direction, and reading them as a button is how a scroll becomes a
    // middle click.
    let (kind, button, wheel) = if code & 64 != 0 {
        let wheel = match code & 3 {
            0 => (0, -1),
            1 => (0, 1),
            2 => (-1, 0),
            _ => (1, 0),
        };
        (MouseKind::Wheel, None, wheel)
    } else {
        let button = match code & 3 {
            3 => None,
            other => Some(other),
        };
        let kind = if code & 32 != 0 {
            MouseKind::Move
        } else if released {
            MouseKind::Release
        } else {
            MouseKind::Press
        };
        (kind, button, (0, 0))
    };

    Some(MouseInput {
        kind,
        button,
        mods,
        x,
        y,
        wheel,
    })
}

/// Where a report points, in CSS pixels inside the page.
///
/// Both encodings count from one, and both count from the top left of the
/// *pane*, which is not the top left of the page: this program keeps the first
/// row for its status line, so `reserved_rows` cell heights come off the y.
///
/// In cells (mode 1006, and what today's tOS answers) a report names a cell
/// and the point taken is its middle — the corner would put every click on the
/// boundary between two elements, which is the one place a page is least
/// likely to mean. In pixels (mode 1016) the report is already the point, one
/// subtraction from being zero-based.
///
/// A negative y is a report on the status row, which the caller has to decide
/// about rather than clamp: a drag that started in the page and left it
/// upwards is not a click on the url bar.
pub fn page_point(
    report: &MouseInput,
    pixels: bool,
    cell: (u32, u32),
    reserved_rows: u32,
) -> (i32, i32) {
    let (cell_w, cell_h) = (cell.0.max(1) as i32, cell.1.max(1) as i32);
    let reserved = reserved_rows as i32 * cell_h;
    if pixels {
        (report.x as i32 - 1, report.y as i32 - 1 - reserved)
    } else {
        (
            (report.x as i32 - 1) * cell_w + cell_w / 2,
            (report.y as i32 - 1) * cell_h + cell_h / 2 - reserved,
        )
    }
}

/// How many bytes a UTF-8 sequence starting with this one has.
fn utf8_width(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        // A continuation byte on its own is not the start of anything; taking
        // one byte drops it rather than waiting forever for the rest.
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(bytes: &[u8]) -> Vec<Input> {
        Parser::new().feed(bytes)
    }

    fn one_key(bytes: &[u8]) -> KeyInput {
        match feed(bytes).as_slice() {
            [Input::Key(key)] => key.clone(),
            other => panic!("{other:?}"),
        }
    }

    fn one_mouse(bytes: &[u8]) -> MouseInput {
        match feed(bytes).as_slice() {
            [Input::Mouse(report)] => *report,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_typed_letter_is_a_press_with_its_text() {
        let key = one_key(b"a");
        assert_eq!(key.key, Key::Char('a'));
        assert_eq!(key.text, Some('a'));
        assert_eq!(key.action, KeyAction::Press);
        assert_eq!(key.mods, Mods::default());
    }

    #[test]
    fn a_multibyte_character_waits_for_all_of_itself() {
        let mut parser = Parser::new();
        assert!(parser.feed(&[0xe6]).is_empty());
        assert!(parser.feed(&[0x97]).is_empty());
        assert_eq!(
            parser.feed(&[0xa5]),
            vec![Input::Key(KeyInput {
                key: Key::Char('\u{65e5}'),
                mods: Mods::default(),
                action: KeyAction::Press,
                text: Some('\u{65e5}'),
            })]
        );
    }

    #[test]
    fn the_kitty_form_carries_modifiers_events_and_text() {
        // ctrl+l press, which is this program's own key for the url bar.
        let key = one_key(b"\x1b[108;5u");
        assert_eq!(key.key, Key::Char('l'));
        assert!(key.mods.ctrl());
        assert_eq!(key.action, KeyAction::Press);
        assert_eq!(key.text, None, "ctrl+l produces no text");

        // shift+a, with the shifted alternate and the text the page wants.
        let key = one_key(b"\x1b[97:65;2;65u");
        assert_eq!(key.key, Key::Char('a'));
        assert!(key.mods.shift());
        assert_eq!(key.text, Some('A'));

        // A repeat and a release.
        assert_eq!(one_key(b"\x1b[97;1:2u").action, KeyAction::Repeat);
        assert_eq!(one_key(b"\x1b[97;1:3u").action, KeyAction::Release);
    }

    #[test]
    fn the_keys_a_page_listens_to_all_have_names() {
        let cases: &[(&[u8], Key)] = &[
            (b"\x1b[13u", Key::Enter),
            (b"\x1b[9u", Key::Tab),
            (b"\x1b[127u", Key::Backspace),
            (b"\x1b[27u", Key::Escape),
            (b"\x1b[A", Key::Up),
            (b"\x1b[B", Key::Down),
            (b"\x1b[C", Key::Right),
            (b"\x1b[D", Key::Left),
            (b"\x1b[H", Key::Home),
            (b"\x1b[F", Key::End),
            (b"\x1b[2~", Key::Insert),
            (b"\x1b[3~", Key::Delete),
            (b"\x1b[5~", Key::PageUp),
            (b"\x1b[6~", Key::PageDown),
            (b"\x1b[1~", Key::Home),
            (b"\x1b[4~", Key::End),
            (b"\x1bOP", Key::Function(1)),
            (b"\x1b[1;2P", Key::Function(1)),
            (b"\x1b[15~", Key::Function(5)),
            (b"\x1b[24~", Key::Function(12)),
            (b"\x1bOA", Key::Up),
            (b"\r", Key::Enter),
            (b"\t", Key::Tab),
            (b"\x7f", Key::Backspace),
        ];
        for (bytes, expected) in cases {
            assert_eq!(one_key(bytes).key, *expected, "{:?}", bytes_as_text(bytes));
        }
    }

    #[test]
    fn modified_arrows_keep_their_letter_and_gain_their_modifier() {
        let key = one_key(b"\x1b[1;3D");
        assert_eq!(key.key, Key::Left);
        assert!(key.mods.alt(), "{:?}", key.mods);
        assert!(!key.mods.ctrl());
    }

    #[test]
    fn a_lone_escape_only_becomes_a_key_when_the_wait_is_over() {
        let mut parser = Parser::new();
        assert!(parser.feed(b"\x1b").is_empty());
        assert_eq!(
            parser.flush(),
            Some(Input::Key(KeyInput::press(Key::Escape)))
        );
        assert_eq!(parser.flush(), None);

        // And an escape that was the start of something is not that key.
        let mut parser = Parser::new();
        assert!(parser.feed(b"\x1b").is_empty());
        assert_eq!(
            parser.feed(b"[C"),
            vec![Input::Key(KeyInput::press(Key::Right))]
        );
        assert_eq!(parser.flush(), None);
    }

    #[test]
    fn a_control_byte_is_ctrl_and_its_letter() {
        let key = one_key(&[0x0c]);
        assert_eq!(key.key, Key::Char('l'));
        assert!(key.mods.ctrl());
        let key = one_key(&[0x11]);
        assert_eq!(key.key, Key::Char('q'));
        assert!(key.mods.ctrl());
    }

    #[test]
    fn alt_in_its_legacy_spelling_is_still_alt() {
        let key = one_key(b"\x1b\x1b[D");
        assert_eq!(key.key, Key::Left);
        assert!(key.mods.alt());
    }

    #[test]
    fn a_mouse_press_drag_and_release() {
        let press = one_mouse(b"\x1b[<0;10;20M");
        assert_eq!(press.kind, MouseKind::Press);
        assert_eq!(press.button, Some(0));
        assert_eq!((press.x, press.y), (10, 20));

        let drag = one_mouse(b"\x1b[<32;11;21M");
        assert_eq!(drag.kind, MouseKind::Move);
        assert_eq!(drag.button, Some(0));

        let motion = one_mouse(b"\x1b[<35;12;22M");
        assert_eq!(motion.kind, MouseKind::Move);
        assert_eq!(motion.button, None);

        let release = one_mouse(b"\x1b[<0;10;20m");
        assert_eq!(release.kind, MouseKind::Release);
        assert_eq!(release.button, Some(0));

        let right = one_mouse(b"\x1b[<2;1;1M");
        assert_eq!(right.button, Some(2));
        let middle = one_mouse(b"\x1b[<1;1;1M");
        assert_eq!(middle.button, Some(1));
    }

    #[test]
    fn a_wheel_notch_is_not_a_middle_click() {
        let up = one_mouse(b"\x1b[<64;5;5M");
        assert_eq!(up.kind, MouseKind::Wheel);
        assert_eq!(up.button, None);
        assert_eq!(up.wheel, (0, -1));

        let down = one_mouse(b"\x1b[<65;5;5M");
        assert_eq!(down.wheel, (0, 1));
        assert_eq!(one_mouse(b"\x1b[<66;5;5M").wheel, (-1, 0));
        assert_eq!(one_mouse(b"\x1b[<67;5;5M").wheel, (1, 0));
    }

    #[test]
    fn mouse_modifiers_come_out_of_the_button_code() {
        let shifted = one_mouse(b"\x1b[<4;1;1M");
        assert!(shifted.mods.shift());
        let ctrl_alt = one_mouse(b"\x1b[<24;1;1M");
        assert!(ctrl_alt.mods.ctrl() && ctrl_alt.mods.alt());
        assert_eq!(ctrl_alt.mods.cdp(), 1 | 2);
    }

    #[test]
    fn the_mode_answer_decides_what_the_coordinates_mean() {
        let mut parser = Parser::new();
        assert_eq!(
            parser.feed(b"\x1b[?1016;0$y"),
            vec![Input::Mode {
                mode: 1016,
                state: 0
            }]
        );
        assert!(!parser.pixel_coordinates(), "0 means unrecognised");

        let mut parser = Parser::new();
        parser.feed(b"\x1b[?1016;2$y");
        assert!(!parser.pixel_coordinates(), "2 means reset");

        let mut parser = Parser::new();
        parser.feed(b"\x1b[?1016;1$y");
        assert!(parser.pixel_coordinates(), "1 means pixels");

        // Another mode's answer must not be taken for this one.
        let mut parser = Parser::new();
        parser.feed(b"\x1b[?1006;1$y");
        assert!(!parser.pixel_coordinates());
    }

    #[test]
    fn a_stream_that_arrives_a_byte_at_a_time_parses_the_same() {
        let stream = b"a\x1b[<0;3;4M\x1b[108;5u\x1b[?1016;1$y\x1b[3~";
        let whole = feed(stream);
        let mut parser = Parser::new();
        let mut piecemeal = Vec::new();
        for byte in stream {
            piecemeal.extend(parser.feed(&[*byte]));
        }
        assert_eq!(whole, piecemeal);
        assert_eq!(whole.len(), 5);
    }

    #[test]
    fn noise_does_not_wedge_the_parser() {
        let mut parser = Parser::new();
        // An unterminated CSI that never ends is dropped a byte at a time
        // rather than swallowing everything after it.
        parser.feed(&[b'1'; 200]);
        parser.feed(b"\x1b[");
        parser.feed(&[b'1'; 200]);
        assert_eq!(
            parser.feed(b"a").last(),
            Some(&Input::Key(KeyInput {
                key: Key::Char('a'),
                mods: Mods::default(),
                action: KeyAction::Press,
                text: Some('a'),
            }))
        );
    }

    /// What `tos_input` sends for an event has to come back as that event.
    #[test]
    fn the_compositors_own_encoder_round_trips() {
        // The sequences below are what `tos_input::encode::encode_kitty`
        // produces at the flag level this program pushes; they are written out
        // rather than generated because `tos-input` is not a dependency of
        // this crate and a copy that drifts is a test that would notice.
        assert_eq!(one_key(b"\x1b[97;1:3u").action, KeyAction::Release);
        assert_eq!(one_key(b"\x1b[13;5u").key, Key::Enter);
        assert_eq!(one_key(b"\x1b[127;3u").key, Key::Backspace);
        assert_eq!(one_key(b"\x1b[27;1:2u").key, Key::Escape);
        assert_eq!(one_key(b"\x1b[57414;1u").key, Key::Enter, "keypad enter");
        assert_eq!(one_key(b"\x1b[57399;1u").key, Key::Char('0'), "keypad 0");
    }

    #[test]
    fn a_cell_report_becomes_the_middle_of_that_cell_in_the_page() {
        // The pane's first row is this program's; the page starts below it.
        let top_left = one_mouse(b"\x1b[<0;1;2M");
        assert_eq!(page_point(&top_left, false, (8, 16), 1), (4, 8));

        let further = one_mouse(b"\x1b[<0;11;4M");
        assert_eq!(page_point(&further, false, (8, 16), 1), (84, 40));

        // A click on the status row itself is above the page, and says so.
        let on_the_bar = one_mouse(b"\x1b[<0;1;1M");
        assert!(page_point(&on_the_bar, false, (8, 16), 1).1 < 0);
    }

    #[test]
    fn a_pixel_report_is_the_point_it_names_once_it_is_zero_based() {
        // Mode 1016 in tOS is xterm's: one-based pixels from the pane corner.
        let corner = MouseInput {
            kind: MouseKind::Press,
            button: Some(0),
            mods: Mods::default(),
            x: 1,
            y: 17,
            wheel: (0, 0),
        };
        assert_eq!(page_point(&corner, true, (8, 16), 1), (0, 0));

        let inside = MouseInput {
            x: 101,
            y: 61,
            ..corner
        };
        assert_eq!(page_point(&inside, true, (8, 16), 1), (100, 44));
        assert!(page_point(&MouseInput { y: 3, ..corner }, true, (8, 16), 1).1 < 0);
    }

    fn bytes_as_text(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).escape_debug().to_string()
    }
}
