//! Selecting text with the mouse, end to end: real panes, real pointer
//! events, and the text that comes out of them.

use std::time::{Duration, Instant};

use tos_compositor::{Compositor, Config};
use tos_input::{InputEvent, KeyCode, KeyEvent, Modifiers, MouseAction, MouseButton, PointerEvent};
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

/// Press a key at the compositor, the way a keyboard does.
fn key(c: &mut Compositor, code: KeyCode, modifiers: Modifiers) -> bool {
    c.handle_input(InputEvent::Key(KeyEvent::new(code, modifiers)))
}

/// One of the two combinations #153 is about.
fn ctrl_shift(c: &mut Compositor, ch: char) -> bool {
    key(
        c,
        KeyCode::Char(ch),
        Modifiers::CTRL.union(Modifiers::SHIFT),
    )
}

/// The leader key and then a key, which is the other way to reach the
/// clipboard and the only one a nested session has.
fn leader(c: &mut Compositor, ch: char) -> bool {
    key(c, KeyCode::Char('a'), Modifiers::CTRL);
    key(c, KeyCode::Char(ch), Modifiers::NONE)
}

/// What the focused pane has on screen, rows and all.
fn screen(c: &Compositor) -> String {
    let focus = c.session().focus();
    c.pane(focus).unwrap().terminal.grid().to_text()
}

