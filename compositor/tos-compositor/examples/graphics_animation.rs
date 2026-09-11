//! Play a Kitty graphics animation in a tOS pane and save what it looks like.
//!
//! ```text
//! cargo run --example graphics_animation -- /tmp/tos-animation
//! ```
//!
//! The animation is transmitted the way a video player would send one: a base
//! image holding the static background, then one frame per moment in time
//! carrying only the rectangle the moving object covers, composed onto that
//! background. The background frame is given a negative gap so it is only ever
//! a canvas, never a picture on screen.
//!
//! Time is stepped by hand here so the snapshots are reproducible. The
//! compositor's own `tick()` does exactly the same thing with the wall clock,
//! which is what makes an animation play in a live session.

use std::time::{Duration, Instant};

use tos_compositor::{Compositor, Config};
use tos_render::OwnedFramebuffer;
use tos_term::graphics::encode_base64;

const SIZE: (u32, u32) = (1280, 720);
/// The animation is a small picture so a frame fits in one PTY write.
const WIDTH: u32 = 320;
const HEIGHT: u32 = 180;
/// Radius of the moving disc, and the half-width of the rectangle each frame
/// sends.
const RADIUS: i32 = 18;
const BOX: i32 = RADIUS + 2;
const FRAMES: u32 = 30;
const GAP_MS: u32 = 40;
/// Base64 is sent in chunks, as the protocol asks for.
const CHUNK: usize = 4096;
/// How far the clock moves between snapshots: three frames' worth, so
/// consecutive pictures are obviously different.
const STEP: Duration = Duration::from_millis(3 * GAP_MS as u64);

fn main() -> std::io::Result<()> {
    let dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "tos-animation".to_string());
    std::fs::create_dir_all(&dir)?;

    let config = Config {
        command: Some(vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf 'tOS graphics animation: %d frames, %dms gap\\n\\n' 30 40; sleep 30".into(),
        ]),
        ..Config::default()
    };
    let mut compositor = Compositor::new(config, SIZE, None)?;

    // Let the shell draw its line before the image lands under it.
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        compositor.pump_panes();
        std::thread::sleep(Duration::from_millis(25));
    }

    compositor.inject(&transmission());
    let focus = compositor.session().focus();

    let mut framebuffer = OwnedFramebuffer::new(SIZE.0, SIZE.1);
    let mut previous: Vec<u32> = Vec::new();
    let start = Instant::now();
    // The first call to a terminal's animations only tells it what time it is,
    // which is when playback starts.
    compositor
        .pane_mut(focus)
        .unwrap()
        .terminal
        .advance_animations(start);

    for snapshot in 0..10 {
        let now = start + STEP * (snapshot + 1);
        let moved = compositor
            .pane_mut(focus)
            .unwrap()
            .terminal
            .advance_animations(now);
        // After the first picture the framebuffer still holds the last frame,
        // so the renderer is asked to repaint only what the terminal damaged.
        {
            let mut surface = framebuffer.surface();
            compositor.render_frame(&mut surface, snapshot > 0);
        }

        let frame = compositor
            .pane(focus)
            .unwrap()
            .terminal
            .graphics()
            .image(1)
            .map(|image| image.current_frame())
            .unwrap_or(0);
        let changed = previous
            .iter()
            .zip(framebuffer.pixels())
            .filter(|(a, b)| a != b)
            .count();
        previous = framebuffer.pixels().to_vec();

        let path = format!("{dir}/frame-{snapshot:02}.ppm");
        std::fs::write(&path, framebuffer.to_ppm())?;
        println!(
            "{path}: showing frame {frame}, {} pixels changed{}",
            changed,
            if moved { "" } else { " (no new frame)" }
        );
    }

    println!("wrote 10 snapshots to {dir}");
    Ok(())
}

/// Every escape sequence the animation needs, in the order a player sends
/// them: the background image, its gap, one frame per moment, then play.
fn transmission() -> Vec<u8> {
    let mut out = b"\r\n".to_vec();

    // The background is transmitted and displayed as image 1, and becomes the
    // animation's root frame.
    out.extend_from_slice(&command(
        &format!("a=T,f=32,s={WIDTH},v={HEIGHT},i=1,C=1"),
        &background(),
    ));
    // A negative gap makes the root frame a canvas rather than a picture: the
    // animation never stops on it.
    out.extend_from_slice(&command("a=a,i=1,r=1,z=-1", &[]));

    for index in 0..FRAMES {
        let (left, top, pixels) = disc(index);
        out.extend_from_slice(&command(
            &format!(
                "a=f,f=32,i=1,x={left},y={top},s={},v={},c=1,z={GAP_MS}",
                BOX as u32 * 2,
                BOX as u32 * 2
            ),
            &pixels,
        ));
    }

    // v=1 loops forever, s=3 runs the animation.
    out.extend_from_slice(&command("a=a,i=1,v=1,s=3", &[]));
    out
}

/// One graphics command, chunked the way the protocol expects for payloads
/// that do not fit in a single escape sequence.
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

/// The static background: a dark gradient with a grid, so a repaint that goes
/// wrong is obvious rather than subtle.
fn background() -> Vec<u8> {
    let mut data = Vec::with_capacity((WIDTH * HEIGHT * 4) as usize);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let shade = 24 + (y * 40 / HEIGHT) as u8;
            let grid = x % 40 == 0 || y % 40 == 0;
            let value = if grid { shade + 28 } else { shade };
            data.extend_from_slice(&[value / 2, value / 2, value, 255]);
        }
    }
    data
}

/// The moving disc for a frame: where its rectangle sits, and the RGBA pixels
/// of that rectangle. The edge is transparent, so the terminal has to blend it
/// onto the background rather than just copying it.
fn disc(index: u32) -> (u32, u32, Vec<u8>) {
    let phase = index as f32 / FRAMES as f32 * std::f32::consts::TAU;
    let span_x = (WIDTH as i32 - 2 * BOX) as f32;
    let span_y = (HEIGHT as i32 - 2 * BOX) as f32;
    let centre_x = BOX as f32 + span_x * (0.5 - 0.5 * phase.cos());
    // Twice round the vertical axis for every horizontal sweep, which is
    // enough motion that a stuck frame stands out.
    let centre_y = BOX as f32 + span_y * (0.5 - 0.5 * (2.0 * phase).cos());
    let left = (centre_x as i32 - BOX).max(0);
    let top = (centre_y as i32 - BOX).max(0);

    let size = BOX * 2;
    let mut data = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let dx = (left + x) as f32 - centre_x;
            let dy = (top + y) as f32 - centre_y;
            let distance = (dx * dx + dy * dy).sqrt();
            // One pixel of feathering at the rim keeps the disc from
            // shimmering as it moves.
            let alpha = ((RADIUS as f32 - distance).clamp(0.0, 1.0) * 255.0) as u8;
            let heat = (distance / RADIUS as f32).clamp(0.0, 1.0);
            data.extend_from_slice(&[
                255,
                (200.0 - 120.0 * heat) as u8,
                (60.0 * (1.0 - heat)) as u8,
                alpha,
            ]);
        }
    }
    (left as u32, top as u32, data)
}
