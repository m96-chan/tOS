//! A very small terminal UI.
//!
//! The installer runs in a tOS pane, so it talks to the compositor the same
//! way any other application does: escape sequences out, key events in. It
//! draws into a cell buffer and emits only what changed, which keeps the
//! output small enough to stay readable when a pane is redrawing.

use std::io::{self, Write};

use tos_term::Rgb;

/// A colour, either the terminal's own default or a literal one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    Default,
    Rgb(Rgb),
}

impl Color {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Color {
        Color::Rgb(Rgb::new(r, g, b))
    }
}

/// How a cell is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub dim: bool,
    pub reverse: bool,
}

impl Default for Style {
    fn default() -> Self {
        Style {
            fg: Color::Default,
            bg: Color::Default,
            bold: false,
            dim: false,
            reverse: false,
        }
    }
}

impl Style {
    pub fn fg(color: Color) -> Style {
        Style {
            fg: color,
            ..Style::default()
        }
    }

    pub fn bold(mut self) -> Style {
        self.bold = true;
        self
    }

    pub fn dim(mut self) -> Style {
        self.dim = true;
        self
    }

    pub fn reversed(mut self) -> Style {
        self.reverse = true;
        self
    }

    pub fn on(mut self, color: Color) -> Style {
        self.bg = color;
        self
    }

    /// The SGR sequence that selects this style.
    fn sgr(&self) -> String {
        let mut out = String::from("\x1b[0");
        if self.bold {
            out.push_str(";1");
        }
        if self.dim {
            out.push_str(";2");
        }
        if self.reverse {
            out.push_str(";7");
        }
        if let Color::Rgb(c) = self.fg {
            out.push_str(&format!(";38;2;{};{};{}", c.r, c.g, c.b));
        }
        if let Color::Rgb(c) = self.bg {
            out.push_str(&format!(";48;2;{};{};{}", c.r, c.g, c.b));
        }
        out.push('m');
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cell {
    ch: char,
    style: Style,
}

impl Default for Cell {
    fn default() -> Self {
        Cell {
            ch: ' ',
            style: Style::default(),
        }
    }
}

/// A rectangle of cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl Rect {
    pub const fn new(x: u16, y: u16, width: u16, height: u16) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    pub fn right(&self) -> u16 {
        self.x + self.width
    }

    pub fn bottom(&self) -> u16 {
        self.y + self.height
    }

    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// The same rectangle inset by `n` cells on every side.
    pub fn inset(&self, n: u16) -> Rect {
        Rect {
            x: self.x + n,
            y: self.y + n,
            width: self.width.saturating_sub(n * 2),
            height: self.height.saturating_sub(n * 2),
        }
    }

    /// A rectangle of this size centred inside `outer`.
    pub fn centred(outer: Rect, width: u16, height: u16) -> Rect {
        let width = width.min(outer.width);
        let height = height.min(outer.height);
        Rect {
            x: outer.x + (outer.width - width) / 2,
            y: outer.y + (outer.height - height) / 2,
            width,
            height,
        }
    }
}

/// A cell buffer that knows what it last drew.
pub struct Screen {
    cols: u16,
    rows: u16,
    cells: Vec<Cell>,
    previous: Vec<Cell>,
    /// Where the cursor should end up, if it should be visible at all.
    cursor: Option<(u16, u16)>,
    first_frame: bool,
}

impl Screen {
    pub fn new(cols: u16, rows: u16) -> Screen {
        let cols = cols.max(1);
        let rows = rows.max(1);
        Screen {
            cols,
            rows,
            cells: vec![Cell::default(); cols as usize * rows as usize],
            previous: Vec::new(),
            cursor: None,
            first_frame: true,
        }
    }

    pub fn cols(&self) -> u16 {
        self.cols
    }

    pub fn rows(&self) -> u16 {
        self.rows
    }

    pub fn area(&self) -> Rect {
        Rect::new(0, 0, self.cols, self.rows)
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        if cols == self.cols && rows == self.rows {
            return;
        }
        self.cols = cols.max(1);
        self.rows = rows.max(1);
        self.cells = vec![Cell::default(); self.cols as usize * self.rows as usize];
        // Nothing on screen can be trusted after a resize.
        self.previous.clear();
        self.first_frame = true;
    }

