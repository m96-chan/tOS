//! Japanese input, driven through a whole compositor.
//!
//! Everything here runs under the headless backend on a machine with no
//! keyboard, no CJK font and no dictionary: the keys arrive as
//! `InputEvent::Key`, the frames land in an `OwnedFramebuffer`, and the
//! dictionary is four entries written inline and handed over through the
//! `Source` seam `tos-ime` provides for exactly this.
//!
//! The preedit is drawn on the chrome's accent, so the tests find it by
//! looking for that colour rather than for glyphs — which is what makes them
//! independent of whether the machine running them has a face with かな in
//! it. The accent and the border colour are set to values nothing else in the
//! session uses, so a pixel wearing one is a pixel the IME painted.

use std::time::{Duration, Instant};

use tos_compositor::chrome::Chrome;
use tos_compositor::{Compositor, Config};
use tos_ime::dict::BytesSource;
use tos_input::{ImeKey, InputEvent, KeyCode, KeyEvent, Modifiers};
use tos_render::OwnedFramebuffer;
use tos_session::{Action, Axis, Direction, PaneId, Rect};
use tos_term::Rgb;

const SIZE: (u32, u32) = (640, 360);
/// The preedit's background. Nothing else in the session is this colour.
const PREEDIT: Rgb = Rgb::new(0x00, 0xff, 0x00);
/// The candidate window's border.
const BORDER: Rgb = Rgb::new(0xff, 0x00, 0x00);

fn config() -> Config {
    Config {
        command: Some(vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()]),
        bitmap_scale: Some(1),
        font: Some("/nonexistent-so-the-bitmap-font-is-used".into()),
        system_root: "/nonexistent-so-this-machine-has-no-hardware".into(),
        inactive_fade: 0,
        chrome: Chrome {
            accent: PREEDIT,
            divider_focused: BORDER,
            ..Chrome::default()
        },
        ..Config::default()
    }
}

/// A compositor with a four entry dictionary in it.
fn compositor() -> Compositor {
    let mut c = Compositor::new(config(), SIZE, None).expect("compositor");
    c.ime_mut()
        .load(&BytesSource::new(
            ";; okuri-nasi entries.\n\
             かんじ /漢字/感じ;feeling/幹事/\n\
             かんれい /慣例/寒冷/管領/艦齢/\n\
             にほんご /日本語/\n",
        ))
        .expect("the dictionary should open");
    c
}

fn press(c: &mut Compositor, code: KeyCode) {
    c.handle_input(InputEvent::Key(KeyEvent::new(code, Modifiers::NONE)));
}

fn type_romaji(c: &mut Compositor, text: &str) {
    for ch in text.chars() {
        press(c, KeyCode::Char(ch));
    }
}

/// Turn kana mode on for the focused pane, the way a user does.
fn kana_on(c: &mut Compositor) {
    c.perform(Action::ImeToggle);
}

fn render(c: &mut Compositor, retained: bool) -> OwnedFramebuffer {
    let mut framebuffer = OwnedFramebuffer::new(SIZE.0, SIZE.1);
    {
        let mut surface = framebuffer.surface();
        c.render_frame(&mut surface, retained);
    }
    framebuffer
}

fn geometry(c: &Compositor) -> Vec<(PaneId, Rect)> {
    let area = c.grid_area();
    c.session().active().geometry(area)
}

fn rect_of(c: &Compositor, id: PaneId) -> Rect {
    geometry(c)
        .into_iter()
        .find(|(pane, _)| *pane == id)
        .map(|(_, rect)| rect)
        .expect("the pane should be on screen")
}

/// Rows of `rect`, in the pane's own coordinates, that have a pixel of
/// `color` on them.
fn rows_painted(cell: (u32, u32), fb: &OwnedFramebuffer, rect: Rect, color: Rgb) -> Vec<usize> {
    let (cw, ch) = cell;
    (0..rect.height as usize)
        .filter(|row| {
            let top = (rect.y as usize + row) as u32 * ch;
            (top..top + ch).any(|y| {
                let left = rect.x * cw;
                (left..left + rect.width * cw).any(|x| fb.pixel(x, y) == color.pack())
            })
        })
        .collect()
}

