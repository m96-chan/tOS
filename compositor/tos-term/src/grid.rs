//! The terminal grid: the visible screen plus scrollback.

use std::collections::VecDeque;

use crate::cell::{Attrs, Cell, Flags};

/// One line of cells.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    cells: Vec<Cell>,
    /// True when the line was ended by wrapping rather than by a newline.
    pub wrapped: bool,
}

impl Row {
    pub fn new(cols: usize, attrs: &Attrs) -> Self {
        Row {
            cells: vec![Cell::blank(attrs); cols],
            wrapped: false,
        }
    }

    pub fn len(&self) -> usize {
        self.cells.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    pub fn cells(&self) -> &[Cell] {
        &self.cells
    }

    pub fn cells_mut(&mut self) -> &mut [Cell] {
        &mut self.cells
    }

    pub fn get(&self, x: usize) -> Option<&Cell> {
        self.cells.get(x)
    }

    pub fn get_mut(&mut self, x: usize) -> Option<&mut Cell> {
        self.cells.get_mut(x)
    }

    pub fn clear(&mut self, attrs: &Attrs) {
        for cell in &mut self.cells {
            cell.clear(attrs);
        }
        self.wrapped = false;
    }

    /// Clear `[from, to)`, clamped to the row.
    pub fn clear_range(&mut self, from: usize, to: usize, attrs: &Attrs) {
        let to = to.min(self.cells.len());
        for cell in &mut self.cells[from.min(to)..to] {
            cell.clear(attrs);
        }
    }

    fn resize(&mut self, cols: usize, attrs: &Attrs) {
        if cols < self.cells.len() {
            self.cells.truncate(cols);
            // A truncated wide leader would leave a dangling half glyph.
            if let Some(last) = self.cells.last_mut() {
                if last.attrs.flags.contains(Flags::WIDE) {
                    last.clear(attrs);
                }
            }
        } else {
            self.cells.resize(cols, Cell::blank(attrs));
        }
    }

    /// Text content with trailing blanks removed, used for selection and tests.
    ///
    /// Not removed from a row that wrapped. A row is marked wrapped because
    /// the text ran past its last column, so a space sitting in that column is
    /// one somebody typed between two words rather than the emptiness at the
    /// end of a line — and trimming it ran the words together when the two
    /// halves were put back. Which words that happened to, and so whether
    /// anyone noticed, depended on where the wrap landed: it surfaced as a
    /// test that passed or failed on the length of a path.
    pub fn to_text(&self) -> String {
        let mut s = String::new();
        for cell in &self.cells {
            if cell.attrs.flags.contains(Flags::WIDE_SPACER)
                || cell.attrs.flags.contains(Flags::WRAP_PAD)
            {
                continue;
            }
            s.push(cell.ch);
            if let Some(zw) = &cell.zerowidth {
                s.extend(zw.iter());
            }
        }
        if !self.wrapped {
            while s.ends_with(' ') {
                s.pop();
            }
        }
        s
    }
}

/// An inclusive-exclusive vertical region of the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub top: usize,
    pub bottom: usize,
}

impl Region {
    pub fn new(top: usize, bottom: usize) -> Self {
        Region { top, bottom }
    }

    pub fn height(&self) -> usize {
        self.bottom.saturating_sub(self.top)
    }
}

/// Visible screen plus optional scrollback history.
#[derive(Debug, Clone)]
pub struct Grid {
    cols: usize,
    rows: usize,
    screen: Vec<Row>,
    scrollback: VecDeque<Row>,
    max_scrollback: usize,
    /// How far back the viewport is scrolled, in lines.
    display_offset: usize,
}

