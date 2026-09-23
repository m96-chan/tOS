//! Putting a frame of video where a picture goes.
//!
//! Every screencast frame is a whole picture, so every frame is a
//! transmission of a new image. Sixty times a second, that raises three
//! questions the protocol answers badly if you do not think about them: which
//! image id to use, which format the bytes are in, and how they get there.
//!
//! # Raw pixels, decoded here
//!
//! **`f=24` and `f=32`, not `f=100`.** The frames arrive from the engine as
//! JPEG while the page is moving and as PNG when it stops (see
//! [`crate::motion`]), and neither should be handed to the terminal to
//! decode: it cannot read JPEG on the graphics path at all, and the PNG
//! decode it does do is on the compositor's parse loop, which
//! `docs/design/browser.md` already names as the first cost to delete. So
//! this program decodes — 8 ms for a 1280x770 JPEG frame, on its own thread,
//! in the pane — and hands over pixels, which is the protocol's own raw
//! format and needs no compositor change at all.
//!
//! It costs bytes: 2.9 MB of RGB where the JPEG was 185 kB. Through `t=s`
//! that is a `write` into tmpfs and a `read` out of it, which is a memcpy at
//! memory speed and cheaper than the decode it replaces. Through the inline
//! fallback it is not, and the section below says what that means.
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
//! itself, which keeps the frame out of the PTY sixty times a second — base64
//! would add a third to it and the pane's reader would spend its day on it.
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
//!
//! **The fallback sends the same raw pixels, base64, and it is slow.** That
//! is a decision rather than an oversight. Sending the encoded frame instead
//! is not available: the motion frames are JPEG and no terminal's graphics
//! path reads JPEG. Re-encoding the decoded pixels as PNG would mean a PNG
//! *encoder* in this crate — a third codec written from a specification — to
//! make faster a path that exists only for terminals which are not tOS. So:
//! 2.9 MB becomes 3.9 MB of base64 a frame and a pane in somebody else's
//! terminal gets a slideshow. It is correct, it is obviously correct, and the
//! terminal this program is for never takes it.

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

/// One decoded frame, in the layout its decoder produced.
///
/// Three channels or four, and the protocol has a format for each, so the
/// pixels go across as they are rather than being widened or narrowed:
/// `tos_term::jpeg` produces RGB and a JPEG has no alpha to lose,
/// `tos_term::png` produces RGBA and a still is one frame in a hundred and
/// fifty milliseconds, so neither conversion would buy anything.
#[derive(Debug, Clone, Copy)]
pub struct Raw<'a> {
    pub pixels: &'a [u8],
    pub width: u32,
    pub height: u32,
    /// Bytes a pixel: three for `f=24`, four for `f=32`.
    pub channels: u32,
}

