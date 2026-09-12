//! What a video stream costs the cell and surface model, measured rather than
//! guessed.
//!
//! ```text
//! cargo run --release --example video_throughput
//! ```
//!
//! **Only a release build says anything true here.** Every leg below is a
//! per-pixel loop, and at `opt-level = 0` they are slower by more than an
//! order of magnitude and slower by *different* factors, so a debug run does
//! not even preserve the ordering the conclusions rest on. The harness prints
//! a warning rather than trusting the reader to remember.
//!
//! The measurement drives [`tos_render::render`] against a [`Terminal`]
//! directly instead of a whole [`Compositor`]. That is not a shortcut past the
//! real path: `Compositor::render_frame` (`src/compositor.rs:2268`) ends in
//! exactly this call, with exactly these arguments, once per pane. What it
//! leaves out is a shell writing its prompt, a status bar and whichever font
//! the machine happens to have installed — three sources of noise that would
//! make the numbers unreproducible without changing what is being measured.
//! The last section runs a real `Compositor` anyway, so the difference between
//! the isolated path and the whole program is on the record instead of assumed.
//!
//! The clock is stepped by hand, the way `graphics_animation.rs` steps it, so
//! playback advances one frame per call rather than one frame per whim of the
//! scheduler.

use std::io;
use std::time::{Duration, Instant};

use tos_compositor::{Compositor, Config};
use tos_font::{BitmapFont, FontStack};
use tos_pty::{Pty, PtyConfig, Winsize};
use tos_render::{OwnedFramebuffer, Rect, RenderOptions, TextureCache};
use tos_term::graphics::{decode_base64, encode_base64};
use tos_term::{Terminal, TerminalConfig};

/// The picture sizes, from something a terminal would plausibly show to 720p.
const SIZES: [(u32, u32); 4] = [(320, 180), (640, 360), (960, 540), (1280, 720)];
/// Frames per measurement. Sixty at a 40ms gap is two and a half seconds of
/// 25fps video, which is long enough to average out a scheduler hiccup and
/// short enough that the 720p animation run fits the store's byte budget.
const FRAMES: u32 = 60;
const GAP_MS: u32 = 40;
/// Base64 is sent in chunks, as the protocol asks for.
const CHUNK: usize = 4096;
/// The default graphics store budget, from `TerminalConfig::default`. Named
/// here because the frame-capacity table is arithmetic on it.
const STORE_BUDGET: usize = 256 * 1024 * 1024;

fn main() {
    println!("tOS video experiments — issue #14");
    if cfg!(debug_assertions) {
        println!();
        println!("  !! DEBUG BUILD. These numbers are meaningless. Re-run with --release. !!");
        println!();
    } else {
        println!("build: release");
    }

    // A run records the state of the machine it ran on, because every number
    // below is a single-threaded loop and a contended machine reports the run
    // queue instead of the work. Nobody should have to take the reporter's
    // word for how quiet it was.
    match std::fs::read_to_string("/proc/loadavg") {
        Ok(load) => println!("load: {}", load.trim()),
        Err(_) => println!("load: unknown"),
    }

    let mut bench = Bench::new();
    println!(
        "cell {}x{} px, pane {} cols x {} rows ({}x{} px), {FRAMES} frames per run, gap {GAP_MS}ms",
        bench.cell.0,
        bench.cell.1,
        bench.cols,
        bench.rows,
        bench.fb.width(),
        bench.fb.height(),
    );

    silent_retransmission(&mut bench);
    transport();
    routes(&mut bench);
    capacity();
    texture_cache(&mut bench);
    damage(&mut bench);
    whole_compositor();
}

// ---------------------------------------------------------------------------
// The harness
// ---------------------------------------------------------------------------

struct Bench {
    fb: OwnedFramebuffer,
    fonts: FontStack,
    term: Terminal,
    textures: TextureCache,
    cell: (u32, u32),
    cols: usize,
    rows: usize,
    /// What [`Bench::reset`] gives the next run. A budget of zero makes
    /// `get_or_scale` decline every lookup, which sends the renderer down its
    /// uncached path — the A/B the texture cache section needs.
    cache_budget: usize,
}

