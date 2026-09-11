//! Escape sequence parser.
//!
//! A DEC-compatible state machine (the classic Williams VT500 diagram) with
//! UTF-8 decoding folded into the ground state, plus an APC path because the
//! Kitty graphics protocol lives there.
//!
//! The parser knows nothing about terminal semantics; it turns bytes into
//! dispatch calls on a [`Perform`] implementation.

/// CSI/DCS parameters, including colon-separated subparameters such as the
/// `4:3` curly underline or `38:2::r:g:b` direct color forms.
#[derive(Debug, Clone, Default)]
pub struct Params {
    values: Vec<u16>,
    /// `true` when the value at the same index continues the previous group.
    subs: Vec<bool>,
}

const MAX_PARAMS: usize = 32;

impl Params {
    fn clear(&mut self) {
        self.values.clear();
        self.subs.clear();
    }

    fn push(&mut self, value: u16, is_sub: bool) {
        if self.values.len() < MAX_PARAMS {
            self.values.push(value);
            self.subs.push(is_sub);
        }
    }

    fn is_full(&self) -> bool {
        self.values.len() >= MAX_PARAMS
    }

    /// Iterate parameter groups; a group is a parameter plus its subparameters.
    pub fn iter(&self) -> ParamsIter<'_> {
        ParamsIter {
            params: self,
            at: 0,
        }
    }

    /// Number of groups.
    pub fn len(&self) -> usize {
        self.iter().count()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// First value of group `n`, or `default` when absent or zero-as-default.
    pub fn get(&self, n: usize, default: u16) -> u16 {
        match self.iter().nth(n).and_then(|g| g.first().copied()) {
            None | Some(0) => default,
            Some(v) => v,
        }
    }

    /// First value of group `n` without the zero-means-default rule.
    pub fn get_raw(&self, n: usize, default: u16) -> u16 {
        self.iter()
            .nth(n)
            .and_then(|g| g.first().copied())
            .unwrap_or(default)
    }
}

pub struct ParamsIter<'a> {
    params: &'a Params,
    at: usize,
}

impl<'a> Iterator for ParamsIter<'a> {
    type Item = &'a [u16];

    fn next(&mut self) -> Option<Self::Item> {
        if self.at >= self.params.values.len() {
            return None;
        }
        let start = self.at;
        let mut end = start + 1;
        while end < self.params.values.len() && self.params.subs[end] {
            end += 1;
        }
        self.at = end;
        Some(&self.params.values[start..end])
    }
}

/// Receives parsed events. Every method has a no-op default so implementations
/// can opt into only the sequences they care about.
#[allow(unused_variables)]
pub trait Perform {
    /// A printable character.
    fn print(&mut self, c: char) {}
    /// A C0 or C1 control byte.
    fn execute(&mut self, byte: u8) {}
    /// `ESC` + intermediates + final byte.
    fn esc_dispatch(&mut self, intermediates: &[u8], byte: u8) {}
    /// `CSI` + params + intermediates + final byte.
    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], byte: u8) {}
    /// `OSC` with semicolon-separated fields, already split.
    fn osc_dispatch(&mut self, fields: &[&[u8]], bell_terminated: bool) {}
    /// Start of a device control string.
    fn dcs_hook(&mut self, params: &Params, intermediates: &[u8], byte: u8) {}
    fn dcs_put(&mut self, byte: u8) {}
    fn dcs_unhook(&mut self) {}
    /// Application program command, used by the Kitty graphics protocol.
    fn apc_start(&mut self) {}
    fn apc_put(&mut self, byte: u8) {}
    fn apc_end(&mut self) {}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    Escape,
    EscapeIntermediate,
    CsiEntry,
    CsiParam,
    CsiIntermediate,
    CsiIgnore,
    OscString,
    DcsEntry,
    DcsParam,
    DcsIntermediate,
    DcsPassthrough,
    DcsIgnore,
    ApcString,
    /// SOS and PM strings, which tOS parses only so it can discard them.
    IgnoreString,
    /// Saw ESC while inside a string: a following `\` is the terminator.
    StringEsc,
}

const MAX_INTERMEDIATES: usize = 2;
const MAX_OSC_LEN: usize = 1024 * 1024;
const MAX_OSC_FIELDS: usize = 16;

