//! Run the `tos` binary for real, on a pseudoterminal, in nested mode.
//!
//! This is the closest thing to booting tOS that a developer machine can do:
//! the binary takes over a terminal, starts a shell on its own PTY, renders a
//! pixel framebuffer, and encodes it back out as escape sequences.

use std::time::{Duration, Instant};

use tos_pty::{Pty, PtyConfig, Winsize};

/// Upper half block: two framebuffer rows per host cell.
const HALF_BLOCK: &str = "\u{2580}";

fn spawn_tos(args: &[&str]) -> Pty {
    let config = PtyConfig::command(
        env!("CARGO_BIN_EXE_tos"),
        args.iter().map(|s| s.to_string()).collect(),
        Winsize::new(120, 40, 960, 640),
    );
    Pty::spawn(&config).expect("spawn tos")
}

/// Collect output until `predicate` holds or time runs out.
fn read_until(pty: &mut Pty, timeout: Duration, predicate: impl Fn(&str) -> bool) -> String {
    let deadline = Instant::now() + timeout;
    let mut output = String::new();
    let mut buf = [0u8; 65536];
    while Instant::now() < deadline {
        if !pty.poll_readable(50).unwrap_or(false) {
            continue;
        }
        match pty.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                output.push_str(&String::from_utf8_lossy(&buf[..n]));
                if predicate(&output) {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(_) => break,
        }
    }
    output
}

#[test]
fn nested_mode_takes_over_the_terminal_and_paints() {
    let mut pty = spawn_tos(&[
        "--backend",
        "nested",
        "--bitmap-scale",
        "1",
        "-e",
        "/bin/sh",
        "-c",
        "sleep 10",
    ]);

    let output = read_until(&mut pty, Duration::from_secs(10), |text| {
        text.contains(HALF_BLOCK) && text.contains("\x1b[38;2;")
    });

    // It switched to the alternate screen and hid the host cursor.
    assert!(output.contains("\x1b[?1049h"), "no alternate screen");
    assert!(output.contains("\x1b[?25l"), "host cursor not hidden");
    // It painted the framebuffer as half blocks in true colour.
    assert!(output.contains(HALF_BLOCK), "no framebuffer content");
    assert!(output.contains("\x1b[38;2;"), "no true colour output");

    // Asking it to quit puts the terminal back.
    pty.write(b"\x01q").expect("write");
    let farewell = read_until(&mut pty, Duration::from_secs(5), |text| {
        text.contains("\x1b[?1049l")
    });
    assert!(
        farewell.contains("\x1b[?1049l"),
        "the host terminal was not restored: {:?}",
        &farewell[farewell.len().saturating_sub(120)..]
    );
}

#[test]
fn typing_reaches_the_shell_inside_the_nested_session() {
    let mut pty = spawn_tos(&[
        "--backend",
        "nested",
        "--bitmap-scale",
        "1",
        "--no-status-bar",
        "-e",
        "/bin/sh",
        "-c",
        // Echo whatever is typed as a distinctive colour, which shows up in
        // the nested output as a true colour escape.
        "read line; printf '\\033[48;2;7;77;177m%s\\033[0m' \"$line\"; sleep 5",
    ]);

    // Wait until it is painting before typing.
    read_until(&mut pty, Duration::from_secs(10), |text| {
        text.contains(HALF_BLOCK)
    });
    pty.write(b"hello\r").expect("write");

    let output = read_until(&mut pty, Duration::from_secs(10), |text| {
        text.contains("48;2;7;77;177")
    });
    assert!(
        output.contains("48;2;7;77;177"),
        "the keystrokes never reached the shell"
    );
}