impl Bench {
    fn new() -> Bench {
        // The bitmap font rather than whatever the machine has installed, so
        // the cell size — and therefore how many rows an image covers, and
        // therefore how much damage it does — is the same on every machine
        // that runs this.
        let fonts = FontStack::new(Box::new(BitmapFont::new(1)));
        let metrics = fonts.metrics();
        let cell = (metrics.cell_width.max(1), metrics.cell_height.max(1));

        // The pane is sized from the largest picture rather than from a screen
        // size, so no run is measuring a blit the viewport quietly clipped.
        // The two spare rows are there so the damage section has rows that are
        // not the image.
        let widest = SIZES.iter().map(|s| s.0).max().unwrap_or(1);
        let tallest = SIZES.iter().map(|s| s.1).max().unwrap_or(1);
        let cols = widest.div_ceil(cell.0) as usize;
        let rows = tallest.div_ceil(cell.1) as usize + 2;

        let fb = OwnedFramebuffer::new(cols as u32 * cell.0, rows as u32 * cell.1);
        let term = Terminal::new(cols, rows, config(cell));
        Bench {
            fb,
            fonts,
            term,
            textures: TextureCache::default(),
            cell,
            cols,
            rows,
            cache_budget: tos_render::texture::DEFAULT_BUDGET,
        }
    }

    /// Start a run with nothing carried over: no stored image, no cached
    /// texture, and a framebuffer whose contents cannot make a later retained
    /// render look cheap.
    fn reset(&mut self) {
        self.term = Terminal::new(self.cols, self.rows, config(self.cell));
        self.textures = TextureCache::new(self.cache_budget);
        self.draw(true);
        self.term.clear_damage();
    }

    /// One pass of the pane renderer. `force` false is the retained path, the
    /// one that skips undamaged rows.
    fn draw(&mut self, force: bool) {
        let Bench {
            fb,
            fonts,
            term,
            textures,
            ..
        } = self;
        let options = RenderOptions {
            force,
            draw_cursor: false,
            ..RenderOptions::default()
        };
        let area = Rect::new(0, 0, fb.width(), fb.height());
        let mut surface = fb.surface();
        tos_render::render(&mut surface, area, term, fonts, textures, &options);
    }

    /// The cell extent a picture of this pixel size is placed at, which is what
    /// `c=` and `r=` carry. Sending the picture at its natural size is the case
    /// worth measuring: any other choice makes the renderer rescale, and that
    /// is a cost the protocol chose rather than one video imposes.
    fn cells_for(&self, size: (u32, u32)) -> (usize, usize) {
        (
            size.0.div_ceil(self.cell.0) as usize,
            size.1.div_ceil(self.cell.1) as usize,
        )
    }
}

fn config(cell: (u32, u32)) -> TerminalConfig {
    TerminalConfig {
        cell_width: cell.0,
        cell_height: cell.1,
        ..TerminalConfig::default()
    }
}

/// Where the time went. Every leg keeps one sample per frame rather than a
/// running total, because the columns report the lowest of them and not the
/// mean.
///
/// That is not a cosmetic choice, and it is worth being exact about what it
/// buys and what it hides. These legs are single-threaded loops over a few
/// megabytes; run one on a machine that is compiling something else and the
/// number it returns is mostly a statement about the run queue. Measured under
/// a load average of 20 on 8 cores, the mean of the 720p run moved by a factor
/// of two between consecutive invocations. The lowest sample is the frame that
/// got an uncontended core, which is the cost of the work itself — the thing a
/// conclusion about the architecture has to be built on, because the
/// alternative conclusion is about whatever else was running.
///
/// What that hides is queueing, so the table prints the median total beside
/// the sum of the minimums. When the two are close the machine was quiet and
/// the numbers can be quoted; when they are far apart the reader is looking at
/// a busy machine and should say so.
#[derive(Default, Clone)]
struct Legs {
    /// Base64 and the escape-sequence framing around it, on the sender's side.
    encode: Vec<Duration>,
    /// Base64 back to bytes, on the terminal's side, measured on its own.
    decode: Vec<Duration>,
    /// Everything `Terminal::advance` does with the frame: scanning the escape
    /// sequence, reassembling the chunks, decoding, and storing or composing.
    ingest: Vec<Duration>,
    /// Stepping the animation to the next frame and damaging its rows.
    present: Vec<Duration>,
    /// The retained render: scale and blit into the framebuffer.
    render: Vec<Duration>,
    wire_bytes: u64,
    frames: u32,
    /// The scaled-texture cache's own count for this run, which is the whole
    /// of what #12 left behind on this path.
    hits: u64,
    misses: u64,
}

