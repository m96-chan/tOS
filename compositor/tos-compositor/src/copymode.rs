//! Copy mode: the keyboard driving a selection.
//!
//! A selection that can be extended needs somewhere to remember where its
//! loose end is, and every key that moves that end has to reach it without
//! being looked up in the keymap first. The alternative was an `Action` per
//! motion — `h`, `j`, `w`, `$` and the rest as twenty more variants behind
//! the leader — and it falls apart on the second keypress: the leader disarms
//! after one key, so walking a selection ten cells to the right would be ten
//! leader presses. So this takes the keyboard for as long as it is up, the
//! way [`crate::lock`] does, and the compositor's share of it is three
//! things: enter, hand the keys over, leave.
//!
//! Nothing here touches a pane, a terminal, the clipboard or a display. The
//! state is a copy cursor and, once `v` has been pressed, the fixed end it is
//! dragging away from. Both are [`Anchor`]s, which are absolute lines rather
//! than screen rows, and that is the whole reason the cursor can walk off the
//! top of the viewport into history instead of stopping at the edge of the
//! screen: the coordinates it moves in do not know where the viewport is.
//! What the viewport should do about a cursor that has left it is
//! [`CopyMode::scroll_to_show`], which returns the number the compositor
//! hands to `scroll_display`. Computing it here rather than there is what
//! lets a test drive the whole thing — enter, walk up out of the screen,
//! select, yank — against a bare `Terminal`, with no pane and no display.
//!
//! The motions are vi's, because a keyboard copy mode is a thing only people
//! who already have those motions in their fingers go looking for, and a
//! second set of arrow-key-only bindings would be a mode that is slower than
//! the mouse it replaces:
//!
//! ```text
//!   h j k l          one cell, one line          arrows do the same
//!   w b e            word forward, back, end
//!   0 $              the ends of the line        home, end
//!   g G              the ends of the buffer      G is the live screen again
//!   ctrl+f ctrl+b    a screenful                 pagedown, pageup
//!   v                start or drop the selection
//!   y                yank what is selected, and leave    enter does too
//!   escape, q        leave, selecting nothing
//! ```

use tos_input::{KeyCode, KeyEvent, Modifiers};
use tos_term::Grid;

use crate::selection::{class_at, Anchor, Class, Selection, SelectionMode};

/// The most cells a single motion will walk over looking for a word.
///
/// A `w` at the end of a ten thousand line history with nothing but blanks
/// below it would otherwise scan every cell of every line before admitting
/// there is no next word, and it would do it again on every key repeat. The
/// bound is generous enough that no word motion within a screenful can reach
/// it, and what happens when it is reached — the cursor stops where the walk
/// gave up — is in the direction the user asked to go anyway.
const MAX_WALK: usize = 4096;

/// What copy mode did with a key.
///
/// Every variant means the key was taken. Copy mode owns the keyboard while
/// it is up, so there is no `Passthrough` here, and that absence is what
/// keeps a `j` from reaching the shell underneath while somebody is reading
/// their way back up a build log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyOutcome {
    /// Taken, and nothing moved.
    Consumed,
    /// The cursor or the selection moved: the pane needs repainting, and the
    /// viewport may need to follow.
    Changed,
    /// Yank what is selected, then leave.
    Copied,
    /// Leave, having selected nothing.
    Left,
}

/// The copy cursor, and the selection it is dragging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopyMode {
    /// Where the keyboard is pointing, in the pane's text.
    cursor: Anchor,
    /// The end that stays put, once `v` has fixed one.
    ///
    /// `None` until then, and that is a state of its own rather than a
    /// zero-width selection: the difference between looking around and
    /// selecting is the difference between a motion that highlights nothing
    /// and one that highlights everything it passes, and somebody who cannot
    /// see which of the two they are in will copy the wrong thing.
    anchor: Option<Anchor>,
}

impl CopyMode {
    /// Enter at a point in the text, which the caller works out from the
    /// terminal's own cursor.
    pub fn new(at: Anchor) -> Self {
        CopyMode {
            cursor: at,
            anchor: None,
        }
    }

    pub fn cursor(&self) -> Anchor {
        self.cursor
    }

    /// Whether `v` has been pressed and the motions are extending something.
    pub fn selecting(&self) -> bool {
        self.anchor.is_some()
    }

