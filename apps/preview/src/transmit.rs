//! Turning an image into the bytes that put it on screen.
//!
//! One `a=T` command, chunked, the way
//! `compositor/tos-compositor/examples/graphics_animation.rs` sends its
//! frames. What is different here is the sequence around the command: a
//! picture is placed at the cursor, and a cursor sitting three rows from the
//! bottom of the pane has no room for twenty rows of image. So the room is
//! made first, by scrolling, and the cursor is walked back up into it.

use tos_term::graphics::encode_base64;

use crate::fit::Cells;

/// Base64 goes out in chunks, as the protocol asks for payloads that do not
/// fit one escape sequence.
pub const CHUNK: usize = 4096;

/// What is being sent, and how the terminal is told to read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Payload {
    /// `f=100`: the file as it sits on disk, decoded by the terminal.
    Png,
    /// `f=32`: pixels this program decoded, for a terminal that cannot.
    Rgba { width: u32, height: u32 },
}

impl Payload {
    fn keys(self) -> String {
        match self {
            Payload::Png => "f=100".to_string(),
            Payload::Rgba { width, height } => format!("f=32,s={width},v={height}"),
        }
    }
}

/// Every byte `tos-preview` writes for one picture, in order.
///
/// The image is given `cells.rows` blank rows to live in before it is sent,
/// because `a=T` places at the cursor and clips at the bottom of the screen
/// rather than scrolling to make room. Scrolling first and stepping back up
/// means the picture lands whole wherever the cursor happened to be, and the
/// cursor is left on the row below it so the next prompt does not print over
/// the top of what was just drawn.
///
/// Responses are suppressed with `q=2`. A reply would be written to this
/// program's own terminal input, where nothing is reading it and the shell
/// would collect it as typing once `tos-preview` exits; the alternative is
/// taking the tty into raw mode to swallow an answer that only ever says
/// what the exit status already says.
pub fn sequence(payload: Payload, data: &[u8], cells: Cells) -> Vec<u8> {
    let rows = cells.rows.max(1);
    let mut out = Vec::new();

    out.push(b'\r');
    out.extend(std::iter::repeat(b'\n').take(rows as usize));
    out.extend_from_slice(format!("\x1b[{rows}A").as_bytes());

    let control = format!(
        "a=T,{},c={},r={},C=1,q=2",
        payload.keys(),
        cells.cols.max(1),
        rows
    );
    out.extend_from_slice(&command(&control, data));

    out.extend_from_slice(format!("\x1b[{rows}B\r").as_bytes());
    out
}

/// One graphics command, split across as many escape sequences as its base64
/// needs.
fn command(control: &str, payload: &[u8]) -> Vec<u8> {
    let encoded = encode_base64(payload);
    if encoded.is_empty() {
        return format!("\x1b_G{control};\x1b\\").into_bytes();
    }

    let chunks: Vec<&[u8]> = encoded.as_bytes().chunks(CHUNK).collect();
    let mut out = Vec::new();
    for (index, chunk) in chunks.iter().enumerate() {
        let more = u8::from(index + 1 < chunks.len());
        let text = std::str::from_utf8(chunk).expect("base64 is ascii");
        let head = if index == 0 {
            format!("\x1b_G{control},m={more};")
        } else {
            format!("\x1b_Gm={more};")
        };
        out.extend_from_slice(head.as_bytes());
        out.extend_from_slice(text.as_bytes());
        out.extend_from_slice(b"\x1b\\");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tos_term::graphics::{decode_base64, Action, Format, GraphicsCommand};

    fn cells(cols: u32, rows: u32) -> Cells {
        Cells { cols, rows }
    }

    /// Pull the APC bodies back out, the way the terminal's parser hands them
    /// to `handle_graphics`.
    fn apc_bodies(bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut bodies = Vec::new();
        let mut rest = bytes;
        while let Some(start) = find(rest, b"\x1b_G") {
            let body = &rest[start + 3..];
            let end = find(body, b"\x1b\\").expect("unterminated APC");
            bodies.push(body[..end].to_vec());
            rest = &body[end + 2..];
        }
        bodies
    }

    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    #[test]
    fn a_png_is_sent_as_a_file_the_terminal_decodes() {
        let bytes = sequence(Payload::Png, b"not really a png", cells(10, 4));
        let bodies = apc_bodies(&bytes);
        assert_eq!(bodies.len(), 1);
        let cmd = GraphicsCommand::parse(&bodies[0]).expect("parses");
        assert_eq!(cmd.action, Action::TransmitAndDisplay);
        assert_eq!(cmd.format, Format::Png);
        assert_eq!((cmd.cols, cmd.rows), (10, 4));
        assert!(cmd.cursor_stays);
        assert_eq!(cmd.quiet, 2);
        assert_eq!(cmd.payload, b"not really a png");
    }

    #[test]
    fn raw_pixels_carry_the_dimensions_they_need() {
        let pixels = vec![0u8; 2 * 3 * 4];
        let payload = Payload::Rgba {
            width: 2,
            height: 3,
        };
        let bodies = apc_bodies(&sequence(payload, &pixels, cells(1, 1)));
        let cmd = GraphicsCommand::parse(&bodies[0]).expect("parses");
        assert_eq!(cmd.format, Format::Rgba);
        assert_eq!((cmd.width, cmd.height), (2, 3));
        assert_eq!(cmd.payload, pixels);
    }

    #[test]
    fn a_payload_too_big_for_one_sequence_is_chunked() {
        // Three bytes become four of base64, so this is comfortably past a
        // single chunk.
        let data: Vec<u8> = (0..CHUNK * 3).map(|i| i as u8).collect();
        let bodies = apc_bodies(&sequence(Payload::Png, &data, cells(10, 4)));
        assert!(bodies.len() > 1);

        let first = GraphicsCommand::parse(&bodies[0]).expect("parses");
        assert!(first.more, "the first chunk must say more is coming");
        let last = GraphicsCommand::parse(bodies.last().unwrap()).expect("parses");
        assert!(!last.more, "the last chunk must say it is the last");

        // The terminal concatenates the payloads, so they have to reassemble
        // into the file that went in.
        let mut joined = Vec::new();
        for body in &bodies {
            let semicolon = body.iter().position(|&b| b == b';').unwrap();
            joined.extend_from_slice(&decode_base64(&body[semicolon + 1..]));
        }
        assert_eq!(joined, data);
    }

    #[test]
    fn the_room_made_before_the_picture_is_the_room_it_needs() {
        let bytes = sequence(Payload::Png, b"x", cells(10, 7));
        let newlines = bytes.iter().filter(|&&b| b == b'\n').count();
        assert_eq!(newlines, 7);
        assert!(find(&bytes, b"\x1b[7A").is_some(), "step back up");
        assert!(find(&bytes, b"\x1b[7B").is_some(), "step back down");
        // Up before the command, down after it: the picture goes in the gap.
        let up = find(&bytes, b"\x1b[7A").unwrap();
        let down = find(&bytes, b"\x1b[7B").unwrap();
        let apc = find(&bytes, b"\x1b_G").unwrap();
        assert!(up < apc && apc < down);
    }

    #[test]
    fn nothing_is_written_before_the_cursor_reaches_column_zero() {
        let bytes = sequence(Payload::Png, b"x", cells(10, 2));
        assert_eq!(bytes[0], b'\r');
        assert_eq!(*bytes.last().unwrap(), b'\r');
    }
}
