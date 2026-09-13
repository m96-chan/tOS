//! The lock screen against a whole compositor: real processes on real PTYs,
//! rendered to pixels.
//!
//! The unit tests next to the code drive the state machine. These ask the two
//! questions that only a frame can answer — whether the session is still on
//! the screen, and whether a pane that goes on running while the screen is
//! locked can get its output onto it.

use std::time::{Duration, Instant};

use tos_compositor::{Compositor, Config};
use tos_input::{InputEvent, KeyCode, KeyEvent, Modifiers, MouseAction, PointerEvent};
use tos_render::OwnedFramebuffer;

const SIZE: (u32, u32) = (800, 480);
/// What every test here unlocks with.
const PASSWORD: &str = "the password";
/// And whose password it is. Named rather than left to the environment, which
/// is where `Config::default()` gets it from.
const ACCOUNT: &str = "tos";

/// A compositor running `command`, with a credential file of this test's own
/// making beside it.
fn compositor(name: &str, command: &[&str]) -> Compositor {
    built(name, command, false)
}

/// The same on a machine's console, which is the display that gets asked who
/// is there: it comes up at a login screen with no session under it (#112).
fn console(name: &str) -> Compositor {
    built(name, &["/bin/sh", "-c", "sleep 30"], true)
}

fn built(name: &str, command: &[&str], gated: bool) -> Compositor {
    let path = std::env::temp_dir().join(format!("tos-lock-test-{}-{name}", std::process::id()));
    let hash = tos_crypt::sha512crypt::hash(PASSWORD.as_bytes(), b"tOSlockscreen");
    // /etc/shadow's format, because that is the file the lock reads now: the
    // session's account among the others, and root above it with no password
    // of its own.
    std::fs::write(&path, format!("root:*:::::::\n{ACCOUNT}:{hash}:::::::\n"))
        .expect("credential file");

    let config = Config {
        command: Some(command.iter().map(|s| s.to_string()).collect()),
        bitmap_scale: Some(2),
        // Force the built-in face so the tests do not depend on the host's
        // fonts, and turn off the fade so colours can be asserted exactly.
        font: Some("/nonexistent".into()),
        inactive_fade: 0,
        credential: path,
        credential_user: ACCOUNT.into(),
        gated,
        ..Config::default()
    };
    Compositor::new(config, SIZE, None).expect("compositor")
}

/// Pump the compositor until `predicate` holds or time runs out.
fn wait_for(
    compositor: &mut Compositor,
    timeout: Duration,
    predicate: impl Fn(&Compositor) -> bool,
) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        compositor.pump_panes();
        if predicate(compositor) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    compositor.pump_panes();
    predicate(compositor)
}

/// Paint into a framebuffer that already holds the last frame, which is what
/// `retained` means and what makes a partial repaint possible at all.
fn render_onto(compositor: &mut Compositor, framebuffer: &mut OwnedFramebuffer) {
    let mut surface = framebuffer.surface();
    compositor.render_frame(&mut surface, true);
}

fn type_password(compositor: &mut Compositor, password: &str) {
    for c in password.chars() {
        compositor.handle_input(InputEvent::Key(KeyEvent::new(
            KeyCode::Char(c),
            Modifiers::NONE,
        )));
    }
    compositor.handle_input(InputEvent::Key(KeyEvent::new(
        KeyCode::Enter,
        Modifiers::NONE,
    )));
}

/// Whether anything but the background is on this row of pixels.
fn row_has_ink(framebuffer: &OwnedFramebuffer, y: u32, background: u32) -> bool {
    (0..SIZE.0).any(|x| framebuffer.pixel(x, y) != background)
}

/// Whether anything but the background is inside this square of pixels.
fn ink_in(framebuffer: &OwnedFramebuffer, at: (u32, u32), side: u32, background: u32) -> bool {
    (at.1..at.1 + side).any(|y| (at.0..at.0 + side).any(|x| framebuffer.pixel(x, y) != background))
}

