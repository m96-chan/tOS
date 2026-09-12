//! Selected text, anchored to the text rather than to the screen.
//!
//! A selection made with the mouse has to keep meaning the same words after
//! the viewport has moved, which is why nothing here is stored in displayed
//! rows. The coordinates are the absolute lines of [`Grid::line`]: history
//! first, then the screen. Scrolling changes which of them are visible and
//! nothing else, so a selection made on the live screen is still on the same
//! words once they have scrolled into history, and a selection can be made in
//! history in the first place.

use tos_term::{Flags, Grid, Row};

/// A point in a pane's text: an absolute line, and a column on it.
///
/// The field order is the sort order, because ordering two points is what
/// tells a drag which end of it came first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Anchor {
    pub line: usize,
    pub col: usize,
}

impl Anchor {
    pub fn new(line: usize, col: usize) -> Self {
        Anchor { line, col }
    }
}

/// How much of the text one press takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionMode {
    /// From the cell pressed to the cell released.
    Cell,
    /// The word under the pointer, grown at both ends.
    Word,
    /// The whole line, including the rows a wrapped line continues onto.
    Line,
}

impl SelectionMode {
    /// What a run of `clicks` presses in quick succession selects. The count
    /// wraps back to a plain cell selection after three, the convention every
    /// terminal and text field follows.
    pub fn for_clicks(clicks: u32) -> SelectionMode {
        match clicks {
            0 => SelectionMode::Cell,
            n => match n % 3 {
                2 => SelectionMode::Word,
                0 => SelectionMode::Line,
                _ => SelectionMode::Cell,
            },
        }
    }
}

/// A selection in progress or finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    /// Where the press landed. It stays put while the head follows the mouse,
    /// so a drag can go either way from it.
    pub anchor: Anchor,
    pub head: Anchor,
    /// Rectangular rather than line-wise.
    pub block: bool,
    pub mode: SelectionMode,
}

impl Selection {
    pub fn new(anchor: Anchor, block: bool, mode: SelectionMode) -> Self {
        Selection {
            anchor,
            head: anchor,
            block,
            mode,
        }
    }

    /// Move the loose end of the selection.
    pub fn drag_to(&mut self, head: Anchor) {
        self.head = head;
    }

