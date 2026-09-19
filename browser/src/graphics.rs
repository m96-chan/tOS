//! Putting a frame of video where a picture goes.
//!
//! Every screencast frame is a whole PNG, so every frame is a transmission of
//! a new image. Sixty times a second, that raises two questions the protocol
//! answers badly if you do not think about them: which image id to use, and
//! how the bytes get there.
//!
//! # One image id, retransmitted
//!
//! A fixed image id (`i=1`) with a fixed placement id (`p=1`), sent as `a=T`
//! every frame. Not two ids alternating, and no `a=d` delete of the old one.
//!
//! The reason is in the store rather than in the protocol.
//! `GraphicsStore::store` removes any image already under the id and subtracts
//! its bytes before inserting the new one, and `GraphicsStore::place` drops
//! any placement with the same image and placement id before making the new
//! one (`compositor/tos-term/src/graphics.rs`). So a retransmission under the
//! same pair leaves exactly one image and one placement behind, whatever the
//! frame rate — there is nothing to leak and nothing to delete. Alternating
//! two ids would hold two frames' worth of RGBA instead of one and would need
//! a delete after every frame, and a delete is a command whose effect on the
//! screen lands between the new image and its placement: a window, however
//! small, in which the pane has no picture in it. Retransmitting has no such
//! window, because the replacement happens inside one parse.
//!
//! # A name, not the bytes
//!
//! `t=s` hands the compositor a POSIX shared memory name and it reads the file
//! itself, which keeps a 58 kB frame out of the PTY sixty times a second —
//! base64 would make it 77 kB and the pane's reader would spend its day on it.
//! The compositor *unlinks the object after reading it*
//! (`docs/design/graphics-file-transmission.md`), so every frame needs a name
//! of its own; the counter in [`Painter`] is that.
//!
//! Two consequences are handled here. A name is written under a `.part`
//! suffix and renamed into place, because the reader takes the file's size
//! from the descriptor and a frame caught mid-write would be read short. And
//! names that were never consumed are collected: if the terminal on the other
//! end does not implement `t=s` — every terminal that is not tOS — the files
//! pile up in `/dev/shm` and nothing appears on screen, so after a few
//! unconsumed names [`Painter`] gives up and sends the bytes inline instead.
//! That check is what makes the same binary work in Kitty.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use tos_preview::fit::Cells;

use crate::base64;

/// Base64 goes out in chunks, as the protocol asks.
pub const CHUNK: usize = 4096;

/// The image and placement this program owns in the terminal's store.
pub const IMAGE_ID: u32 = 1;
pub const PLACEMENT_ID: u32 = 1;

/// Where POSIX shared memory objects live, as files.
pub const SHM_DIR: &str = "/dev/shm";

/// How many frames may be in flight before an unconsumed name means the
/// terminal is not reading them.
///
/// The compositor reads a name when it parses the escape sequence, which is
/// after the bytes have crossed the PTY and got to the front of its queue —
/// so a name written for this frame may well still be there when the next is
/// written. Sixteen frames is a quarter of a second at sixty; a terminal that
/// has not read a name by then is not going to.
const IN_FLIGHT: usize = 16;

/// How the payload reaches the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// `t=s`: a name in `/dev/shm`, which the terminal reads and unlinks.
    SharedMemory,
    /// `t=d`: base64 in the escape sequence itself.
    Inline,
}

/// The state a sequence of frames needs: which names are outstanding, and
/// whether the terminal is reading them.
pub struct Painter {
    transport: Transport,
    dir: PathBuf,
    prefix: String,
    counter: u64,
    outstanding: VecDeque<PathBuf>,
    unconsumed: u32,
}

impl Painter {
    /// Choose a transport by trying the better one.
    ///
    /// `/dev/shm` may be absent, read-only or full — in a container, in a
    /// rescue shell, on a machine whose tmpfs is exhausted — and each of those
    /// is a reason to send frames inline rather than to fail.
    pub fn new() -> Painter {
        Painter::at(Path::new(SHM_DIR))
    }