    /// The selection to highlight, which is nothing until `v`.
    pub fn selection(&self) -> Option<Selection> {
        let anchor = self.anchor?;
        let mut selection = Selection::new(anchor, false, SelectionMode::Cell);
        selection.drag_to(self.cursor);
        Some(selection)
    }

    /// What a `y` takes.
    ///
    /// With no selection this is the whole line the cursor is on rather than
    /// nothing at all. A `y` that did nothing would be exactly the dead key
    /// this mode exists to fix, and the line under the cursor is what
    /// somebody who navigated to it and pressed yank meant: they had already
    /// picked the line, and picking its two ends as well is work the mode can
    /// spare them.
    pub fn yanked(&self) -> Selection {
        self.selection()
            .unwrap_or_else(|| Selection::new(self.cursor, false, SelectionMode::Line))
    }

    /// What the status bar says the session is doing.
    pub fn status(&self) -> &'static str {
        if self.selecting() {
            "copy mode  [selecting]"
        } else {
            "copy mode"
        }
    }

    /// Where the copy cursor is on screen, or `None` when the viewport is not
    /// showing that line.
    ///
    /// Out of view is an answer and not a failure: a program that scrolls its
    /// own screen can move the text out from under the cursor between one
    /// keypress and the next, and drawing the cursor at the nearest edge
    /// instead would claim it is somewhere it is not.
    pub fn display_cursor(&self, grid: &Grid) -> Option<(usize, usize)> {
        let row = self.cursor.line.checked_sub(grid.display_line(0))?;
        (row < grid.rows() && self.cursor.col < grid.cols()).then_some((self.cursor.col, row))
    }

    /// How far the viewport has to move for the copy cursor to be on screen,
    /// in the units [`tos_term::Terminal::scroll_display`] takes: positive is
    /// back in time.
    ///
    /// This is what makes the top of the viewport not be the top of the
    /// world. A `k` on the first row could either clamp or scroll, and
    /// clamping would mean the keyboard can only ever select what is already
    /// on screen while the mouse can drag into history — which is the half of
    /// this feature that would go unnoticed until somebody needed the line
    /// above.
    pub fn scroll_to_show(&self, grid: &Grid) -> isize {
        let rows = grid.rows();
        if rows == 0 {
            return 0;
        }
        let top = grid.display_line(0);
        let bottom = top + rows - 1;
        if self.cursor.line < top {
            (top - self.cursor.line) as isize
        } else if self.cursor.line > bottom {
            -((self.cursor.line - bottom) as isize)
        } else {
            0
        }
    }

    /// Offer a key to copy mode.
    ///
    /// The grid comes in per keystroke rather than being held because the
    /// text moves under the mode constantly: a program writing to the pane
    /// changes what a word is, where the last line is and how far back the
    /// history goes between one press and the next.
    pub fn handle_key(&mut self, key: &KeyEvent, grid: &Grid) -> CopyOutcome {
        if !key.is_press() || matches!(key.code, KeyCode::ModifierKey(_)) {
            return CopyOutcome::Consumed;
        }
        let modifiers = key.modifiers.effective();
        let ctrl = modifiers.contains(Modifiers::CTRL);
        let page = grid.rows().max(1);
        let before = (self.cursor, self.anchor);

        match key.code {
            KeyCode::Escape => return CopyOutcome::Left,
            // Enter yanks as well as `y`, because the other two places tOS
            // asks for a decision — the launcher and the rename prompt — take
            // it with enter, and a mode where enter meant nothing would be
            // the one exception to that.
            KeyCode::Enter => return CopyOutcome::Copied,
            KeyCode::Left => self.left(),
            KeyCode::Right => self.right(grid),
            KeyCode::Up => self.up(1),
            KeyCode::Down => self.down(grid, 1),
            KeyCode::Home => self.cursor.col = 0,
            KeyCode::End => self.line_end(grid),
            KeyCode::PageUp => self.up(page),
            KeyCode::PageDown => self.down(grid, page),
            KeyCode::Char('f') if ctrl => self.down(grid, page),
            KeyCode::Char('b') if ctrl => self.up(page),
            _ => {
                let Some(ch) = typed(key) else {
                    return CopyOutcome::Consumed;
                };
                match ch {
                    'h' => self.left(),
                    'l' => self.right(grid),
                    'k' => self.up(1),
                    'j' => self.down(grid, 1),
                    'w' => self.word_forward(grid),
                    'b' => self.word_backward(grid),
                    'e' => self.word_end(grid),
                    // `^` is vi's first non-blank and `0` its first column;
                    // they are the same key here. A terminal line that is
                    // indented is a shell continuation or a compiler note,
                    // and in both of those the indentation is worth copying.
                    '0' | '^' => self.cursor.col = 0,
                    '$' => self.line_end(grid),
                    'g' => self.buffer_start(),
                    'G' => self.buffer_end(grid),
                    'v' => self.toggle_selection(),
                    'y' => return CopyOutcome::Copied,
                    // `q` as well as escape: this is a pager-shaped thing,
                    // and every pager leaves on q.
                    'q' => return CopyOutcome::Left,
                    _ => return CopyOutcome::Consumed,
                }
            }
        }

        if (self.cursor, self.anchor) == before {
            CopyOutcome::Consumed
        } else {
            CopyOutcome::Changed
        }
    }

    fn toggle_selection(&mut self) {
        self.anchor = match self.anchor {
            Some(_) => None,
            None => Some(self.cursor),
        };
    }

    fn left(&mut self) {
        self.cursor.col = self.cursor.col.saturating_sub(1);
    }

    fn right(&mut self, grid: &Grid) {
        self.cursor.col = (self.cursor.col + 1).min(grid.cols().saturating_sub(1));
    }

    fn up(&mut self, lines: usize) {
        self.cursor.line = self.cursor.line.saturating_sub(lines);
    }

    fn down(&mut self, grid: &Grid, lines: usize) {
        self.cursor.line = (self.cursor.line + lines).min(grid.total_lines().saturating_sub(1));
    }

    /// The last character on the line, or the first column when the line is
    /// blank. Not the last column of the pane: `$` on an eighty column pane
    /// showing a ten character line should not walk seventy cells of nothing.
    fn line_end(&mut self, grid: &Grid) {
        self.cursor.col = grid
            .line(self.cursor.line)
            .and_then(|row| {
                row.cells()
                    .iter()
                    .rposition(|cell| !cell.ch.is_whitespace())
            })
            .unwrap_or(0);
    }

    fn buffer_start(&mut self) {
        self.cursor = Anchor::new(0, 0);
    }

    fn buffer_end(&mut self, grid: &Grid) {
        self.cursor = Anchor::new(grid.total_lines().saturating_sub(1), 0);
    }

    /// To the start of the next word, over whatever run the cursor is in and
    /// the blanks after it.
    fn word_forward(&mut self, grid: &Grid) {
        let run = class_of(grid, self.cursor);
        let past = walk(grid, self.cursor, true, |class| class == run);
        self.cursor = walk(grid, past, true, |class| class == Class::Blank);
    }

    /// To the start of the word before the cursor.
    fn word_backward(&mut self, grid: &Grid) {
        let Some(from) = step(grid, self.cursor, false) else {
            return;
        };
        let at = walk(grid, from, false, |class| class == Class::Blank);
        let run = class_of(grid, at);
        if run == Class::Blank {
            // Nothing but blanks between here and the start of the buffer.
            self.cursor = at;
            return;
        }
        let before = walk(grid, at, false, |class| class == run);
        // The walk stops on the first cell that is not part of the run, which
        // is one short of the word — unless it ran into the start of the
        // buffer, where there is no cell before the word to stop on.
        self.cursor = if class_of(grid, before) == run {
            before
        } else {
            step(grid, before, true).unwrap_or(before)
        };
    }

    /// To the last character of the word the cursor is in, or of the next one
    /// when it is already there.
    fn word_end(&mut self, grid: &Grid) {
        let Some(from) = step(grid, self.cursor, true) else {
            return;
        };
        let at = walk(grid, from, true, |class| class == Class::Blank);
        let run = class_of(grid, at);
        if run == Class::Blank {
            self.cursor = at;
            return;
        }
        let past = walk(grid, at, true, |class| class == run);
        self.cursor = if class_of(grid, past) == run {
            past
        } else {
            step(grid, past, false).unwrap_or(past)
        };
    }
}