#[test]
fn the_compositor_exits_when_its_shell_does() {
    let mut pty = spawn_tos(&[
        "--backend",
        "nested",
        "--bitmap-scale",
        "1",
        "-e",
        "/bin/sh",
        "-c",
        "exit 0",
    ]);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut status = None;
    while Instant::now() < deadline {
        // Drain output so the compositor is never blocked on a full pipe.
        let mut buf = [0u8; 65536];
        let _ = pty.read(&mut buf);
        if let Some(code) = pty.try_wait().expect("wait") {
            status = Some(code);
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(status, Some(0), "tos did not exit cleanly");
}

// The rest of this file drives the compositor directly rather than the binary.
// Turning a host terminal's mouse reports into cells of the grid panes are
// laid out in is the compositor's arithmetic, not something the escape
// sequences the backend writes can be read for.
use tos_compositor::{Compositor, Config};
use tos_input::{InputEvent, Modifiers, MouseAction, MouseButton, MouseEvent};
use tos_session::{Action, Axis, Rect};

/// A host terminal to size a compositor for. 200x50 is an ordinary window, and
/// small enough in framebuffer pixels that a grid built on it is nothing like
/// it — which is the whole difficulty the mouse has in nested mode.
const HOST: (u32, u32) = (200, 50);

/// A compositor shaped the way the nested backend shapes one.
///
/// The backend hands the compositor a framebuffer of the host's columns by
/// twice its rows, because the half block puts two pixels in every host cell,
/// and the compositor divides that by the font cell to get its own grid. At
/// 200x50 host cells that is a 200x100 pixel framebuffer and, on the built-in
/// face unscaled, a grid of 33 by 9.
fn nested_compositor() -> Compositor {
    let config = Config {
        command: Some(
            ["/bin/sh", "-c", "sleep 30"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        ),
        // The built-in face at the scale nested mode runs it, so the grid does
        // not depend on the fonts the machine running the tests happens to
        // have.
        bitmap_scale: Some(1),
        font: Some("/nonexistent".into()),
        system_root: "/nonexistent-so-this-machine-has-no-hardware".into(),
        ..Config::default()
    };
    Compositor::new(config, (HOST.0, HOST.1 * 2), None).expect("compositor")
}

#[test]
fn a_click_reported_in_host_terminal_cells_lands_on_the_pane_under_the_hand() {
    // The host's cell numbers used to be read as the compositor's own, and on
    // a display this shape the two grids barely overlap: a report for the
    // middle of the terminal, host cell (100, 25), arrived as compositor cell
    // (100, 25) on a grid 33 wide and 9 tall. It matched no pane and was
    // dropped, and so was every other click outside the top left corner —
    // which in nested mode is very nearly all of them.
    let mut c = nested_compositor();
    c.perform(Action::Split(Axis::Columns));
    let right = c.session().focus();
    let left = c
        .session()
        .all_panes()
        .into_iter()
        .find(|p| *p != right)
        .unwrap();

    let geometry = c.session().active().geometry(c.grid_area());
    let rect_of = |pane| geometry.iter().find(|(p, _)| *p == pane).unwrap().1;
    let (cw, ch) = c.cell_size();

    // Point at the middle of a pane and say where that is the way the host
    // terminal would: its column is the framebuffer pixel column, and its row
    // is half the pixel row, because the half block stacks two rows in one
    // cell.
    let press = |c: &mut Compositor, rect: Rect| {
        let x = (rect.x + rect.width / 2) * cw + cw / 2;
        let y = (rect.y + rect.height / 2) * ch + ch / 2;
        c.handle_input(InputEvent::Mouse(MouseEvent {
            button: Some(MouseButton::Left),
            action: MouseAction::Press,
            col: x as usize,
            row: (y / 2) as usize,
            modifiers: Modifiers::NONE,
        }));
    };

    press(&mut c, rect_of(left));
    assert_eq!(
        c.session().focus(),
        left,
        "a press in the left pane went somewhere else"
    );
    press(&mut c, rect_of(right));
    assert_eq!(
        c.session().focus(),
        right,
        "a press in the right pane went somewhere else"
    );
}