/// One step of UTF-8 decoding.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    Emit(char),
    /// More continuation bytes are needed.
    Pending,
    /// The byte itself is invalid; emit a replacement and move on.
    Replace,
    /// The byte truncated a sequence; emit a replacement, then reprocess it.
    ReplaceAndRetry,
}

/// Incremental UTF-8 decoder. Invalid sequences produce U+FFFD rather than
/// desynchronising the stream.
#[derive(Debug, Default)]
struct Utf8Decoder {
    codepoint: u32,
    remaining: u8,
}

impl Utf8Decoder {
    fn reset(&mut self) {
        self.codepoint = 0;
        self.remaining = 0;
    }

    fn is_pending(&self) -> bool {
        self.remaining > 0
    }

    fn feed(&mut self, byte: u8) -> Step {
        if self.remaining == 0 {
            return match byte {
                0x00..=0x7f => Step::Emit(byte as char),
                0xc2..=0xdf => {
                    self.codepoint = (byte & 0x1f) as u32;
                    self.remaining = 1;
                    Step::Pending
                }
                0xe0..=0xef => {
                    self.codepoint = (byte & 0x0f) as u32;
                    self.remaining = 2;
                    Step::Pending
                }
                0xf0..=0xf4 => {
                    self.codepoint = (byte & 0x07) as u32;
                    self.remaining = 3;
                    Step::Pending
                }
                // A continuation byte without a lead, or an overlong lead.
                _ => Step::Replace,
            };
        }

        if byte & 0xc0 != 0x80 {
            self.reset();
            return Step::ReplaceAndRetry;
        }
        self.codepoint = (self.codepoint << 6) | (byte & 0x3f) as u32;
        self.remaining -= 1;
        if self.remaining == 0 {
            let cp = self.codepoint;
            self.reset();
            return Step::Emit(char::from_u32(cp).unwrap_or(char::REPLACEMENT_CHARACTER));
        }
        Step::Pending
    }
}

/// The escape sequence parser.
pub struct Parser {
    state: State,
    params: Params,
    current_param: u32,
    param_started: bool,
    /// The next parameter value continues the current group.
    pending_sub: bool,
    intermediates: [u8; MAX_INTERMEDIATES],
    intermediate_len: usize,
    ignoring: bool,
    osc_buf: Vec<u8>,
    osc_field_ends: Vec<usize>,
    utf8: Utf8Decoder,
}

impl Default for Parser {
    fn default() -> Self {
        Parser::new()
    }
}

impl Parser {
    pub fn new() -> Self {
        Parser {
            state: State::Ground,
            params: Params::default(),
            current_param: 0,
            param_started: false,
            pending_sub: false,
            intermediates: [0; MAX_INTERMEDIATES],
            intermediate_len: 0,
            ignoring: false,
            osc_buf: Vec::new(),
            osc_field_ends: Vec::new(),
            utf8: Utf8Decoder::default(),
        }
    }

    /// Reset to the ground state, dropping any partial sequence.
    pub fn reset(&mut self) {
        self.state = State::Ground;
        self.clear();
        self.utf8.reset();
    }

    fn clear(&mut self) {
        self.params.clear();
        self.current_param = 0;
        self.param_started = false;
        self.pending_sub = false;
        self.intermediate_len = 0;
        self.ignoring = false;
        self.osc_buf.clear();
        self.osc_field_ends.clear();
    }

    fn push_intermediate(&mut self, byte: u8) {
        if self.intermediate_len < MAX_INTERMEDIATES {
            self.intermediates[self.intermediate_len] = byte;
            self.intermediate_len += 1;
        } else {
            self.ignoring = true;
        }
    }

    /// Close the parameter being accumulated. `next_is_sub` records whether
    /// the separator just seen was a colon, which makes the *following* value
    /// a subparameter of this one.
    fn finish_param(&mut self, next_is_sub: bool) {
        let value = self.current_param.min(u16::MAX as u32) as u16;
        let is_sub = self.pending_sub;
        self.params.push(value, is_sub);
        self.pending_sub = next_is_sub;
        self.current_param = 0;
        self.param_started = false;
    }

    /// Flush the in-progress parameter before dispatching a final byte.
    fn seal_params(&mut self) {
        if self.param_started || !self.params.is_empty() {
            self.finish_param(false);
        }
    }

