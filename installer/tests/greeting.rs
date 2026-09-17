//! The greeting's picture, in a real pane, on real pixels.
//!
//! The unit tests next to `motd.rs` say what bytes the greeting writes. This
//! says the terminal at the other end does something with them: a headless
//! compositor spawns a shell, the shell writes those exact bytes to its own
//! tty, and the frame that comes back has a placement in it the size the
//! greeting asked for.
//!
//! The frame is written out as a PPM so a person can open it:
//!
//! ```text
//! cargo test -p tos-install --test greeting -- --nocapture
//! ```

use std::time::{Duration, Instant};

use tos_compositor::{Compositor, Config};
use tos_install::motd::{self, Screen};
use tos_render::OwnedFramebuffer;

const SIZE: (u32, u32) = (800, 480);

/// The picture tOS ships, which is the one a machine has at
/// [`motd::PICTURE_PATH`] — the same file the login screen draws.
const PICTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../compositor/tos-compositor/assets/splash.png"
);

#[test]
fn the_greetings_picture_reaches_the_pane() {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("tos-greeting");
    std::fs::create_dir_all(&dir).expect("temp dir");

    // A pane 100 cells wide is the terminal the greeting is written for; what
    // it asks for is worked out here so the assertion below is against the
    // rule and not against a number typed twice.
    let screen = Screen {
        cols: 100,
        rows: 30,
        cell: (8, 16),
    };
    // The whole greeting, not only the picture: what a shell prints is the
    // picture and then ten lines under it, and a picture that landed on top
    // of those lines would pass a test that only looked at the picture.
    // The size is handed over as well, so this is also what says the picture
    // still wins on a pane wide enough to draw one in cells (#162).
    let sent = motd::greeting_from(
        Some(screen),
        Some((screen.cols, screen.rows)),
        std::path::Path::new(PICTURE),
    );
    assert!(sent.contains("the terminal is the desktop"));
    assert!(sent.contains("ctrl+shift+enter"), "the keys are part of it");
    let bytes = dir.join("greeting.bin");
    std::fs::write(&bytes, sent.as_bytes()).expect("write the greeting");

    let mut compositor = compositor(&format!("cat {}; sleep 30", bytes.display()));
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

    let placed = {
        let store = compositor.pane(focus).unwrap().terminal.graphics();
        let placement = store
            .placements()
            .next()
            .expect("the greeting put nothing on the terminal");
        (placement.cols as u32, placement.rows as u32)
    };
    assert_eq!(
        placed,
        (64, 11),
        "512x170 in an 8x16 cell is 64 by 11, one picture pixel to one screen pixel"
    );

    let mut framebuffer = OwnedFramebuffer::new(SIZE.0, SIZE.1);
    {
        let mut surface = framebuffer.surface();
        compositor.render_frame(&mut surface, false);
    }
    let snapshot = dir.join("greeting.ppm");
    std::fs::write(&snapshot, framebuffer.to_ppm()).expect("write the snapshot");
    println!("wrote {}", snapshot.display());

    // The picture is drawn rather than merely accepted: its rows carry
    // hundreds of colours where a pane of text carries a handful.
    let cell = compositor.cell_size();
    let mut seen = std::collections::HashSet::new();
    for y in 0..placed.1 * cell.1 {
        for x in 0..placed.0 * cell.0 {
            seen.insert(framebuffer.pixel(x, y));
        }
    }
    assert!(
        seen.len() > 100,
        "the placement's rows hold {} colours, so nothing was painted; look at {}",
        seen.len(),
        snapshot.display()
    );
}

/// A headless compositor running one shell.
fn compositor(command: &str) -> Compositor {
    let config = Config {
        command: Some(vec!["/bin/sh".into(), "-c".into(), command.into()]),
        // The host's fonts are not this test's business, and the cell size is
        // what the picture is measured against.
        font: Some("/nonexistent".into()),
        bitmap_scale: Some(1),
        ..Config::default()
    };
    Compositor::new(config, SIZE, None).expect("compositor")
}