/// Wait until `text` is on the focused pane's screen at least `times` over.
///
/// `times` is the whole count on the screen and not the number of new ones, so
/// a caller naming a count the screen already holds has written no wait at all
/// — and, worse, one that returns before the thing it was waiting for. It is
/// also the *first* write to reach that count that ends the wait, which is not
/// always the last write the round trip makes: #166 was a paste whose echo
/// landed one write ahead of `cat`'s copy, with the assertion under the wait
/// racing the rest. Where more than one write is coming, wait on all of them —
/// `japanese_and_more_than_one_line_survive_the_round_trip` uses [`wait_for`]
/// with a predicate over both counts rather than this.
fn wait_for_echo(c: &mut Compositor, text: &str, times: usize) -> bool {
    wait_for(c, Duration::from_secs(5), |c| {
        screen(c).matches(text).count() >= times
    })
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

// ---- ctrl+shift+c and ctrl+shift+v, which are #153 ----------------------

#[test]
fn ctrl_shift_c_copies_the_selection_and_ctrl_shift_v_pastes_it() {
    // The gesture the issue is about, done in one go: drag over a word, press
    // the two combinations every terminal emulator on the machine uses, and
    // the word is typed back into the pane. cat echoes what it is given, so
    // what the paste sent shows up on screen.
    let mut c = compositor(&["/bin/cat"]);
    c.inject(b"zulu\r\n");
    drag(&mut c, (0, 0), (3, 0));

    assert!(ctrl_shift(&mut c, 'c'));
    assert_eq!(clipboard(&c), "zulu");

    assert!(ctrl_shift(&mut c, 'v'));
    assert!(wait_for_echo(&mut c, "zulu", 2), "the paste never arrived");
}

#[test]
fn what_one_pane_copies_another_pane_can_paste() {
    let mut c = compositor(&["/bin/cat"]);
    c.inject(b"yankee\r\n");
    drag(&mut c, (0, 0), (5, 0));
    let copied_in = c.session().focus();
    assert!(ctrl_shift(&mut c, 'c'));
    assert_eq!(clipboard(&c), "yankee");

    // The split focuses the new pane, so this is the clipboard crossing from
    // one program to another and not a pane pasting to itself.
    c.perform(Action::Split(Axis::Columns));
    assert_ne!(c.session().focus(), copied_in);
    assert!(ctrl_shift(&mut c, 'v'));
    assert!(
        wait_for_echo(&mut c, "yankee", 1),
        "the clipboard did not cross the split"
    );
}

#[test]
fn japanese_and_more_than_one_line_survive_the_round_trip() {
    // Two things at once, because they break in the same place: a selection
    // is a run of cells, and both a kana and a newline are worth more than one
    // cell — the first because it is two columns wide, the second because it
    // is not a column at all.
    let mut c = compositor(&["/bin/cat"]);
    c.inject("日本語のテキスト\r\nと二行目\r\n".as_bytes());
    // The first line is eight characters in sixteen columns and the second
    // four in eight, so this covers both lines entirely.
    drag(&mut c, (0, 0), (7, 1));

    assert!(ctrl_shift(&mut c, 'c'));
    assert_eq!(clipboard(&c), "日本語のテキスト\nと二行目");

    assert!(ctrl_shift(&mut c, 'v'));
    // What the screen settles at, and why each copy is there. `inject` is not
    // the pty — it advances the terminal — so the text starts on the screen
    // once. The paste then goes the whole way round: the line discipline
    // echoes it, and `cat` writes the first line back, because `encode_paste`
    // turns the newline into a carriage return and the tty turns that into the
    // newline that completes a line. The second line is never terminated, so
    // cat is still holding it and it stays at two.
    //
    // | | injected | echoed | written by cat |
    // |---|---|---|---|
    // | 日本語のテキスト | 1 | 1 | 1 |
    // | と二行目 | 1 | 1 | — |
    //
    // The wait is on the whole settled screen rather than on any one write of
    // it. It used to be on the echo of the second line, which is the first of
    // the three to land, so it returned with the screen one write short and
    // the count below raced the scheduler for the rest — about one run in five
    // (#166). Waiting on cat's copy alone would be the same mistake with a
    // longer fuse: the order those two writes reach the master is the line
    // discipline's business and not something to infer, since it wakes the
    // reader at the newline in the middle of the buffer and flushes the echo
    // of what follows afterwards.
    assert!(
        wait_for(&mut c, Duration::from_secs(5), |c| {
            let screen = screen(c);
            screen.matches("日本語のテキスト").count() >= 3
                && screen.matches("と二行目").count() >= 2
        }),
        "the paste never came back: {:?}",
        screen(&c)
    );
    assert_eq!(
        screen(&c).matches("日本語のテキスト").count(),
        3,
        "something wrote the first line a fourth time: {:?}",
        screen(&c)
    );
    assert_eq!(
        screen(&c).matches("と二行目").count(),
        2,
        "the second line is not terminated, so cat cannot have written it: {:?}",
        screen(&c)
    );
}

#[test]
fn a_selection_made_in_the_scrollback_copies_on_the_same_keys() {
    // The selection remembers the line it was made over rather than the row
    // it was drawn on, so a copy is worth asserting from history as well as
    // from the live screen.
    let mut c = quiet();
    c.inject(b"alpha beta\r\n");
    scroll_the_screen_away(&mut c);
    let focus = c.session().focus();
    let history = c.pane(focus).unwrap().terminal.grid().scrollback_len();
    c.perform(Action::Scroll(-(history as i32)));
    assert_eq!(display_row(&c, 0), "alpha beta");

    drag(&mut c, (0, 0), (9, 0));
    assert!(ctrl_shift(&mut c, 'c'));
    assert_eq!(clipboard(&c), "alpha beta");
}

#[test]
fn copying_with_nothing_selected_leaves_the_clipboard_as_it_was() {
    // The miss this protects against: the clipboard is where somebody
    // deliberately put something, and a key pressed with no selection must
    // not empty it.
    let mut c = quiet();
    c.inject(b"alpha beta\r\n");
    // Nothing is selected yet, and the key says so rather than looking broken.
    ctrl_shift(&mut c, 'c');
    assert_eq!(
        c.notifications().status_line().as_deref(),
        Some("nothing to copy")
    );
    assert!(clipboard(&c).is_empty());

    drag(&mut c, (0, 0), (4, 0));
    assert!(ctrl_shift(&mut c, 'c'));
    assert_eq!(clipboard(&c), "alpha");

    // A resize is one of the things that drops a selection; any of them would
    // do, and this one needs no second pane.
    c.resize((640, 400));
    assert!(!has_selection(&c));
    ctrl_shift(&mut c, 'c');
    assert_eq!(
        clipboard(&c),
        "alpha",
        "an empty hand emptied the clipboard"
    );
}

#[test]
fn pasting_an_empty_clipboard_does_nothing_at_all() {
    let mut c = compositor(&["/bin/cat"]);
    assert!(clipboard(&c).is_empty());
    assert!(
        !ctrl_shift(&mut c, 'v'),
        "an empty clipboard asked for a frame"
    );
    // Not even the brackets: a program that has asked for bracketed paste
    // would otherwise be walked into paste mode and out of it around nothing.
    wait_for(&mut c, Duration::from_millis(200), |_| false);
    assert_eq!(screen(&c).trim(), "", "something reached the pane");
}

#[test]
fn neither_combination_reaches_the_program_in_the_pane() {
    // What being consumed means, from the outside. Under the legacy encoding
    // shift is dropped, so a ctrl+shift+c that got through would arrive as ^C
    // — an interrupt, which cat answers by dying — and the line discipline
    // would echo "^C" on the way. This is the cost #153 accepted, written
    // down as the thing that has to go on happening.
    let mut c = compositor(&["/bin/cat"]);
    ctrl_shift(&mut c, 'c');
    ctrl_shift(&mut c, 'v');
    wait_for(&mut c, Duration::from_millis(200), |_| false);
    assert!(
        !screen(&c).contains("^C"),
        "the interrupt reached the pane: {:?}",
        screen(&c)
    );

    // And the child is still there reading, which is the other half of not
    // having been interrupted.
    c.inject(b"still here\r\n");
    drag(&mut c, (0, 0), (9, 0));
    ctrl_shift(&mut c, 'c');
    ctrl_shift(&mut c, 'v');
    assert!(
        wait_for_echo(&mut c, "still here", 2),
        "cat stopped reading: {:?}",
        screen(&c)
    );
}

#[test]
fn the_leader_bindings_copy_and_paste_the_way_they_always_did() {
    // #153 added keys rather than moving them. A nested session needs that:
    // the host terminal takes ctrl+shift+c for its own clipboard before tOS is
    // ever offered the key, so leader y is the only way to copy inside one.
    let mut c = compositor(&["/bin/cat"]);
    c.inject(b"xray\r\n");
    drag(&mut c, (0, 0), (3, 0));

    assert!(leader(&mut c, 'y'));
    assert_eq!(clipboard(&c), "xray");
    assert!(leader(&mut c, ']'));
    assert!(wait_for_echo(&mut c, "xray", 2), "leader ] pasted nothing");
}