/// The character a keypress stands for in copy mode.
///
/// The text the layout produced rather than the key code, so that `$` and `G`
/// are themselves on a keyboard that makes them with shift and on one that
/// does not. A key held with ctrl, alt or super is somebody reaching for a
/// binding that copy mode has taken away from them; it must not also be a
/// motion, because `alt+j` quietly meaning "down" is how a mode ends up
/// eating a combination that was aimed somewhere else entirely.
fn typed(key: &KeyEvent) -> Option<char> {
    let modifiers = key.modifiers.effective();
    if !modifiers.without(Modifiers::SHIFT).is_empty() {
        return None;
    }
    let text = key.text.filter(|c| !c.is_control());
    // A synthesised event carries the unshifted character with the shift left
    // in the modifiers, which is how the keymap writes bindings down; a real
    // keyboard has already applied the shift. Both have to come out as `G`.
    if modifiers.contains(Modifiers::SHIFT) {
        if let Some(shifted) = text.and_then(tos_input::keymap::shifted) {
            return Some(shifted);
        }
    }
    text
}

/// What the cell at `at` counts as for a word motion.
///
/// The same classification a double click uses, taken from
/// [`crate::selection`] rather than restated, so that `w` and a double click
/// can never disagree about where a word ends. A line that is not there, or a
/// column past the end of one, is blank: off the edge of the text is no part
/// of any word.
fn class_of(grid: &Grid, at: Anchor) -> Class {
    let Some(row) = grid.line(at.line) else {
        return Class::Blank;
    };
    let cells = row.cells();
    if at.col >= cells.len() {
        return Class::Blank;
    }
    class_at(cells, at.col)
}