/// The median of a set of samples, in milliseconds.
fn median(samples: &[Duration]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut millis: Vec<f64> = samples.iter().map(|d| d.as_secs_f64() * 1000.0).collect();
    millis.sort_by(|a, b| a.total_cmp(b));
    millis[millis.len() / 2]
}

impl Legs {
    /// The cheapest frame's cost for one leg.
    fn ms(&self, leg: &[Duration]) -> f64 {
        leg.iter()
            .min()
            .map(|d| d.as_secs_f64() * 1000.0)
            .unwrap_or(0.0)
    }

    /// Every frame's whole cost, so the spread has something to be taken over.
    fn totals(&self) -> Vec<Duration> {
        (0..self.frames as usize)
            .map(|i| {
                let at = |leg: &Vec<Duration>| leg.get(i).copied().unwrap_or_default();
                at(&self.encode) + at(&self.ingest) + at(&self.present) + at(&self.render)
            })
            .collect()
    }

    /// What ingest costs once the separately measured base64 decode is taken
    /// out of it: the store's own work, which for `a=f` is the frame compose.
    fn store_ms(&self) -> f64 {
        (self.ms(&self.ingest) - self.ms(&self.decode)).max(0.0)
    }

    fn total_ms(&self) -> f64 {
        self.ms(&self.encode)
            + self.ms(&self.ingest)
            + self.ms(&self.present)
            + self.ms(&self.render)
    }

    fn fps(&self) -> f64 {
        1000.0 / self.total_ms().max(f64::MIN_POSITIVE)
    }

    fn row(&self, label: &str) {
        println!(
            "  {label:<22} {:>9} {:>8.3} {:>8.3} {:>8.3} {:>8.3} {:>8.3} {:>9.3} {:>6.0} {:>8.3} {:>5}/{:<5}",
            kib(self.wire_bytes / self.frames.max(1) as u64),
            self.ms(&self.encode),
            self.ms(&self.decode),
            self.store_ms(),
            self.ms(&self.present),
            self.ms(&self.render),
            self.total_ms(),
            self.fps(),
            median(&self.totals()),
            self.hits,
            self.misses,
        );
    }
}

fn header() {
    println!(
        "  {:<22} {:>9} {:>8} {:>8} {:>8} {:>8} {:>8} {:>9} {:>6} {:>8} {:>11}",
        "route",
        "wire/fr",
        "encode",
        "decode",
        "store",
        "present",
        "render",
        "total",
        "fps",
        "p50",
        "hit/miss"
    );
    println!(
        "  {:-<22} {:->9} {:->8} {:->8} {:->8} {:->8} {:->8} {:->9} {:->6} {:->8} {:->11}",
        "", "", "", "", "", "", "", "", "", "", ""
    );
}

fn kib(bytes: u64) -> String {
    format!("{} KiB", bytes / 1024)
}

// ---------------------------------------------------------------------------
// A route that does not work at all
// ---------------------------------------------------------------------------