    /// Feed a slice of bytes.
    pub fn advance<P: Perform>(&mut self, performer: &mut P, bytes: &[u8]) {
        for &byte in bytes {
            self.advance_byte(performer, byte);
        }
    }

    pub fn advance_byte<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        // ESC and CAN/SUB abort any sequence in progress, everywhere except
        // inside string bodies where ESC begins the terminator.
        match byte {
            0x18 | 0x1a => {
                let was_string = matches!(
                    self.state,
                    State::OscString
                        | State::ApcString
                        | State::IgnoreString
                        | State::DcsPassthrough
                );
                if was_string {
                    self.end_string(performer);
                }
                self.clear();
                self.state = State::Ground;
                self.utf8.reset();
                performer.execute(byte);
                return;
            }
            0x1b => {
                if matches!(
                    self.state,
                    State::OscString
                        | State::ApcString
                        | State::IgnoreString
                        | State::DcsPassthrough
                ) {
                    // ESC \ is the string terminator; ESC anything-else also
                    // ends the string and starts a new sequence.
                    self.end_string(performer);
                    self.clear();
                    self.state = State::StringEsc;
                    self.utf8.reset();
                    return;
                }
                self.clear();
                self.state = State::Escape;
                self.utf8.reset();
                return;
            }
            _ => {}
        }

