//! How long [`tos_term::jpeg`] takes over one frame.
//!
//! The decoder is on the hot path of `tos-browser`: a screencast frame
//! arrives, is decoded here, and is handed to the terminal as raw RGB, and
//! the whole of that has to fit inside the 17 ms between two frames at 58
//! frames a second. So the number this prints is a requirement rather than a
//! curiosity, and it is an example rather than a test because it means
//! nothing outside a release build.
//!
//! ```text
//! magick -size 1280x770 gradient:'#123456-#abcdef' -sampling-factor 2x2 \
//!     -quality 85 /tmp/frame.jpg
//! cargo run --release -p tos-term --example jpeg_decode -- /tmp/frame.jpg
//! ```
//!
//! A real screencast frame — a page of text with a picture on it — is what
//! the number should be read from; a gradient is mostly flat blocks and
//! decodes faster than anything a browser sends. The files themselves are not
//! in the repository: a 1280x770 frame is 185 kB and the fixtures next door
//! are the correctness tests, which are a few hundred bytes each on purpose.

use std::time::Instant;

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: jpeg_decode <file.jpg> [rounds]");
        eprintln!("       times tos_term::jpeg::decode over one file");
        std::process::exit(2);
    };
    let rounds: u32 = args
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(200);

    let data = match std::fs::read(&path) {
        Ok(data) => data,
        Err(err) => {
            eprintln!("{path}: {err}");
            std::process::exit(1);
        }
    };

    let image = match tos_term::jpeg::decode(&data, 256 * 1024 * 1024) {
        Ok(image) => image,
        Err(err) => {
            eprintln!("{path}: {err}");
            std::process::exit(1);
        }
    };
    println!(
        "{path}: {}x{}, {:.1} kB of JPEG, {:.2} MB of RGB",
        image.width,
        image.height,
        data.len() as f64 / 1024.0,
        image.rgb.len() as f64 / (1024.0 * 1024.0),
    );

    // A few rounds first, so that the number below is not the first-touch
    // cost of the pages the output is written into.
    for _ in 0..(rounds / 10).max(1) {
        let _ = tos_term::jpeg::decode(&data, 256 * 1024 * 1024);
    }

    let mut worst = f64::MIN;
    let mut best = f64::MAX;
    let started = Instant::now();
    for _ in 0..rounds {
        let round = Instant::now();
        let decoded = tos_term::jpeg::decode(&data, 256 * 1024 * 1024).expect("decodes");
        // Touched so that nothing above can be optimised away.
        std::hint::black_box(decoded.rgb.first());
        let ms = round.elapsed().as_secs_f64() * 1000.0;
        worst = worst.max(ms);
        best = best.min(ms);
    }
    let mean = started.elapsed().as_secs_f64() * 1000.0 / rounds as f64;

    println!(
        "{rounds} decodes: {mean:.2} ms mean, {best:.2} ms best, {worst:.2} ms worst \
         ({:.1} frames a second if nothing else happened)",
        1000.0 / mean
    );
}