/// A hand moving over the panel, in display pixels.
fn move_pointer(compositor: &mut Compositor, x: f64, y: f64) {
    compositor.handle_input(InputEvent::Pointer(PointerEvent {
        x,
        y,
        button: None,
        action: MouseAction::Motion,
        modifiers: Modifiers::NONE,
    }));
}

/// The corner of the panel, which the box in the middle of it does not reach,
/// so anything that turns up here is the arrow and nothing else.
const CORNER: (u32, u32) = (8, 8);
const CORNER_SIDE: u32 = 48;

#[test]
fn a_locked_screen_shows_nothing_of_the_session() {
    let mut c = compositor("erases", &["/bin/sh", "-c", "sleep 30"]);
    let background = Config::default().chrome.background.pack();
    let (_, ch) = c.cell_size();
    // The first text row of the top pane, and the status bar under everything.
    let text_row = ch / 2;
    let bar_row = c.grid_area().height * ch + ch / 2;

    c.inject("SECRET-IN-A-PANE".repeat(4).as_bytes());

    // A display that hands back the frame it was given last time, which is
    // the case a lock has to get right: the frame after this one repaints
    // only the cells a pane says it damaged.
    let mut framebuffer = OwnedFramebuffer::new(SIZE.0, SIZE.1);
    render_onto(&mut c, &mut framebuffer);
    assert!(
        row_has_ink(&framebuffer, text_row, background),
        "the pane never drew anything to hide"
    );
    assert!(
        row_has_ink(&framebuffer, bar_row, background),
        "the status bar never drew anything to hide"
    );

    assert!(c.lock_session());
    render_onto(&mut c, &mut framebuffer);
    assert!(
        !row_has_ink(&framebuffer, text_row, background),
        "the pane is still on the screen under the lock"
    );
    assert!(
        !row_has_ink(&framebuffer, bar_row, background),
        "the status bar is still on the screen under the lock"
    );
    // The box itself is drawn, so the lock is visible rather than the screen
    // merely being blank.
    assert!(framebuffer
        .pixels()
        .iter()
        .any(|&px| px == Config::default().chrome.divider_focused.pack()));
}

#[test]
fn every_locked_frame_erases_the_one_it_is_given() {
    // A DRM display has two buffers and hands out the one it is not showing,
    // so a clear that ran once cleared one of them. Two framebuffers, used in
    // turn, is what that looks like from here.
    let mut c = compositor("buffers", &["/bin/sh", "-c", "sleep 30"]);
    let background = Config::default().chrome.background.pack();
    let (_, ch) = c.cell_size();
    let text_row = ch / 2;

    c.inject("SECRET-IN-A-PANE".repeat(4).as_bytes());
    let mut buffers = [
        OwnedFramebuffer::new(SIZE.0, SIZE.1),
        OwnedFramebuffer::new(SIZE.0, SIZE.1),
    ];
    for buffer in &mut buffers {
        render_onto(&mut c, buffer);
        assert!(row_has_ink(buffer, text_row, background));
    }

    c.lock_session();
    for buffer in &mut buffers {
        render_onto(&mut c, buffer);
        assert!(
            !row_has_ink(buffer, text_row, background),
            "the buffer that was not shown first still holds the session"
        );
    }
}