    /// The same, in a directory a test can watch.
    pub fn at(dir: &Path) -> Painter {
        let prefix = format!("tos-browser-{}", std::process::id());
        let probe = dir.join(format!("{prefix}-probe"));
        let usable = std::fs::write(&probe, b"probe").is_ok();
        let _ = std::fs::remove_file(&probe);
        Painter {
            transport: if usable {
                Transport::SharedMemory
            } else {
                Transport::Inline
            },
            dir: dir.to_path_buf(),
            prefix,
            counter: 0,
            outstanding: VecDeque::new(),
            unconsumed: 0,
        }
    }

    pub fn transport(&self) -> Transport {
        self.transport
    }

    /// The bytes that put `png` on screen at `row`, `col`, sized to `cells`.
    ///
    /// The cursor is moved to the placement's corner first, because `a=T`
    /// places at the cursor, and `C=1` keeps it there so that the status line
    /// can be written afterwards without the picture having moved anything.
    pub fn frame(&mut self, png: &[u8], cells: Cells, row: u32, col: u32) -> Vec<u8> {
        let mut out = format!("\x1b[{row};{col}H").into_bytes();
        match self.transport {
            Transport::SharedMemory => match self.write_object(png) {
                Some(name) => out.extend_from_slice(&shared_memory_command(&name, cells)),
                None => {
                    // Writing failed, so the directory that worked at startup
                    // does not any more; the picture is more important than
                    // the transport it arrives by.
                    self.transport = Transport::Inline;
                    out.extend_from_slice(&inline_command(png, cells));
                }
            },
            Transport::Inline => out.extend_from_slice(&inline_command(png, cells)),
        }
        out
    }

    /// Write one frame under a fresh name, and retire the names that are old
    /// enough to have been read by now.
    fn write_object(&mut self, png: &[u8]) -> Option<String> {
        self.counter += 1;
        let name = format!("{}-{}", self.prefix, self.counter);
        let path = self.dir.join(&name);
        let partial = self.dir.join(format!("{name}.part"));

        // Written under another name and renamed, so that the compositor never
        // sees a file that is still growing: it takes the size from the
        // descriptor and would read a short image.
        if std::fs::write(&partial, png).is_err() {
            let _ = std::fs::remove_file(&partial);
            return None;
        }
        if std::fs::rename(&partial, &path).is_err() {
            let _ = std::fs::remove_file(&partial);
            return None;
        }

        self.outstanding.push_back(path);
        if self.outstanding.len() > IN_FLIGHT {
            if let Some(old) = self.outstanding.pop_front() {
                // Still there means the terminal never read it. A few of those
                // in a row and this is not a terminal that speaks `t=s`.
                if std::fs::remove_file(&old).is_ok() {
                    self.unconsumed += 1;
                    if self.unconsumed >= IN_FLIGHT as u32 / 2 {
                        self.transport = Transport::Inline;
                    }
                } else {
                    self.unconsumed = 0;
                }
            }
        }
        Some(format!("/{name}"))
    }