/// Everything the child's terminal line has echoed back, which is the only
/// evidence from outside the compositor of what actually reached the PTY.
///
/// Read off the master directly rather than through `Pane::pump`, because the
/// question is about bytes and not about what a terminal made of them.
fn bytes_that_reached_the_pty(c: &mut Compositor) -> Vec<u8> {
    let focus = c.session().focus();
    let pane = c.pane_mut(focus).expect("the focused pane");
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    // Four empty polls in a row is the line discipline having nothing more to
    // say. A single deadline would either be flaky or slow; this is neither.
    let mut idle = 0;
    while idle < 4 {
        if pane.pty.poll_readable(50).unwrap_or(false) {
            if let Ok(n) = pane.pty.read(&mut buf) {
                if n > 0 {
                    out.extend_from_slice(&buf[..n]);
                    idle = 0;
                    continue;
                }
            }
        }
        idle += 1;
    }
    out
}

/// Wait for the child to be far enough along that the tty is echoing.
fn settle(c: &mut Compositor) {
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        c.pump_panes();
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = bytes_that_reached_the_pty(c);
}

// ---- #61: routing ------------------------------------------------------

#[test]
fn direct_mode_puts_the_same_bytes_on_the_pty_with_an_ime_as_without_one() {
    // The keys a person types at a shell, including the ones an IME is most
    // likely to want: a space, a digit, punctuation and Enter.
    let typed = "echo 12 -l /usr;\r";
    let bytes = |mut c: Compositor| {
        settle(&mut c);
        for ch in typed.chars() {
            let code = match ch {
                '\r' => KeyCode::Enter,
                ch => KeyCode::Char(ch),
            };
            press(&mut c, code);
        }
        bytes_that_reached_the_pty(&mut c)
    };

    // One compositor with a dictionary loaded and an input method sitting in
    // the `Passthrough` arm, and one with no dictionary at all. Neither has
    // been toggled into kana, so both are in Direct mode.
    let with_ime = bytes(compositor());
    let without_ime = bytes(Compositor::new(config(), SIZE, None).expect("compositor"));

    assert!(
        !with_ime.is_empty(),
        "nothing reached the PTY, so this comparison proves nothing"
    );
    assert_eq!(
        with_ime, without_ime,
        "Direct mode is not free: {with_ime:?} against {without_ime:?}"
    );
    // And the bytes really are the characters that were typed, rather than
    // two identically mangled copies of them.
    assert!(
        String::from_utf8_lossy(&with_ime).contains("echo 12 -l /usr;"),
        "{:?}",
        String::from_utf8_lossy(&with_ime)
    );
}

#[test]
fn kana_mode_is_what_makes_the_difference_and_sends_the_pane_nothing() {
    // The control for the test above: the same keys with the toggle pressed
    // reach the program as nothing at all, so the comparison there is a
    // comparison something could have failed.
    let mut c = compositor();
    settle(&mut c);
    kana_on(&mut c);
    type_romaji(&mut c, "kanji");
    assert!(
        bytes_that_reached_the_pty(&mut c).is_empty(),
        "a preedit reached the program"
    );
    // And the whole word arrives at once when it is committed.
    press(&mut c, KeyCode::Enter);
    assert_eq!(
        String::from_utf8_lossy(&bytes_that_reached_the_pty(&mut c)),
        "かんじ"
    );
}

#[test]
fn a_committed_candidate_arrives_as_the_bytes_typing_it_would_have_produced() {
    let mut c = compositor();
    settle(&mut c);
    kana_on(&mut c);
    type_romaji(&mut c, "kanji");
    press(&mut c, KeyCode::Ime(ImeKey::Convert));
    press(&mut c, KeyCode::Enter);
    let bytes = bytes_that_reached_the_pty(&mut c);
    assert_eq!(
        bytes,
        "漢字".as_bytes(),
        "{:?}",
        String::from_utf8_lossy(&bytes)
    );
    // Deliberately not bracketed: a commit is typing, and `ESC[200~` would
    // put the program into paste mode for text typed one key at a time.
    assert!(!bytes.windows(6).any(|w| w == b"\x1b[200~"));
}