/// One cell along, wrapping onto the next or previous line.
///
/// `None` at the two ends of the buffer, which is what stops every walk.
fn step(grid: &Grid, at: Anchor, forward: bool) -> Option<Anchor> {
    let width = grid.cols().max(1);
    if forward {
        if at.col + 1 < width {
            return Some(Anchor::new(at.line, at.col + 1));
        }
        return (at.line + 1 < grid.total_lines()).then(|| Anchor::new(at.line + 1, 0));
    }
    if at.col > 0 {
        return Some(Anchor::new(at.line, at.col - 1));
    }
    at.line
        .checked_sub(1)
        .map(|line| Anchor::new(line, width - 1))
}

/// Walk from `from` while the cell being stood on satisfies `on`, stopping on
/// the first one that does not.
///
/// Standing on rather than stepping onto, because a motion is described by
/// what it walks over: "over this word" and then "over the blanks after it"
/// compose into `w` without either having to know about the other.
fn walk(grid: &Grid, from: Anchor, forward: bool, mut on: impl FnMut(Class) -> bool) -> Anchor {
    let mut at = from;
    for _ in 0..MAX_WALK {
        if !on(class_of(grid, at)) {
            break;
        }
        match step(grid, at, forward) {
            Some(next) => at = next,
            None => break,
        }
    }
    at
}

#[cfg(test)]
mod tests {
    use super::*;
    use tos_term::{Terminal, TerminalConfig};