    /// Remove anything this program left in `/dev/shm`.
    ///
    /// Called on the way out, including from the panic path: a frame written
    /// and not read is a file that would otherwise sit in a tmpfs until the
    /// machine is rebooted.
    pub fn clean_up(&mut self) {
        for path in self.outstanding.drain(..) {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl Default for Painter {
    fn default() -> Self {
        Painter::new()
    }
}

impl Drop for Painter {
    fn drop(&mut self) {
        self.clean_up();
    }
}

/// The control keys every frame carries.
fn control(cells: Cells) -> String {
    format!(
        "a=T,f=100,i={IMAGE_ID},p={PLACEMENT_ID},c={},r={},C=1,q=2",
        cells.cols.max(1),
        cells.rows.max(1)
    )
}

/// `t=s`: the payload is the name of the object, not the image.
pub fn shared_memory_command(name: &str, cells: Cells) -> Vec<u8> {
    let control = control(cells);
    format!(
        "\x1b_G{control},t=s;{}\x1b\\",
        base64::encode(name.as_bytes())
    )
    .into_bytes()
}

/// `t=d`: the image itself, base64, in as many escape sequences as it takes.
pub fn inline_command(png: &[u8], cells: Cells) -> Vec<u8> {
    let control = control(cells);
    let encoded = base64::encode(png);
    if encoded.is_empty() {
        return format!("\x1b_G{control};\x1b\\").into_bytes();
    }

    let chunks: Vec<&[u8]> = encoded.as_bytes().chunks(CHUNK).collect();
    let mut out = Vec::new();
    for (index, chunk) in chunks.iter().enumerate() {
        let more = u8::from(index + 1 < chunks.len());
        let head = if index == 0 {
            format!("\x1b_G{control},m={more};")
        } else {
            format!("\x1b_Gm={more};")
        };
        out.extend_from_slice(head.as_bytes());
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\x1b\\");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tos_term::graphics::{Action, Format, GraphicsCommand, Medium};

    fn cells(cols: u32, rows: u32) -> Cells {
        Cells { cols, rows }
    }

    /// Pull the APC bodies out the way the terminal's parser does.
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
    fn a_shared_memory_frame_names_an_object_and_says_nothing_back() {
        let bytes = shared_memory_command("/tos-browser-1-2", cells(80, 23));
        let bodies = apc_bodies(&bytes);
        assert_eq!(bodies.len(), 1);
        let cmd = GraphicsCommand::parse(&bodies[0]).expect("parses");
        assert_eq!(cmd.action, Action::TransmitAndDisplay);
        assert_eq!(cmd.format, Format::Png);
        assert_eq!(cmd.medium, Medium::SharedMemory);
        assert_eq!(cmd.image_id, IMAGE_ID);
        assert_eq!(cmd.placement_id, PLACEMENT_ID);
        assert_eq!((cmd.cols, cmd.rows), (80, 23));
        assert!(cmd.cursor_stays, "the status line is written after this");
        assert_eq!(cmd.quiet, 2, "nothing is reading a reply");
        assert_eq!(cmd.payload, b"/tos-browser-1-2");
    }

    #[test]
    fn an_inline_frame_is_the_picture_in_chunks_that_reassemble() {
        let png: Vec<u8> = (0..CHUNK * 5).map(|i| (i * 31) as u8).collect();
        let bodies = apc_bodies(&inline_command(&png, cells(10, 4)));
        assert!(bodies.len() > 1);

        let first = GraphicsCommand::parse(&bodies[0]).expect("parses");
        assert_eq!(first.medium, Medium::Direct);
        assert_eq!(first.image_id, IMAGE_ID);
        assert!(first.more);
        assert!(
            !GraphicsCommand::parse(bodies.last().unwrap())
                .expect("parses")
                .more
        );

        let mut joined = Vec::new();
        for body in &bodies {
            let semicolon = body.iter().position(|&b| b == b';').unwrap();
            joined.extend_from_slice(&tos_term::graphics::decode_base64(&body[semicolon + 1..]));
        }
        assert_eq!(joined, png);
    }

    #[test]
    fn a_frame_puts_the_cursor_where_the_picture_goes() {
        let mut painter = Painter::at(Path::new("/nonexistent-for-a-test"));
        assert_eq!(painter.transport(), Transport::Inline);
        let bytes = painter.frame(b"png", cells(4, 2), 2, 1);
        assert!(bytes.starts_with(b"\x1b[2;1H"), "{bytes:?}");
    }

    #[test]
    fn frames_get_a_fresh_name_each_time_because_the_last_one_was_eaten() {
        let dir = temp_dir("names");
        let mut painter = Painter::at(&dir);
        assert_eq!(painter.transport(), Transport::SharedMemory);

        let mut names = Vec::new();
        for _ in 0..4 {
            let bytes = painter.frame(b"png", cells(4, 2), 2, 1);
            let body = apc_bodies(&bytes).remove(0);
            let cmd = GraphicsCommand::parse(&body).expect("parses");
            let name = String::from_utf8(cmd.payload).expect("a name");
            assert!(name.starts_with('/'), "a POSIX name: {name}");
            assert!(!name.contains(".part"), "{name}");
            // The file is there, whole, and nothing is left half-written.
            let path = dir.join(name.trim_start_matches('/'));
            assert_eq!(std::fs::read(&path).unwrap(), b"png");
            names.push(name);
            // The terminal reads and unlinks; here the test does.
            std::fs::remove_file(&path).expect("consume");
        }
        names.sort();
        names.dedup();
        assert_eq!(names.len(), 4, "every frame needs its own name");
        assert!(
            dir.read_dir().unwrap().next().is_none(),
            "nothing left over"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_terminal_that_never_reads_a_name_gets_the_bytes_instead() {
        let dir = temp_dir("unread");
        let mut painter = Painter::at(&dir);
        for _ in 0..IN_FLIGHT * 2 {
            painter.frame(b"png", cells(4, 2), 2, 1);
        }
        assert_eq!(
            painter.transport(),
            Transport::Inline,
            "unconsumed names mean the terminal does not speak t=s"
        );
        painter.clean_up();
        let left = dir.read_dir().unwrap().count();
        assert_eq!(left, 0, "and nothing is left behind in /dev/shm");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn what_the_terminal_makes_of_a_frame_is_one_image_and_one_placement() {
        // The real thing: drive a terminal with the bytes and look at its
        // store. Retransmitting under the same id must not accumulate.
        let mut terminal = tos_term::Terminal::new(40, 12, tos_term::TerminalConfig::default());
        let png = tiny_png();
        for _ in 0..8 {
            terminal.advance(&inline_command(&png, cells(8, 4)));
        }
        let store = terminal.graphics();
        assert_eq!(store.placements().count(), 1);
        let image = store.image(IMAGE_ID).expect("the one image");
        assert_eq!((image.width, image.height), (2, 2));
        let placement = store.placements().next().expect("the one placement");
        assert_eq!(placement.image_id, IMAGE_ID);
        assert_eq!(placement.placement_id, PLACEMENT_ID);
        assert_eq!((placement.cols, placement.rows), (8, 4));
    }

    /// A 2x2 PNG, built by the encoder in the test suite next door.
    fn tiny_png() -> Vec<u8> {
        let mut raw = Vec::new();
        for _ in 0..2 {
            // Each row starts with its filter byte, which is "none".
            raw.extend_from_slice(&[0u8]);
            for x in 0..2u8 {
                raw.extend_from_slice(&[x * 100, 40, 200, 255]);
            }
        }
        let mut idat = vec![0x78, 0x01];
        // One stored deflate block, which needs no compressor.
        idat.push(1);
        idat.extend_from_slice(&(raw.len() as u16).to_le_bytes());
        idat.extend_from_slice(&(!(raw.len() as u16)).to_le_bytes());
        idat.extend_from_slice(&raw);
        idat.extend_from_slice(&adler32(&raw).to_be_bytes());

        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&2u32.to_be_bytes());
        ihdr.extend_from_slice(&2u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);

        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        push_chunk(&mut png, b"IHDR", &ihdr);
        push_chunk(&mut png, b"IDAT", &idat);
        push_chunk(&mut png, b"IEND", &[]);
        png
    }

    fn push_chunk(png: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        png.extend_from_slice(&(data.len() as u32).to_be_bytes());
        png.extend_from_slice(kind);
        png.extend_from_slice(data);
        let mut crc_input = kind.to_vec();
        crc_input.extend_from_slice(data);
        png.extend_from_slice(&crc32(&crc_input).to_be_bytes());
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xffff_ffffu32;
        for &byte in data {
            crc ^= byte as u32;
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xedb8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    fn adler32(data: &[u8]) -> u32 {
        let (mut a, mut b) = (1u32, 0u32);
        for &byte in data {
            a = (a + byte as u32) % 65521;
            b = (b + a) % 65521;
        }
        (b << 16) | a
    }

    fn temp_dir(what: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tos-browser-{what}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("a directory to write in");
        dir
    }
}
