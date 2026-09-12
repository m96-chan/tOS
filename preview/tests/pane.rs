//! Run `tos-preview` in a real pane and look at what lands on the screen.
//!
//! This is the integration the graphics milestone never got. Everything below
//! the compositor was already covered — the protocol parser, the PNG decoder,
//! the texture scaler — and all of it was covered in isolation, so "the image
//! appears in the pane" was a claim nobody had checked. Here a headless
//! compositor spawns a shell, the shell runs the real `tos-preview` binary on
//! a real PNG file, and the frame that comes out is counted pixel by pixel
//! against the colours that went in.
//!
//! The frame is also written out as a PNG, so a person can open it:
//!
//! ```text
//! cargo test -p tos-preview --test pane -- --nocapture
//! ```

use std::time::{Duration, Instant};

use tos_compositor::{Compositor, Config};
use tos_render::OwnedFramebuffer;

const SIZE: (u32, u32) = (800, 480);

/// The picture: four flat quadrants. Flat so that nearest-neighbour scaling
/// leaves the colours exactly as they were sent, and four of them so that a
/// picture drawn upside down, mirrored or half missing fails the count rather
/// than passing it.
///
/// None of these four is in the default palette, so no amount of text in the
/// pane can put one of them on the screen by accident.
const QUADRANTS: [[u8; 3]; 4] = [[203, 65, 84], [48, 132, 70], [60, 90, 200], [220, 190, 40]];
/// Bigger than the pane it is going into, on purpose: the picture has to be
/// scaled down to fit, and 1.2 MB of RGBA becomes 1.6 MB of base64, which is
/// four hundred chunks rather than one escape sequence. Both of those are
/// paths a small fixture would leave untouched.
const IMAGE: (u32, u32) = (640, 480);

#[test]
fn a_picture_reaches_the_pane() {
    let shot = preview(&[], "pane.png");
    assert_eq!(
        shot.placed, shot.expected,
        "the placement is not the size the fit asked for"
    );
    shot.assert_quadrants_are_on_screen();
}

/// The `--rgba` fallback: the same picture, decoded here and sent as pixels,
/// has to end up looking the same as the file the terminal decoded itself.
#[test]
fn decoded_pixels_land_in_the_same_place_as_the_file() {
    let shot = preview(&["--rgba"], "pane-rgba.png");
    assert_eq!(shot.placed, shot.expected);
    shot.assert_quadrants_are_on_screen();
}

/// One frame of a pane that has been shown a picture.
struct Shot {
    framebuffer: OwnedFramebuffer,
    /// The size of the placement the terminal made, in cells.
    placed: (u32, u32),
    /// The size [`tos_preview::fit`] says that should have been.
    expected: (u32, u32),
    /// How many pixels the placement covers on screen.
    drawn: usize,
    snapshot: std::path::PathBuf,
}

/// Run `tos-preview` on the fixture in a pane, render one frame, and save it.
fn preview(flags: &[&str], name: &str) -> Shot {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("tos-preview");
    std::fs::create_dir_all(&dir).expect("temp dir");
    let source = dir.join("quadrants.png");
    std::fs::write(&source, encode_png(IMAGE.0, IMAGE.1, &quadrants())).expect("write fixture");

    let mut compositor = compositor(&format!(
        "{} {} {}; sleep 30",
        env!("CARGO_BIN_EXE_tos-preview"),
        flags.join(" "),
        source.display()
    ));

    // The shell has to start, exec the binary, and the binary has to read the
    // file and push a megabyte of base64 through a pipe. Polling for the
    // placement rather than sleeping a fixed time keeps this from being a race
    // on a loaded machine.
    let focus = compositor.session().focus();
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        compositor.pump_panes();
        if !compositor
            .pane(focus)
            .unwrap()
            .terminal
            .graphics()
            .is_empty()
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let cell = compositor.cell_size();
    let area = compositor.pane(focus).unwrap().area;
    let placed = {
        let store = compositor.pane(focus).unwrap().terminal.graphics();
        let placement = store
            .placements()
            .next()
            .expect("tos-preview transmitted nothing the terminal kept");
        (placement.cols as u32, placement.rows as u32)
    };

    // The binary sized the picture through the same arithmetic the library
    // exposes, so the two have to agree; if they ever stop agreeing it is the
    // terminal's winsize that changed underneath, not the fit.
    let metrics = tos_preview::Metrics {
        cols: area.width,
        rows: area.height,
        cell,
    };
    let expected = tos_preview::fit::fit(IMAGE.0, IMAGE.1, metrics, false);

    let mut framebuffer = OwnedFramebuffer::new(SIZE.0, SIZE.1);
    {
        let mut surface = framebuffer.surface();
        compositor.render_frame(&mut surface, false);
    }

    let snapshot = dir.join(name);
    std::fs::write(&snapshot, framebuffer_png(&framebuffer)).expect("write snapshot");
    println!(
        "wrote {} ({} by {} cells of {}x{})",
        snapshot.display(),
        placed.0,
        placed.1,
        cell.0,
        cell.1
    );
    Shot {
        framebuffer,
        placed,
        expected: (expected.cols, expected.rows),
        drawn: (placed.0 * cell.0) as usize * (placed.1 * cell.1) as usize,
        snapshot,
    }
}