#[test]
fn a_key_release_does_not_type_the_character_a_second_time() {
    // `Keymap::resolve` hands back `Passthrough` for releases too, so an IME
    // that did not check would double every keystroke.
    let mut c = compositor();
    kana_on(&mut c);
    for state in [tos_input::KeyState::Press, tos_input::KeyState::Release] {
        c.handle_input(InputEvent::Key(
            KeyEvent::new(KeyCode::Char('a'), Modifiers::NONE).with_state(state),
        ));
    }
    let focus = c.session().focus();
    assert_eq!(c.pane(focus).unwrap().ime.display(), "あ");
}

#[test]
fn the_toggle_is_a_binding_and_not_a_key_a_japanese_keyboard_sends_as_a_backtick() {
    let mut c = compositor();
    let focus = c.session().focus();
    // A backtick is a backtick, whatever key on whatever layout produced it:
    // 半角/全角 arrives as `KEY_GRAVE` and is indistinguishable from one.
    press(&mut c, KeyCode::Char('`'));
    assert!(!c.pane(focus).unwrap().ime.is_enabled());
    // The binding, and the かな key, which has nothing else it could mean.
    c.handle_input(InputEvent::Key(KeyEvent::new(
        KeyCode::Char('i'),
        Modifiers::SUPER,
    )));
    assert!(c.pane(focus).unwrap().ime.is_enabled());
    press(&mut c, KeyCode::Ime(ImeKey::KanaMode));
    assert!(!c.pane(focus).unwrap().ime.is_enabled());
}

#[test]
fn kana_is_per_pane_because_the_two_panes_are_being_typed_at_differently() {
    let mut c = compositor();
    let first = c.session().focus();
    kana_on(&mut c);
    c.perform(Action::Split(Axis::Columns));
    let second = c.session().focus();
    assert_ne!(first, second);
    assert!(c.pane(first).unwrap().ime.is_enabled());
    assert!(
        !c.pane(second).unwrap().ime.is_enabled(),
        "kana followed the focus into a pane that never asked for it"
    );
}

#[test]
fn a_pane_that_dies_with_a_preedit_open_commits_nothing_and_loses_nothing() {
    let mut c = compositor();
    c.perform(Action::Split(Axis::Columns));
    let dying = c.session().focus();
    let survivor = *c
        .session()
        .all_panes()
        .iter()
        .find(|id| **id != dying)
        .expect("two panes");
    kana_on(&mut c);
    type_romaji(&mut c, "kanji");
    press(&mut c, KeyCode::Ime(ImeKey::Convert));
    assert!(c.ime().conversion(dying).is_some());

    c.close_pane(dying);

    // Nothing was ever sent, so there is nothing to flush and nothing to
    // lose. The context went with the pane, and the candidate list — the one
    // piece that is not on the pane — went with it.
    assert!(c.pane(dying).is_none());
    assert!(c.ime().conversion(dying).is_none());
    assert!(c.is_running());
    assert!(!c.pane(survivor).unwrap().ime.is_composing());
    // And the pane that is left can still be drawn, which is the crash this
    // is really about.
    let _ = render(&mut c, false);
}

#[test]
fn a_preedit_stays_with_its_pane_when_the_focus_moves() {
    let mut c = compositor();
    let first = c.session().focus();
    kana_on(&mut c);
    type_romaji(&mut c, "kanji");
    c.perform(Action::Split(Axis::Columns));
    let second = c.session().focus();
    assert_ne!(first, second);

    // Not carried: the second pane has nothing half-typed in it.
    assert!(!c.pane(second).unwrap().ime.is_composing());
    // Not committed: the first pane still holds it, and its child was sent
    // nothing.
    assert_eq!(c.pane(first).unwrap().ime.display(), "かんじ");
    assert_eq!(c.pane(first).unwrap().pending_input(), 0);

    // And it is still there when the focus comes back.
    c.perform(Action::Focus(Direction::Left));
    assert_eq!(c.session().focus(), first);
    assert_eq!(c.pane(first).unwrap().ime.display(), "かんじ");
}

