//! Run a program in a headless tOS pane, drive it with scripted pointer and
//! keyboard input, and save what the screen looked like.
//!
//! ```text
//! cargo run --release --example pane_drive -- OUT_DIR PROGRAM [ARG] [step ...]
//! ```
//!
//! `PROGRAM` is anything that runs in a pane; the first argument after it is
//! passed through (a url, a file). Steps, in order: `wait:MS`, `click:X,Y`
//! (pixels in the pane below the first row — the row a browser keeps for its
//! url line), `move:X,Y`, `wheel:N` (notches; negative is up), `key:ctrl+l`,
//! `key:ctrl+t`, `key:ctrl+w`, `key:ctrl+tab`, `key:alt+1`, `key:enter`,
//! `key:esc`, `type:TEXT`. A snapshot is written every `SNAP_MS` milliseconds
//! (100 by default) as `frame-NNNN.ppm` in `OUT_DIR`; `magick` or `ffmpeg`
//! turn those into a strip or a GIF.
//!
//! This is the path a booted tOS takes — a `Compositor` with a pane whose
//! command is the program, pumped and rendered — minus the DRM scanout and
//! the evdev devices, which are replaced here by an `OwnedFramebuffer` and the
//! script. It was written to watch blinkterm (the browser that started in
//! this tree) scroll, click and switch tabs on a machine with no display,
//! and it is kept because a pane program that draws pictures has no other
//! way to be looked at in a test harness: the integration tests assert on
//! the graphics store, and this is how a person sees what they asserted.

use std::time::{Duration, Instant};

use tos_compositor::{Compositor, Config};
use tos_input::{InputEvent, KeyCode, KeyEvent, Modifiers, MouseAction, MouseButton, PointerEvent};
use tos_render::OwnedFramebuffer;

const SIZE: (u32, u32) = (1024, 640);
const SNAP: Duration = Duration::from_millis(100);

fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("OUT_DIR");
    let program = args.next().expect("PROGRAM");
    let url = args.next().expect("ARG");
    let steps: Vec<String> = args.collect();
    std::fs::create_dir_all(&dir)?;

    let config = Config {
        command: Some(vec![program, url]),
        bitmap_scale: Some(1),
        font: Some("/nonexistent-so-the-bitmap-font-is-used".into()),
        system_root: "/nonexistent-so-this-machine-has-no-hardware".into(),
        ..Config::default()
    };
    let mut compositor = Compositor::new(config, SIZE, None)?;
    let mut fb = OwnedFramebuffer::new(SIZE.0, SIZE.1);

    // Where the page is on the display: the pane's cells, minus the one row
    // a browser keeps for its url line.
    let (cw, ch) = compositor.cell_size();
    let area = compositor.grid_area();
    let origin = ((area.x * cw) as f64, ((area.y + 1) * ch) as f64);
    println!("cell {cw}x{ch}, pane {:?}, page origin {origin:?}", area);

    let start = Instant::now();
    let mut next_snap = start;
    let mut frame = 0u32;
    let mut pointer = (origin.0 + 10.0, origin.1 + 10.0);

    let mut run_until = |compositor: &mut Compositor, fb: &mut OwnedFramebuffer, until: Instant| {
        while Instant::now() < until {
            compositor.pump_panes();
            compositor.tick();
            if Instant::now() >= next_snap {
                {
                    let mut surface = fb.surface();
                    compositor.render_frame(&mut surface, frame > 0);
                }
                std::fs::write(format!("{dir}/frame-{frame:04}.ppm"), fb.to_ppm()).unwrap();
                frame += 1;
                next_snap += SNAP;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    };

    // Let the engine come up and paint before anything is asked of it.
    run_until(
        &mut compositor,
        &mut fb,
        start + Duration::from_millis(2500),
    );

    for step in &steps {
        let (what, arg) = step.split_once(':').unwrap_or((step.as_str(), ""));
        println!("{:>6.2}s  {step}", start.elapsed().as_secs_f64());
        match what {
            "wait" => {
                let ms: u64 = arg.parse().expect("wait:MS");
                run_until(
                    &mut compositor,
                    &mut fb,
                    Instant::now() + Duration::from_millis(ms),
                );
            }
            "move" | "click" => {
                let (x, y) = arg.split_once(',').expect("X,Y");
                pointer = (
                    origin.0 + x.parse::<f64>().unwrap(),
                    origin.1 + y.parse::<f64>().unwrap(),
                );
                compositor.handle_input(InputEvent::Pointer(PointerEvent {
                    button: None,
                    action: MouseAction::Motion,
                    x: pointer.0,
                    y: pointer.1,
                    modifiers: Modifiers::NONE,
                }));
                if what == "click" {
                    run_until(
                        &mut compositor,
                        &mut fb,
                        Instant::now() + Duration::from_millis(150),
                    );
                    for action in [MouseAction::Press, MouseAction::Release] {
                        compositor.handle_input(InputEvent::Pointer(PointerEvent {
                            button: Some(MouseButton::Left),
                            action,
                            x: pointer.0,
                            y: pointer.1,
                            modifiers: Modifiers::NONE,
                        }));
                        run_until(
                            &mut compositor,
                            &mut fb,
                            Instant::now() + Duration::from_millis(60),
                        );
                    }
                }
            }
            "wheel" => {
                let n: i32 = arg.parse().expect("wheel:N");
                let button = if n < 0 {
                    MouseButton::WheelUp
                } else {
                    MouseButton::WheelDown
                };
                for _ in 0..n.abs() {
                    compositor.handle_input(InputEvent::Pointer(PointerEvent {
                        button: Some(button),
                        action: MouseAction::Press,
                        x: pointer.0,
                        y: pointer.1,
                        modifiers: Modifiers::NONE,
                    }));
                    run_until(
                        &mut compositor,
                        &mut fb,
                        Instant::now() + Duration::from_millis(120),
                    );
                }
            }
            "key" => {
                let (code, modifiers) = match arg {
                    "ctrl+l" => (KeyCode::Char('l'), Modifiers::CTRL),
                    "ctrl+r" => (KeyCode::Char('r'), Modifiers::CTRL),
                    "enter" => (KeyCode::Enter, Modifiers::NONE),
                    "esc" => (KeyCode::Escape, Modifiers::NONE),
                    "alt+left" => (KeyCode::Left, Modifiers::ALT),
                    "tab" => (KeyCode::Tab, Modifiers::NONE),
                    "ctrl+t" => (KeyCode::Char('t'), Modifiers::CTRL),
                    "ctrl+w" => (KeyCode::Char('w'), Modifiers::CTRL),
                    "ctrl+tab" => (KeyCode::Tab, Modifiers::CTRL),
                    "ctrl+shift+tab" => (KeyCode::Tab, Modifiers::CTRL.union(Modifiers::SHIFT)),
                    "alt+1" => (KeyCode::Char('1'), Modifiers::ALT),
                    "alt+2" => (KeyCode::Char('2'), Modifiers::ALT),
                    "alt+3" => (KeyCode::Char('3'), Modifiers::ALT),
                    other => panic!("unknown key {other}"),
                };
                compositor.handle_input(InputEvent::Key(KeyEvent::new(code, modifiers)));
                run_until(
                    &mut compositor,
                    &mut fb,
                    Instant::now() + Duration::from_millis(100),
                );
            }
            "type" => {
                for c in arg.chars() {
                    let modifiers = if c.is_ascii_uppercase() {
                        Modifiers::SHIFT
                    } else {
                        Modifiers::NONE
                    };
                    compositor
                        .handle_input(InputEvent::Key(KeyEvent::new(KeyCode::Char(c), modifiers)));
                    run_until(
                        &mut compositor,
                        &mut fb,
                        Instant::now() + Duration::from_millis(70),
                    );
                }
            }
            other => panic!("unknown step {other}"),
        }
    }

    println!("wrote {frame} snapshots to {dir}");
    Ok(())
}
