//! Selecting text with the mouse, end to end: real panes, real pointer
//! events, and the text that comes out of them.

use std::time::{Duration, Instant};

use tos_compositor::{Compositor, Config};
use tos_input::{InputEvent, Modifiers, MouseAction, MouseButton, PointerEvent};
use tos_session::{Action, Axis, Direction};

const SIZE: (u32, u32) = (800, 480);

fn compositor(command: &[&str]) -> Compositor {
    let config = Config {
        command: Some(command.iter().map(|s| s.to_string()).collect()),
        bitmap_scale: Some(2),
        // The built-in face, so the cell size does not depend on the host's
        // fonts and a test can turn cells into pixels itself.
        font: Some("/nonexistent".into()),
        ..Config::default()
    };
    Compositor::new(config, SIZE, None).expect("compositor")
}

/// A pane that will sit still: nothing it prints can clear a selection out
/// from under the test.
fn quiet() -> Compositor {
    compositor(&["/bin/sh", "-c", "sleep 30"])
}

fn pointer(c: &mut Compositor, cell: (u32, u32), button: MouseButton, action: MouseAction) -> bool {
    let (cw, ch) = c.cell_size();
    c.handle_input(InputEvent::Pointer(PointerEvent {
        button: Some(button),
        action,
        x: (cell.0 * cw) as f64,
        y: (cell.1 * ch) as f64,
        modifiers: Modifiers::NONE,
    }))
}

/// Press, drag and release, the way a hand does it.
fn drag(c: &mut Compositor, from: (u32, u32), to: (u32, u32)) {
    pointer(c, from, MouseButton::Left, MouseAction::Press);
    pointer(c, to, MouseButton::Left, MouseAction::Drag);
    pointer(c, to, MouseButton::Left, MouseAction::Release);
}

/// Drag over the first word of the focused pane, wherever the layout has
/// put it.
fn drag_in_focused_pane(c: &mut Compositor) {
    let focus = c.session().focus();
    let area = c.pane(focus).unwrap().area;
    drag(c, (area.x, area.y), (area.x + 4, area.y));
}

/// Click `times` in the same place, fast enough to be one gesture.
fn click(c: &mut Compositor, cell: (u32, u32), times: usize) {
    for _ in 0..times {
        pointer(c, cell, MouseButton::Left, MouseAction::Press);
        pointer(c, cell, MouseButton::Left, MouseAction::Release);
    }
}

fn primary(c: &Compositor) -> String {
    String::from_utf8_lossy(c.clipboard('p').unwrap_or_default()).into_owned()
}

fn clipboard(c: &Compositor) -> String {
    String::from_utf8_lossy(c.clipboard('c').unwrap_or_default()).into_owned()
}

fn has_selection(c: &Compositor) -> bool {
    let focus = c.session().focus();
    c.pane(focus).unwrap().selection.is_some()
}

fn display_row(c: &Compositor, y: usize) -> String {
    let focus = c.session().focus();
    c.pane(focus)
        .unwrap()
        .terminal
        .grid()
        .display_row(y)
        .to_text()
}

/// Pump the compositor until `predicate` holds or time runs out.
fn wait_for(
    c: &mut Compositor,
    timeout: Duration,
    predicate: impl Fn(&Compositor) -> bool,
) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        c.pump_panes();
        if predicate(c) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    c.pump_panes();
    predicate(c)
}

/// Fill the screen so that whatever was on it is pushed into history.
fn scroll_the_screen_away(c: &mut Compositor) {
    let focus = c.session().focus();
    let rows = c.pane(focus).unwrap().terminal.rows();
    let filler: String = (0..rows + 2).map(|i| format!("filler {i}\r\n")).collect();
    c.inject(filler.as_bytes());
}

#[test]
fn a_drag_copies_the_text_it_covered() {
    let mut c = quiet();
    c.inject(b"alpha beta\r\ngamma\r\n");
    drag(&mut c, (0, 0), (4, 0));
    assert_eq!(primary(&c), "alpha");
}