    pub fn clear(&mut self) {
        self.cells.fill(Cell::default());
        self.cursor = None;
    }

    pub fn set_cursor(&mut self, x: u16, y: u16) {
        self.cursor = Some((x, y));
    }

    fn put(&mut self, x: u16, y: u16, cell: Cell) {
        if x >= self.cols || y >= self.rows {
            return;
        }
        self.cells[y as usize * self.cols as usize + x as usize] = cell;
    }

    /// Write text, clipped to the screen. Returns the column just past it.
    pub fn text(&mut self, x: u16, y: u16, text: &str, style: Style) -> u16 {
        let mut cursor = x;
        for ch in text.chars() {
            if cursor >= self.cols {
                break;
            }
            let width = tos_term::char_width(ch).max(1) as u16;
            self.put(cursor, y, Cell { ch, style });
            // The second half of a wide glyph is the terminal's business, not
            // ours, but the cursor still has to step over it.
            cursor += width;
        }
        cursor
    }

    /// Write text truncated to `width`, with an ellipsis when it does not fit.
    pub fn text_clipped(&mut self, x: u16, y: u16, width: u16, text: &str, style: Style) {
        if tos_term::str_width(text) as u16 <= width {
            self.text(x, y, text, style);
            return;
        }
        let mut budget = width.saturating_sub(1) as usize;
        let mut clipped = String::new();
        for ch in text.chars() {
            let w = tos_term::char_width(ch).max(1);
            if w > budget {
                break;
            }
            budget -= w;
            clipped.push(ch);
        }
        clipped.push('…');
        self.text(x, y, &clipped, style);
    }

    pub fn fill(&mut self, rect: Rect, ch: char, style: Style) {
        for y in rect.y..rect.bottom() {
            for x in rect.x..rect.right() {
                self.put(x, y, Cell { ch, style });
            }
        }
    }

    /// A single line frame, with an optional title in the top edge.
    pub fn frame(&mut self, rect: Rect, title: Option<&str>, style: Style) {
        if rect.width < 2 || rect.height < 2 {
            return;
        }
        let (left, top) = (rect.x, rect.y);
        let (right, bottom) = (rect.right() - 1, rect.bottom() - 1);

        self.put(left, top, Cell { ch: '┌', style });
        self.put(right, top, Cell { ch: '┐', style });
        self.put(left, bottom, Cell { ch: '└', style });
        self.put(right, bottom, Cell { ch: '┘', style });
        for x in left + 1..right {
            self.put(x, top, Cell { ch: '─', style });
            self.put(x, bottom, Cell { ch: '─', style });
        }
        for y in top + 1..bottom {
            self.put(left, y, Cell { ch: '│', style });
            self.put(right, y, Cell { ch: '│', style });
        }

        if let Some(title) = title {
            let label = format!(" {title} ");
            let room = rect.width.saturating_sub(4);
            if room > 0 {
                self.text_clipped(left + 2, top, room, &label, style.bold());
            }
        }
    }

    /// Centre a single line of text inside a rectangle.
    pub fn centre(&mut self, rect: Rect, y: u16, text: &str, style: Style) {
        let width = tos_term::str_width(text) as u16;
        let x = if width >= rect.width {
            rect.x
        } else {
            rect.x + (rect.width - width) / 2
        };
        self.text_clipped(x, y, rect.width, text, style);
    }

    /// Produce the escape sequences that turn the last frame into this one.
    pub fn render(&mut self) -> Vec<u8> {
        let mut out = Vec::with_capacity(1024);
        let same_size = self.previous.len() == self.cells.len();
        if self.first_frame || !same_size {
            out.extend_from_slice(b"\x1b[2J");
        }

        let mut last_style: Option<Style> = None;
        let mut last_position: Option<(u16, u16)> = None;

        for y in 0..self.rows {
            for x in 0..self.cols {
                let index = y as usize * self.cols as usize + x as usize;
                let cell = self.cells[index];
                if same_size && !self.first_frame && self.previous[index] == cell {
                    continue;
                }
                // Only move the cursor when the next cell is not where it
                // already is; runs of changed cells then cost nothing extra.
                if last_position != Some((x, y)) {
                    out.extend_from_slice(format!("\x1b[{};{}H", y + 1, x + 1).as_bytes());
                }
                if last_style != Some(cell.style) {
                    out.extend_from_slice(cell.style.sgr().as_bytes());
                    last_style = Some(cell.style);
                }
                let mut buf = [0u8; 4];
                out.extend_from_slice(cell.ch.encode_utf8(&mut buf).as_bytes());
                let width = tos_term::char_width(cell.ch).max(1) as u16;
                last_position = Some((x + width, y));
            }
        }

        out.extend_from_slice(b"\x1b[0m");
        match self.cursor {
            Some((x, y)) => {
                out.extend_from_slice(format!("\x1b[{};{}H\x1b[?25h", y + 1, x + 1).as_bytes());
            }
            None => out.extend_from_slice(b"\x1b[?25l"),
        }

        self.previous.clear();
        self.previous.extend_from_slice(&self.cells);
        self.first_frame = false;
        out
    }