// ---- #59: drawing the preedit, and erasing it --------------------------

#[test]
fn a_preedit_is_drawn_over_the_panes_cursor_row() {
    let mut c = compositor();
    let cell = c.cell_size();
    let focus = c.session().focus();
    let rect = rect_of(&c, focus);
    let before = render(&mut c, false);
    assert!(rows_painted(cell, &before, rect, PREEDIT).is_empty());

    kana_on(&mut c);
    type_romaji(&mut c, "kanji");
    let after = render(&mut c, true);
    assert_eq!(
        rows_painted(cell, &after, rect, PREEDIT),
        vec![0],
        "the preedit is not on the cursor's row"
    );
}

#[test]
fn the_frame_after_a_commit_erases_the_preedit_rather_than_leaving_it_twice() {
    let mut c = compositor();
    let cell = c.cell_size();
    let focus = c.session().focus();
    let rect = rect_of(&c, focus);
    kana_on(&mut c);
    type_romaji(&mut c, "kanji");
    render(&mut c, false);
    let composing = render(&mut c, true);
    assert_eq!(rows_painted(cell, &composing, rect, PREEDIT), vec![0]);

    press(&mut c, KeyCode::Enter);
    // A retained frame, which is the whole difficulty: `render()` skips any
    // row the pane did not damage, and the pane has no idea the preedit was
    // there. Nothing is pumped, so the only thing that can have marked that
    // row is the IME itself.
    let committed = render(&mut c, true);
    assert!(
        rows_painted(cell, &committed, rect, PREEDIT).is_empty(),
        "the committed glyphs are on screen twice"
    );
}

#[test]
fn a_preedit_that_shrinks_gives_back_the_cells_it_had() {
    let mut c = compositor();
    let (cw, ch) = c.cell_size();
    let focus = c.session().focus();
    let rect = rect_of(&c, focus);
    kana_on(&mut c);
    type_romaji(&mut c, "nihongo");
    render(&mut c, false);
    let wide = render(&mut c, true);
    let width_of = |fb: &OwnedFramebuffer| {
        (0..rect.width * cw)
            .filter(|x| (0..ch).any(|y| fb.pixel(*x, y) == PREEDIT.pack()))
            .count()
    };
    let before = width_of(&wide);
    assert!(before > 0);

    for _ in 0..3 {
        press(&mut c, KeyCode::Backspace);
    }
    let narrow = render(&mut c, true);
    let after = width_of(&narrow);
    assert!(
        after < before,
        "a preedit that shrank left {after} cells where it had {before}"
    );
}

#[test]
fn a_pane_resized_under_an_open_preedit_draws_it_at_the_cursor_it_has_now() {
    let mut c = compositor();
    let cell = c.cell_size();
    let focus = c.session().focus();
    // Put the program's cursor a long way down, the way a program that has
    // been writing would.
    let rows = c.pane(focus).unwrap().terminal.rows();
    let feed = "\r\n".repeat(rows + 4);
    c.pane_mut(focus).unwrap().terminal.advance(feed.as_bytes());
    kana_on(&mut c);
    type_romaji(&mut c, "kanji");
    render(&mut c, false);
    let rect = rect_of(&c, focus);
    let was = rows_painted(cell, &render(&mut c, true), rect, PREEDIT);
    assert_eq!(was, vec![c.pane(focus).unwrap().terminal.cursor().y]);

    // Split it in half, which resizes the pane and reflows its terminal: the
    // cursor is on the same line of text and on a different row of the pane.
    c.perform(Action::Split(Axis::Rows));
    c.perform(Action::Focus(Direction::Up));
    assert_eq!(c.session().focus(), focus);
    let rect = rect_of(&c, focus);
    let now = c.pane(focus).unwrap().terminal.cursor().y;
    assert_ne!(was, vec![now], "the resize did not move the cursor");

    render(&mut c, false);
    assert_eq!(
        rows_painted(cell, &render(&mut c, true), rect, PREEDIT),
        vec![now],
        "the rectangle was remembered rather than recomputed from the cursor"
    );
}