#[test]
fn a_wrapped_line_keeps_the_space_it_wrapped_on() {
    // A row that wrapped is the same line as the next one, so the two are
    // joined with no newline between them — and the space the line wrapped on
    // sat in the last column, where trimming trailing blanks took it. The two
    // words either side of it came out of the clipboard run together, which
    // for a pasted command line is the difference between one argument and
    // two.
    let mut c = quiet();
    let focus = c.session().focus();
    let cols = c.pane(focus).unwrap().terminal.grid().cols();
    // The space lands in the last column, so `b` begins the next row and the
    // two rows are one wrapped line.
    let first = "a".repeat(cols - 1);
    c.inject(format!("{first} b\r\n").as_bytes());

    drag(&mut c, (0, 0), (0, 1));
    assert_eq!(primary(&c), format!("{first} b"));
}

#[test]
fn a_drag_across_rows_takes_both_lines() {
    let mut c = quiet();
    c.inject(b"alpha beta\r\ngamma\r\n");
    drag(&mut c, (6, 0), (4, 1));
    assert_eq!(primary(&c), "beta\ngamma");
}

/// The bug this whole thing is about: a selection pinned to the viewport
/// copies whatever text has since scrolled into its place.
#[test]
fn a_selection_copies_what_it_was_made_over_once_the_screen_has_scrolled() {
    let mut c = quiet();
    c.inject(b"alpha beta\r\n");
    drag(&mut c, (0, 0), (9, 0));
    assert_eq!(primary(&c), "alpha beta");

    scroll_the_screen_away(&mut c);
    // The row the selection was drawn on now holds something else entirely.
    assert!(
        !display_row(&c, 0).starts_with("alpha"),
        "the test did not scroll: {:?}",
        display_row(&c, 0)
    );

    assert!(c.perform(Action::Copy));
    assert_eq!(clipboard(&c), "alpha beta");
}

#[test]
fn the_highlight_follows_its_text_into_the_scrollback() {
    let mut c = quiet();
    c.inject(b"alpha beta\r\n");
    drag(&mut c, (0, 0), (9, 0));
    scroll_the_screen_away(&mut c);

    let focus = c.session().focus();
    // Off the top of the viewport there is nothing to draw.
    assert!(c.pane(focus).unwrap().display_selection().is_none());

    // Scrolling back to it puts the highlight on the row it is now shown in.
    let history = c.pane(focus).unwrap().terminal.grid().scrollback_len();
    assert!(c.perform(Action::Scroll(-(history as i32))));
    let drawn = c
        .pane(focus)
        .unwrap()
        .display_selection()
        .expect("the selection is on screen again");
    assert_eq!(display_row(&c, drawn.start.1), "alpha beta");
}

#[test]
fn a_selection_can_be_made_in_the_scrollback() {
    let mut c = quiet();
    c.inject(b"alpha beta\r\n");
    scroll_the_screen_away(&mut c);
    let focus = c.session().focus();
    let history = c.pane(focus).unwrap().terminal.grid().scrollback_len();
    c.perform(Action::Scroll(-(history as i32)));
    assert_eq!(display_row(&c, 0), "alpha beta");

    drag(&mut c, (0, 0), (9, 0));
    assert_eq!(primary(&c), "alpha beta");
}

#[test]
fn a_double_click_takes_the_word_and_a_triple_click_the_line() {
    let mut c = quiet();
    c.inject(b"cargo build --release\r\n");

    // Each gesture is on a cell of its own, because clicks in the same place
    // go on counting: a click right after a double click is a triple click.
    click(&mut c, (2, 0), 1);
    assert_eq!(primary(&c), "r");

    click(&mut c, (7, 0), 2);
    assert_eq!(primary(&c), "build");

    click(&mut c, (14, 0), 3);
    assert_eq!(primary(&c), "cargo build --release");
}

#[test]
fn clicks_far_apart_on_the_screen_are_not_one_gesture() {
    let mut c = quiet();
    c.inject(b"cargo build --release\r\n");
    pointer(&mut c, (7, 0), MouseButton::Left, MouseAction::Press);
    pointer(&mut c, (7, 0), MouseButton::Left, MouseAction::Release);
    // A press somewhere else starts counting again, so this is a first click
    // and takes one cell rather than the word under it.
    pointer(&mut c, (2, 0), MouseButton::Left, MouseAction::Press);
    pointer(&mut c, (2, 0), MouseButton::Left, MouseAction::Release);
    assert_eq!(primary(&c), "r");
}

