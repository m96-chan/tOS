//! Enough JSON for the Chrome DevTools Protocol, and no more.
//!
//! CDP is JSON in both directions, so something has to read and write it, and
//! in this workspace that something is not a crate. What is here is the whole
//! grammar — objects, arrays, strings with escapes and surrogate pairs,
//! numbers, the three literals — because a parser that handled "most" JSON
//! would be a parser that fails on the one page whose title has an emoji in
//! it. What is *not* here is everything a general library adds around the
//! grammar: no derive, no borrowing parser, no number type that is not `f64`.
//!
//! An object keeps its fields in a `Vec` rather than a map. CDP messages have
//! a handful of keys each, a linear scan of five strings beats hashing them,
//! and the order a message was written in survives into the bytes, which
//! makes a test that asserts on a payload readable.

use std::fmt::{self, Write as _};

/// A JSON value.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

/// Where the parse gave up, and on what.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonError {
    pub offset: usize,
    pub what: &'static str,
}

impl fmt::Display for JsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at byte {}", self.what, self.offset)
    }
}

impl Json {
    /// An object from its fields, which is how every command in [`crate::cdp`]
    /// is built.
    pub fn object<S: Into<String>>(fields: Vec<(S, Json)>) -> Json {
        Json::Object(fields.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }

    /// An object with no fields, which is what most CDP commands take.
    pub fn empty() -> Json {
        Json::Object(Vec::new())
    }

    /// A string value.
    pub fn string<S: Into<String>>(value: S) -> Json {
        Json::String(value.into())
    }

    /// A number value.
    pub fn number<N: Into<f64>>(value: N) -> Json {
        Json::Number(value.into())
    }

    /// The value of a field, for an object; `None` for anything else.
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// Follow a chain of field names, for the nested shapes CDP replies have.
    pub fn path(&self, keys: &[&str]) -> Option<&Json> {
        let mut here = self;
        for key in keys {
            here = here.get(key)?;
        }
        Some(here)
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Number(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        self.as_f64().map(|n| n as i64)
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Json]> {
        match self {
            Json::Array(items) => Some(items),
            _ => None,
        }
    }

    /// Read a value out of a complete document.
    pub fn parse(input: &str) -> Result<Json, JsonError> {
        let mut parser = Parser {
            bytes: input.as_bytes(),
            at: 0,
        };
        parser.skip_space();
        let value = parser.value()?;
        parser.skip_space();
        if parser.at != parser.bytes.len() {
            return Err(parser.fail("trailing data"));
        }
        Ok(value)
    }
}

impl fmt::Display for Json {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Json::Null => f.write_str("null"),
            Json::Bool(true) => f.write_str("true"),
            Json::Bool(false) => f.write_str("false"),
            Json::Number(n) => {
                // A number that is not finite has no JSON spelling at all, and
                // the honest stand-in is the one value that means "nothing
                // here" rather than a string a peer would try to add.
                if n.is_finite() {
                    if *n == n.trunc() && n.abs() < 1e15 {
                        write!(f, "{}", *n as i64)
                    } else {
                        write!(f, "{n}")
                    }
                } else {
                    f.write_str("null")
                }
            }
            Json::String(s) => write_string(f, s),
            Json::Array(items) => {
                f.write_str("[")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        f.write_str(",")?;
                    }
                    write!(f, "{item}")?;
                }
                f.write_str("]")
            }
            Json::Object(fields) => {
                f.write_str("{")?;
                for (i, (key, value)) in fields.iter().enumerate() {
                    if i > 0 {
                        f.write_str(",")?;
                    }
                    write_string(f, key)?;
                    write!(f, ":{value}")?;
                }
                f.write_str("}")
            }
        }
    }
}

/// Write a JSON string, escaping what has to be escaped and nothing else.
///
/// Non-ASCII goes out as UTF-8 rather than `\u` escapes: the transport is a
/// WebSocket text frame, which is UTF-8 by definition, so escaping would
/// double the size of every Japanese page title for no reader's benefit.
fn write_string(f: &mut fmt::Formatter<'_>, value: &str) -> fmt::Result {
    f.write_str("\"")?;
    for c in value.chars() {
        match c {
            '"' => f.write_str("\\\"")?,
            '\\' => f.write_str("\\\\")?,
            '\n' => f.write_str("\\n")?,
            '\r' => f.write_str("\\r")?,
            '\t' => f.write_str("\\t")?,
            '\u{8}' => f.write_str("\\b")?,
            '\u{c}' => f.write_str("\\f")?,
            c if (c as u32) < 0x20 => write!(f, "\\u{:04x}", c as u32)?,
            c => f.write_char(c)?,
        }
    }
    f.write_str("\"")
}