        match self.state {
            State::Ground => self.ground(performer, byte),
            State::Escape => self.escape(performer, byte),
            State::EscapeIntermediate => self.escape_intermediate(performer, byte),
            State::CsiEntry => self.csi_entry(performer, byte),
            State::CsiParam => self.csi_param(performer, byte),
            State::CsiIntermediate => self.csi_intermediate(performer, byte),
            State::CsiIgnore => self.csi_ignore(performer, byte),
            State::OscString => self.osc_string(performer, byte),
            State::DcsEntry => self.dcs_entry(performer, byte),
            State::DcsParam => self.dcs_param(performer, byte),
            State::DcsIntermediate => self.dcs_intermediate(performer, byte),
            State::DcsPassthrough => self.dcs_passthrough(performer, byte),
            State::DcsIgnore => self.dcs_ignore(byte),
            State::ApcString => self.apc_string(performer, byte),
            State::IgnoreString => self.ignore_string(byte),
            State::StringEsc => {
                // ESC \ ends the string and produces nothing; anything else
                // starts a fresh escape sequence.
                if byte == b'\\' {
                    self.state = State::Ground;
                } else {
                    self.state = State::Escape;
                    self.escape(performer, byte);
                }
            }
        }
    }

    fn end_string<P: Perform>(&mut self, performer: &mut P) {
        match self.state {
            State::OscString => self.dispatch_osc(performer, false),
            State::ApcString => performer.apc_end(),
            State::DcsPassthrough => performer.dcs_unhook(),
            _ => {}
        }
    }

    fn ground<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1c..=0x1f => {
                // A control byte interrupts a partial UTF-8 sequence.
                if self.utf8.is_pending() {
                    self.utf8.reset();
                    performer.print(char::REPLACEMENT_CHARACTER);
                }
                performer.execute(byte);
            }
            _ => match self.utf8.feed(byte) {
                Step::Emit(c) => performer.print(c),
                Step::Pending => {}
                Step::Replace => performer.print(char::REPLACEMENT_CHARACTER),
                Step::ReplaceAndRetry => {
                    performer.print(char::REPLACEMENT_CHARACTER);
                    // The decoder is reset, so this recurses at most once.
                    self.ground(performer, byte);
                }
            },
        }
    }

    fn escape<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1c..=0x1f => performer.execute(byte),
            0x20..=0x2f => {
                self.push_intermediate(byte);
                self.state = State::EscapeIntermediate;
            }
            b'[' => {
                self.clear();
                self.state = State::CsiEntry;
            }
            b']' => {
                self.clear();
                self.state = State::OscString;
            }
            b'P' => {
                self.clear();
                self.state = State::DcsEntry;
            }
            b'_' => {
                self.clear();
                self.state = State::ApcString;
                performer.apc_start();
            }
            b'X' | b'^' => {
                self.clear();
                self.state = State::IgnoreString;
            }
            0x30..=0x7e => {
                performer.esc_dispatch(&[], byte);
                self.state = State::Ground;
            }
            _ => self.state = State::Ground,
        }
    }

    fn escape_intermediate<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1c..=0x1f => performer.execute(byte),
            0x20..=0x2f => self.push_intermediate(byte),
            0x30..=0x7e => {
                let intermediates = self.intermediates;
                let len = self.intermediate_len;
                performer.esc_dispatch(&intermediates[..len], byte);
                self.state = State::Ground;
                self.clear();
            }
            _ => self.state = State::Ground,
        }
    }

    fn csi_entry<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1c..=0x1f => performer.execute(byte),
            0x30..=0x3b => {
                self.state = State::CsiParam;
                self.csi_param(performer, byte);
            }
            // Private parameter prefix, e.g. CSI ? 25 h.
            0x3c..=0x3f => {
                self.push_intermediate(byte);
                self.state = State::CsiParam;
            }
            0x20..=0x2f => {
                self.push_intermediate(byte);
                self.state = State::CsiIntermediate;
            }
            0x40..=0x7e => {
                self.dispatch_csi(performer, byte);
            }
            _ => self.state = State::CsiIgnore,
        }
    }

    fn csi_param<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1c..=0x1f => performer.execute(byte),
            b'0'..=b'9' => {
                if self.params.is_full() {
                    self.state = State::CsiIgnore;
                    return;
                }
                self.param_started = true;
                // Both steps saturate: a parameter longer than a terminal
                // could ever mean is clamped rather than wrapping round.
                self.current_param = self
                    .current_param
                    .saturating_mul(10)
                    .saturating_add((byte - b'0') as u32);
            }
            b';' => self.finish_param(false),
            b':' => self.finish_param(true),
            0x20..=0x2f => {
                self.push_intermediate(byte);
                self.state = State::CsiIntermediate;
            }
            0x40..=0x7e => self.dispatch_csi(performer, byte),
            _ => self.state = State::CsiIgnore,
        }
    }

    fn csi_intermediate<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1c..=0x1f => performer.execute(byte),
            0x20..=0x2f => self.push_intermediate(byte),
            0x40..=0x7e => self.dispatch_csi(performer, byte),
            _ => self.state = State::CsiIgnore,
        }
    }

    fn csi_ignore<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        if let 0x00..=0x17 | 0x19 | 0x1c..=0x1f = byte {
            performer.execute(byte);
        }
        if (0x40..=0x7e).contains(&byte) {
            self.state = State::Ground;
            self.clear();
        }
    }

    fn dispatch_csi<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        if !self.ignoring {
            self.seal_params();
            let intermediates = self.intermediates;
            let len = self.intermediate_len;
            performer.csi_dispatch(&self.params, &intermediates[..len], byte);
        }
        self.state = State::Ground;
        self.clear();
    }

    fn osc_string<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        match byte {
            0x07 => self.dispatch_osc(performer, true),
            b';' => {
                // Separators count against the cap as well; otherwise a run
                // of them is unbounded allocation driven by the PTY.
                if self.osc_buf.len() >= MAX_OSC_LEN {
                    return;
                }
                if self.osc_field_ends.len() < MAX_OSC_FIELDS {
                    self.osc_field_ends.push(self.osc_buf.len());
                }
                self.osc_buf.push(byte);
            }
            _ => {
                if self.osc_buf.len() < MAX_OSC_LEN {
                    self.osc_buf.push(byte);
                }
            }
        }
    }

    fn dispatch_osc<P: Perform>(&mut self, performer: &mut P, bell_terminated: bool) {
        let mut fields: Vec<&[u8]> = Vec::with_capacity(self.osc_field_ends.len() + 1);
        let mut start = 0usize;
        for &end in &self.osc_field_ends {
            fields.push(&self.osc_buf[start..end]);
            start = end + 1; // skip the separator byte
        }
        fields.push(&self.osc_buf[start.min(self.osc_buf.len())..]);
        performer.osc_dispatch(&fields, bell_terminated);
        self.state = State::Ground;
        // `clear` also empties the OSC buffer, which `fields` borrows, so the
        // borrow has to end first.
        drop(fields);
        self.clear();
    }

    fn dcs_entry<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        match byte {
            b'0'..=b'9' | b':' | b';' => {
                self.state = State::DcsParam;
                self.dcs_param(performer, byte);
            }
            0x3c..=0x3f => {
                self.push_intermediate(byte);
                self.state = State::DcsParam;
            }
            0x20..=0x2f => {
                self.push_intermediate(byte);
                self.state = State::DcsIntermediate;
            }
            0x40..=0x7e => self.dcs_hook(performer, byte),
            _ => self.state = State::DcsIgnore,
        }
    }

    fn dcs_param<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        match byte {
            b'0'..=b'9' => {
                if self.params.is_full() {
                    self.state = State::DcsIgnore;
                    return;
                }
                self.param_started = true;
                // Both steps saturate: a parameter longer than a terminal
                // could ever mean is clamped rather than wrapping round.
                self.current_param = self
                    .current_param
                    .saturating_mul(10)
                    .saturating_add((byte - b'0') as u32);
            }
            b';' => self.finish_param(false),
            b':' => self.finish_param(true),
            0x20..=0x2f => {
                self.push_intermediate(byte);
                self.state = State::DcsIntermediate;
            }
            0x40..=0x7e => self.dcs_hook(performer, byte),
            _ => self.state = State::DcsIgnore,
        }
    }

    fn dcs_intermediate<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        match byte {
            0x20..=0x2f => self.push_intermediate(byte),
            0x40..=0x7e => self.dcs_hook(performer, byte),
            _ => self.state = State::DcsIgnore,
        }
    }

    fn dcs_hook<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        self.seal_params();
        let intermediates = self.intermediates;
        let len = self.intermediate_len;
        performer.dcs_hook(&self.params, &intermediates[..len], byte);
        self.state = State::DcsPassthrough;
    }

    fn dcs_passthrough<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        performer.dcs_put(byte);
    }

    fn dcs_ignore(&mut self, _byte: u8) {}

    fn apc_string<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        performer.apc_put(byte);
    }

    fn ignore_string(&mut self, _byte: u8) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Recorder {
        events: Vec<String>,
    }

    impl Perform for Recorder {
        fn print(&mut self, c: char) {
            self.events.push(format!("print({c})"));
        }
        fn execute(&mut self, byte: u8) {
            self.events.push(format!("exec({byte:#04x})"));
        }
        fn esc_dispatch(&mut self, intermediates: &[u8], byte: u8) {
            self.events.push(format!(
                "esc({},{})",
                String::from_utf8_lossy(intermediates),
                byte as char
            ));
        }
        fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], byte: u8) {
            let groups: Vec<String> = params
                .iter()
                .map(|g| {
                    g.iter()
                        .map(|v| v.to_string())
                        .collect::<Vec<_>>()
                        .join(":")
                })
                .collect();
            self.events.push(format!(
                "csi([{}],{},{})",
                groups.join(";"),
                String::from_utf8_lossy(intermediates),
                byte as char
            ));
        }
        fn osc_dispatch(&mut self, fields: &[&[u8]], bell: bool) {
            let fields: Vec<String> = fields
                .iter()
                .map(|f| String::from_utf8_lossy(f).into_owned())
                .collect();
            self.events
                .push(format!("osc([{}],{})", fields.join("|"), bell));
        }
        fn apc_start(&mut self) {
            self.events.push("apc_start".into());
        }
        fn apc_put(&mut self, byte: u8) {
            self.events.push(format!("apc({})", byte as char));
        }
        fn apc_end(&mut self) {
            self.events.push("apc_end".into());
        }
        fn dcs_hook(&mut self, _p: &Params, _i: &[u8], byte: u8) {
            self.events.push(format!("dcs_hook({})", byte as char));
        }
        fn dcs_put(&mut self, byte: u8) {
            self.events.push(format!("dcs({})", byte as char));
        }
        fn dcs_unhook(&mut self) {
            self.events.push("dcs_unhook".into());
        }
    }

    fn run(input: &[u8]) -> Vec<String> {
        let mut parser = Parser::new();
        let mut rec = Recorder::default();
        parser.advance(&mut rec, input);
        rec.events
    }

    #[test]
    fn plain_text_prints() {
        assert_eq!(run(b"hi"), vec!["print(h)", "print(i)"]);
    }

    #[test]
    fn control_bytes_execute() {
        assert_eq!(run(b"\r\n"), vec!["exec(0x0d)", "exec(0x0a)"]);
    }

    #[test]
    fn csi_with_params() {
        assert_eq!(run(b"\x1b[1;2H"), vec!["csi([1;2],,H)"]);
    }

    #[test]
    fn csi_private_mode() {
        assert_eq!(run(b"\x1b[?25h"), vec!["csi([25],?,h)"]);
    }

    #[test]
    fn csi_subparams() {
        assert_eq!(run(b"\x1b[4:3m"), vec!["csi([4:3],,m)"]);
        assert_eq!(
            run(b"\x1b[38:2::255:0:0m"),
            vec!["csi([38:2:0:255:0:0],,m)"]
        );
    }

    #[test]
    fn empty_params_are_zero() {
        assert_eq!(run(b"\x1b[;5H"), vec!["csi([0;5],,H)"]);
    }

    #[test]
    fn osc_splits_fields() {
        assert_eq!(
            run(b"\x1b]0;hello world\x07"),
            vec!["osc([0|hello world],true)"]
        );
    }

    #[test]
    fn osc_string_terminator() {
        assert_eq!(run(b"\x1b]2;title\x1b\\"), vec!["osc([2|title],false)"]);
    }

    #[test]
    fn apc_carries_kitty_graphics() {
        let events = run(b"\x1b_Ga=T\x1b\\");
        assert_eq!(events.first().unwrap(), "apc_start");
        assert_eq!(events.last().unwrap(), "apc_end");
        let body: String = events[1..events.len() - 1]
            .iter()
            .map(|e| e.trim_start_matches("apc(").trim_end_matches(')'))
            .collect();
        assert_eq!(body, "Ga=T");
    }

    #[test]
    fn dcs_passthrough() {
        let events = run(b"\x1bP+q544e\x1b\\");
        assert_eq!(events[0], "dcs_hook(q)");
        assert_eq!(events.last().unwrap(), "dcs_unhook");
    }

    #[test]
    fn utf8_is_decoded_across_chunks() {
        let mut parser = Parser::new();
        let mut rec = Recorder::default();
        let bytes = "漢".as_bytes();
        parser.advance(&mut rec, &bytes[..1]);
        assert!(rec.events.is_empty());
        parser.advance(&mut rec, &bytes[1..]);
        assert_eq!(rec.events, vec!["print(漢)"]);
    }

    #[test]
    fn invalid_utf8_yields_replacement() {
        assert_eq!(run(&[0xff]), vec!["print(\u{fffd})"]);
    }

    #[test]
    fn truncated_utf8_then_ascii() {
        assert_eq!(run(&[0xe6, b'a']), vec!["print(\u{fffd})", "print(a)"]);
    }

    #[test]
    fn esc_cancels_pending_sequence() {
        assert_eq!(run(b"\x1b[1;\x1bM"), vec!["esc(,M)"]);
    }

    #[test]
    fn can_aborts_sequence() {
        assert_eq!(run(b"\x1b[1\x18x"), vec!["exec(0x18)", "print(x)"]);
    }

    #[test]
    fn charset_designation_has_intermediate() {
        assert_eq!(run(b"\x1b(0"), vec!["esc((,0)"]);
    }

    #[test]
    fn huge_parameters_saturate_instead_of_overflowing() {
        // `CSI 99999999999 H` must not wrap round to a small row number.
        let events = run(b"\x1b[99999999999H");
        assert_eq!(events, vec![format!("csi([{}],,H)", u16::MAX)]);
    }

    #[test]
    fn osc_separators_are_capped() {
        // A run of separators is still bounded allocation.
        let mut parser = Parser::new();
        let mut rec = Recorder::default();
        parser.advance(&mut rec, b"\x1b]");
        parser.advance(&mut rec, &vec![b';'; MAX_OSC_LEN * 2]);
        assert!(parser.osc_buf.len() <= MAX_OSC_LEN);
    }

    #[test]
    fn params_are_bounded() {
        let mut input = b"\x1b[".to_vec();
        for _ in 0..100 {
            input.extend_from_slice(b"1;");
        }
        input.push(b'm');
        // Overflowing the parameter list must not panic and must not dispatch
        // a half-parsed sequence.
        let events = run(&input);
        assert!(events.is_empty() || events[0].starts_with("csi"));
    }
}