#[test]
fn a_preedit_is_clipped_to_its_own_pane_rather_than_to_the_screen() {
    let mut c = compositor();
    let cell = c.cell_size();
    c.perform(Action::Split(Axis::Columns));
    let left = *c
        .session()
        .all_panes()
        .iter()
        .find(|id| **id != c.session().focus())
        .expect("two panes");
    c.perform(Action::Focus(Direction::Left));
    assert_eq!(c.session().focus(), left);
    let rect = rect_of(&c, left);
    kana_on(&mut c);
    // Far more kana than the half-width pane has columns.
    for _ in 0..40 {
        type_romaji(&mut c, "nihongo");
    }
    render(&mut c, false);
    let fb = render(&mut c, true);
    let (cw, ch) = cell;
    // Nothing of it past this pane's right edge, where a program that has no
    // idea about any of this is drawing.
    let edge = (rect.x + rect.width) * cw;
    for x in edge..SIZE.0 {
        for y in 0..rect.height * ch {
            assert_ne!(
                fb.pixel(x, y),
                PREEDIT.pack(),
                "the preedit spilled into the neighbour at {x},{y}"
            );
        }
    }
    assert_eq!(rows_painted(cell, &fb, rect, PREEDIT), vec![0]);
}

#[test]
fn a_commit_wider_than_the_pane_goes_to_the_program_whole_and_the_program_wraps_it() {
    let mut c = compositor();
    settle(&mut c);
    let focus = c.session().focus();
    let cols = c.pane(focus).unwrap().terminal.cols();
    kana_on(&mut c);
    // Two cells per kana, so this is comfortably wider than the pane.
    let kana = cols;
    for _ in 0..kana {
        type_romaji(&mut c, "a");
    }
    press(&mut c, KeyCode::Enter);

    let bytes = bytes_that_reached_the_pty(&mut c);
    let expected = "あ".repeat(kana);
    assert_eq!(
        String::from_utf8_lossy(&bytes),
        expected,
        "the IME wrapped, split or dropped what it had already handed over"
    );
    // The program is what wraps it. Those same bytes, coming back out of a
    // program that echoed them, land on more than one row — the terminal
    // wraps them, and the IME wrapped nothing it had already handed over.
    c.pane_mut(focus).unwrap().terminal.advance(&bytes);
    let text = c.pane(focus).unwrap().terminal.grid().to_text();
    assert!(
        text.lines().filter(|line| line.contains('あ')).count() > 1,
        "the program did not wrap it: {text:?}"
    );
}

// ---- #60: the candidate window -----------------------------------------

#[test]
fn the_candidate_window_is_drawn_below_the_cursor_and_not_centred() {
    let mut c = compositor();
    let cell = c.cell_size();
    let focus = c.session().focus();
    let rect = rect_of(&c, focus);
    kana_on(&mut c);
    type_romaji(&mut c, "kanrei");
    press(&mut c, KeyCode::Ime(ImeKey::Convert));
    render(&mut c, false);
    let fb = render(&mut c, true);

    let border = rows_painted(cell, &fb, rect, BORDER);
    assert!(!border.is_empty(), "no candidate window was drawn");
    assert_eq!(
        border.first().copied(),
        Some(1),
        "the box is not immediately under the cursor's row: {border:?}"
    );
    // Four candidates, two borders.
    assert_eq!(border.len(), 6, "{border:?}");
    // At the cursor, which is column zero here, rather than centred in the
    // screen the way an overlay would be. The leftmost cell of the box is
    // allowed to be blank, because the built-in bitmap face has no corner
    // glyph and this is not a test about fonts.
    let (cw, ch) = cell;
    let leftmost =
        (0..rect.width * cw).find(|x| (ch..ch * 7).any(|y| fb.pixel(*x, y) == BORDER.pack()));
    assert!(
        leftmost.is_some_and(|x| x < cw * 2),
        "the box was centred rather than put at the cursor: {leftmost:?}"
    );
}