#[test]
fn a_pane_goes_on_running_and_none_of_it_reaches_the_screen() {
    // The program behind a pane does not know the screen is locked and is not
    // told: it keeps running, its output keeps arriving, and none of it is
    // drawn until somebody has said who they are.
    let mut c = compositor(
        "running",
        &[
            "/bin/sh",
            "-c",
            "echo before-the-lock; sleep 0.4; echo after-the-lock; sleep 30",
        ],
    );
    let background = Config::default().chrome.background.pack();
    let (_, ch) = c.cell_size();
    let text_row = ch / 2;
    assert!(
        wait_for(&mut c, Duration::from_secs(5), |c| {
            let focus = c.session().focus();
            c.pane(focus)
                .unwrap()
                .terminal
                .grid()
                .to_text()
                .contains("before-the-lock")
        }),
        "the pane never started"
    );

    let mut framebuffer = OwnedFramebuffer::new(SIZE.0, SIZE.1);
    render_onto(&mut c, &mut framebuffer);
    c.lock_session();
    render_onto(&mut c, &mut framebuffer);

    let focus = c.session().focus();
    assert!(
        wait_for(&mut c, Duration::from_secs(5), |c| {
            c.pane(focus)
                .unwrap()
                .terminal
                .grid()
                .to_text()
                .contains("after-the-lock")
        }),
        "the pane stopped running under the lock"
    );
    // It arrived, and a frame painted while it was arriving shows none of it.
    render_onto(&mut c, &mut framebuffer);
    assert!(!row_has_ink(&framebuffer, text_row, background));

    // And the whole screen comes back on the password, out of the terminal
    // rather than out of the damage that was thrown away while it was locked.
    type_password(&mut c, PASSWORD);
    assert!(!c.is_locked());
    render_onto(&mut c, &mut framebuffer);
    assert!(row_has_ink(&framebuffer, text_row, background));
    assert!(c
        .pane(focus)
        .unwrap()
        .terminal
        .grid()
        .to_text()
        .contains("after-the-lock"));
}

#[test]
fn a_wrong_password_changes_nothing_but_the_message() {
    let mut c = compositor("wrong", &["/bin/sh", "-c", "sleep 30"]);
    let background = Config::default().chrome.background.pack();
    let (_, ch) = c.cell_size();
    let text_row = ch / 2;
    c.inject(b"SECRET-IN-A-PANE");

    let mut framebuffer = OwnedFramebuffer::new(SIZE.0, SIZE.1);
    render_onto(&mut c, &mut framebuffer);
    c.lock_session();
    type_password(&mut c, "not the password");
    render_onto(&mut c, &mut framebuffer);

    assert!(c.is_locked(), "a wrong password is not a way out");
    assert_eq!(c.lock_screen().expect("still locked").attempts(), 1);
    assert!(!row_has_ink(&framebuffer, text_row, background));
}

#[test]
fn a_login_screen_paints_the_arrow_where_the_hand_is() {
    // #122. `pointer_rect` deciding there ought to be an arrow is a different
    // claim from a frame having one in it, and an arrow that is not on the
    // screen is the whole of what this is about — which is why this asks the
    // pixels rather than the compositor.
    let mut c = console("login-arrow");
    assert!(
        c.is_locked(),
        "a console came up without asking who was there"
    );
    let background = Config::default().chrome.background.pack();
    let mut framebuffer = OwnedFramebuffer::new(SIZE.0, SIZE.1);

    render_onto(&mut c, &mut framebuffer);
    assert!(
        !ink_in(&framebuffer, CORNER, CORNER_SIDE, background),
        "the corner had something in it before the pointer did"
    );

    move_pointer(&mut c, 12.0, 12.0);
    render_onto(&mut c, &mut framebuffer);
    assert!(
        ink_in(&framebuffer, CORNER, CORNER_SIDE, background),
        "the login screen drew no arrow where the hand was"
    );
}

#[test]
fn a_locked_screen_paints_no_arrow() {
    // The other half, which did not change: a lock has a session behind it
    // and a hand in front of it that has not said whose it is.
    let mut c = compositor("lock-arrow", &["/bin/sh", "-c", "sleep 30"]);
    let background = Config::default().chrome.background.pack();
    let mut framebuffer = OwnedFramebuffer::new(SIZE.0, SIZE.1);

    move_pointer(&mut c, 12.0, 12.0);
    assert!(c.lock_session());
    render_onto(&mut c, &mut framebuffer);
    assert!(
        !ink_in(&framebuffer, CORNER, CORNER_SIDE, background),
        "the arrow was left on top of the password box"
    );

    move_pointer(&mut c, 20.0, 20.0);
    render_onto(&mut c, &mut framebuffer);
    assert!(!ink_in(&framebuffer, CORNER, CORNER_SIDE, background));
}