    /// The two ends in order, after the mode has grown them.
    ///
    /// Growing happens here rather than when the press is handled so that
    /// dragging after a double click keeps extending by whole words, which is
    /// what the gesture promises.
    pub fn span(&self, grid: &Grid) -> (Anchor, Anchor) {
        let (start, end) = if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        };
        // A rectangular selection is the columns the user drew; widening it to
        // words or lines would hand back something they did not ask for.
        if self.block {
            return (start, end);
        }
        match self.mode {
            SelectionMode::Cell => (start, end),
            SelectionMode::Word => (word_start(grid, start), word_end(grid, end)),
            SelectionMode::Line => (line_start(grid, start), line_end(grid, end)),
        }
    }

    /// The selection in displayed rows, for the renderer, or `None` when the
    /// viewport has moved away from it entirely.
    pub fn display(&self, grid: &Grid) -> Option<tos_render::Selection> {
        let rows = grid.rows();
        let cols = grid.cols();
        if rows == 0 || cols == 0 {
            return None;
        }
        let (start, end) = self.span(grid);
        let top = grid.display_line(0);
        let bottom = top + rows - 1;
        if end.line < top || start.line > bottom {
            return None;
        }
        // Clipping a line-wise selection to the viewport has to open the
        // clipped end out to the edge of the screen, or the highlight would
        // stop short in the middle of a line the selection does cover.
        let start_col = if start.line < top && !self.block {
            0
        } else {
            start.col
        };
        let end_col = if end.line > bottom && !self.block {
            cols - 1
        } else {
            end.col
        };
        let start_row = start.line.max(top) - top;
        let end_row = end.line.min(bottom) - top;
        Some(tos_render::Selection::new(
            (start_col, start_row),
            (end_col, end_row),
            self.block,
        ))
    }

    /// The text the selection covers.
    pub fn text(&self, grid: &Grid) -> Option<String> {
        let (start, end) = self.span(grid);
        let mut out = String::new();
        let mut pending_newline = false;
        for line in start.line..=end.line {
            let Some(row) = grid.line(line) else { continue };
            if row.is_empty() {
                continue;
            }
            let (from, to) = self.columns(row, line, start, end);
            let mut text = String::new();
            for x in from..=to.min(row.len().saturating_sub(1)) {
                let cell = &row.cells()[x];
                if cell.attrs.flags.contains(Flags::WIDE_SPACER)
                    || cell.attrs.flags.contains(Flags::WRAP_PAD)
                {
                    continue;
                }
                text.push(cell.ch);
                if let Some(marks) = &cell.zerowidth {
                    text.extend(marks.iter());
                }
            }
            // A row that wrapped ran out of columns rather than ending, so a
            // blank in its last cell is a space somebody typed between two
            // words — and the next row is the same line, joined with no
            // newline below. Trimming it ran the two words together in
            // whatever was pasted. A block selection is a rectangle and every
            // row of it is its own line, so there the trailing blanks are the
            // emptiness they look like.
            let trimmed = if row.wrapped && !self.block {
                text.as_str()
            } else {
                text.trim_end()
            };
            if trimmed.is_empty() && out.is_empty() {
                // Blank lines before anything else are not worth copying.
                continue;
            }
            if pending_newline {
                out.push('\n');
            }
            out.push_str(trimmed);
            // A row that ended by wrapping is the same line as the next one,
            // so joining them with a newline would break a pasted command.
            pending_newline = !row.wrapped || self.block;
        }
        (!out.is_empty()).then_some(out)
    }

    /// The inclusive column range this line contributes.
    fn columns(&self, row: &Row, line: usize, start: Anchor, end: Anchor) -> (usize, usize) {
        let last = row.len().saturating_sub(1);
        if self.block {
            return (start.col.min(end.col), start.col.max(end.col));
        }
        let from = if line == start.line { start.col } else { 0 };
        let to = if line == end.line { end.col } else { last };
        (from, to)
    }
}

/// What a character counts as when a double click grows to a word.
///
/// Visible to the crate because [`crate::copymode`]'s `w`, `b` and `e` have
/// to agree with what a double click takes; two answers to "where does this
/// word end" is one more than a terminal can have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Class {
    Blank,
    Word,
    Separator,
}

/// Characters that end a word without belonging to one: quotes, brackets and
/// the punctuation a shell line is built from. Everything else that is not
/// blank counts as a word character, so a path, a URL, a `--flag` or a
/// `key=value` comes out of a double click in one piece.
const SEPARATORS: &str = ",`|:\"'()[]{}<>";

fn class(ch: char) -> Class {
    if ch.is_whitespace() {
        Class::Blank
    } else if SEPARATORS.contains(ch) {
        Class::Separator
    } else {
        Class::Word
    }
}

/// The class of a cell, which for the trailing half of a double width glyph
/// is the class of the glyph itself: the two cells are one character, and a
/// word made of them must not be cut in half.
pub(crate) fn class_at(cells: &[tos_term::Cell], x: usize) -> Class {
    if cells[x].attrs.flags.contains(Flags::WIDE_SPACER) && x > 0 {
        return class(cells[x - 1].ch);
    }
    class(cells[x].ch)
}

fn word_start(grid: &Grid, at: Anchor) -> Anchor {
    let Some(row) = grid.line(at.line) else {
        return at;
    };
    let cells = row.cells();
    if cells.is_empty() {
        return at;
    }
    let col = at.col.min(cells.len() - 1);
    let wanted = class_at(cells, col);
    let mut start = col;
    while start > 0 && class_at(cells, start - 1) == wanted {
        start -= 1;
    }
    Anchor::new(at.line, start)
}

