//! Run `tos-preview` on a file in a tOS pane and save what the screen shows.
//!
//! ```text
//! cargo run -p tos-preview --example preview_in_pane -- picture.png /tmp/tos-preview.ppm
//! ```
//!
//! The same thing `apps/preview/tests/pane.rs` does, without the assertions and
//! with a file of your choosing, for when the question is what a particular
//! picture looks like in a pane rather than whether one arrived at all.

use std::time::{Duration, Instant};

use tos_compositor::{Compositor, Config};
use tos_render::OwnedFramebuffer;

const SIZE: (u32, u32) = (1280, 720);

fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(picture) = args.next() else {
        eprintln!("usage: preview_in_pane <picture.png> [out.ppm]");
        std::process::exit(2);
    };
    let out = args.next().unwrap_or_else(|| "tos-preview.ppm".to_string());

    // The binary this workspace just built, rather than whatever a PATH lookup
    // would find. Cargo sets `CARGO_BIN_EXE_*` for tests but not for examples,
    // so the path is worked out from where the example itself was put:
    // target/<profile>/examples/preview_in_pane sits one directory below it.
    let binary = std::env::current_exe()?
        .parent()
        .and_then(|dir| dir.parent())
        .map(|dir| dir.join("tos-preview"))
        .expect("the example is in target/<profile>/examples");
    let binary = binary.display();

    let config = Config {
        command: Some(vec![
            "/bin/sh".into(),
            "-c".into(),
            format!("{binary} {picture}; printf 'the line after the picture\\n'; sleep 30"),
        ]),
        ..Config::default()
    };
    let mut compositor = Compositor::new(config, SIZE, None)?;

    let focus = compositor.session().focus();
    let deadline = Instant::now() + Duration::from_secs(5);
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
        std::thread::sleep(Duration::from_millis(25));
    }

    let mut framebuffer = OwnedFramebuffer::new(SIZE.0, SIZE.1);
    {
        let mut surface = framebuffer.surface();
        compositor.render_frame(&mut surface, false);
    }
    std::fs::write(&out, framebuffer.to_ppm())?;

    match compositor
        .pane(focus)
        .unwrap()
        .terminal
        .graphics()
        .placements()
        .next()
    {
        Some(placement) => println!(
            "{out}: {} columns by {} rows at ({}, {})",
            placement.cols, placement.rows, placement.col, placement.row
        ),
        None => println!("{out}: nothing was placed; the pane has tos-preview's complaint on it"),
    }
    Ok(())
}