impl Grid {
    pub fn new(cols: usize, rows: usize, max_scrollback: usize) -> Self {
        let attrs = Attrs::default();
        Grid {
            cols,
            rows,
            screen: vec![Row::new(cols, &attrs); rows],
            scrollback: VecDeque::new(),
            max_scrollback,
            display_offset: 0,
        }
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn scrollback_len(&self) -> usize {
        self.scrollback.len()
    }

    pub fn display_offset(&self) -> usize {
        self.display_offset
    }

    /// Scroll the viewport back into history. Returns true if it moved.
    pub fn scroll_display(&mut self, delta: isize) -> bool {
        let max = self.scrollback.len();
        let new = (self.display_offset as isize + delta).clamp(0, max as isize) as usize;
        let moved = new != self.display_offset;
        self.display_offset = new;
        moved
    }

    pub fn reset_display_offset(&mut self) -> bool {
        let moved = self.display_offset != 0;
        self.display_offset = 0;
        moved
    }

    /// A row of the active screen, ignoring the scrollback viewport.
    pub fn row(&self, y: usize) -> &Row {
        &self.screen[y]
    }

    pub fn row_mut(&mut self, y: usize) -> &mut Row {
        &mut self.screen[y]
    }

    pub fn cell(&self, x: usize, y: usize) -> Option<&Cell> {
        self.screen.get(y).and_then(|r| r.get(x))
    }

    pub fn cell_mut(&mut self, x: usize, y: usize) -> Option<&mut Cell> {
        self.screen.get_mut(y).and_then(|r| r.get_mut(x))
    }

    /// A row as currently displayed, taking the scrollback viewport into
    /// account. `y` is 0..rows.
    pub fn display_row(&self, y: usize) -> &Row {
        let back = self.display_offset;
        if y >= back {
            &self.screen[y - back]
        } else {
            // The first `back` rows come from history, counting back from the
            // most recent scrolled-off line.
            &self.scrollback[self.scrollback.len() - back + y]
        }
    }

    /// Line in history, 0 being the oldest.
    pub fn history_row(&self, i: usize) -> Option<&Row> {
        self.scrollback.get(i)
    }

    /// How many lines the grid holds in all: history first, then the screen.
    pub fn total_lines(&self) -> usize {
        self.scrollback.len() + self.rows
    }

    /// A row by absolute line number, which counts history first: line 0 is
    /// the oldest line still in scrollback and [`Grid::scrollback_len`] is the
    /// top of the screen.
    ///
    /// This is the coordinate a selection is anchored in. A displayed row is
    /// the wrong thing to remember, because the viewport moves while the text
    /// stays where it is, and the selection belongs to the text.
    pub fn line(&self, line: usize) -> Option<&Row> {
        match line.checked_sub(self.scrollback.len()) {
            Some(y) => self.screen.get(y),
            None => self.scrollback.get(line),
        }
    }

    /// The absolute line the viewport is showing in its row `y`.
    pub fn display_line(&self, y: usize) -> usize {
        // The offset can never exceed the history, so this cannot go negative.
        self.scrollback.len() + y - self.display_offset
    }

    pub fn clear_screen(&mut self, attrs: &Attrs) {
        for row in &mut self.screen {
            row.clear(attrs);
        }
    }

    pub fn clear_history(&mut self) {
        self.scrollback.clear();
        self.display_offset = 0;
    }

    /// Move the whole screen into scrollback, leaving a blank screen. This is
    /// what a shell's `clear` does when it wants history preserved.
    pub fn scroll_screen_into_history(&mut self, attrs: &Attrs) {
        let fresh = vec![Row::new(self.cols, attrs); self.rows];
        for row in std::mem::replace(&mut self.screen, fresh) {
            self.push_history(row);
        }
    }

    fn push_history(&mut self, row: Row) {
        if self.max_scrollback == 0 {
            return;
        }
        if self.scrollback.len() == self.max_scrollback {
            self.scrollback.pop_front();
            // The viewport is anchored to the bottom, so dropping the oldest
            // line means the offset now points one line further back.
            self.display_offset = self.display_offset.saturating_sub(1);
        }
        self.scrollback.push_back(row);
        if self.display_offset > 0 {
            self.display_offset = (self.display_offset + 1).min(self.scrollback.len());
        }
    }

    /// Scroll `region` up by `n` lines, i.e. content moves toward the top.
    ///
    /// Lines leaving the top enter scrollback only when the region starts at
    /// the top of the screen and the caller asks for it: a line feed keeps
    /// history, but `delete line` must not.
    pub fn scroll_up(&mut self, region: Region, n: usize, attrs: &Attrs, keep_history: bool) {
        let height = region.height();
        if height == 0 || n == 0 {
            return;
        }
        let n = n.min(height);
        let to_history = keep_history && region.top == 0 && self.max_scrollback > 0;

        for i in 0..n {
            let row =
                std::mem::replace(&mut self.screen[region.top + i], Row::new(self.cols, attrs));
            if to_history {
                self.push_history(row);
            }
        }
        self.screen[region.top..region.bottom].rotate_left(n);
    }

    /// Scroll `region` down by `n` lines, i.e. content moves toward the bottom.
    pub fn scroll_down(&mut self, region: Region, n: usize, attrs: &Attrs) {
        let height = region.height();
        if height == 0 || n == 0 {
            return;
        }
        let n = n.min(height);
        for i in 0..n {
            self.screen[region.bottom - 1 - i] = Row::new(self.cols, attrs);
        }
        self.screen[region.top..region.bottom].rotate_right(n);
    }

    /// Resize the grid. Returns how many lines the cursor should move up
    /// because content was pulled out of scrollback.
    ///
    /// Width changes truncate or pad; reflowing wrapped lines is deliberately
    /// left for a later milestone.
    pub fn resize(&mut self, cols: usize, rows: usize, cursor_y: usize, attrs: &Attrs) -> usize {
        if cols != self.cols {
            for row in &mut self.screen {
                row.resize(cols, attrs);
            }
            for row in &mut self.scrollback {
                row.resize(cols, attrs);
            }
            self.cols = cols;
        }

        let mut cursor_shift = 0;
        if rows > self.rows {
            let grow = rows - self.rows;
            // Prefer restoring scrolled-off history below the cursor line.
            let from_history = grow.min(self.scrollback.len());
            for _ in 0..from_history {
                let row = self.scrollback.pop_back().unwrap();
                self.screen.insert(0, row);
                cursor_shift += 1;
            }
            for _ in from_history..grow {
                self.screen.push(Row::new(cols, attrs));
            }
        } else if rows < self.rows {
            let shrink = self.rows - rows;
            // Drop lines below the cursor first, then push the top away.
            let below = self.rows.saturating_sub(cursor_y + 1).min(shrink);
            for _ in 0..below {
                self.screen.pop();
            }
            for _ in below..shrink {
                let row = self.screen.remove(0);
                self.push_history(row);
            }
        }
        self.rows = rows;
        self.display_offset = self.display_offset.min(self.scrollback.len());
        cursor_shift
    }

    /// All displayed rows, top to bottom.
    pub fn display_rows(&self) -> impl Iterator<Item = &Row> {
        (0..self.rows).map(move |y| self.display_row(y))
    }

    /// Screen contents as text, one line per row, for tests and selection.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for y in 0..self.rows {
            out.push_str(&self.screen[y].to_text());
            if y + 1 < self.rows {
                out.push('\n');
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(grid: &mut Grid, y: usize, text: &str) {
        let attrs = Attrs::default();
        grid.row_mut(y).clear(&attrs);
        for (x, ch) in text.chars().enumerate() {
            grid.cell_mut(x, y).unwrap().ch = ch;
        }
    }

    #[test]
    fn scroll_up_moves_lines_into_history() {
        let mut grid = Grid::new(4, 3, 10);
        let attrs = Attrs::default();
        put(&mut grid, 0, "aaa");
        put(&mut grid, 1, "bbb");
        put(&mut grid, 2, "ccc");

        grid.scroll_up(Region::new(0, 3), 1, &attrs, true);

        assert_eq!(grid.row(0).to_text(), "bbb");
        assert_eq!(grid.row(1).to_text(), "ccc");
        assert_eq!(grid.row(2).to_text(), "");
        assert_eq!(grid.scrollback_len(), 1);
        assert_eq!(grid.history_row(0).unwrap().to_text(), "aaa");
    }

    #[test]
    fn scroll_region_does_not_touch_history() {
        let mut grid = Grid::new(4, 4, 10);
        let attrs = Attrs::default();
        put(&mut grid, 0, "aaa");
        put(&mut grid, 1, "bbb");
        put(&mut grid, 2, "ccc");
        put(&mut grid, 3, "ddd");

        grid.scroll_up(Region::new(1, 3), 1, &attrs, true);

        assert_eq!(grid.row(0).to_text(), "aaa");
        assert_eq!(grid.row(1).to_text(), "ccc");
        assert_eq!(grid.row(2).to_text(), "");
        assert_eq!(grid.row(3).to_text(), "ddd");
        assert_eq!(grid.scrollback_len(), 0);
    }

    #[test]
    fn scroll_down_inserts_blank_lines() {
        let mut grid = Grid::new(4, 3, 10);
        let attrs = Attrs::default();
        put(&mut grid, 0, "aaa");
        put(&mut grid, 1, "bbb");
        put(&mut grid, 2, "ccc");

        grid.scroll_down(Region::new(0, 3), 1, &attrs);

        assert_eq!(grid.row(0).to_text(), "");
        assert_eq!(grid.row(1).to_text(), "aaa");
        assert_eq!(grid.row(2).to_text(), "bbb");
    }

    #[test]
    fn display_offset_reads_history() {
        let mut grid = Grid::new(8, 2, 10);
        let attrs = Attrs::default();
        put(&mut grid, 0, "one");
        put(&mut grid, 1, "two");
        grid.scroll_up(Region::new(0, 2), 1, &attrs, true);
        put(&mut grid, 1, "three");

        assert_eq!(grid.display_row(0).to_text(), "two");
        grid.scroll_display(1);
        assert_eq!(grid.display_row(0).to_text(), "one");
        assert_eq!(grid.display_row(1).to_text(), "two");
    }

    #[test]
    fn absolute_lines_address_history_and_screen_alike() {
        let mut grid = Grid::new(8, 2, 10);
        let attrs = Attrs::default();
        put(&mut grid, 0, "one");
        put(&mut grid, 1, "two");
        grid.scroll_up(Region::new(0, 2), 1, &attrs, true);
        put(&mut grid, 1, "three");

        assert_eq!(grid.total_lines(), 3);
        assert_eq!(grid.line(0).unwrap().to_text(), "one");
        assert_eq!(grid.line(1).unwrap().to_text(), "two");
        assert_eq!(grid.line(2).unwrap().to_text(), "three");
        assert!(grid.line(3).is_none());

        // Scrolling the viewport moves which lines are shown, not which line
        // a piece of text is on.
        assert_eq!(grid.display_line(0), 1);
        grid.scroll_display(1);
        assert_eq!(grid.display_line(0), 0);
        assert_eq!(grid.line(0).unwrap().to_text(), "one");
    }

    #[test]
    fn scrollback_is_bounded() {
        let mut grid = Grid::new(4, 1, 2);
        let attrs = Attrs::default();
        for i in 0..5 {
            put(&mut grid, 0, &i.to_string());
            grid.scroll_up(Region::new(0, 1), 1, &attrs, true);
        }
        assert_eq!(grid.scrollback_len(), 2);
        assert_eq!(grid.history_row(0).unwrap().to_text(), "3");
        assert_eq!(grid.history_row(1).unwrap().to_text(), "4");
    }

    #[test]
    fn growing_pulls_lines_back_from_history() {
        let mut grid = Grid::new(4, 2, 10);
        let attrs = Attrs::default();
        put(&mut grid, 0, "one");
        put(&mut grid, 1, "two");
        grid.scroll_up(Region::new(0, 2), 1, &attrs, true);

        let shift = grid.resize(4, 3, 1, &attrs);
        assert_eq!(shift, 1);
        assert_eq!(grid.row(0).to_text(), "one");
        assert_eq!(grid.scrollback_len(), 0);
    }

    #[test]
    fn shrinking_drops_below_cursor_first() {
        let mut grid = Grid::new(4, 3, 10);
        let attrs = Attrs::default();
        put(&mut grid, 0, "aaa");
        put(&mut grid, 1, "bbb");
        put(&mut grid, 2, "ccc");

        grid.resize(4, 2, 0, &attrs);
        assert_eq!(grid.row(0).to_text(), "aaa");
        assert_eq!(grid.row(1).to_text(), "bbb");
        assert_eq!(grid.scrollback_len(), 0);
    }
}