#[test]
fn the_candidate_window_goes_above_the_cursor_when_there_is_no_room_below() {
    let mut c = compositor();
    let cell = c.cell_size();
    let focus = c.session().focus();
    let rows = c.pane(focus).unwrap().terminal.rows();
    // Drive the program's cursor to the last row of the pane.
    let feed = "\r\n".repeat(rows + 4);
    c.pane_mut(focus).unwrap().terminal.advance(feed.as_bytes());
    let cursor = c.pane(focus).unwrap().terminal.cursor().y;
    assert_eq!(cursor, rows - 1);

    let rect = rect_of(&c, focus);
    kana_on(&mut c);
    type_romaji(&mut c, "kanrei");
    press(&mut c, KeyCode::Ime(ImeKey::Convert));
    render(&mut c, false);
    let fb = render(&mut c, true);

    let border = rows_painted(cell, &fb, rect, BORDER);
    assert!(!border.is_empty(), "no candidate window was drawn");
    assert!(
        border.iter().all(|row| *row < cursor),
        "the box covered the text being converted: {border:?} against {cursor}"
    );
    assert_eq!(border.last().copied(), Some(cursor - 1));
}

#[test]
fn the_candidate_window_does_not_own_the_keyboard() {
    let mut c = compositor();
    let focus = c.session().focus();
    kana_on(&mut c);
    type_romaji(&mut c, "kanji");
    press(&mut c, KeyCode::Ime(ImeKey::Convert));
    assert!(c.ime().conversion(focus).is_some());

    // Typing during a conversion abandons it and extends the preedit — the
    // opposite of what typing into an overlay does, which is why this is not
    // one.
    type_romaji(&mut c, "ya");
    assert!(c.ime().conversion(focus).is_none());
    assert_eq!(c.pane(focus).unwrap().ime.display(), "かんじや");

    // And a binding still fires while a list is up, which an overlay would
    // have swallowed.
    type_romaji(&mut c, "kanji");
    press(&mut c, KeyCode::Ime(ImeKey::Convert));
    c.handle_input(InputEvent::Key(KeyEvent::new(
        KeyCode::Char('d'),
        Modifiers::SUPER,
    )));
    assert_eq!(c.session().all_panes().len(), 2);
}

#[test]
fn a_number_key_picks_a_candidate_out_of_the_window() {
    let mut c = compositor();
    settle(&mut c);
    kana_on(&mut c);
    type_romaji(&mut c, "kanrei");
    press(&mut c, KeyCode::Ime(ImeKey::Convert));
    press(&mut c, KeyCode::Char('2'));
    assert_eq!(
        String::from_utf8_lossy(&bytes_that_reached_the_pty(&mut c)),
        "寒冷"
    );
}

#[test]
fn escape_puts_the_kana_back_and_takes_the_window_down() {
    let mut c = compositor();
    let cell = c.cell_size();
    let focus = c.session().focus();
    let rect = rect_of(&c, focus);
    kana_on(&mut c);
    type_romaji(&mut c, "kanrei");
    press(&mut c, KeyCode::Ime(ImeKey::Convert));
    render(&mut c, false);
    assert!(!rows_painted(cell, &render(&mut c, true), rect, BORDER).is_empty());

    press(&mut c, KeyCode::Escape);
    assert_eq!(c.pane(focus).unwrap().ime.display(), "かんれい");
    // The box is gone from the screen as well as from the state, which is the
    // damage question again: it covered rows nothing else knows about.
    assert!(
        rows_painted(cell, &render(&mut c, true), rect, BORDER).is_empty(),
        "the window is still on screen"
    );
}