impl<'a> Raw<'a> {
    /// Three bytes a pixel, which is what a decoded JPEG is.
    pub fn rgb(pixels: &'a [u8], width: u32, height: u32) -> Raw<'a> {
        Raw {
            pixels,
            width,
            height,
            channels: 3,
        }
    }

    /// Four, which is what a decoded PNG is.
    pub fn rgba(pixels: &'a [u8], width: u32, height: u32) -> Raw<'a> {
        Raw {
            pixels,
            width,
            height,
            channels: 4,
        }
    }

    /// The protocol's `f=` for this layout.
    fn format(&self) -> u32 {
        if self.channels == 3 {
            24
        } else {
            32
        }
    }
}

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

    /// The bytes that put `raw` on screen at `row`, `col`, sized to `cells`.
    ///
    /// The cursor is moved to the placement's corner first, because `a=T`
    /// places at the cursor, and `C=1` keeps it there so that the status line
    /// can be written afterwards without the picture having moved anything.
    pub fn frame(&mut self, raw: Raw<'_>, cells: Cells, row: u32, col: u32) -> Vec<u8> {
        let mut out = format!("\x1b[{row};{col}H").into_bytes();
        match self.transport {
            Transport::SharedMemory => match self.write_object(raw.pixels) {
                Some(name) => out.extend_from_slice(&shared_memory_command(&name, &raw, cells)),
                None => {
                    // Writing failed, so the directory that worked at startup
                    // does not any more; the picture is more important than
                    // the transport it arrives by.
                    self.transport = Transport::Inline;
                    out.extend_from_slice(&inline_command(&raw, cells));
                }
            },
            Transport::Inline => out.extend_from_slice(&inline_command(&raw, cells)),
        }
        out
    }

    /// Write one frame under a fresh name, and retire the names that are old
    /// enough to have been read by now.
    fn write_object(&mut self, pixels: &[u8]) -> Option<String> {
        self.counter += 1;
        let name = format!("{}-{}", self.prefix, self.counter);
        let path = self.dir.join(&name);
        let partial = self.dir.join(format!("{name}.part"));

        // Written under another name and renamed, so that the compositor never
        // sees a file that is still growing: it takes the size from the
        // descriptor and would read a short image.
        if std::fs::write(&partial, pixels).is_err() {
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
///
/// `s=` and `v=` are the picture's real pixel dimensions, and for a raw
/// payload they are not advisory the way they are for a PNG: pixels are a
/// rectangle and nothing else, so this is where the terminal learns its
/// shape. `c=` and `r=` are the cells it is drawn into, which is a separate
/// thing, and the two agree because the page is laid out at the pane's own
/// pixel size — a frame that is the size of the cells it fills is blitted
/// rather than resampled.
fn control(raw: &Raw<'_>, cells: Cells) -> String {
    format!(
        "a=T,f={},s={},v={},i={IMAGE_ID},p={PLACEMENT_ID},c={},r={},C=1,q=2",
        raw.format(),
        raw.width.max(1),
        raw.height.max(1),
        cells.cols.max(1),
        cells.rows.max(1)
    )
}

/// `t=s`: the payload is the name of the object, not the image.
pub fn shared_memory_command(name: &str, raw: &Raw<'_>, cells: Cells) -> Vec<u8> {
    let control = control(raw, cells);
    format!(
        "\x1b_G{control},t=s;{}\x1b\\",
        base64::encode(name.as_bytes())
    )
    .into_bytes()
}

/// Take the picture off the screen and the frame out of the store.
///
/// For switching tabs, which is the one moment the picture on screen belongs
/// to a page that is no longer being shown. The alternative — leaving it until
/// the new tab's first frame lands — would show the old page under the new
/// tab's title for as long as the new page takes to paint, which on a tab that
/// was opened a second ago and has not loaded is as long as the network takes.
/// An empty pane is honest about there being nothing to show yet.
///
/// `d=I` rather than `d=i`: the uppercase form frees the image data as well as
/// the placement, and the data is the pane in RGBA — nearly four megabytes at
/// 1280x770. The next frame transmits a new image under the same id anyway, so
/// there is nothing to keep.
pub fn clear_command() -> Vec<u8> {
    format!("\x1b_Ga=d,d=I,i={IMAGE_ID},p={PLACEMENT_ID},q=2\x1b\\").into_bytes()
}

/// `t=d`: the pixels themselves, base64, in as many escape sequences as it
/// takes — which for a pane-sized frame is several hundred.
pub fn inline_command(raw: &Raw<'_>, cells: Cells) -> Vec<u8> {
    let control = control(raw, cells);
    let encoded = base64::encode(raw.pixels);
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

    /// A 2x2 picture, in each of the two layouts the protocol takes.
    fn rgb() -> Vec<u8> {
        vec![
            10, 20, 30, 40, 50, 60, //
            70, 80, 90, 100, 110, 120,
        ]
    }

    fn rgba() -> Vec<u8> {
        vec![
            10, 20, 30, 255, 40, 50, 60, 255, //
            70, 80, 90, 255, 100, 110, 120, 255,
        ]
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
        let pixels = rgb();
        let raw = Raw::rgb(&pixels, 640, 368);
        let bytes = shared_memory_command("/tos-browser-1-2", &raw, cells(80, 23));
        let bodies = apc_bodies(&bytes);
        assert_eq!(bodies.len(), 1);
        let cmd = GraphicsCommand::parse(&bodies[0]).expect("parses");
        assert_eq!(cmd.action, Action::TransmitAndDisplay);
        assert_eq!(cmd.format, Format::Rgb, "the pixels go over, not a file");
        assert_eq!((cmd.width, cmd.height), (640, 368));
        assert_eq!(cmd.medium, Medium::SharedMemory);
        assert_eq!(cmd.image_id, IMAGE_ID);
        assert_eq!(cmd.placement_id, PLACEMENT_ID);
        assert_eq!((cmd.cols, cmd.rows), (80, 23));
        assert!(cmd.cursor_stays, "the status line is written after this");
        assert_eq!(cmd.quiet, 2, "nothing is reading a reply");
        assert_eq!(cmd.payload, b"/tos-browser-1-2");
    }

    /// A still is RGBA because that is what the PNG decoder produces, and the
    /// protocol takes it under a format of its own rather than a conversion.
    #[test]
    fn a_still_goes_over_as_rgba_and_a_motion_frame_as_rgb() {
        let three = rgb();
        let four = rgba();
        for (raw, format) in [
            (Raw::rgb(&three, 2, 2), Format::Rgb),
            (Raw::rgba(&four, 2, 2), Format::Rgba),
        ] {
            let bodies = apc_bodies(&inline_command(&raw, cells(2, 1)));
            let cmd = GraphicsCommand::parse(&bodies[0]).expect("parses");
            assert_eq!(cmd.format, format);
            assert_eq!((cmd.width, cmd.height), (2, 2));
        }
    }

    #[test]
    fn an_inline_frame_is_the_picture_in_chunks_that_reassemble() {
        let pixels: Vec<u8> = (0..CHUNK * 5).map(|i| (i * 31) as u8).collect();
        let raw = Raw::rgb(&pixels, (CHUNK as u32 * 5) / 3, 1);
        let bodies = apc_bodies(&inline_command(&raw, cells(10, 4)));
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
        assert_eq!(joined, pixels);
    }

    #[test]
    fn switching_tabs_takes_the_old_page_off_the_screen() {
        let mut terminal = tos_term::Terminal::new(40, 12, tos_term::TerminalConfig::default());
        let mut painter = Painter::at(Path::new("/nonexistent-for-a-test"));
        let pixels = rgb();
        terminal.advance(&painter.frame(Raw::rgb(&pixels, 2, 2), cells(4, 2), 2, 1));
        assert_eq!(terminal.graphics().placements().count(), 1);
        assert!(terminal.graphics().image(IMAGE_ID).is_some());

        terminal.advance(&clear_command());
        assert_eq!(
            terminal.graphics().placements().count(),
            0,
            "the picture is gone"
        );
        assert!(
            terminal.graphics().image(IMAGE_ID).is_none(),
            "and so are its pixels"
        );
    }

    #[test]
    fn a_frame_puts_the_cursor_where_the_picture_goes() {
        let mut painter = Painter::at(Path::new("/nonexistent-for-a-test"));
        assert_eq!(painter.transport(), Transport::Inline);
        let bytes = painter.frame(Raw::rgb(b"rgb", 1, 1), cells(4, 2), 2, 1);
        assert!(bytes.starts_with(b"\x1b[2;1H"), "{bytes:?}");
    }

    #[test]
    fn frames_get_a_fresh_name_each_time_because_the_last_one_was_eaten() {
        let dir = temp_dir("names");
        let mut painter = Painter::at(&dir);
        assert_eq!(painter.transport(), Transport::SharedMemory);

        let mut names = Vec::new();
        for _ in 0..4 {
            let bytes = painter.frame(Raw::rgb(b"rgb", 1, 1), cells(4, 2), 2, 1);
            let body = apc_bodies(&bytes).remove(0);
            let cmd = GraphicsCommand::parse(&body).expect("parses");
            let name = String::from_utf8(cmd.payload).expect("a name");
            assert!(name.starts_with('/'), "a POSIX name: {name}");
            assert!(!name.contains(".part"), "{name}");
            // The file is there, whole, and nothing is left half-written.
            let path = dir.join(name.trim_start_matches('/'));
            assert_eq!(std::fs::read(&path).unwrap(), b"rgb");
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
            painter.frame(Raw::rgb(b"rgb", 1, 1), cells(4, 2), 2, 1);
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
        let pixels = rgb();
        for _ in 0..8 {
            terminal.advance(&inline_command(&Raw::rgb(&pixels, 2, 2), cells(8, 4)));
        }
        let store = terminal.graphics();
        assert_eq!(store.placements().count(), 1);
        let image = store.image(IMAGE_ID).expect("the one image");
        assert_eq!((image.width, image.height), (2, 2));
        // RGB widens to the RGBA the store holds, with an opaque alpha.
        assert_eq!(&image.data[..8], &[10, 20, 30, 255, 40, 50, 60, 255]);
        let placement = store.placements().next().expect("the one placement");
        assert_eq!(placement.image_id, IMAGE_ID);
        assert_eq!(placement.placement_id, PLACEMENT_ID);
        assert_eq!((placement.cols, placement.rows), (8, 4));
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
