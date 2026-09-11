//! These tests fork real processes onto real pseudoterminals.

use std::time::{Duration, Instant};

use tos_pty::{Pty, PtyConfig, Winsize};

fn winsize() -> Winsize {
    Winsize::new(80, 24, 640, 384)
}

/// Read from the PTY until `needle` appears or the deadline passes.
fn read_until(pty: &mut Pty, needle: &str, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    let mut output = String::new();
    let mut buf = [0u8; 4096];
    while Instant::now() < deadline {
        if !pty.poll_readable(50).unwrap_or(false) {
            continue;
        }
        match pty.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                output.push_str(&String::from_utf8_lossy(&buf[..n]));
                if output.contains(needle) {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(_) => break,
        }
    }
    output
}

fn sh(script: &str) -> PtyConfig {
    PtyConfig::command("/bin/sh", vec!["-c".into(), script.into()], winsize())
}

#[test]
fn child_output_comes_back_through_the_master() {
    let mut pty = Pty::spawn(&sh("echo tos-works")).expect("spawn");
    let output = read_until(&mut pty, "tos-works", Duration::from_secs(5));
    assert!(output.contains("tos-works"), "got: {output:?}");
}

#[test]
fn input_written_to_the_master_reaches_the_child() {
    let mut pty = Pty::spawn(&sh("read line; echo got:$line")).expect("spawn");
    pty.write(b"ping\n").expect("write");
    let output = read_until(&mut pty, "got:ping", Duration::from_secs(5));
    assert!(output.contains("got:ping"), "got: {output:?}");
}

#[test]
fn the_child_sees_the_window_size() {
    let mut pty = Pty::spawn(&sh("stty size")).expect("spawn");
    let output = read_until(&mut pty, "24 80", Duration::from_secs(5));
    assert!(output.contains("24 80"), "got: {output:?}");
}

#[test]
fn resizing_updates_the_child() {
    let mut pty = Pty::spawn(&sh("read x; stty size")).expect("spawn");
    pty.resize(Winsize::new(100, 30, 800, 480)).expect("resize");
    pty.write(b"\n").expect("write");
    let output = read_until(&mut pty, "30 100", Duration::from_secs(5));
    assert!(output.contains("30 100"), "got: {output:?}");
}

#[test]
fn the_child_gets_a_controlling_terminal() {
    // `tty` fails unless stdin really is a terminal.
    let mut pty = Pty::spawn(&sh("tty")).expect("spawn");
    let output = read_until(&mut pty, "/dev/", Duration::from_secs(5));
    assert!(output.contains("/dev/"), "not a tty: {output:?}");
}

#[test]
fn the_environment_is_configured_for_tos() {
    let mut pty = Pty::spawn(&sh("echo $TERM/$COLORTERM/$TERM_PROGRAM")).expect("spawn");
    let output = read_until(&mut pty, "tOS", Duration::from_secs(5));
    assert!(
        output.contains("xterm-256color/truecolor/tOS"),
        "got: {output:?}"
    );
}

#[test]
fn exit_status_is_reported() {
    let mut pty = Pty::spawn(&sh("exit 3")).expect("spawn");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut status = None;
    while Instant::now() < deadline {
        if let Some(code) = pty.try_wait().expect("wait") {
            status = Some(code);
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(status, Some(3));
}

#[test]
fn reading_a_finished_child_reports_end_of_file() {
    let mut pty = Pty::spawn(&sh("exit 0")).expect("spawn");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut buf = [0u8; 1024];
    let mut saw_eof = false;
    while Instant::now() < deadline {
        if !pty.poll_readable(50).unwrap_or(false) {
            continue;
        }
        match pty.read(&mut buf) {
            Ok(0) => {
                saw_eof = true;
                break;
            }
            Ok(_) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(_) => break,
        }
    }
    assert!(saw_eof, "expected end of file once the child exited");
}

#[test]
fn reads_do_not_block_when_there_is_nothing_to_read() {
    let mut pty = Pty::spawn(&sh("sleep 5")).expect("spawn");
    let mut buf = [0u8; 16];
    let started = Instant::now();
    let result = pty.read(&mut buf);
    assert!(started.elapsed() < Duration::from_secs(1), "read blocked");
    match result {
        Ok(_) => {}
        Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::WouldBlock),
    }
}

#[test]
fn signals_reach_the_child_process_group() {
    let mut pty = Pty::spawn(&sh("sleep 30")).expect("spawn");
    pty.signal(libc_sigterm()).expect("signal");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut exited = false;
    while Instant::now() < deadline {
        if pty.try_wait().expect("wait").is_some() {
            exited = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(exited, "child ignored SIGTERM");
}

#[test]
fn which_resolves_programs_on_the_path() {
    assert!(tos_pty::which("sh").is_some());
    assert!(tos_pty::which("definitely-not-a-real-program-xyz").is_none());
    assert!(tos_pty::which("/bin/sh").is_some());
}

/// SIGTERM without pulling libc into the test's dependencies.
fn libc_sigterm() -> i32 {
    15
}

// ---------------------------------------------------------------------------
// Regressions found in review
// ---------------------------------------------------------------------------

#[test]
fn dropping_a_pty_leaves_no_zombie() {
    // A compositor closes panes for its whole life; one zombie per pane would
    // accumulate without bound.
    let pids: Vec<i32> = (0..4)
        .map(|_| {
            let pty = Pty::spawn(&sh("sleep 30")).expect("spawn");
            let pid = pty.pid();
            drop(pty);
            pid
        })
        .collect();

    let deadline = Instant::now() + Duration::from_secs(5);
    for pid in pids {
        loop {
            // `kill(pid, 0)` fails with ESRCH once the process is fully gone.
            let alive = unsafe { libc_kill(pid, 0) } == 0;
            if !alive {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "process {pid} is still around after being dropped"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[test]
fn a_non_utf8_environment_is_not_fatal() {
    // The environment tOS inherits is not under its control.
    std::env::set_var(
        std::ffi::OsStr::from_bytes(b"TOS_TEST_BINARY"),
        std::ffi::OsStr::from_bytes(b"\xff\xfe"),
    );
    let result = Pty::spawn(&sh("echo survived"));
    std::env::remove_var("TOS_TEST_BINARY");
    let mut pty = result.expect("spawning must not panic on a non-UTF-8 environment");
    let output = read_until(&mut pty, "survived", Duration::from_secs(5));
    assert!(output.contains("survived"), "got: {output:?}");
}

use std::os::unix::ffi::OsStrExt;

extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}
