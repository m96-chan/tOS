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