#[test]
fn the_mouse_writes_primary_and_leaves_the_clipboard_alone() {
    let mut c = quiet();
    let payload = tos_term::graphics::encode_base64(b"deliberately copied");
    c.inject(format!("\x1b]52;c;{payload}\x07").as_bytes());
    c.pump_panes();
    assert_eq!(clipboard(&c), "deliberately copied");

    c.inject(b"alpha beta\r\n");
    drag(&mut c, (0, 0), (4, 0));
    assert_eq!(primary(&c), "alpha");
    assert_eq!(clipboard(&c), "deliberately copied");
}

#[test]
fn a_middle_click_pastes_what_the_mouse_selected() {
    // cat echoes what it is given, so what the paste sent shows up on screen.
    let mut c = compositor(&["/bin/cat"]);
    c.inject(b"zulu\r\n");
    drag(&mut c, (0, 0), (3, 0));
    assert_eq!(primary(&c), "zulu");

    pointer(&mut c, (0, 1), MouseButton::Middle, MouseAction::Press);
    assert!(
        wait_for(&mut c, Duration::from_secs(5), |c| {
            let focus = c.session().focus();
            let text = c.pane(focus).unwrap().terminal.grid().to_text();
            text.matches("zulu").count() >= 2
        }),
        "the paste never arrived"
    );
}

#[test]
fn output_from_the_program_drops_the_selection() {
    let mut c = compositor(&["/bin/sh", "-c", "sleep 0.2; echo tick"]);
    c.inject(b"alpha beta\r\n");
    drag(&mut c, (0, 0), (4, 0));
    assert!(has_selection(&c), "the drag should have selected something");

    assert!(
        wait_for(&mut c, Duration::from_secs(5), |c| {
            let focus = c.session().focus();
            c.pane(focus)
                .map(|p| p.terminal.grid().to_text().contains("tick"))
                .unwrap_or(false)
        }),
        "the child never printed"
    );
    assert!(
        !has_selection(&c),
        "output moved the text, so the selection had to go"
    );
}

#[test]
fn resizing_the_display_drops_the_selection() {
    let mut c = quiet();
    c.inject(b"alpha beta\r\n");
    drag(&mut c, (0, 0), (4, 0));
    assert!(has_selection(&c));

    c.resize((640, 400));
    assert!(!has_selection(&c));
}

#[test]
fn moving_the_focus_away_drops_the_selection() {
    let mut c = quiet();
    c.perform(Action::Split(Axis::Columns));
    let selected_in = c.session().focus();
    c.inject(b"alpha beta\r\n");
    drag_in_focused_pane(&mut c);
    assert!(c.pane(selected_in).unwrap().selection.is_some());

    assert!(c.perform(Action::Focus(Direction::Left)));
    assert_ne!(c.session().focus(), selected_in);
    assert!(
        c.pane(selected_in).unwrap().selection.is_none(),
        "the selection outlived the focus that made it"
    );
}

#[test]
fn clicking_in_another_pane_drops_the_first_pane_s_selection() {
    let mut c = quiet();
    c.perform(Action::Split(Axis::Columns));
    let right = c.session().focus();
    c.inject(b"alpha beta\r\n");
    drag_in_focused_pane(&mut c);
    assert!(c.pane(right).unwrap().selection.is_some());

    // The left pane starts at column zero, and the split put the focused one
    // to the right of it.
    let left_edge = c.pane(right).unwrap().area.x;
    assert!(left_edge > 0, "the split did not leave a pane on the left");
    pointer(&mut c, (0, 0), MouseButton::Left, MouseAction::Press);
    pointer(&mut c, (0, 0), MouseButton::Left, MouseAction::Release);
    assert_ne!(c.session().focus(), right);
    assert!(c.pane(right).unwrap().selection.is_none());
}