/// The obvious way to stream by retransmission is to place the image once and
/// then send `a=t` for every frame after it, since the placement already says
/// where the picture goes. It does not work, and the reason is one missing
/// call: `Terminal::handle_graphics` damages rows after `a=f`
/// (`tos-term/src/term.rs:1801`) and after `a=a` (`:1836`), but the `a=t` arm
/// (`:1808`) stores the new pixels and returns. Nothing marks the rows the
/// existing placement covers, so the old frame stays on screen until something
/// unrelated repaints it.
///
/// This is checked rather than argued, because it is a claim about behaviour
/// and the harness is standing right next to the behaviour.
fn silent_retransmission(bench: &mut Bench) {
    let size = (320u32, 180u32);
    let (cols, rows) = bench.cells_for(size);
    bench.reset();
    let first = picture(size, 0, Rect::new(0, 0, size.0, size.1));
    bench.term.advance(&command(
        &format!(
            "a=T,f=32,s={},v={},i=1,p=1,c={cols},r={rows},C=1,q=2",
            size.0, size.1
        ),
        &first,
    ));
    bench.draw(true);
    bench.term.clear_damage();

    let second = picture(size, 30, Rect::new(0, 0, size.0, size.1));
    bench.term.advance(&command(
        &format!("a=t,f=32,s={},v={},i=1,q=2", size.0, size.1),
        &second,
    ));
    let dirty = bench.term.damage().is_dirty();

    // Whether the framebuffer moved is the question the user would ask, so ask
    // it the same way: render retained, and compare against the pixels that
    // were there before.
    let before = bench.fb.pixels().to_vec();
    bench.draw(false);
    let changed = before
        .iter()
        .zip(bench.fb.pixels())
        .filter(|(a, b)| a != b)
        .count();

    println!();
    println!("a=t under a live placement — new pixels, no repaint");
    println!("  damage after the retransmission: {dirty}");
    println!("  pixels the retained render changed: {changed}");
}

// ---------------------------------------------------------------------------
// Leg 1: the wire
// ---------------------------------------------------------------------------

/// What the kernel charges to carry a frame from a player's stdout to the
/// compositor's `read()`.
///
/// Two sizes are timed and the throughput is taken from the slope between
/// them, because a single run would be measuring `fork`, `execve` and the
/// dynamic loader as much as the pseudoterminal, and those are paid once per
/// player rather than once per frame.
fn transport() {
    println!();
    println!("Transport — a child process to the compositor's read(), through a real PTY");

    let small = 4 * 1024 * 1024;
    let large = 64 * 1024 * 1024;
    let (Some(t_small), Some(t_large)) = (best_of(small), best_of(large)) else {
        println!("  unavailable: no usable `cat` on PATH, or the PTY child failed to start");
        return;
    };
    let slope = t_large - t_small;
    if slope <= 0.0 {
        println!("  unusable: the larger transfer did not take longer than the smaller one");
        return;
    }
    let per_second = (large - small) as f64 / slope;
    println!("  {:.0} MiB/s", per_second / (1024.0 * 1024.0));
    println!("  at that rate, one frame on the wire costs:");
    for (w, h) in SIZES {
        // Base64 is four bytes out for every three in, and the escape framing
        // on top of that is a few dozen bytes per 4 KiB chunk.
        let wire = (w as u64 * h as u64 * 4).div_ceil(3) * 4;
        println!(
            "    {w}x{h:<5} {:>8} {:>8.2}ms  ({:.0} fps if the wire were the only cost)",
            kib(wire),
            wire as f64 / per_second * 1000.0,
            per_second / wire as f64,
        );
    }
}

/// The quickest of three attempts. The slowest are other processes on the
/// machine, not the pseudoterminal, and the point of subtracting two of these
/// from each other is lost if either carries somebody else's scheduling.
fn best_of(bytes: usize) -> Option<f64> {
    (0..3)
        .filter_map(|_| pty_seconds(bytes))
        .fold(None, |best, t| {
            Some(match best {
                Some(b) if b < t => b,
                _ => t,
            })
        })
}