    /// The screen as plain text, for tests.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for y in 0..self.rows {
            let mut line = String::new();
            for x in 0..self.cols {
                line.push(self.cells[y as usize * self.cols as usize + x as usize].ch);
            }
            out.push_str(line.trim_end());
            if y + 1 < self.rows {
                out.push('\n');
            }
        }
        out
    }

    /// Whether any cell contains this text, for tests.
    pub fn contains(&self, needle: &str) -> bool {
        self.to_text().contains(needle)
    }
}

/// Take over the terminal for a full screen application, and give it back.
pub struct Terminal {
    raw: Option<tos_platform::tty::RawMode>,
    output: std::io::Stdout,
    released: bool,
}

impl Terminal {
    /// Switch to the alternate screen and put the terminal in raw mode.
    pub fn acquire() -> io::Result<Terminal> {
        use std::os::unix::io::AsRawFd;
        let raw = tos_platform::tty::RawMode::acquire(io::stdin().as_raw_fd()).ok();
        let mut terminal = Terminal {
            raw,
            output: io::stdout(),
            released: false,
        };
        terminal.write(b"\x1b[?1049h\x1b[?25l\x1b[2J")?;
        Ok(terminal)
    }

    pub fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.output.write_all(bytes)?;
        self.output.flush()
    }

    pub fn size(&self) -> (u16, u16) {
        use std::os::unix::io::AsRawFd;
        match tos_platform::tty::terminal_size(io::stdout().as_raw_fd()) {
            Ok(size) if size.cols > 0 && size.rows > 0 => (size.cols, size.rows),
            // A sensible default beats refusing to draw at all.
            _ => (80, 24),
        }
    }

    /// Put the terminal back the way it was found.
    pub fn release(&mut self) {
        if self.released {
            return;
        }
        let _ = self.output.write_all(b"\x1b[0m\x1b[?25h\x1b[?1049l");
        let _ = self.output.flush();
        if let Some(raw) = self.raw.as_mut() {
            raw.restore();
        }
        self.released = true;
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        // However the installer ends, including a panic, the user gets their
        // shell back rather than a raw alternate screen.
        self.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_lands_where_it_is_put() {
        let mut screen = Screen::new(20, 3);
        screen.text(2, 1, "hello", Style::default());
        assert_eq!(screen.to_text(), "\n  hello\n");
    }

    #[test]
    fn text_is_clipped_at_the_edge() {
        let mut screen = Screen::new(6, 1);
        screen.text(3, 0, "abcdef", Style::default());
        assert_eq!(screen.to_text(), "   abc");
    }

    #[test]
    fn clipped_text_gets_an_ellipsis() {
        let mut screen = Screen::new(20, 1);
        screen.text_clipped(0, 0, 6, "abcdefghij", Style::default());
        assert_eq!(screen.to_text(), "abcde…");
    }

    #[test]
    fn short_text_is_not_clipped() {
        let mut screen = Screen::new(20, 1);
        screen.text_clipped(0, 0, 10, "abc", Style::default());
        assert_eq!(screen.to_text(), "abc");
    }

    #[test]
    fn a_frame_has_corners_and_edges() {
        let mut screen = Screen::new(6, 3);
        screen.frame(Rect::new(0, 0, 6, 3), None, Style::default());
        assert_eq!(screen.to_text(), "┌────┐\n│    │\n└────┘");
    }

    #[test]
    fn a_frame_can_carry_a_title() {
        let mut screen = Screen::new(14, 3);
        screen.frame(Rect::new(0, 0, 14, 3), Some("disks"), Style::default());
        assert!(screen.contains("disks"));
        assert!(screen.to_text().starts_with("┌─ disks "));
    }

    #[test]
    fn centring_puts_text_in_the_middle() {
        let mut screen = Screen::new(11, 1);
        screen.centre(Rect::new(0, 0, 11, 1), 0, "abc", Style::default());
        assert_eq!(screen.to_text(), "    abc");
    }

    #[test]
    fn the_first_frame_clears_the_screen() {
        let mut screen = Screen::new(4, 2);
        screen.text(0, 0, "hi", Style::default());
        let bytes = String::from_utf8(screen.render()).unwrap();
        assert!(bytes.contains("\x1b[2J"));
        assert!(bytes.contains("hi"));
    }

    #[test]
    fn an_unchanged_frame_emits_almost_nothing() {
        let mut screen = Screen::new(20, 5);
        screen.text(0, 0, "hello", Style::default());
        screen.render();
        // Drawing the same thing again should not repaint any cell.
        screen.text(0, 0, "hello", Style::default());
        let bytes = String::from_utf8(screen.render()).unwrap();
        assert!(
            !bytes.contains("hello"),
            "redrew an unchanged frame: {bytes:?}"
        );
    }

    #[test]
    fn only_changed_cells_are_repainted() {
        let mut screen = Screen::new(20, 2);
        screen.text(0, 0, "hello", Style::default());
        screen.text(0, 1, "world", Style::default());
        screen.render();

        screen.clear();
        screen.text(0, 0, "hello", Style::default());
        screen.text(0, 1, "WORLD", Style::default());
        let bytes = String::from_utf8(screen.render()).unwrap();
        assert!(bytes.contains("WORLD"));
        assert!(!bytes.contains("hello"));
    }

    #[test]
    fn resizing_forces_a_full_repaint() {
        let mut screen = Screen::new(20, 2);
        screen.text(0, 0, "hello", Style::default());
        screen.render();
        screen.resize(30, 4);
        screen.text(0, 0, "hello", Style::default());
        let bytes = String::from_utf8(screen.render()).unwrap();
        assert!(bytes.contains("\x1b[2J"));
        assert!(bytes.contains("hello"));
    }

    #[test]
    fn the_cursor_is_hidden_unless_it_is_placed() {
        let mut screen = Screen::new(10, 2);
        let bytes = String::from_utf8(screen.render()).unwrap();
        assert!(bytes.ends_with("\x1b[?25l"));

        screen.set_cursor(3, 1);
        let bytes = String::from_utf8(screen.render()).unwrap();
        assert!(bytes.contains("\x1b[2;4H"));
        assert!(bytes.ends_with("\x1b[?25h"));
    }

    #[test]
    fn styles_become_sgr_sequences() {
        let plain = Style::default().sgr();
        assert_eq!(plain, "\x1b[0m");
        let fancy = Style::fg(Color::rgb(1, 2, 3)).bold().sgr();
        assert_eq!(fancy, "\x1b[0;1;38;2;1;2;3m");
    }

    #[test]
    fn a_style_run_emits_one_sequence() {
        let mut screen = Screen::new(20, 1);
        screen.text(0, 0, "aaaa", Style::fg(Color::rgb(9, 9, 9)));
        let bytes = String::from_utf8(screen.render()).unwrap();
        assert_eq!(bytes.matches("38;2;9;9;9").count(), 1);
    }

    #[test]
    fn rectangles_inset_and_centre() {
        let outer = Rect::new(0, 0, 20, 10);
        assert_eq!(outer.inset(2), Rect::new(2, 2, 16, 6));
        assert_eq!(Rect::centred(outer, 10, 4), Rect::new(5, 3, 10, 4));
        // Insetting past the middle yields an empty rectangle, not a panic.
        assert!(Rect::new(0, 0, 2, 2).inset(4).is_empty());
    }

    #[test]
    fn wide_characters_advance_two_columns() {
        let mut screen = Screen::new(10, 1);
        let end = screen.text(0, 0, "漢a", Style::default());
        assert_eq!(end, 3);
    }
}