    /// A terminal fed the way a program would feed one, so that wrapping and
    /// the line that scrolls off are the real thing rather than a grid built
    /// by hand.
    fn terminal(cols: usize, rows: usize, text: &str) -> Terminal {
        let mut term = Terminal::new(
            cols,
            rows,
            TerminalConfig {
                scrollback: 100,
                ..TerminalConfig::default()
            },
        );
        term.advance(text.as_bytes());
        term
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, Modifiers::NONE)
    }

    fn key(ch: char) -> KeyEvent {
        press(KeyCode::Char(ch))
    }

    /// Press a run of characters, and say where the cursor ended up.
    fn drive(mode: &mut CopyMode, grid: &Grid, keys: &str) -> Anchor {
        for ch in keys.chars() {
            mode.handle_key(&key(ch), grid);
        }
        mode.cursor()
    }

    #[test]
    fn the_motions_move_one_cell_and_one_line_at_a_time() {
        let term = terminal(20, 3, "alpha\r\nbeta\r\ngamma");
        let mut mode = CopyMode::new(Anchor::new(1, 1));
        assert_eq!(drive(&mut mode, term.grid(), "ll"), Anchor::new(1, 3));
        assert_eq!(drive(&mut mode, term.grid(), "h"), Anchor::new(1, 2));
        assert_eq!(drive(&mut mode, term.grid(), "j"), Anchor::new(2, 2));
        assert_eq!(drive(&mut mode, term.grid(), "kk"), Anchor::new(0, 2));
    }

    #[test]
    fn the_arrow_keys_are_the_same_motions() {
        // Somebody who does not know vi still has to be able to leave with
        // something copied.
        let term = terminal(20, 3, "alpha\r\nbeta");
        let mut mode = CopyMode::new(Anchor::new(0, 0));
        for code in [KeyCode::Right, KeyCode::Right, KeyCode::Down] {
            mode.handle_key(&press(code), term.grid());
        }
        assert_eq!(mode.cursor(), Anchor::new(1, 2));
    }

    #[test]
    fn the_cursor_stops_at_the_edges_of_the_buffer() {
        let term = terminal(6, 2, "ab");
        let mut mode = CopyMode::new(Anchor::new(0, 0));
        assert_eq!(drive(&mut mode, term.grid(), "hhh"), Anchor::new(0, 0));
        assert_eq!(drive(&mut mode, term.grid(), "kkk"), Anchor::new(0, 0));
        // Six columns and two rows, and nothing has scrolled off yet.
        assert_eq!(drive(&mut mode, term.grid(), "llllllll"), Anchor::new(0, 5));
        assert_eq!(drive(&mut mode, term.grid(), "jjjj"), Anchor::new(1, 5));
    }

    #[test]
    fn a_word_motion_walks_by_words_and_crosses_the_line_break() {
        // Wide enough that the line does not wrap: `w` crossing a wrap is
        // the same line continuing, which is not what this is about.
        let term = terminal(40, 3, "cargo build --release\r\nnext line");
        let grid = term.grid();
        let mut mode = CopyMode::new(Anchor::new(0, 0));
        assert_eq!(drive(&mut mode, grid, "w"), Anchor::new(0, 6), "build");
        assert_eq!(drive(&mut mode, grid, "w"), Anchor::new(0, 12), "--release");
        // Off the end of the line and onto the first word of the next.
        assert_eq!(drive(&mut mode, grid, "w"), Anchor::new(1, 0), "next");
        assert_eq!(drive(&mut mode, grid, "b"), Anchor::new(0, 12), "back");
        assert_eq!(drive(&mut mode, grid, "bb"), Anchor::new(0, 0), "cargo");
    }

    #[test]
    fn the_end_motion_lands_on_the_last_character_of_the_word() {
        let term = terminal(20, 2, "cargo build");
        let grid = term.grid();
        let mut mode = CopyMode::new(Anchor::new(0, 0));
        assert_eq!(drive(&mut mode, grid, "e"), Anchor::new(0, 4), "cargo");
        assert_eq!(drive(&mut mode, grid, "e"), Anchor::new(0, 10), "build");
    }

    #[test]
    fn a_word_is_the_same_thing_a_double_click_takes() {
        // The classification is shared, so a path is one word in both.
        let term = terminal(40, 2, "run /usr/bin/env now");
        let mut mode = CopyMode::new(Anchor::new(0, 0));
        assert_eq!(drive(&mut mode, term.grid(), "w"), Anchor::new(0, 4));
        assert_eq!(drive(&mut mode, term.grid(), "w"), Anchor::new(0, 17));
    }

    #[test]
    fn the_line_ends_are_the_text_rather_than_the_pane() {
        let term = terminal(20, 2, "  hello");
        let mut mode = CopyMode::new(Anchor::new(0, 3));
        assert_eq!(drive(&mut mode, term.grid(), "$"), Anchor::new(0, 6));
        assert_eq!(drive(&mut mode, term.grid(), "0"), Anchor::new(0, 0));
        // A blank line has no last character to sit on.
        let mut mode = CopyMode::new(Anchor::new(1, 4));
        assert_eq!(drive(&mut mode, term.grid(), "$"), Anchor::new(1, 0));
    }

    #[test]
    fn a_shifted_key_is_read_as_the_character_it_types() {
        // `$` and `G` arrive as themselves from a real keyboard and as the
        // unshifted key plus shift from a synthesised event; both are the
        // motion.
        let term = terminal(20, 2, "hello");
        let mut mode = CopyMode::new(Anchor::new(0, 0));
        let shifted = KeyEvent::new(KeyCode::Char('4'), Modifiers::SHIFT).with_text(Some('4'));
        mode.handle_key(&shifted, term.grid());
        assert_eq!(mode.cursor(), Anchor::new(0, 4), "shift+4 should be $");
    }

    #[test]
    fn the_buffer_ends_reach_the_oldest_line_and_the_live_screen() {
        let mut term = terminal(20, 2, "one\r\ntwo\r\nthree\r\nfour\r\n");
        assert!(term.grid().scrollback_len() >= 3);
        let mut mode = CopyMode::new(Anchor::new(term.grid().scrollback_len(), 0));
        assert_eq!(drive(&mut mode, term.grid(), "g"), Anchor::new(0, 0));
        let last = term.grid().total_lines() - 1;
        assert_eq!(drive(&mut mode, term.grid(), "G"), Anchor::new(last, 0));
        // And G is the way back to the live screen: showing that line means
        // the viewport is at the bottom again.
        term.scroll_display(3);
        assert!(mode.scroll_to_show(term.grid()) < 0);
    }

    #[test]
    fn a_page_is_a_screenful_of_lines() {
        let term = terminal(20, 4, "a\r\nb\r\nc\r\nd\r\ne\r\nf\r\ng\r\nh\r\n");
        let grid = term.grid();
        let start = Anchor::new(grid.total_lines() - 1, 0);
        let mut mode = CopyMode::new(start);
        let ctrl_b = KeyEvent::new(KeyCode::Char('b'), Modifiers::CTRL);
        mode.handle_key(&ctrl_b, grid);
        assert_eq!(mode.cursor().line, start.line - 4);
        mode.handle_key(&press(KeyCode::PageDown), grid);
        assert_eq!(mode.cursor(), start);
    }

    #[test]
    fn nothing_is_selected_until_v_is_pressed() {
        let term = terminal(20, 2, "alpha beta");
        let mut mode = CopyMode::new(Anchor::new(0, 0));
        drive(&mut mode, term.grid(), "lll");
        assert!(mode.selection().is_none(), "a motion selected on its own");
        assert!(!mode.selecting());
    }

    #[test]
    fn v_starts_a_selection_that_the_motions_extend() {
        let term = terminal(20, 2, "alpha beta");
        let grid = term.grid();
        let mut mode = CopyMode::new(Anchor::new(0, 0));
        drive(&mut mode, grid, "v");
        assert_eq!(mode.selection().unwrap().text(grid).as_deref(), Some("a"));
        drive(&mut mode, grid, "e");
        assert_eq!(
            mode.selection().unwrap().text(grid).as_deref(),
            Some("alpha"),
            "the motion did not extend the selection"
        );
        drive(&mut mode, grid, "$");
        assert_eq!(
            mode.selection().unwrap().text(grid).as_deref(),
            Some("alpha beta")
        );
    }

    #[test]
    fn pressing_v_again_puts_the_selection_away() {
        let term = terminal(20, 2, "alpha beta");
        let mut mode = CopyMode::new(Anchor::new(0, 0));
        drive(&mut mode, term.grid(), "vll");
        assert!(mode.selecting());
        assert_eq!(
            mode.handle_key(&key('v'), term.grid()),
            CopyOutcome::Changed
        );
        assert!(mode.selection().is_none());
        // And the cursor stayed where the motions left it.
        assert_eq!(mode.cursor(), Anchor::new(0, 2));
    }

    #[test]
    fn a_selection_made_with_the_keyboard_reaches_back_into_history() {
        // Three lines in two rows: the first has scrolled off, and the
        // keyboard still has to be able to select it.
        let term = terminal(20, 2, "one\r\ntwo\r\nthree\r\n");
        let grid = term.grid();
        let history = grid.scrollback_len();
        assert!(history >= 2);
        let mut mode = CopyMode::new(Anchor::new(history, 4));
        drive(&mut mode, grid, "v");
        for _ in 0..history {
            mode.handle_key(&key('k'), grid);
        }
        drive(&mut mode, grid, "0");
        assert_eq!(
            mode.yanked().text(grid).as_deref(),
            Some("one\ntwo\nthree"),
            "the selection stopped at the top of the screen"
        );
    }

    #[test]
    fn walking_above_the_viewport_asks_for_the_scroll_that_shows_it() {
        let term = terminal(20, 2, "one\r\ntwo\r\nthree\r\n");
        let grid = term.grid();
        let mut mode = CopyMode::new(Anchor::new(grid.display_line(0), 0));
        assert_eq!(mode.scroll_to_show(grid), 0, "it starts on screen");
        mode.handle_key(&key('k'), grid);
        assert_eq!(mode.scroll_to_show(grid), 1, "one line back in history");
        mode.handle_key(&key('k'), grid);
        assert_eq!(mode.scroll_to_show(grid), 2);
        // And a cursor below the viewport asks to come forward again.
        mode.handle_key(&key('G'), grid);
        assert!(mode.scroll_to_show(grid) <= 0);
    }

    #[test]
    fn the_copy_cursor_is_drawn_only_where_it_can_be_seen() {
        let mut term = terminal(20, 2, "one\r\ntwo\r\nthree\r\n");
        let top = term.grid().display_line(0);
        let mode = CopyMode::new(Anchor::new(top.saturating_sub(1), 3));
        assert_eq!(
            mode.display_cursor(term.grid()),
            None,
            "a cursor in history was drawn on the screen"
        );
        term.scroll_display(1);
        assert_eq!(mode.display_cursor(term.grid()), Some((3, 0)));
    }

    #[test]
    fn a_yank_with_nothing_selected_takes_the_line_under_the_cursor() {
        let term = terminal(20, 3, "alpha beta\r\ngamma");
        let grid = term.grid();
        let mut mode = CopyMode::new(Anchor::new(0, 7));
        assert_eq!(mode.handle_key(&key('y'), grid), CopyOutcome::Copied);
        assert_eq!(mode.yanked().text(grid).as_deref(), Some("alpha beta"));
    }

    #[test]
    fn a_wrapped_line_is_one_line_to_a_yank() {
        // Ten columns and fourteen characters: two rows, one line.
        let term = terminal(10, 4, "0123456789abcd\r\nnext\r\n");
        let grid = term.grid();
        let mut mode = CopyMode::new(Anchor::new(1, 2));
        assert_eq!(mode.handle_key(&key('y'), grid), CopyOutcome::Copied);
        assert_eq!(mode.yanked().text(grid).as_deref(), Some("0123456789abcd"));
    }

    #[test]
    fn escape_and_q_leave_without_copying() {
        let term = terminal(20, 2, "alpha");
        let mut mode = CopyMode::new(Anchor::new(0, 0));
        assert_eq!(
            mode.handle_key(&press(KeyCode::Escape), term.grid()),
            CopyOutcome::Left
        );
        assert_eq!(mode.handle_key(&key('q'), term.grid()), CopyOutcome::Left);
    }

    #[test]
    fn a_key_that_means_nothing_here_is_swallowed_rather_than_typed() {
        let term = terminal(20, 2, "alpha");
        let mut mode = CopyMode::new(Anchor::new(0, 1));
        for event in [
            key('z'),
            KeyEvent::new(KeyCode::Char('d'), Modifiers::SUPER),
            KeyEvent::new(KeyCode::Char('j'), Modifiers::ALT),
            press(KeyCode::Tab),
        ] {
            assert_eq!(
                mode.handle_key(&event, term.grid()),
                CopyOutcome::Consumed,
                "{event:?} was not swallowed"
            );
        }
        assert_eq!(mode.cursor(), Anchor::new(0, 1), "a modifier moved it");
    }

    #[test]
    fn a_motion_that_cannot_move_asks_for_no_repaint() {
        // Held keys at the edge of the buffer are the common case, and a
        // frame per repeat of a key that changes nothing is a frame wasted.
        let term = terminal(20, 2, "alpha");
        let mut mode = CopyMode::new(Anchor::new(0, 0));
        assert_eq!(
            mode.handle_key(&key('h'), term.grid()),
            CopyOutcome::Consumed
        );
        assert_eq!(
            mode.handle_key(&key('l'), term.grid()),
            CopyOutcome::Changed
        );
    }

    #[test]
    fn the_status_says_whether_anything_is_being_selected() {
        let term = terminal(20, 2, "alpha");
        let mut mode = CopyMode::new(Anchor::new(0, 0));
        assert_eq!(mode.status(), "copy mode");
        mode.handle_key(&key('v'), term.grid());
        assert_eq!(mode.status(), "copy mode  [selecting]");
    }
}