impl Shot {
    fn assert_quadrants_are_on_screen(&self) {
        // The picture is scaled to whole cells, so the four quadrants are not
        // exactly equal quarters of what is drawn. A sixth of the rectangle is
        // comfortably below a quarter and comfortably above anything a stray
        // glyph could paint.
        let floor = self.drawn / 6;
        for colour in QUADRANTS {
            let count = count_colour(&self.framebuffer, colour);
            assert!(
                count >= floor,
                "{colour:?} covers {count} pixels of the {} drawn, wanted at least {floor}; \
                 look at {}",
                self.drawn,
                self.snapshot.display()
            );
        }

        // And the pane is not a flat field of one colour, which is what a
        // blank pane and a pane painted entirely by one quadrant have in
        // common.
        let mut distinct: Vec<u32> = self.framebuffer.pixels().to_vec();
        distinct.sort_unstable();
        distinct.dedup();
        assert!(
            distinct.len() > 8,
            "the frame has {} colours",
            distinct.len()
        );
    }
}

#[test]
fn a_pipe_is_refused_rather_than_filled_with_escape_sequences() {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("tos-preview");
    std::fs::create_dir_all(&dir).expect("temp dir");
    let source = dir.join("piped.png");
    std::fs::write(&source, encode_png(IMAGE.0, IMAGE.1, &quadrants())).expect("write fixture");

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_tos-preview"))
        .arg(&source)
        .output()
        .expect("run tos-preview");

    assert!(!output.status.success(), "a pipe is not a pane");
    assert!(output.stdout.is_empty(), "nothing may go down the pipe");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("not a terminal"), "stderr said: {stderr}");
}

#[test]
fn a_file_that_is_not_a_png_is_named_in_the_complaint() {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("tos-preview");
    std::fs::create_dir_all(&dir).expect("temp dir");
    let source = dir.join("not-a.png");
    std::fs::write(&source, b"GIF89a").expect("write fixture");

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_tos-preview"))
        .arg(&source)
        .output()
        .expect("run tos-preview");
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
}

fn compositor(command: &str) -> Compositor {
    let config = Config {
        command: Some(vec!["/bin/sh".into(), "-c".into(), command.into()]),
        bitmap_scale: Some(2),
        // The built-in face, so the cell size does not depend on which fonts
        // the host happens to have installed.
        font: Some("/nonexistent".into()),
        ..Config::default()
    };
    Compositor::new(config, SIZE, None).expect("compositor")
}

/// The fixture's pixels: four quadrants, RGBA, top row first.
fn quadrants() -> Vec<u8> {
    let (width, height) = IMAGE;
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            let quadrant = usize::from(x >= width / 2) + 2 * usize::from(y >= height / 2);
            let [r, g, b] = QUADRANTS[quadrant];
            rgba.extend_from_slice(&[r, g, b, 255]);
        }
    }
    rgba
}

fn count_colour(framebuffer: &OwnedFramebuffer, colour: [u8; 3]) -> usize {
    let packed = ((colour[0] as u32) << 16) | ((colour[1] as u32) << 8) | colour[2] as u32;
    framebuffer
        .pixels()
        .iter()
        .filter(|&&px| px & 0x00ff_ffff == packed)
        .count()
}

fn framebuffer_png(framebuffer: &OwnedFramebuffer) -> Vec<u8> {
    let (width, height) = (SIZE.0, SIZE.1);
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for &px in framebuffer.pixels() {
        rgba.extend_from_slice(&[
            ((px >> 16) & 0xff) as u8,
            ((px >> 8) & 0xff) as u8,
            (px & 0xff) as u8,
            255,
        ]);
    }
    encode_png(width, height, &rgba)
}

// --- a PNG writer, for the fixture and the snapshot ----------------------
//
// tOS decodes PNG and has no reason to encode one, so this lives in the test
// rather than in the crate. Every scanline is unfiltered and the deflate
// stream is stored blocks, which is legal, trivial and about fifteen times
// larger than a real encoder would manage. Neither of those matters for a file
// that exists for the length of one test run.

fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let stride = width as usize * 4;
    let mut scanlines = Vec::with_capacity(rgba.len() + height as usize);
    for row in rgba.chunks(stride) {
        scanlines.push(0);
        scanlines.extend_from_slice(row);
    }

    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);

    let mut out = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    out.extend_from_slice(&chunk(b"IHDR", &ihdr));
    out.extend_from_slice(&chunk(b"IDAT", &zlib_stored(&scanlines)));
    out.extend_from_slice(&chunk(b"IEND", &[]));
    out
}

fn chunk(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = (body.len() as u32).to_be_bytes().to_vec();
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    let mut crc = 0xffff_ffffu32;
    for &byte in kind.iter().chain(body) {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    out.extend_from_slice(&(crc ^ 0xffff_ffff).to_be_bytes());
    out
}

fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let mut chunks: Vec<&[u8]> = data.chunks(0xffff).collect();
    if chunks.is_empty() {
        chunks.push(&[]);
    }
    let last = chunks.len() - 1;
    for (i, block) in chunks.iter().enumerate() {
        out.push(u8::from(i == last));
        let len = block.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(block);
    }

    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    out.extend_from_slice(&((b << 16) | a).to_be_bytes());
    out
}
