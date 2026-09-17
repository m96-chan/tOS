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
use tos_render::{OwnedFramebuffer, Rect};
use tos_term::Rgb;

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
        // The pictures tOS ships, never ones the machine running the tests
        // happens to have put in /etc.
        splash: "/nonexistent".into(),
        lock_picture: "/nonexistent".into(),
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

/// Paint into a framebuffer the compositor has been told nothing about, which
/// is what a backend that does not retain its contents hands out.
fn render_onto_unknown(compositor: &mut Compositor, framebuffer: &mut OwnedFramebuffer) {
    let mut surface = framebuffer.surface();
    compositor.render_frame(&mut surface, false);
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

/// The same, over the part of a row the lock's own picture cannot reach.
///
/// That picture is in the bottom right corner and is given a third of the
/// display each way (`Splash::room_in_corner`), so the rest of the row is the
/// session's or nobody's. It is needed for the status bar and for nothing
/// else: the bar is the one thing of a session drawn low enough to share a row
/// with a corner, and asking about the whole row there would be asking whether
/// the lock drew its own picture.
fn session_row_has_ink(framebuffer: &OwnedFramebuffer, y: u32, background: u32) -> bool {
    (0..SIZE.0 - SIZE.0 / 3).any(|x| framebuffer.pixel(x, y) != background)
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
        session_row_has_ink(&framebuffer, bar_row, background),
        "the status bar never drew anything to hide"
    );

    assert!(c.lock_session());
    render_onto(&mut c, &mut framebuffer);
    assert!(
        !row_has_ink(&framebuffer, text_row, background),
        "the pane is still on the screen under the lock"
    );
    assert!(
        !session_row_has_ink(&framebuffer, bar_row, background),
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
fn a_locked_frame_erases_a_surface_it_has_not_been_promised() {
    // A display that hands out a surface without saying what is on it — two
    // buffers used in turn, which is what `retained` being false means — gets
    // the whole screen erased on every locked frame, session and all.
    //
    // The DRM backend is not that display and has not been since #105: it
    // composites into one shadow and copies out what changed, and the second
    // dumb buffer is squared by `Shadow::owed` rather than by a clear that
    // runs twice (`tos_platform::drm`'s own tests say so). That is what lets a
    // locked frame be a partial repaint at all — #164, where every locked
    // frame repainting every pixel is what was making the screen blink.
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
        render_onto_unknown(&mut c, buffer);
        assert!(row_has_ink(buffer, text_row, background));
    }

    c.lock_session();
    for buffer in &mut buffers {
        render_onto_unknown(&mut c, buffer);
        assert!(
            !row_has_ink(buffer, text_row, background),
            "the buffer that was not shown first still holds the session"
        );
    }
}

#[test]
fn a_retained_locked_frame_repaints_the_box_and_leaves_the_rest() {
    // The frames that made the screen blink: a caret that does not blink, a
    // reading that moved, a minute turning over — a whole display repainted
    // for something nobody could see (#164). What a locked frame costs now is
    // the box, and what an idle locked screen costs is nothing at all.
    let mut c = compositor("retained", &["/bin/sh", "-c", "sleep 30"]);
    let background = Config::default().chrome.background.pack();
    let (_, ch) = c.cell_size();
    let text_row = ch / 2;

    c.inject("SECRET-IN-A-PANE".repeat(4).as_bytes());
    let mut framebuffer = OwnedFramebuffer::new(SIZE.0, SIZE.1);
    wait_for(&mut c, Duration::from_secs(5), |c| c.needs_render());
    render_onto(&mut c, &mut framebuffer);
    assert!(row_has_ink(&framebuffer, text_row, background));

    assert!(c.lock_session());
    render_onto(&mut c, &mut framebuffer);
    assert!(
        !row_has_ink(&framebuffer, text_row, background),
        "the first locked frame has to erase the session"
    );

    // The second one is asked for a frame it should not be able to fill with
    // anything but the box. Painted over a surface that is deliberately not
    // the one the lock last drew: everything outside the box has to come back
    // untouched, which is what makes this a partial repaint and not a clear.
    let marker = 0x00ff_00ffu32;
    let mut scribbled = OwnedFramebuffer::new(SIZE.0, SIZE.1);
    scribbled
        .surface()
        .fill(Rect::new(0, 0, SIZE.0, SIZE.1), Rgb::new(255, 0, 255));
    render_onto(&mut c, &mut scribbled);
    assert_eq!(
        scribbled.pixel(0, 0),
        marker,
        "the corner of a retained locked frame should not have been touched"
    );
    assert!(
        (0..SIZE.0).any(|x| scribbled.pixel(x, SIZE.1 / 2) != marker),
        "the box itself should have been drawn"
    );
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

#[test]
fn the_login_picture_is_over_the_box_and_the_locks_is_in_the_corner() {
    // #132. The screen that opens the machine says what the machine is over
    // its box; the one guarding a session keeps out of the way of its box,
    // because there is a session behind it and its job is to be answered. So
    // the lock has a picture too, in the bottom right corner — the placement
    // is the whole of the difference, and this asserts it through a real
    // compositor rather than against `LockScreen::draw` alone.
    //
    // A picture is the one thing on either screen that is drawn in pixels
    // rather than cells, and that is what this counts: the box, the rule and
    // the text are three colours between them, and a picture is thousands.
    let mut framebuffer = OwnedFramebuffer::new(SIZE.0, SIZE.1);

    let mut c = console("picture-login");
    assert!(
        c.is_locked(),
        "a console came up without asking who was there"
    );
    render_onto(&mut c, &mut framebuffer);
    let over_the_box = colours_in(&framebuffer, Half::Top, Half::Left).max(colours_in(
        &framebuffer,
        Half::Top,
        Half::Right,
    ));
    assert!(
        over_the_box > 1000,
        "the login screen has {over_the_box} colours over its box, so no picture"
    );

    let mut c = compositor("picture-lock", &["/bin/sh", "-c", "sleep 30"]);
    c.inject(b"SECRET-IN-A-PANE");
    assert!(c.lock_session());
    render_onto(&mut c, &mut framebuffer);
    let corner = colours_in(&framebuffer, Half::Bottom, Half::Right);
    assert!(
        corner > 1000,
        "the locked screen has {corner} colours in its bottom right, so no picture"
    );
    for (word, down, across) in [
        ("top left", Half::Top, Half::Left),
        ("top right", Half::Top, Half::Right),
        ("bottom left", Half::Bottom, Half::Left),
    ] {
        let drawn = colours_in(&framebuffer, down, across);
        assert!(
            drawn < 16,
            "the locked screen has {drawn} colours in its {word}, which is a picture"
        );
    }
}

/// Which half of the display a count is over, on each axis.
#[derive(Clone, Copy)]
enum Half {
    Top,
    Bottom,
    Left,
    Right,
}

/// How many distinct colours are in one quarter of the display.
///
/// Where a picture's thousands of colours land is how these tests tell where
/// it was drawn without knowing what it is a picture of. A quarter with only
/// chrome in it brings a handful.
fn colours_in(framebuffer: &OwnedFramebuffer, down: Half, across: Half) -> usize {
    let rows = match down {
        Half::Top => 0..SIZE.1 / 2,
        _ => SIZE.1 / 2..SIZE.1,
    };
    let cols = match across {
        Half::Left => 0..SIZE.0 / 2,
        _ => SIZE.0 / 2..SIZE.0,
    };
    let mut seen = std::collections::HashSet::new();
    for y in rows {
        for x in cols.clone() {
            seen.insert(framebuffer.pixel(x, y));
        }
    }
    seen.len()
}