/// Seconds to move `bytes` from a child's stdout to here, including the child.
fn pty_seconds(bytes: usize) -> Option<f64> {
    let cat = tos_pty::which("cat")?;
    let path = std::env::temp_dir().join(format!("tos-video-{bytes}.b64"));
    // Base64's own alphabet, so that the terminal's output post-processing has
    // nothing to expand: a newline would become a carriage return and a
    // newline, and the byte count would stop matching the frame's.
    let alphabet: Vec<u8> = (0..bytes).map(|i| ALPHABET[i % ALPHABET.len()]).collect();
    std::fs::write(&path, &alphabet).ok()?;

    let winsize = Winsize::new(80, 24, 640, 384);
    let args = vec![path.to_string_lossy().into_owned()];
    let mut pty = Pty::spawn(&PtyConfig::command(cat, args, winsize)).ok()?;

    let start = Instant::now();
    let read = drain(&mut pty).ok()?;
    let elapsed = start.elapsed().as_secs_f64();

    let _ = pty.signal(libc::SIGKILL);
    let _ = std::fs::remove_file(&path);
    // A short read means the child died early, and timing that would be
    // timing a failure.
    (read >= bytes).then_some(elapsed)
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn drain(pty: &mut Pty) -> io::Result<usize> {
    let mut buf = vec![0u8; 64 * 1024];
    let mut total = 0usize;
    loop {
        // The master is non-blocking, so the poll is what stops this from
        // spinning, and its timeout is what stops a wedged child from hanging
        // the whole harness.
        if !pty.poll_readable(5_000)? {
            return Ok(total);
        }
        match pty.read(&mut buf) {
            Ok(0) => return Ok(total),
            Ok(n) => total += n,
            Err(err)
                if err.kind() == io::ErrorKind::WouldBlock
                    || err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
}

// ---------------------------------------------------------------------------
// Legs 2-5: the two transmission routes
// ---------------------------------------------------------------------------

fn routes(bench: &mut Bench) {
    for size in SIZES {
        println!();
        println!("{}x{} — {FRAMES} frames", size.0, size.1);
        header();
        retransmit(bench, size).row("a=T whole image");
        frames(bench, size, Rect::new(0, 0, size.0, size.1)).row("a=f frame, whole rect");
        let moved = moving_rect(size);
        frames(bench, size, moved)
            .row(&format!("a=f frame, {}x{} rect", moved.width, moved.height));
    }
}

/// Route one: send the whole picture again, every frame.
///
/// `a=T` rather than `a=t` followed by `a=p`, because the two are the same
/// bytes plus one short sequence — and because `a=t` on its own does not
/// repaint anything, which this checks below rather than asserting.
fn retransmit(bench: &mut Bench, size: (u32, u32)) -> Legs {
    let (w, h) = size;
    let (cols, rows) = bench.cells_for(size);
    bench.reset();
    let mut legs = Legs {
        frames: FRAMES,
        ..Legs::default()
    };

    for index in 0..FRAMES {
        let pixels = picture(size, index, Rect::new(0, 0, w, h));

        let start = Instant::now();
        // `p=1` so each frame replaces the last placement instead of piling a
        // new one on top of it, and `C=1` so the cursor does not walk down the
        // pane as the video plays.
        let wire = command(
            &format!("a=T,f=32,s={w},v={h},i=1,p=1,c={cols},r={rows},C=1,q=2"),
            &pixels,
        );
        legs.encode.push(start.elapsed());
        legs.wire_bytes += wire.len() as u64;

        // The decode leg on its own, on the same base64 the terminal is about
        // to see. Measured separately because it is the one part of ingest
        // that is identical on both routes, so subtracting it is what makes
        // the store's own work comparable.
        let encoded = encode_base64(&pixels);
        let start = Instant::now();
        let decoded = decode_base64(encoded.as_bytes());
        legs.decode.push(start.elapsed());
        std::hint::black_box(&decoded);

        let start = Instant::now();
        bench.term.advance(&wire);
        legs.ingest.push(start.elapsed());

        let start = Instant::now();
        bench.draw(false);
        legs.render.push(start.elapsed());
        bench.term.clear_damage();
    }
    legs.hits = bench.textures.hits();
    legs.misses = bench.textures.misses();
    legs
}

/// Route two: a base picture, then one `a=f` frame per moment, composed onto
/// it and played back by the animation clock.
///
/// `rect` is what each frame carries. A video's "what moved" is the whole
/// picture, so the interesting run is the one where `rect` is the whole image;
/// the smaller rect is there to price what the protocol was designed for.
fn frames(bench: &mut Bench, size: (u32, u32), rect: Rect) -> Legs {
    let (w, h) = size;
    let (cols, rows) = bench.cells_for(size);
    bench.reset();
    let mut legs = Legs {
        frames: FRAMES,
        ..Legs::default()
    };

    let base = picture(size, 0, Rect::new(0, 0, w, h));
    bench.term.advance(&command(
        &format!("a=T,f=32,s={w},v={h},i=1,p=1,c={cols},r={rows},C=1,q=2"),
        &base,
    ));
    // A negative gap makes the root frame a canvas rather than a picture, so
    // the animation passes straight over it instead of stopping on the still.
    bench.term.advance(&command("a=a,i=1,r=1,z=-1,q=2", &[]));
    bench.draw(true);
    bench.term.clear_damage();

    let (rx, ry) = (rect.x as u32, rect.y as u32);
    for index in 1..=FRAMES {
        let pixels = picture(size, index, rect);

        let start = Instant::now();
        // `X=1` overwrites the base pixels instead of blending onto them,
        // which is both what opaque video wants and the cheaper of the two —
        // so a route that still loses here loses on its best day.
        let wire = command(
            &format!(
                "a=f,f=32,i=1,x={rx},y={ry},s={},v={},c=1,X=1,z={GAP_MS},q=2",
                rect.width, rect.height
            ),
            &pixels,
        );
        legs.encode.push(start.elapsed());
        legs.wire_bytes += wire.len() as u64;

        let encoded = encode_base64(&pixels);
        let start = Instant::now();
        let decoded = decode_base64(encoded.as_bytes());
        legs.decode.push(start.elapsed());
        std::hint::black_box(&decoded);

        let start = Instant::now();
        bench.term.advance(&wire);
        legs.ingest.push(start.elapsed());
    }

    // Frames arrive before playback here, which is the friendly case: a real
    // stream interleaves the two and pays both costs inside one gap. Adding
    // the columns together, which is what `total` does, prices that honestly.
    bench.term.advance(&command("a=a,i=1,v=1,s=3,q=2", &[]));

    let origin = Instant::now();
    bench.term.advance_animations(origin);
    for step in 1..=FRAMES {
        let now = origin + Duration::from_millis((GAP_MS * step) as u64);
        let start = Instant::now();
        bench.term.advance_animations(now);
        legs.present.push(start.elapsed());

        let start = Instant::now();
        bench.draw(false);
        legs.render.push(start.elapsed());
        bench.term.clear_damage();
    }
    legs.hits = bench.textures.hits();
    legs.misses = bench.textures.misses();
    legs
}

/// The rectangle a frame would carry if only part of the picture moved: a
/// third of the width and height, centred, which is a generous reading of
/// "what moved" for anything that is not a talking head on a fixed camera.
fn moving_rect(size: (u32, u32)) -> Rect {
    let (w, h) = (size.0 / 3, size.1 / 3);
    Rect::new((size.0 / 3) as i32, (size.1 / 3) as i32, w.max(1), h.max(1))
}

/// One frame's pixels for a rectangle of the picture.
///
/// Video is the hard case precisely because every pixel differs from the frame
/// before it, so the pattern moves everywhere at once rather than being a
/// shape on a background that holds still.
fn picture(size: (u32, u32), index: u32, rect: Rect) -> Vec<u8> {
    let (width, height) = size;
    let phase = index.wrapping_mul(11);
    let mut data = Vec::with_capacity((rect.width as usize) * (rect.height as usize) * 4);
    for y in rect.y as u32..rect.y as u32 + rect.height {
        for x in rect.x as u32..rect.x as u32 + rect.width {
            let r = ((x + phase) * 255 / width.max(1)) as u8;
            let g = ((y + phase) * 255 / height.max(1)) as u8;
            let b = (((x ^ y) + phase) & 0xff) as u8;
            data.extend_from_slice(&[r, g, b, 255]);
        }
    }
    data
}

/// One graphics command, chunked the way the protocol expects for payloads
/// that do not fit in a single escape sequence.
fn command(control: &str, payload: &[u8]) -> Vec<u8> {
    let encoded = encode_base64(payload);
    if encoded.is_empty() {
        return format!("\x1b_G{control};\x1b\\").into_bytes();
    }
    let chunks: Vec<&[u8]> = encoded.as_bytes().chunks(CHUNK).collect();
    let mut out = Vec::with_capacity(encoded.len() + chunks.len() * 32);
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

// ---------------------------------------------------------------------------
// How much video the store can hold at all
// ---------------------------------------------------------------------------

/// An `a=f` frame is stored at full image size whatever rectangle it carried
/// (`tos-term/src/graphics.rs:392`), so an animation's memory is frames times
/// the whole picture and has nothing to do with how much of it moved. That
/// puts a hard ceiling on how many seconds of video the animation route can
/// hold, and the ceiling is short enough to print.
fn capacity() {
    println!();
    println!(
        "Animation store capacity — an a=f frame costs a whole picture, whatever rect it sent"
    );
    println!(
        "  {:<12} {:>12} {:>10} {:>12}",
        "size", "bytes/frame", "frames", "at 25fps"
    );
    for (w, h) in SIZES {
        let per_frame = w as usize * h as usize * 4;
        let held = STORE_BUDGET / per_frame;
        println!(
            "  {:<12} {:>12} {:>10} {:>11.1}s",
            format!("{w}x{h}"),
            per_frame,
            held,
            held as f64 / 25.0,
        );
    }
    println!(
        "  (budget {} MiB, TerminalConfig::default)",
        STORE_BUDGET / (1024 * 1024)
    );
}

// ---------------------------------------------------------------------------
// What #12 actually left on this path
// ---------------------------------------------------------------------------

/// Issue #12 asked for a GPU texture cache and closed without one; what landed
/// is the CPU scaled-texture cache in `tos-render/src/texture.rs`, whose key
/// carries the image's generation and its frame number (`texture.rs:103-117`).
/// Video moves both of those on every single frame — a retransmission bumps
/// the generation, an animation step changes the frame — so the cache cannot
/// hit, by construction rather than by bad luck. The tables above show that as
/// 0 hits and 60 misses, everywhere, on every route.
///
/// A cache that never hits is not free. On a miss the renderer scales into a
/// `Texture` and then blits that `Texture` into the surface; declining the
/// lookup scales straight into the surface in one pass
/// (`tos-render/src/terminal.rs:499`), for pixels a test already proves are
/// identical (`tos-render/tests/render.rs`, `the_texture_cache_changes_no_pixels`).
/// So what the second pass costs is measurable rather than arguable, and this
/// measures it.
fn texture_cache(bench: &mut Bench) {
    println!();
    println!("Texture cache — every video frame is a miss, so it is a copy nobody reuses");
    println!(
        "  {:<12} {:>12} {:>12} {:>10}",
        "size", "cached", "bypassed", "saved"
    );
    for size in SIZES {
        bench.cache_budget = tos_render::texture::DEFAULT_BUDGET;
        let cached = retransmit(bench, size);
        bench.cache_budget = 0;
        let bypassed = retransmit(bench, size);

        let (with, without) = (cached.ms(&cached.render), bypassed.ms(&bypassed.render));
        println!(
            "  {:<12} {:>10.3}ms {:>10.3}ms {:>9.0}%",
            format!("{}x{}", size.0, size.1),
            with,
            without,
            (with - without) / with * 100.0,
        );
    }
    bench.cache_budget = tos_render::texture::DEFAULT_BUDGET;
}

// ---------------------------------------------------------------------------
// The damage claim
// ---------------------------------------------------------------------------

/// The architecture claims only damaged rows repaint. That is true of text and
/// it is not true of an image: `draw_graphics` (`tos-render/src/terminal.rs:469`)
/// asks whether *any* row a placement covers is dirty, and if one is it blits
/// the whole placement. This prices the difference, with the texture cache warm
/// so that what is left is the blit and nothing else.
fn damage(bench: &mut Bench) {
    println!();
    println!("Damage — what a repaint costs, texture cache warm, no rescale in the number");
    println!(
        "  {:<34} {:>10} {:>12}",
        "what was damaged", "rows", "render"
    );

    for (w, h) in [SIZES[0], SIZES[3]] {
        let (cols, rows) = bench.cells_for((w, h));
        bench.reset();
        let pixels = picture((w, h), 0, Rect::new(0, 0, w, h));
        bench.term.advance(&command(
            &format!("a=T,f=32,s={w},v={h},i=1,p=1,c={cols},r={rows},C=1,q=2"),
            &pixels,
        ));
        // Two renders before timing: the first fills the cache, the second
        // proves the third is measuring a hit.
        bench.draw(true);
        bench.term.clear_damage();
        bench.draw(true);
        bench.term.clear_damage();

        println!("  {w}x{h}, placed over {cols}x{rows} cells:");
        // Every row the picture covers and no others, which is what an honest
        // full repaint of the image costs.
        let covered = repeat(bench, move |term| term.damage_mut().mark_range(0, rows));
        println!(
            "  {:<34} {:>10} {:>11.3}ms",
            "  every row the image covers", rows, covered
        );

        // One row of the same placement. If damage meant anything to an image
        // this would be a sixty-sixth of the line above. It is not.
        let one = repeat(bench, |term| term.damage_mut().mark_row(0));
        println!(
            "  {:<34} {:>10} {:>11.3}ms",
            "  one row of the image", 1, one
        );

        let all = repeat(bench, |term| term.damage_mut().mark_all());
        println!(
            "  {:<34} {:>10} {:>11.3}ms",
            "  the whole pane", bench.rows, all
        );

        let none = repeat(bench, |_| {});
        println!("  {:<34} {:>10} {:>11.3}ms", "  nothing", 0, none);

        // A row past the bottom of the picture is a row the image does not
        // cover, which is the control: it proves the cost above belongs to the
        // placement rather than to the render call itself.
        let outside = bench.rows - 1;
        let text = repeat(bench, move |term| term.damage_mut().mark_row(outside));
        println!(
            "  {:<34} {:>10} {:>11.3}ms",
            "  one row below the image", 1, text
        );
    }
}

/// The cheapest of many retained renders, after `prepare` decides what is
/// dirty. Repeated because a single 320x180 blit is down in the noise of the
/// clock, and taken as a minimum for the reason [`Legs`] gives.
fn repeat(bench: &mut Bench, prepare: impl Fn(&mut Terminal)) -> f64 {
    const PASSES: u32 = 40;
    let mut samples = Vec::with_capacity(PASSES as usize);
    for _ in 0..PASSES {
        prepare(&mut bench.term);
        let start = Instant::now();
        bench.draw(false);
        samples.push(start.elapsed());
        bench.term.clear_damage();
    }
    lowest(&samples)
}

/// The cheapest of a set of samples, in milliseconds.
fn lowest(samples: &[Duration]) -> f64 {
    samples
        .iter()
        .min()
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

// ---------------------------------------------------------------------------
// The same thing through the whole program
// ---------------------------------------------------------------------------

/// Everything above runs the pane renderer on its own. This runs a real
/// `Compositor` — a shell on a pseudoterminal, a status bar, the session's
/// layout — so the gap between the isolated path and the whole program is a
/// measurement rather than a hope.
fn whole_compositor() {
    println!();
    println!("Whole compositor — 640x360, a=T per frame, shell and status bar included");

    let config = Config {
        command: Some(vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()]),
        ..Config::default()
    };
    let Ok(mut compositor) = Compositor::new(config, (1280, 800), None) else {
        println!("  unavailable: no pseudoterminal here");
        return;
    };

    let (cw, ch) = compositor.cell_size();
    let size = (640u32, 360u32);
    let (cols, rows) = (size.0.div_ceil(cw), size.1.div_ceil(ch));
    let mut framebuffer = OwnedFramebuffer::new(1280, 800);
    {
        let mut surface = framebuffer.surface();
        compositor.render_frame(&mut surface, false);
    }

    let mut samples = Vec::with_capacity(FRAMES as usize);
    for index in 0..FRAMES {
        let pixels = picture(size, index, Rect::new(0, 0, size.0, size.1));
        let wire = command(
            &format!(
                "a=T,f=32,s={},v={},i=1,p=1,c={cols},r={rows},C=1,q=2",
                size.0, size.1
            ),
            &pixels,
        );
        let start = Instant::now();
        compositor.inject(&wire);
        let mut surface = framebuffer.surface();
        compositor.render_frame(&mut surface, true);
        samples.push(start.elapsed());
    }

    let per_frame = lowest(&samples);
    println!(
        "  cell {cw}x{ch} px, ingest + render {per_frame:.2}ms per frame ({:.0} fps), p50 {:.2}ms",
        1000.0 / per_frame,
        median(&samples),
    );
    println!("  (encode and the wire are on top of that, as in the tables above)");
}