struct Parser<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Parser<'_> {
    fn fail(&self, what: &'static str) -> JsonError {
        JsonError {
            offset: self.at,
            what,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn skip_space(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn expect(&mut self, byte: u8, what: &'static str) -> Result<(), JsonError> {
        if self.peek() == Some(byte) {
            self.at += 1;
            Ok(())
        } else {
            Err(self.fail(what))
        }
    }

    fn literal(&mut self, word: &str, value: Json) -> Result<Json, JsonError> {
        if self.bytes[self.at..].starts_with(word.as_bytes()) {
            self.at += word.len();
            Ok(value)
        } else {
            Err(self.fail("unexpected value"))
        }
    }

    fn value(&mut self) -> Result<Json, JsonError> {
        match self.peek() {
            None => Err(self.fail("value expected")),
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => Ok(Json::String(self.string()?)),
            Some(b't') => self.literal("true", Json::Bool(true)),
            Some(b'f') => self.literal("false", Json::Bool(false)),
            Some(b'n') => self.literal("null", Json::Null),
            Some(_) => self.number(),
        }
    }

    fn object(&mut self) -> Result<Json, JsonError> {
        self.at += 1;
        let mut fields = Vec::new();
        self.skip_space();
        if self.peek() == Some(b'}') {
            self.at += 1;
            return Ok(Json::Object(fields));
        }
        loop {
            self.skip_space();
            let key = self.string()?;
            self.skip_space();
            self.expect(b':', "':' expected")?;
            self.skip_space();
            fields.push((key, self.value()?));
            self.skip_space();
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(b'}') => {
                    self.at += 1;
                    return Ok(Json::Object(fields));
                }
                _ => return Err(self.fail("',' or '}' expected")),
            }
        }
    }

    fn array(&mut self) -> Result<Json, JsonError> {
        self.at += 1;
        let mut items = Vec::new();
        self.skip_space();
        if self.peek() == Some(b']') {
            self.at += 1;
            return Ok(Json::Array(items));
        }
        loop {
            self.skip_space();
            items.push(self.value()?);
            self.skip_space();
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(b']') => {
                    self.at += 1;
                    return Ok(Json::Array(items));
                }
                _ => return Err(self.fail("',' or ']' expected")),
            }
        }
    }

    fn string(&mut self) -> Result<String, JsonError> {
        self.expect(b'"', "string expected")?;
        let mut out = String::new();
        loop {
            let byte = self
                .peek()
                .ok_or_else(|| self.fail("unterminated string"))?;
            match byte {
                b'"' => {
                    self.at += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.at += 1;
                    let escape = self
                        .peek()
                        .ok_or_else(|| self.fail("unterminated escape"))?;
                    self.at += 1;
                    match escape {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => out.push(self.unicode_escape()?),
                        _ => return Err(self.fail("unknown escape")),
                    }
                }
                _ => {
                    // The input is a `&str`, so the bytes from here to the next
                    // quote or backslash are valid UTF-8 by construction and
                    // can be taken whole rather than decoded one at a time.
                    let start = self.at;
                    while let Some(b) = self.peek() {
                        if b == b'"' || b == b'\\' {
                            break;
                        }
                        if b < 0x20 {
                            return Err(self.fail("control character in string"));
                        }
                        self.at += 1;
                    }
                    out.push_str(
                        std::str::from_utf8(&self.bytes[start..self.at])
                            .map_err(|_| self.fail("invalid UTF-8"))?,
                    );
                }
            }
        }
    }

    /// `\uXXXX`, and the surrogate pair that a character above the basic plane
    /// arrives as. An unpaired surrogate becomes the replacement character
    /// rather than an error: it is what a page's title may genuinely contain,
    /// and losing the title is better than losing the message it is in.
    fn unicode_escape(&mut self) -> Result<char, JsonError> {
        let first = self.hex4()?;
        if !(0xd800..0xdc00).contains(&first) {
            return Ok(char::from_u32(first).unwrap_or('\u{fffd}'));
        }
        if self.bytes[self.at..].starts_with(b"\\u") {
            let mark = self.at;
            self.at += 2;
            let second = self.hex4()?;
            if (0xdc00..0xe000).contains(&second) {
                let combined = 0x10000 + ((first - 0xd800) << 10) + (second - 0xdc00);
                return Ok(char::from_u32(combined).unwrap_or('\u{fffd}'));
            }
            self.at = mark;
        }
        Ok('\u{fffd}')
    }

    fn hex4(&mut self) -> Result<u32, JsonError> {
        let digits = self
            .bytes
            .get(self.at..self.at + 4)
            .ok_or_else(|| self.fail("short \\u escape"))?;
        let mut value = 0u32;
        for &digit in digits {
            let nibble = match digit {
                b'0'..=b'9' => digit - b'0',
                b'a'..=b'f' => digit - b'a' + 10,
                b'A'..=b'F' => digit - b'A' + 10,
                _ => return Err(self.fail("bad \\u escape")),
            };
            value = value * 16 + nibble as u32;
        }
        self.at += 4;
        Ok(value)
    }

    fn number(&mut self) -> Result<Json, JsonError> {
        let start = self.at;
        if self.peek() == Some(b'-') {
            self.at += 1;
        }
        while matches!(
            self.peek(),
            Some(b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-')
        ) {
            self.at += 1;
        }
        let text = std::str::from_utf8(&self.bytes[start..self.at])
            .map_err(|_| self.fail("invalid UTF-8"))?;
        text.parse::<f64>()
            .map(Json::Number)
            .map_err(|_| JsonError {
                offset: start,
                what: "not a number",
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_devtools_reply_reads_back() {
        let text = r#"{"id":7,"result":{"frameId":"A1","errorText":null},"extra":[1,2.5,-3e2]}"#;
        let value = Json::parse(text).unwrap();
        assert_eq!(value.get("id").and_then(Json::as_i64), Some(7));
        assert_eq!(
            value.path(&["result", "frameId"]).and_then(Json::as_str),
            Some("A1")
        );
        assert_eq!(value.path(&["result", "errorText"]), Some(&Json::Null));
        assert_eq!(
            value.get("extra").and_then(Json::as_array).unwrap()[2],
            Json::Number(-300.0)
        );
    }

    #[test]
    fn escapes_come_back_as_the_characters_they_stand_for() {
        let value = Json::parse(r#""a\"b\\c\/d\b\f\n\r\teA日""#).unwrap();
        assert_eq!(value.as_str(), Some("a\"b\\c/d\u{8}\u{c}\n\r\teA\u{65e5}"));
    }

    #[test]
    fn a_surrogate_pair_is_one_character() {
        assert_eq!(Json::parse(r#""😀""#).unwrap().as_str(), Some("\u{1f600}"));
        // A lone high surrogate is not a reason to drop the whole message.
        assert_eq!(
            Json::parse(r#""x\ud83dy""#).unwrap().as_str(),
            Some("x\u{fffd}y")
        );
    }

    #[test]
    fn what_is_written_is_what_parses_back() {
        let value = Json::object(vec![
            ("method", Json::string("Input.dispatchKeyEvent")),
            (
                "params",
                Json::object(vec![
                    ("text", Json::string("a\"b\n\u{1}\u{65e5}")),
                    ("windowsVirtualKeyCode", Json::number(13)),
                    ("autoRepeat", Json::Bool(true)),
                    ("nothing", Json::Null),
                    ("fraction", Json::number(0.5)),
                ]),
            ),
        ]);
        let text = value.to_string();
        assert!(text.contains(r#""windowsVirtualKeyCode":13"#), "{text}");
        assert!(text.contains("\\u0001"), "{text}");
        assert!(text.contains('\u{65e5}'), "{text}");
        assert_eq!(Json::parse(&text).unwrap(), value);
    }

    #[test]
    fn whitespace_between_everything_is_allowed() {
        let text = " { \"a\" : [ 1 , { } ] , \"b\" : null } ";
        assert_eq!(
            Json::parse(text).unwrap(),
            Json::object(vec![
                (
                    "a",
                    Json::Array(vec![Json::Number(1.0), Json::Object(vec![])])
                ),
                ("b", Json::Null),
            ])
        );
    }

    #[test]
    fn a_broken_document_says_where_it_broke() {
        assert_eq!(
            Json::parse("{\"a\":1,}"),
            Err(JsonError {
                offset: 7,
                what: "string expected"
            })
        );
        assert!(Json::parse("{\"a\":1} tail").is_err());
        assert!(Json::parse("").is_err());
        assert!(Json::parse("\"unterminated").is_err());
        assert!(Json::parse("tru").is_err());
    }
}