fn word_end(grid: &Grid, at: Anchor) -> Anchor {
    let Some(row) = grid.line(at.line) else {
        return at;
    };
    let cells = row.cells();
    if cells.is_empty() {
        return at;
    }
    let col = at.col.min(cells.len() - 1);
    let wanted = class_at(cells, col);
    let mut end = col;
    while end + 1 < cells.len() && class_at(cells, end + 1) == wanted {
        end += 1;
    }
    Anchor::new(at.line, end)
}

/// The first row of the logical line `at` is on: a line that arrived wider
/// than the pane is several rows, and all of them are the one line.
fn line_start(grid: &Grid, at: Anchor) -> Anchor {
    let mut line = at.line;
    while line > 0 {
        match grid.line(line - 1) {
            Some(row) if row.wrapped => line -= 1,
            _ => break,
        }
    }
    Anchor::new(line, 0)
}

fn line_end(grid: &Grid, at: Anchor) -> Anchor {
    let mut line = at.line;
    while grid.line(line).map(|row| row.wrapped).unwrap_or(false) && line + 1 < grid.total_lines() {
        line += 1;
    }
    let last = grid.line(line).map(|row| row.len()).unwrap_or(1);
    Anchor::new(line, last.saturating_sub(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tos_term::{Attrs, Terminal, TerminalConfig};

    /// A terminal with a small screen and room for history, fed as a program
    /// would feed it so that wrapping and scrolling are the real thing.
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

    fn select(grid: &Grid, from: Anchor, to: Anchor, mode: SelectionMode) -> Option<String> {
        let mut selection = Selection::new(from, false, mode);
        selection.drag_to(to);
        selection.text(grid)
    }

    #[test]
    fn a_selection_keeps_its_text_when_the_viewport_moves() {
        let mut term = terminal(20, 3, "alpha\r\nbeta\r\ngamma\r\n");
        let grid = term.grid();
        let picked = select(
            grid,
            Anchor::new(0, 0),
            Anchor::new(0, 4),
            SelectionMode::Cell,
        );
        assert_eq!(picked.as_deref(), Some("alpha"));

        // Push the line into history and look again: the same coordinates
        // still name the same text.
        term.advance(b"delta\r\nepsilon\r\n");
        assert!(term.grid().scrollback_len() >= 2);
        let picked = select(
            term.grid(),
            Anchor::new(0, 0),
            Anchor::new(0, 4),
            SelectionMode::Cell,
        );
        assert_eq!(picked.as_deref(), Some("alpha"));
    }

    #[test]
    fn a_selection_spans_history_and_screen() {
        let term = terminal(20, 2, "one\r\ntwo\r\nthree\r\n");
        let history = term.grid().scrollback_len();
        assert!(history >= 2);
        let text = select(
            term.grid(),
            Anchor::new(0, 0),
            Anchor::new(history, 4),
            SelectionMode::Cell,
        );
        assert_eq!(text.as_deref(), Some("one\ntwo\nthree"));
    }

    #[test]
    fn a_double_click_takes_the_word_under_it() {
        let term = terminal(40, 2, "cargo build --release");
        let grid = term.grid();
        for col in 6..=10 {
            let text = select(
                grid,
                Anchor::new(0, col),
                Anchor::new(0, col),
                SelectionMode::Word,
            );
            assert_eq!(text.as_deref(), Some("build"), "at column {col}");
        }
        // A flag and a path are one word each, because a double click on them
        // is asking for the whole thing.
        let text = select(
            grid,
            Anchor::new(0, 14),
            Anchor::new(0, 14),
            SelectionMode::Word,
        );
        assert_eq!(text.as_deref(), Some("--release"));
    }

    #[test]
    fn a_double_click_stops_at_quotes_and_brackets() {
        let term = terminal(40, 2, "echo (\"hello\")");
        let grid = term.grid();
        let text = select(
            grid,
            Anchor::new(0, 8),
            Anchor::new(0, 8),
            SelectionMode::Word,
        );
        assert_eq!(text.as_deref(), Some("hello"));
    }

    #[test]
    fn a_double_click_drag_extends_by_whole_words() {
        let term = terminal(40, 2, "cargo build --release");
        let text = select(
            term.grid(),
            Anchor::new(0, 7),
            Anchor::new(0, 15),
            SelectionMode::Word,
        );
        assert_eq!(text.as_deref(), Some("build --release"));
    }

    #[test]
    fn a_triple_click_takes_the_whole_line() {
        let term = terminal(20, 3, "alpha beta\r\ngamma\r\n");
        let text = select(
            term.grid(),
            Anchor::new(0, 6),
            Anchor::new(0, 6),
            SelectionMode::Line,
        );
        assert_eq!(text.as_deref(), Some("alpha beta"));
    }

    #[test]
    fn a_triple_click_follows_a_wrapped_line_onto_its_other_rows() {
        // Ten columns and a fourteen character line: the terminal wraps it,
        // and the two rows are still the one line.
        let term = terminal(10, 4, "0123456789abcd\r\nnext\r\n");
        let grid = term.grid();
        assert!(grid.line(0).unwrap().wrapped);
        let text = select(
            grid,
            Anchor::new(1, 2),
            Anchor::new(1, 2),
            SelectionMode::Line,
        );
        assert_eq!(text.as_deref(), Some("0123456789abcd"));
    }

    #[test]
    fn a_block_selection_takes_a_rectangle() {
        let term = terminal(20, 3, "abcdef\r\nghijkl\r\n");
        let mut selection = Selection::new(Anchor::new(0, 1), true, SelectionMode::Cell);
        selection.drag_to(Anchor::new(1, 3));
        assert_eq!(selection.text(term.grid()).as_deref(), Some("bcd\nhij"));
    }

    #[test]
    fn a_wide_glyph_is_one_word_not_two() {
        let term = terminal(20, 2, "日本語");
        let text = select(
            term.grid(),
            Anchor::new(0, 3),
            Anchor::new(0, 3),
            SelectionMode::Word,
        );
        assert_eq!(text.as_deref(), Some("日本語"));
    }

    #[test]
    fn the_renderer_gets_rows_that_are_on_screen() {
        let mut term = terminal(20, 2, "one\r\ntwo\r\nthree\r\n");
        let selection = Selection::new(Anchor::new(0, 0), false, SelectionMode::Line);
        // Line 0 has scrolled off, so there is nothing to highlight.
        assert!(selection.display(term.grid()).is_none());

        // Scroll back to it and the highlight comes back on the top row.
        term.scroll_display(term.grid().scrollback_len() as isize);
        let drawn = selection.display(term.grid()).expect("on screen now");
        assert_eq!(drawn.start, (0, 0));
        assert_eq!(drawn.end.1, 0);
    }

    #[test]
    fn a_selection_reaching_past_the_viewport_is_clipped_open() {
        let term = terminal(20, 3, "alpha\r\nbeta\r\ngamma");
        let mut selection = Selection::new(Anchor::new(0, 2), false, SelectionMode::Cell);
        selection.drag_to(Anchor::new(5, 1));
        let drawn = selection.display(term.grid()).expect("partly on screen");
        // The far end is off the bottom, so the last visible row is selected
        // all the way to its right edge.
        assert_eq!(drawn.start, (2, 0));
        assert_eq!(drawn.end, (19, 2));
    }

    #[test]
    fn clicks_cycle_through_cell_word_and_line() {
        assert_eq!(SelectionMode::for_clicks(1), SelectionMode::Cell);
        assert_eq!(SelectionMode::for_clicks(2), SelectionMode::Word);
        assert_eq!(SelectionMode::for_clicks(3), SelectionMode::Line);
        assert_eq!(SelectionMode::for_clicks(4), SelectionMode::Cell);
    }

    #[test]
    fn a_selection_of_blanks_copies_nothing() {
        let mut grid = Grid::new(8, 2, 0);
        grid.row_mut(0).clear(&Attrs::default());
        let text = select(
            &grid,
            Anchor::new(0, 0),
            Anchor::new(1, 7),
            SelectionMode::Cell,
        );
        assert_eq!(text, None);
    }
}
