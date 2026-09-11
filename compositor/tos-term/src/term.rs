//! Terminal semantics: everything the parser's events mean.
//!
//! [`Terminal`] owns a primary and an alternate [`Grid`], the cursor, the mode
//! state and the scroll region, and implements [`Perform`] so the parser can
//! drive it directly.

use std::time::{Duration, Instant};

use crate::cell::{Attrs, Cell, Flags, GraphicsRef, Underline};
use crate::color::{Color, Palette, Rgb};
use crate::graphics::{Action, GraphicsCommand, GraphicsStore};
use crate::grid::{Grid, Region};
use crate::modes::{
    CursorShape, CursorStyle, KeyboardFlags, KeyboardStack, Modes, MouseEncoding, MouseState,
    MouseTracking,
};
use crate::parser::{Params, Parser, Perform};
use crate::width::char_width;

/// Character sets that can be designated into G0..G3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Charset {
    #[default]
    Ascii,
    /// DEC special graphics, the line drawing set.
    DecSpecial,
}

impl Charset {
    fn map(self, c: char) -> char {
        match self {
            Charset::Ascii => c,
            Charset::DecSpecial => match c {
                '`' => '◆',
                'a' => '▒',
                'b' => '\t',
                'c' => '\u{c}',
                'd' => '\r',
                'e' => '\n',
                'f' => '°',
                'g' => '±',
                'h' => '␤',
                'i' => '\u{b}',
                'j' => '┘',
                'k' => '┐',
                'l' => '┌',
                'm' => '└',
                'n' => '┼',
                'o' => '⎺',
                'p' => '⎻',
                'q' => '─',
                'r' => '⎼',
                's' => '⎽',
                't' => '├',
                'u' => '┤',
                'v' => '┴',
                'w' => '┬',
                'x' => '│',
                'y' => '≤',
                'z' => '≥',
                '{' => 'π',
                '|' => '≠',
                '}' => '£',
                '~' => '·',
                '_' => ' ',
                other => other,
            },
        }
    }
}

/// Cursor position and pen.
#[derive(Debug, Clone, Copy, Default)]
pub struct Cursor {
    pub x: usize,
    pub y: usize,
    pub attrs: Attrs,
    /// Set once the cursor has written the last column; the wrap happens on
    /// the next printable character, not immediately.
    pub wrap_pending: bool,
}

#[derive(Debug, Clone, Copy)]
struct SavedCursor {
    cursor: Cursor,
    charsets: [Charset; 4],
    gl: usize,
    gr: usize,
    origin: bool,
}

impl Default for SavedCursor {
    fn default() -> Self {
        SavedCursor {
            cursor: Cursor::default(),
            charsets: [Charset::Ascii; 4],
            gl: 0,
            gr: 0,
            origin: false,
        }
    }
}

/// Something the compositor has to act on.
#[derive(Debug, Clone, PartialEq)]
pub enum TermEvent {
    Bell,
    TitleChanged(String),
    IconTitleChanged(String),
    /// OSC 7: the shell reported its working directory.
    CwdChanged(String),
    /// OSC 9 / OSC 777: a desktop notification.
    Notify {
        title: String,
        body: String,
    },
    /// OSC 52 store; `selection` is the raw selector character.
    ClipboardStore {
        selection: char,
        data: Vec<u8>,
    },
    /// OSC 52 query; the compositor answers with [`Terminal::report_clipboard`].
    ClipboardLoad {
        selection: char,
    },
    /// The application changed the cursor shape.
    CursorStyleChanged(CursorStyle),
    /// Mouse or keyboard reporting changed; input encoding must be re-read.
    ReportingChanged,
    /// Screen contents changed enough that a repaint is required.
    Repaint,
    /// A window manipulation request tOS chose to expose (CSI t).
    WindowOp(u16, u16, u16),
}

/// Row level damage tracking, so the renderer only repaints what moved.
#[derive(Debug, Clone)]
pub struct Damage {
    rows: Vec<bool>,
    full: bool,
}

impl Damage {
    fn new(rows: usize) -> Self {
        Damage {
            rows: vec![true; rows],
            full: true,
        }
    }

    fn resize(&mut self, rows: usize) {
        self.rows.resize(rows, true);
        self.full = true;
    }

    pub fn mark_row(&mut self, y: usize) {
        if let Some(slot) = self.rows.get_mut(y) {
            *slot = true;
        }
    }

    pub fn mark_range(&mut self, from: usize, to: usize) {
        for y in from..to.min(self.rows.len()) {
            self.rows[y] = true;
        }
    }

    pub fn mark_all(&mut self) {
        self.full = true;
        self.rows.fill(true);
    }

    pub fn is_row_dirty(&self, y: usize) -> bool {
        self.full || self.rows.get(y).copied().unwrap_or(true)
    }

    pub fn is_dirty(&self) -> bool {
        self.full || self.rows.iter().any(|d| *d)
    }

    pub fn is_full(&self) -> bool {
        self.full
    }

    pub fn clear(&mut self) {
        self.full = false;
        self.rows.fill(false);
    }
}

/// Tunables that a session may want to differ per pane.
#[derive(Debug, Clone)]
pub struct TerminalConfig {
    pub scrollback: usize,
    /// Byte budget for stored graphics data.
    pub graphics_budget: usize,
    /// Pixel size of one cell, needed to size graphics placements.
    pub cell_width: u32,
    pub cell_height: u32,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        TerminalConfig {
            scrollback: 10_000,
            graphics_budget: 256 * 1024 * 1024,
            cell_width: 8,
            cell_height: 16,
        }
    }
}

/// A terminal: grid, cursor, modes and the parser that drives them.
pub struct Terminal {
    parser: Parser,
    config: TerminalConfig,

    screen: Grid,
    /// The buffer that is not currently displayed.
    inactive: Grid,

    cursor: Cursor,
    saved_cursor: SavedCursor,
    saved_cursor_alt: SavedCursor,
    /// Row the cursor sat on in the buffer that is not displayed, so that
    /// resizing trims the right end of it.
    inactive_cursor_y: usize,

    scroll_region: Region,
    tabs: Vec<bool>,

    pub modes: Modes,
    mouse: MouseState,
    keyboard: KeyboardStack,
    cursor_style: CursorStyle,

    charsets: [Charset; 4],
    gl: usize,
    gr: usize,
    /// Set by SS2/SS3 for exactly one character.
    single_shift: Option<usize>,

    palette: Palette,
    default_palette: Palette,

    hyperlinks: Vec<String>,
    title: String,

    graphics: GraphicsStore,
    apc_buf: Vec<u8>,
    dcs_buf: Vec<u8>,
    dcs_kind: Option<u8>,

    damage: Damage,
    output: Vec<u8>,
    events: Vec<TermEvent>,
    /// Last printable character, for REP (CSI b).
    last_printed: Option<char>,
}

const MAX_HYPERLINKS: usize = 4096;
const TAB_WIDTH: usize = 8;

impl Terminal {
    pub fn new(cols: usize, rows: usize, config: TerminalConfig) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        let palette = Palette::new();
        Terminal {
            parser: Parser::new(),
            screen: Grid::new(cols, rows, config.scrollback),
            inactive: Grid::new(cols, rows, 0),
            cursor: Cursor::default(),
            saved_cursor: SavedCursor::default(),
            saved_cursor_alt: SavedCursor::default(),
            inactive_cursor_y: 0,
            scroll_region: Region::new(0, rows),
            tabs: default_tabs(cols),
            modes: Modes::default(),
            mouse: MouseState::default(),
            keyboard: KeyboardStack::default(),
            cursor_style: CursorStyle::default(),
            charsets: [Charset::Ascii; 4],
            gl: 0,
            gr: 0,
            single_shift: None,
            default_palette: palette.clone(),
            palette,
            hyperlinks: Vec::new(),
            title: String::new(),
            graphics: GraphicsStore::new(config.graphics_budget),
            apc_buf: Vec::new(),
            dcs_buf: Vec::new(),
            dcs_kind: None,
            damage: Damage::new(rows),
            output: Vec::new(),
            events: Vec::new(),
            last_printed: None,
            config,
        }
    }

    /// Feed bytes read from the PTY.
    pub fn advance(&mut self, bytes: &[u8]) {
        // The parser is moved out so it can borrow `self` as the performer.
        let mut parser = std::mem::take(&mut self.parser);
        parser.advance(self, bytes);
        self.parser = parser;
    }

    pub fn cols(&self) -> usize {
        self.screen.cols()
    }

    pub fn rows(&self) -> usize {
        self.screen.rows()
    }

    pub fn grid(&self) -> &Grid {
        &self.screen
    }

    pub fn grid_mut(&mut self) -> &mut Grid {
        &mut self.screen
    }

    pub fn cursor(&self) -> Cursor {
        self.cursor
    }

    pub fn cursor_style(&self) -> CursorStyle {
        self.cursor_style
    }

    pub fn palette(&self) -> &Palette {
        &self.palette
    }

    /// Replace the palette, including the one a reset goes back to.
    ///
    /// Colours that came from configuration have to become this terminal's
    /// idea of default as well, or the first `OSC 104` an application sends
    /// would quietly put the built-in palette back.
    pub fn set_palette(&mut self, palette: Palette) {
        self.default_palette = palette.clone();
        self.palette = palette;
    }

    pub fn mouse(&self) -> MouseState {
        self.mouse
    }

    pub fn keyboard_flags(&self) -> KeyboardFlags {
        self.keyboard.current()
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn graphics(&self) -> &GraphicsStore {
        &self.graphics
    }

    pub fn hyperlink(&self, id: u16) -> Option<&str> {
        self.hyperlinks.get(id as usize).map(|s| s.as_str())
    }

    pub fn damage(&self) -> &Damage {
        &self.damage
    }

    pub fn damage_mut(&mut self) -> &mut Damage {
        &mut self.damage
    }

    pub fn clear_damage(&mut self) {
        self.damage.clear();
    }

    /// Bytes that must be written back to the PTY (query responses).
    pub fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.output)
    }

    pub fn take_events(&mut self) -> Vec<TermEvent> {
        std::mem::take(&mut self.events)
    }

    pub fn write_to_pty(&mut self, bytes: &[u8]) {
        self.output.extend_from_slice(bytes);
    }

    /// Answer an OSC 52 clipboard query. Whether the real selection or an
    /// empty one goes back is the compositor's call — reading the clipboard is
    /// a privilege the pane has to be granted, and the terminal does not know
    /// what the user granted.
    pub fn report_clipboard(&mut self, selection: char, data: &[u8]) {
        let encoded = crate::graphics::encode_base64(data);
        let response = format!("\x1b]52;{selection};{encoded}\x1b\\");
        self.output.extend_from_slice(response.as_bytes());
    }

    /// Scroll the viewport through history. Returns true if anything moved.
    pub fn scroll_display(&mut self, delta: isize) -> bool {
        // The alternate screen has no history to scroll through.
        if self.modes.alt_screen {
            return false;
        }
        let moved = self.screen.scroll_display(delta);
        if moved {
            self.damage.mark_all();
        }
        moved
    }

    pub fn reset_display_offset(&mut self) {
        if self.screen.reset_display_offset() {
            self.damage.mark_all();
        }
    }

    pub fn display_offset(&self) -> usize {
        self.screen.display_offset()
    }

    /// Resize the terminal, keeping the cursor on its line.
    pub fn resize(&mut self, cols: usize, rows: usize) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if cols == self.cols() && rows == self.rows() {
            return;
        }

        let attrs = self.cursor.attrs;
        let shift = self.screen.resize(cols, rows, self.cursor.y, &attrs);
        // Passing zero here would make the hidden buffer shed rows from the
        // bottom, destroying the newest output of whichever screen is not on
        // display.
        let inactive_y = self.inactive_cursor_y.min(self.inactive.rows() - 1);
        let inactive_shift = self.inactive.resize(cols, rows, inactive_y, &attrs);
        self.inactive_cursor_y = (inactive_y + inactive_shift).min(rows - 1);

        self.cursor.y = (self.cursor.y + shift).min(rows - 1);
        self.cursor.x = self.cursor.x.min(cols - 1);
        self.cursor.wrap_pending = false;

        self.scroll_region = Region::new(0, rows);
        self.tabs = default_tabs(cols);
        self.damage.resize(rows);
        self.graphics.retain_rows(rows as u16);
        self.events.push(TermEvent::Repaint);
    }

    /// RIS: reset everything.
    pub fn reset(&mut self) {
        let cols = self.cols();
        let rows = self.rows();
        let config = self.config.clone();
        // Responses already queued belong to the host, not to the screen.
        let output = std::mem::take(&mut self.output);
        let mut events = std::mem::take(&mut self.events);
        *self = Terminal::new(cols, rows, config);
        self.output = output;
        events.push(TermEvent::Repaint);
        events.push(TermEvent::ReportingChanged);
        self.events = events;
    }

    // ---- internal helpers ------------------------------------------------

    fn mark_cursor_row(&mut self) {
        let y = self.cursor.y;
        self.damage.mark_row(y);
    }

    /// Top and bottom limits for cursor movement, respecting origin mode.
    fn bounds(&self) -> (usize, usize) {
        if self.modes.origin {
            (self.scroll_region.top, self.scroll_region.bottom)
        } else {
            (0, self.rows())
        }
    }

    fn set_cursor(&mut self, x: usize, y: usize) {
        let (top, bottom) = self.bounds();
        self.mark_cursor_row();
        self.cursor.x = x.min(self.cols().saturating_sub(1));
        self.cursor.y = (top + y).min(bottom.saturating_sub(1));
        self.cursor.wrap_pending = false;
        self.mark_cursor_row();
    }

    fn move_cursor_rel(&mut self, dx: isize, dy: isize) {
        self.mark_cursor_row();
        let cols = self.cols() as isize;
        let x = (self.cursor.x as isize + dx).clamp(0, cols - 1) as usize;

        // Vertical movement stops at the scroll region when the cursor starts
        // inside it, which is what DEC terminals do.
        let (top, bottom) = if self.cursor.y >= self.scroll_region.top
            && self.cursor.y < self.scroll_region.bottom
        {
            (self.scroll_region.top, self.scroll_region.bottom)
        } else {
            (0, self.rows())
        };
        let y = (self.cursor.y as isize + dy).clamp(top as isize, bottom as isize - 1) as usize;

        self.cursor.x = x;
        self.cursor.y = y;
        self.cursor.wrap_pending = false;
        self.mark_cursor_row();
    }

    /// Move down one line, scrolling the region if already at the bottom.
    fn linefeed(&mut self) {
        self.mark_cursor_row();
        if self.cursor.y + 1 == self.scroll_region.bottom {
            let attrs = self.cursor.attrs;
            self.screen.scroll_up(self.scroll_region, 1, &attrs, true);
            self.graphics.scroll(1);
            self.damage.mark_all();
        } else if self.cursor.y + 1 < self.rows() {
            self.cursor.y += 1;
            self.mark_cursor_row();
        }
    }

    fn reverse_index(&mut self) {
        self.mark_cursor_row();
        if self.cursor.y == self.scroll_region.top {
            let attrs = self.cursor.attrs;
            self.screen.scroll_down(self.scroll_region, 1, &attrs);
            self.damage.mark_all();
        } else if self.cursor.y > 0 {
            self.cursor.y -= 1;
            self.mark_cursor_row();
        }
    }

    fn carriage_return(&mut self) {
        self.mark_cursor_row();
        self.cursor.x = 0;
        self.cursor.wrap_pending = false;
    }

    fn backspace(&mut self) {
        if self.cursor.wrap_pending {
            self.cursor.wrap_pending = false;
        } else if self.cursor.x > 0 {
            self.cursor.x -= 1;
        }
        self.mark_cursor_row();
    }

    fn tab(&mut self, count: usize) {
        let cols = self.cols();
        for _ in 0..count.max(1) {
            let mut x = self.cursor.x + 1;
            while x < cols && !self.tabs[x] {
                x += 1;
            }
            self.cursor.x = x.min(cols - 1);
        }
        self.cursor.wrap_pending = false;
        self.mark_cursor_row();
    }

    fn back_tab(&mut self, count: usize) {
        for _ in 0..count.max(1) {
            if self.cursor.x == 0 {
                break;
            }
            let mut x = self.cursor.x - 1;
            while x > 0 && !self.tabs[x] {
                x -= 1;
            }
            self.cursor.x = x;
        }
        self.cursor.wrap_pending = false;
        self.mark_cursor_row();
    }

    fn wrap_line(&mut self) {
        let y = self.cursor.y;
        self.screen.row_mut(y).wrapped = true;
        self.cursor.x = 0;
        self.cursor.wrap_pending = false;
        self.linefeed();
    }

    /// Write one character at the cursor and advance.
    fn put_char(&mut self, c: char) {
        let width = char_width(c);

        if width == 0 {
            // Combining mark: attach to the cell the cursor last wrote.
            let (x, y) = if self.cursor.wrap_pending {
                (self.cursor.x, self.cursor.y)
            } else if self.cursor.x > 0 {
                (self.cursor.x - 1, self.cursor.y)
            } else {
                return;
            };
            if let Some(cell) = self.screen.cell_mut(x, y) {
                cell.push_zerowidth(c);
                self.damage.mark_row(y);
            }
            return;
        }

        if self.cursor.wrap_pending {
            if self.modes.wraparound {
                self.wrap_line();
            } else {
                self.cursor.wrap_pending = false;
            }
        }

        let cols = self.cols();
        if width == 2 && self.cursor.x + 1 >= cols {
            if self.modes.wraparound {
                // A double width glyph never straddles the right margin.
                let (x, y) = (self.cursor.x, self.cursor.y);
                if let Some(cell) = self.screen.cell_mut(x, y) {
                    cell.clear(&self.cursor.attrs);
                }
                self.wrap_line();
            } else {
                return;
            }
        }

        if self.modes.insert {
            self.shift_right(width);
        }

        let attrs = self.cursor.attrs;
        let (x, y) = (self.cursor.x, self.cursor.y);

        // Overwriting half of an existing wide glyph must erase its other half.
        self.clean_wide_neighbours(x, y, width);

        if let Some(cell) = self.screen.cell_mut(x, y) {
            *cell = Cell::new(c, attrs);
            if width == 2 {
                cell.attrs.flags.insert(Flags::WIDE);
            }
        }
        if width == 2 {
            if let Some(spacer) = self.screen.cell_mut(x + 1, y) {
                *spacer = Cell::new(' ', attrs);
                spacer.attrs.flags.insert(Flags::WIDE_SPACER);
            }
        }
        self.damage.mark_row(y);

        self.last_printed = Some(c);

        let next = x + width;
        if next >= cols {
            self.cursor.x = cols - 1;
            self.cursor.wrap_pending = true;
        } else {
            self.cursor.x = next;
        }
    }

    /// Clear the dangling half of any wide glyph that the write at `x` breaks.
    fn clean_wide_neighbours(&mut self, x: usize, y: usize, width: usize) {
        let attrs = self.cursor.attrs;
        if x > 0 {
            let left_is_wide = self
                .screen
                .cell(x - 1, y)
                .map(|c| c.attrs.flags.contains(Flags::WIDE))
                .unwrap_or(false);
            let here_is_spacer = self
                .screen
                .cell(x, y)
                .map(|c| c.attrs.flags.contains(Flags::WIDE_SPACER))
                .unwrap_or(false);
            if left_is_wide && here_is_spacer {
                if let Some(cell) = self.screen.cell_mut(x - 1, y) {
                    cell.clear(&attrs);
                }
            }
        }
        let last = x + width;
        if self
            .screen
            .cell(last, y)
            .map(|c| c.attrs.flags.contains(Flags::WIDE_SPACER))
            .unwrap_or(false)
        {
            if let Some(cell) = self.screen.cell_mut(last, y) {
                cell.clear(&attrs);
            }
        }
    }

    fn shift_right(&mut self, count: usize) {
        let (x, y) = (self.cursor.x, self.cursor.y);
        let cols = self.cols();
        let attrs = self.cursor.attrs;
        let row = self.screen.row_mut(y);
        let cells = row.cells_mut();
        let count = count.min(cols - x);
        cells[x..].rotate_right(count);
        for cell in &mut cells[x..x + count] {
            cell.clear(&attrs);
        }
        self.damage.mark_row(y);
    }

    fn shift_left(&mut self, count: usize) {
        let (x, y) = (self.cursor.x, self.cursor.y);
        let cols = self.cols();
        let attrs = self.cursor.attrs;
        let row = self.screen.row_mut(y);
        let cells = row.cells_mut();
        let count = count.min(cols - x);
        cells[x..].rotate_left(count);
        let from = cols - count;
        for cell in &mut cells[from..] {
            cell.clear(&attrs);
        }
        self.damage.mark_row(y);
    }

    fn swap_alt_screen(&mut self, to_alt: bool) {
        if to_alt == self.modes.alt_screen {
            return;
        }
        // The row the cursor is leaving belongs to the buffer being hidden.
        self.inactive_cursor_y = self.cursor.y;
        std::mem::swap(&mut self.screen, &mut self.inactive);
        self.modes.alt_screen = to_alt;
        if to_alt {
            // Entering the alternate screen always starts from a clean slate.
            let attrs = self.cursor.attrs;
            self.screen.clear_screen(&attrs);
            self.screen.clear_history();
        }
        self.graphics.clear();
        self.damage.mark_all();
        self.events.push(TermEvent::Repaint);
    }

    fn save_cursor(&mut self) {
        let saved = SavedCursor {
            cursor: self.cursor,
            charsets: self.charsets,
            gl: self.gl,
            gr: self.gr,
            origin: self.modes.origin,
        };
        if self.modes.alt_screen {
            self.saved_cursor_alt = saved;
        } else {
            self.saved_cursor = saved;
        }
    }

    fn restore_cursor(&mut self) {
        let saved = if self.modes.alt_screen {
            self.saved_cursor_alt
        } else {
            self.saved_cursor
        };
        self.mark_cursor_row();
        self.cursor = saved.cursor;
        self.charsets = saved.charsets;
        self.gl = saved.gl;
        self.gr = saved.gr;
        self.modes.origin = saved.origin;
        self.cursor.x = self.cursor.x.min(self.cols() - 1);
        self.cursor.y = self.cursor.y.min(self.rows() - 1);
        self.mark_cursor_row();
    }

    fn intern_hyperlink(&mut self, uri: &str) -> Option<u16> {
        if uri.is_empty() {
            return None;
        }
        if let Some(i) = self.hyperlinks.iter().position(|u| u == uri) {
            return Some(i as u16);
        }
        if self.hyperlinks.len() >= MAX_HYPERLINKS {
            return None;
        }
        self.hyperlinks.push(uri.to_string());
        Some((self.hyperlinks.len() - 1) as u16)
    }
}

fn default_tabs(cols: usize) -> Vec<bool> {
    (0..cols).map(|i| i > 0 && i % TAB_WIDTH == 0).collect()
}

// ---------------------------------------------------------------------------
// Erase and edit operations
// ---------------------------------------------------------------------------

impl Terminal {
    /// ED: erase in display.
    fn erase_in_display(&mut self, mode: u16) {
        let attrs = self.cursor.attrs;
        let (x, y) = (self.cursor.x, self.cursor.y);
        let rows = self.rows();
        let cols = self.cols();
        match mode {
            0 => {
                self.screen.row_mut(y).clear_range(x, cols, &attrs);
                for row in y + 1..rows {
                    self.screen.row_mut(row).clear(&attrs);
                }
                self.damage.mark_range(y, rows);
            }
            1 => {
                self.screen.row_mut(y).clear_range(0, x + 1, &attrs);
                for row in 0..y {
                    self.screen.row_mut(row).clear(&attrs);
                }
                self.damage.mark_range(0, y + 1);
            }
            2 => {
                self.screen.clear_screen(&attrs);
                self.graphics.clear();
                self.damage.mark_all();
            }
            3 => {
                self.screen.clear_history();
                self.damage.mark_all();
            }
            _ => {}
        }
        self.cursor.wrap_pending = false;
    }

    /// EL: erase in line.
    fn erase_in_line(&mut self, mode: u16) {
        let attrs = self.cursor.attrs;
        let (x, y) = (self.cursor.x, self.cursor.y);
        let cols = self.cols();
        let row = self.screen.row_mut(y);
        match mode {
            0 => row.clear_range(x, cols, &attrs),
            1 => row.clear_range(0, x + 1, &attrs),
            2 => row.clear(&attrs),
            _ => {}
        }
        self.damage.mark_row(y);
        self.cursor.wrap_pending = false;
    }

    /// ECH: erase characters without moving the cursor.
    fn erase_chars(&mut self, count: usize) {
        let attrs = self.cursor.attrs;
        let (x, y) = (self.cursor.x, self.cursor.y);
        let end = (x + count).min(self.cols());
        self.screen.row_mut(y).clear_range(x, end, &attrs);
        self.damage.mark_row(y);
    }

    /// IL: insert blank lines at the cursor, within the scroll region.
    fn insert_lines(&mut self, count: usize) {
        if self.cursor.y < self.scroll_region.top || self.cursor.y >= self.scroll_region.bottom {
            return;
        }
        let attrs = self.cursor.attrs;
        let region = Region::new(self.cursor.y, self.scroll_region.bottom);
        self.screen.scroll_down(region, count, &attrs);
        self.cursor.x = 0;
        self.damage.mark_range(region.top, region.bottom);
    }

    /// DL: delete lines at the cursor, within the scroll region.
    fn delete_lines(&mut self, count: usize) {
        if self.cursor.y < self.scroll_region.top || self.cursor.y >= self.scroll_region.bottom {
            return;
        }
        let attrs = self.cursor.attrs;
        let region = Region::new(self.cursor.y, self.scroll_region.bottom);
        // Deleted lines are discarded, never pushed into scrollback.
        self.screen.scroll_up(region, count, &attrs, false);
        self.cursor.x = 0;
        self.damage.mark_range(region.top, region.bottom);
    }

    /// DECALN: fill the screen with E, used by conformance tests.
    fn screen_alignment_test(&mut self) {
        let rows = self.rows();
        let cols = self.cols();
        for y in 0..rows {
            let row = self.screen.row_mut(y);
            for x in 0..cols {
                row.cells_mut()[x] = Cell::new('E', Attrs::default());
            }
        }
        self.cursor.x = 0;
        self.cursor.y = 0;
        self.cursor.wrap_pending = false;
        self.damage.mark_all();
    }

    /// DECSTR: soft reset.
    fn soft_reset(&mut self) {
        self.modes.origin = false;
        self.modes.insert = false;
        self.modes.wraparound = true;
        self.modes.cursor_visible = true;
        self.modes.application_cursor_keys = false;
        self.modes.application_keypad = false;
        self.scroll_region = Region::new(0, self.rows());
        self.cursor.attrs = Attrs::default();
        self.charsets = [Charset::Ascii; 4];
        self.gl = 0;
        self.gr = 0;
        self.single_shift = None;
        self.saved_cursor = SavedCursor::default();
        self.saved_cursor_alt = SavedCursor::default();
        self.keyboard.reset();
        self.mouse = MouseState::default();
        self.set_cursor(0, 0);
        self.events.push(TermEvent::ReportingChanged);
    }
}

// ---------------------------------------------------------------------------
// SGR
// ---------------------------------------------------------------------------

/// Read an extended color starting at group `i`.
///
/// Handles both the colon form (`38:2::r:g:b`, one group) and the legacy
/// semicolon form (`38;2;r;g;b`, several groups).
fn parse_color(groups: &[&[u16]], i: &mut usize) -> Option<Color> {
    let group = groups[*i];
    if group.len() > 1 {
        // Colon form: everything is inside this group.
        return match group[1] {
            2 => {
                // Either 38:2:r:g:b or 38:2:colorspace:r:g:b.
                let rest = &group[2..];
                let rgb = match rest.len() {
                    3 => (rest[0], rest[1], rest[2]),
                    n if n >= 4 => (rest[1], rest[2], rest[3]),
                    _ => return None,
                };
                Some(Color::Rgb(Rgb::new(rgb.0 as u8, rgb.1 as u8, rgb.2 as u8)))
            }
            5 => group.get(2).map(|&n| Color::Indexed(n as u8)),
            _ => None,
        };
    }

    // Semicolon form: consume the following groups.
    let kind = groups.get(*i + 1).and_then(|g| g.first().copied())?;
    match kind {
        2 => {
            let r = groups.get(*i + 2).and_then(|g| g.first().copied())?;
            let g = groups.get(*i + 3).and_then(|g| g.first().copied())?;
            let b = groups.get(*i + 4).and_then(|g| g.first().copied())?;
            *i += 4;
            Some(Color::Rgb(Rgb::new(r as u8, g as u8, b as u8)))
        }
        5 => {
            let n = groups.get(*i + 2).and_then(|g| g.first().copied())?;
            *i += 2;
            Some(Color::Indexed(n as u8))
        }
        _ => None,
    }
}

impl Terminal {
    fn apply_sgr(&mut self, params: &Params) {
        let groups: Vec<&[u16]> = params.iter().collect();
        if groups.is_empty() {
            self.cursor.attrs.reset();
            return;
        }

        let mut i = 0;
        while i < groups.len() {
            let group = groups[i];
            let attrs = &mut self.cursor.attrs;
            match group[0] {
                0 => attrs.reset(),
                1 => attrs.flags.insert(Flags::BOLD),
                2 => attrs.flags.insert(Flags::DIM),
                3 => attrs.flags.insert(Flags::ITALIC),
                4 => {
                    attrs.underline = if group.len() > 1 {
                        Underline::from_sgr_param(group[1])
                    } else {
                        Underline::Single
                    };
                }
                5 | 6 => attrs.flags.insert(Flags::BLINK),
                7 => attrs.flags.insert(Flags::REVERSE),
                8 => attrs.flags.insert(Flags::HIDDEN),
                9 => attrs.flags.insert(Flags::STRIKEOUT),
                21 => attrs.underline = Underline::Double,
                22 => attrs.flags.remove(Flags::BOLD.union(Flags::DIM)),
                23 => attrs.flags.remove(Flags::ITALIC),
                24 => attrs.underline = Underline::None,
                25 => attrs.flags.remove(Flags::BLINK),
                27 => attrs.flags.remove(Flags::REVERSE),
                28 => attrs.flags.remove(Flags::HIDDEN),
                29 => attrs.flags.remove(Flags::STRIKEOUT),
                c @ 30..=37 => attrs.fg = Color::Indexed((c - 30) as u8),
                38 => {
                    if let Some(color) = parse_color(&groups, &mut i) {
                        self.cursor.attrs.fg = color;
                    }
                }
                39 => attrs.fg = Color::Default,
                c @ 40..=47 => attrs.bg = Color::Indexed((c - 40) as u8),
                48 => {
                    if let Some(color) = parse_color(&groups, &mut i) {
                        self.cursor.attrs.bg = color;
                    }
                }
                49 => attrs.bg = Color::Default,
                53 => attrs.flags.insert(Flags::OVERLINE),
                55 => attrs.flags.remove(Flags::OVERLINE),
                58 => {
                    if let Some(color) = parse_color(&groups, &mut i) {
                        self.cursor.attrs.underline_color = color;
                    }
                }
                59 => attrs.underline_color = Color::Default,
                c @ 90..=97 => attrs.fg = Color::Indexed((c - 90 + 8) as u8),
                c @ 100..=107 => attrs.bg = Color::Indexed((c - 100 + 8) as u8),
                _ => {}
            }
            i += 1;
        }
    }
}

// ---------------------------------------------------------------------------
// Modes
// ---------------------------------------------------------------------------

impl Terminal {
    fn set_mode(&mut self, mode: u16, enable: bool) {
        match mode {
            4 => self.modes.insert = enable,
            20 => self.modes.linefeed_newline = enable,
            _ => {}
        }
    }

    fn set_private_mode(&mut self, mode: u16, enable: bool) {
        let mut reporting_changed = false;
        match mode {
            1 => {
                self.modes.application_cursor_keys = enable;
                reporting_changed = true;
            }
            3 => {
                // DECCOLM: tOS panes are sized by the compositor, so the mode
                // only clears the screen as the spec requires.
                self.modes.allow_column_mode = enable;
                let attrs = self.cursor.attrs;
                self.screen.clear_screen(&attrs);
                self.set_cursor(0, 0);
                self.damage.mark_all();
            }
            5 => {
                self.modes.reverse_video = enable;
                self.damage.mark_all();
            }
            6 => {
                self.modes.origin = enable;
                self.set_cursor(0, 0);
            }
            7 => self.modes.wraparound = enable,
            8 => self.modes.autorepeat = enable,
            9 => {
                self.mouse.tracking = if enable {
                    MouseTracking::X10
                } else {
                    MouseTracking::None
                };
                reporting_changed = true;
            }
            12 => {
                self.cursor_style.blinking = enable;
                self.events
                    .push(TermEvent::CursorStyleChanged(self.cursor_style));
            }
            25 => {
                self.modes.cursor_visible = enable;
                self.mark_cursor_row();
            }
            1000 => {
                self.mouse.tracking = if enable {
                    MouseTracking::Normal
                } else {
                    MouseTracking::None
                };
                reporting_changed = true;
            }
            1002 => {
                self.mouse.tracking = if enable {
                    MouseTracking::ButtonEvent
                } else {
                    MouseTracking::None
                };
                reporting_changed = true;
            }
            1003 => {
                self.mouse.tracking = if enable {
                    MouseTracking::AnyEvent
                } else {
                    MouseTracking::None
                };
                reporting_changed = true;
            }
            1004 => {
                self.modes.focus_events = enable;
                reporting_changed = true;
            }
            1005 => {
                self.mouse.encoding = if enable {
                    MouseEncoding::Utf8
                } else {
                    MouseEncoding::X10
                };
                reporting_changed = true;
            }
            1006 => {
                self.mouse.encoding = if enable {
                    MouseEncoding::Sgr
                } else {
                    MouseEncoding::X10
                };
                reporting_changed = true;
            }
            1007 => {
                self.mouse.alternate_scroll = enable;
                reporting_changed = true;
            }
            1015 => {
                self.mouse.encoding = if enable {
                    MouseEncoding::Urxvt
                } else {
                    MouseEncoding::X10
                };
                reporting_changed = true;
            }
            1047 => self.swap_alt_screen(enable),
            1048 => {
                if enable {
                    self.save_cursor()
                } else {
                    self.restore_cursor()
                }
            }
            1049 => {
                if enable {
                    self.save_cursor();
                    self.swap_alt_screen(true);
                } else {
                    self.swap_alt_screen(false);
                    self.restore_cursor();
                }
            }
            2004 => {
                self.modes.bracketed_paste = enable;
                reporting_changed = true;
            }
            2026 => {
                self.modes.synchronized_output = enable;
                if !enable {
                    self.events.push(TermEvent::Repaint);
                }
            }
            _ => {}
        }
        if reporting_changed {
            self.events.push(TermEvent::ReportingChanged);
        }
    }

    /// DECRQM: report whether a mode is set. 0 means unrecognised, 1 set,
    /// 2 reset.
    fn report_private_mode(&mut self, mode: u16) {
        let set = |on: bool| if on { 1 } else { 2 };
        let state: u8 = match mode {
            1 => set(self.modes.application_cursor_keys),
            5 => set(self.modes.reverse_video),
            6 => set(self.modes.origin),
            7 => set(self.modes.wraparound),
            9 => set(self.mouse.tracking == MouseTracking::X10),
            12 => set(self.cursor_style.blinking),
            25 => set(self.modes.cursor_visible),
            1000 => set(self.mouse.tracking == MouseTracking::Normal),
            1002 => set(self.mouse.tracking == MouseTracking::ButtonEvent),
            1003 => set(self.mouse.tracking == MouseTracking::AnyEvent),
            1004 => set(self.modes.focus_events),
            1006 => set(self.mouse.encoding == MouseEncoding::Sgr),
            1049 => set(self.modes.alt_screen),
            2004 => set(self.modes.bracketed_paste),
            2026 => set(self.modes.synchronized_output),
            _ => 0,
        };
        let response = format!("\x1b[?{mode};{state}$y");
        self.output.extend_from_slice(response.as_bytes());
    }
}

// ---------------------------------------------------------------------------
// Parser callbacks
// ---------------------------------------------------------------------------

/// The identity tOS reports for DA1: a VT220 with ANSI color.
const DEVICE_ATTRIBUTES: &[u8] = b"\x1b[?62;22c";
const SECONDARY_ATTRIBUTES: &[u8] = b"\x1b[>0;3;0c";

impl Perform for Terminal {
    fn print(&mut self, c: char) {
        let charset = match self.single_shift.take() {
            Some(slot) => self.charsets[slot],
            None => self.charsets[self.gl],
        };
        // DEL and the C1 range are not printable and must not be treated as
        // combining marks; real terminals drop them.
        if c == '\u{7f}' || ('\u{80}'..='\u{9f}').contains(&c) {
            return;
        }
        let c = charset.map(c);
        // Any output pins the viewport back to the live screen.
        self.reset_display_offset();
        self.put_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x07 => self.events.push(TermEvent::Bell),
            0x08 => self.backspace(),
            0x09 => self.tab(1),
            0x0a..=0x0c => {
                self.reset_display_offset();
                self.linefeed();
                if self.modes.linefeed_newline {
                    // A real carriage return, so the deferred wrap is cleared
                    // too; setting x alone leaves `wrap_pending` armed and the
                    // next character skips a line.
                    self.carriage_return();
                }
            }
            0x0d => self.carriage_return(),
            0x0e => self.gl = 1, // SO
            0x0f => self.gl = 0, // SI
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], byte: u8) {
        match (intermediates.first().copied(), byte) {
            // Charset designation: ESC ( B, ESC ) 0, ...
            (Some(i @ (b'(' | b')' | b'*' | b'+')), set) => {
                let slot = match i {
                    b'(' => 0,
                    b')' => 1,
                    b'*' => 2,
                    _ => 3,
                };
                self.charsets[slot] = match set {
                    b'0' => Charset::DecSpecial,
                    _ => Charset::Ascii,
                };
            }
            (Some(b'#'), b'8') => self.screen_alignment_test(),
            (None, b'D') => {
                self.reset_display_offset();
                self.linefeed();
            }
            (None, b'E') => {
                self.reset_display_offset();
                self.linefeed();
                self.carriage_return();
            }
            (None, b'H') => {
                let x = self.cursor.x;
                self.tabs[x] = true;
            }
            (None, b'M') => self.reverse_index(),
            (None, b'N') => self.single_shift = Some(2), // SS2
            (None, b'O') => self.single_shift = Some(3), // SS3
            (None, b'Z') => self.output.extend_from_slice(DEVICE_ATTRIBUTES),
            (None, b'7') => self.save_cursor(),
            (None, b'8') => self.restore_cursor(),
            (None, b'=') => {
                self.modes.application_keypad = true;
                self.events.push(TermEvent::ReportingChanged);
            }
            (None, b'>') => {
                self.modes.application_keypad = false;
                self.events.push(TermEvent::ReportingChanged);
            }
            (None, b'c') => self.reset(),
            (None, b'n') => self.gl = 2, // LS2
            (None, b'o') => self.gl = 3, // LS3
            (None, b'|') => self.gr = 3, // LS3R
            (None, b'}') => self.gr = 2, // LS2R
            (None, b'~') => self.gr = 1, // LS1R
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], byte: u8) {
        let private = intermediates.first().copied();
        let arg = |n: usize| params.get(n, 1) as usize;

        match (private, byte) {
            // ---- cursor movement ----
            (None, b'@') => self.shift_right(arg(0)),
            (None, b'A') => self.move_cursor_rel(0, -(arg(0) as isize)),
            (None, b'B' | b'e') => self.move_cursor_rel(0, arg(0) as isize),
            (None, b'C' | b'a') => self.move_cursor_rel(arg(0) as isize, 0),
            (None, b'D') => self.move_cursor_rel(-(arg(0) as isize), 0),
            (None, b'E') => {
                self.move_cursor_rel(0, arg(0) as isize);
                self.carriage_return();
            }
            (None, b'F') => {
                self.move_cursor_rel(0, -(arg(0) as isize));
                self.carriage_return();
            }
            (None, b'G' | b'`') => {
                let y = self.cursor.y;
                let (top, _) = self.bounds();
                self.set_cursor(arg(0) - 1, y.saturating_sub(top));
            }
            (None, b'H' | b'f') => self.set_cursor(arg(1) - 1, arg(0) - 1),
            (None, b'I') => self.tab(arg(0)),
            (None, b'Z') => self.back_tab(arg(0)),
            (None, b'd') => {
                // VPA is line addressing, so origin mode applies exactly as
                // it does to CUP; only the column is left alone.
                let x = self.cursor.x;
                self.set_cursor(x, arg(0) - 1);
            }

            // ---- erasing and editing ----
            (None, b'J') => self.erase_in_display(params.get_raw(0, 0)),
            (None, b'K') => self.erase_in_line(params.get_raw(0, 0)),
            (None, b'L') => self.insert_lines(arg(0)),
            (None, b'M') => self.delete_lines(arg(0)),
            (None, b'P') => self.shift_left(arg(0)),
            (None, b'X') => self.erase_chars(arg(0)),
            (None, b'S') => {
                let attrs = self.cursor.attrs;
                self.screen
                    .scroll_up(self.scroll_region, arg(0), &attrs, true);
                self.graphics.scroll(arg(0) as u16);
                self.damage.mark_all();
            }
            (None, b'T') => {
                let attrs = self.cursor.attrs;
                self.screen.scroll_down(self.scroll_region, arg(0), &attrs);
                self.damage.mark_all();
            }
            (None, b'b') => {
                // REP: repeat the last printed character.
                if let Some(c) = self.last_printed {
                    for _ in 0..arg(0) {
                        self.put_char(c);
                    }
                }
            }

            // ---- tabs ----
            (None, b'g') => match params.get_raw(0, 0) {
                0 => {
                    let x = self.cursor.x;
                    self.tabs[x] = false;
                }
                3 => self.tabs.iter_mut().for_each(|t| *t = false),
                _ => {}
            },

            // ---- modes ----
            (None, b'h') => {
                for group in params.iter() {
                    self.set_mode(group[0], true);
                }
            }
            (None, b'l') => {
                for group in params.iter() {
                    self.set_mode(group[0], false);
                }
            }
            (Some(b'?'), b'h') => {
                for group in params.iter() {
                    self.set_private_mode(group[0], true);
                }
            }
            (Some(b'?'), b'l') => {
                for group in params.iter() {
                    self.set_private_mode(group[0], false);
                }
            }
            (Some(b'?'), b'p') if intermediates.get(1) == Some(&b'$') => {
                self.report_private_mode(params.get_raw(0, 0));
            }

            // ---- attributes ----
            (None, b'm') => self.apply_sgr(params),

            // ---- reports ----
            (None, b'c') => self.output.extend_from_slice(DEVICE_ATTRIBUTES),
            (Some(b'>'), b'c') => self.output.extend_from_slice(SECONDARY_ATTRIBUTES),
            (None, b'n') => match params.get_raw(0, 0) {
                5 => self.output.extend_from_slice(b"\x1b[0n"),
                6 => {
                    let (top, _) = self.bounds();
                    let row = self.cursor.y.saturating_sub(top) + 1;
                    let col = self.cursor.x + 1;
                    let response = format!("\x1b[{row};{col}R");
                    self.output.extend_from_slice(response.as_bytes());
                }
                _ => {}
            },
            (Some(b'?'), b'n') => {
                if params.get_raw(0, 0) == 6 {
                    let (top, _) = self.bounds();
                    let row = self.cursor.y.saturating_sub(top) + 1;
                    let col = self.cursor.x + 1;
                    let response = format!("\x1b[?{row};{col};1R");
                    self.output.extend_from_slice(response.as_bytes());
                }
            }

            // ---- scroll region and saved cursor ----
            (None, b'r') => {
                let rows = self.rows();
                let top = (params.get(0, 1) as usize).saturating_sub(1);
                // A bottom past the last row is clamped, not rejected: an
                // application that sends a stale region after a resize must
                // not stay stuck with the previous one.
                let bottom = (params.get(1, rows as u16) as usize).min(rows);
                if top + 1 < bottom {
                    self.scroll_region = Region::new(top, bottom);
                    self.set_cursor(0, 0);
                }
            }
            (None, b's') => self.save_cursor(),
            (None, b'u') => self.restore_cursor(),

            // ---- cursor style ----
            (Some(b' '), b'q') => {
                self.cursor_style = CursorStyle::from_decscusr(params.get_raw(0, 1));
                self.events
                    .push(TermEvent::CursorStyleChanged(self.cursor_style));
                self.mark_cursor_row();
            }

            // ---- kitty keyboard protocol ----
            (Some(b'?'), b'u') => {
                let flags = self.keyboard.current().0;
                let response = format!("\x1b[?{flags}u");
                self.output.extend_from_slice(response.as_bytes());
            }
            (Some(b'='), b'u') => {
                let flags = KeyboardFlags(params.get_raw(0, 0) as u8);
                let mode = params.get_raw(1, 1);
                self.keyboard.set(flags, mode);
                self.events.push(TermEvent::ReportingChanged);
            }
            (Some(b'>'), b'u') => {
                self.keyboard
                    .push(KeyboardFlags(params.get_raw(0, 0) as u8));
                self.events.push(TermEvent::ReportingChanged);
            }
            (Some(b'<'), b'u') => {
                self.keyboard.pop(params.get(0, 1) as usize);
                self.events.push(TermEvent::ReportingChanged);
            }

            // ---- soft reset ----
            (Some(b'!'), b'p') => self.soft_reset(),

            // ---- version report ----
            (Some(b'>'), b'q') => {
                let response = format!("\x1bP>|tOS({})\x1b\\", env!("CARGO_PKG_VERSION"));
                self.output.extend_from_slice(response.as_bytes());
            }

            // ---- window manipulation ----
            (None, b't') => {
                let op = params.get_raw(0, 0);
                match op {
                    14 => {
                        let h = self.rows() as u32 * self.config.cell_height;
                        let w = self.cols() as u32 * self.config.cell_width;
                        let response = format!("\x1b[4;{h};{w}t");
                        self.output.extend_from_slice(response.as_bytes());
                    }
                    16 => {
                        let response = format!(
                            "\x1b[6;{};{}t",
                            self.config.cell_height, self.config.cell_width
                        );
                        self.output.extend_from_slice(response.as_bytes());
                    }
                    18 => {
                        let response = format!("\x1b[8;{};{}t", self.rows(), self.cols());
                        self.output.extend_from_slice(response.as_bytes());
                    }
                    21 => {
                        let title = self.title.clone();
                        let response = format!("\x1b]l{title}\x1b\\");
                        self.output.extend_from_slice(response.as_bytes());
                    }
                    _ => self.events.push(TermEvent::WindowOp(
                        op,
                        params.get_raw(1, 0),
                        params.get_raw(2, 0),
                    )),
                }
            }

            _ => {}
        }
    }

    fn osc_dispatch(&mut self, fields: &[&[u8]], _bell_terminated: bool) {
        let Some(first) = fields.first() else { return };
        let Ok(command) = std::str::from_utf8(first) else {
            return;
        };
        let text = |i: usize| -> String {
            fields
                .get(i)
                .map(|f| String::from_utf8_lossy(f).into_owned())
                .unwrap_or_default()
        };

        match command {
            // Window and icon title.
            "0" | "2" => {
                let title = text(1);
                self.title = title.clone();
                self.events.push(TermEvent::TitleChanged(title));
            }
            "1" => self.events.push(TermEvent::IconTitleChanged(text(1))),
            // Working directory.
            "7" => self.events.push(TermEvent::CwdChanged(text(1))),
            // Desktop notification.
            "9" => self.events.push(TermEvent::Notify {
                title: String::new(),
                body: text(1),
            }),
            "777" => {
                if text(1) == "notify" {
                    self.events.push(TermEvent::Notify {
                        title: text(2),
                        body: text(3),
                    });
                }
            }
            // Palette entries.
            "4" => {
                let mut i = 1;
                while i + 1 < fields.len() {
                    let index: Option<u8> = text(i).parse().ok();
                    let spec = text(i + 1);
                    if let Some(index) = index {
                        if spec == "?" {
                            let color = self.palette.index(index);
                            let response = format!(
                                "\x1b]4;{index};rgb:{:02x}{:02x}/{:02x}{:02x}/{:02x}{:02x}\x1b\\",
                                color.r, color.r, color.g, color.g, color.b, color.b
                            );
                            self.output.extend_from_slice(response.as_bytes());
                        } else if let Some(color) = parse_color_spec(&spec) {
                            self.palette.set_index(index, color);
                            self.damage.mark_all();
                        }
                    }
                    i += 2;
                }
            }
            // Hyperlinks: OSC 8 ; params ; uri
            "8" => {
                let uri = text(2);
                self.cursor.attrs.hyperlink = self.intern_hyperlink(&uri);
            }
            // Default colors.
            "10" | "11" | "12" => {
                let spec = text(1);
                let slot = command;
                if spec == "?" {
                    let color = match slot {
                        "10" => self.palette.foreground,
                        "11" => self.palette.background,
                        _ => self.palette.cursor,
                    };
                    let response = format!(
                        "\x1b]{slot};rgb:{:02x}{:02x}/{:02x}{:02x}/{:02x}{:02x}\x1b\\",
                        color.r, color.r, color.g, color.g, color.b, color.b
                    );
                    self.output.extend_from_slice(response.as_bytes());
                } else if let Some(color) = parse_color_spec(&spec) {
                    match slot {
                        "10" => self.palette.foreground = color,
                        "11" => self.palette.background = color,
                        _ => self.palette.cursor = color,
                    }
                    self.damage.mark_all();
                }
            }
            // Clipboard.
            "52" => {
                let selection = text(1).chars().next().unwrap_or('c');
                let data = text(2);
                if data == "?" {
                    self.events.push(TermEvent::ClipboardLoad { selection });
                } else {
                    let decoded = crate::graphics::decode_base64(data.as_bytes());
                    self.events.push(TermEvent::ClipboardStore {
                        selection,
                        data: decoded,
                    });
                }
            }
            // Palette and default color resets.
            "104" => {
                if fields.len() <= 1 || fields[1].is_empty() {
                    self.palette = self.default_palette.clone();
                } else {
                    for i in 1..fields.len() {
                        if let Ok(index) = text(i).parse::<u8>() {
                            let color = self.default_palette.index(index);
                            self.palette.set_index(index, color);
                        }
                    }
                }
                self.damage.mark_all();
            }
            "110" => {
                self.palette.foreground = self.default_palette.foreground;
                self.damage.mark_all();
            }
            "111" => {
                self.palette.background = self.default_palette.background;
                self.damage.mark_all();
            }
            "112" => {
                self.palette.cursor = self.default_palette.cursor;
                self.damage.mark_all();
            }
            _ => {}
        }
    }

    fn dcs_hook(&mut self, _params: &Params, intermediates: &[u8], byte: u8) {
        self.dcs_buf.clear();
        self.dcs_kind = match (intermediates.first().copied(), byte) {
            // DECRQSS: request the current setting of a control.
            (Some(b'$'), b'q') => Some(b'q'),
            _ => None,
        };
    }

    fn dcs_put(&mut self, byte: u8) {
        if self.dcs_kind.is_some() && self.dcs_buf.len() < 256 {
            self.dcs_buf.push(byte);
        }
    }

    fn dcs_unhook(&mut self) {
        if self.dcs_kind != Some(b'q') {
            self.dcs_kind = None;
            return;
        }
        let request = std::mem::take(&mut self.dcs_buf);
        self.dcs_kind = None;
        let response = match request.as_slice() {
            b"r" => Some(format!(
                "{};{}r",
                self.scroll_region.top + 1,
                self.scroll_region.bottom
            )),
            b"m" => Some(format!("{}m", self.sgr_string())),
            b" q" => {
                let param = match (self.cursor_style.shape, self.cursor_style.blinking) {
                    (CursorShape::Block, true) => 1,
                    (CursorShape::Block, false) => 2,
                    (CursorShape::Underline, true) => 3,
                    (CursorShape::Underline, false) => 4,
                    (CursorShape::Beam, true) => 5,
                    (CursorShape::Beam, false) => 6,
                    (CursorShape::Hollow, _) => 1,
                };
                Some(format!("{param} q"))
            }
            _ => None,
        };
        let reply = match response {
            Some(body) => format!("\x1bP1$r{body}\x1b\\"),
            None => "\x1bP0$r\x1b\\".to_string(),
        };
        self.output.extend_from_slice(reply.as_bytes());
    }

    fn apc_start(&mut self) {
        self.apc_buf.clear();
    }

    fn apc_put(&mut self, byte: u8) {
        // Graphics payloads can be large; the cap is generous but finite.
        if self.apc_buf.len() < 16 * 1024 * 1024 {
            self.apc_buf.push(byte);
        }
    }

    fn apc_end(&mut self) {
        let buf = std::mem::take(&mut self.apc_buf);
        if buf.first() == Some(&b'G') {
            self.handle_graphics(&buf[1..]);
        }
    }
}

/// Parse an X11 color specification: `rgb:r/g/b` or `#rgb` forms.
fn parse_color_spec(spec: &str) -> Option<Rgb> {
    if let Some(rest) = spec.strip_prefix("rgb:") {
        let mut parts = rest.split('/');
        let r = scale_hex(parts.next()?)?;
        let g = scale_hex(parts.next()?)?;
        let b = scale_hex(parts.next()?)?;
        return Some(Rgb::new(r, g, b));
    }
    if let Some(rest) = spec.strip_prefix('#') {
        // The spec reaches here through `from_utf8_lossy`, so it can contain
        // multi-byte replacement characters; splitting by byte offset would
        // panic on a char boundary. Hex digits are ASCII, so anything else
        // makes the whole spec invalid.
        if !rest.is_ascii() {
            return None;
        }
        let per = match rest.len() {
            3 | 6 | 12 => rest.len() / 3,
            _ => return None,
        };
        let r = scale_hex(&rest[..per])?;
        let g = scale_hex(&rest[per..per * 2])?;
        let b = scale_hex(&rest[per * 2..per * 3])?;
        return Some(Rgb::new(r, g, b));
    }
    None
}

/// Convert a hex component of any width to 8 bits.
fn scale_hex(part: &str) -> Option<u8> {
    if part.is_empty() || part.len() > 4 {
        return None;
    }
    let value = u32::from_str_radix(part, 16).ok()?;
    let max = (1u32 << (4 * part.len())) - 1;
    Some(((value * 255 + max / 2) / max) as u8)
}

// ---------------------------------------------------------------------------
// Graphics placement
// ---------------------------------------------------------------------------

impl Terminal {
    fn handle_graphics(&mut self, body: &[u8]) {
        let Some(cmd) = GraphicsCommand::parse(body) else {
            self.graphics_response(&GraphicsCommand::default(), Err("EINVAL:bad command"));
            return;
        };

        match cmd.action {
            Action::Query => {
                // A query must not store anything; it only proves support.
                self.graphics_response(&cmd, Ok(cmd.image_id));
            }
            Action::Transmit | Action::TransmitAndDisplay | Action::TransmitFrame => {
                let Some((full, payload)) = self.graphics.accumulate(&cmd) else {
                    return; // more chunks to come
                };
                // The continuation chunks of a transfer carry no action of
                // their own, so what happens with the data is decided by the
                // command that started it, not by the one that finished it.
                if full.action == Action::TransmitFrame {
                    match self.graphics.store_frame(&full, &payload) {
                        Ok(id) => {
                            // The frame just written may be the one on screen.
                            self.damage_image(id);
                            self.graphics_response(&full, Ok(id));
                        }
                        Err(err) => self.graphics_response(&full, Err(err)),
                    }
                    return;
                }
                match self.graphics.store(&full, &payload) {
                    Ok(id) => {
                        if full.action == Action::TransmitAndDisplay {
                            self.place_at_cursor(&full, id);
                        }
                        self.graphics_response(&full, Ok(id));
                    }
                    Err(err) => self.graphics_response(&full, Err(err)),
                }
            }
            Action::Put => {
                let id = cmd.image_id;
                if self.graphics.image(id).is_some() {
                    self.place_at_cursor(&cmd, id);
                    self.graphics_response(&cmd, Ok(id));
                } else {
                    self.graphics_response(&cmd, Err("ENOENT:no such image"));
                }
            }
            Action::Delete => {
                if self.graphics.delete(&cmd) {
                    self.clear_graphics_refs();
                    self.damage.mark_all();
                }
            }
            Action::AnimationControl => match self.graphics.control_animation(&cmd) {
                Ok(changed) => {
                    if changed {
                        self.damage_image(cmd.image_id);
                    }
                    self.graphics_response(&cmd, Ok(cmd.image_id));
                }
                Err(err) => self.graphics_response(&cmd, Err(err)),
            },
            Action::ComposeFrames => {
                self.graphics_response(&cmd, Err("ENOSUP:frame composition not supported"))
            }
        }
    }

    /// Mark the rows every placement of an image covers. Returns true when the
    /// image is on screen at all, which is the only case a repaint is needed.
    fn damage_image(&mut self, image_id: u32) -> bool {
        let offset = self.display_offset() as i64;
        let rows: Vec<(i64, u16)> = self
            .graphics
            .placements()
            .filter(|p| p.image_id == image_id)
            .map(|p| (p.row as i64 - offset, p.rows))
            .collect();
        // Having a placement is not the same as being visible. Scrolled back
        // far enough, a playing animation is off the top of the viewport, and
        // calling that a repaint would flip the whole page every frame for a
        // picture nobody can see.
        let height = self.grid().rows() as i64;
        let mut on_screen = false;
        for (row, count) in rows {
            let from = row.max(0);
            let to = (row + count as i64).min(height);
            if to <= from {
                continue;
            }
            on_screen = true;
            self.damage.mark_range(from as usize, to as usize);
        }
        on_screen
    }

    /// Move animated images on to the frame `now` selects, and damage the rows
    /// they are placed on. Returns true when the picture changed.
    ///
    /// The time is the caller's, because a terminal that read the clock itself
    /// could not be stepped frame by frame from a test.
    pub fn advance_animations(&mut self, now: Instant) -> bool {
        let mut dirty = false;
        for id in self.graphics.advance_animations(now) {
            dirty |= self.damage_image(id);
        }
        dirty
    }

    /// How long until an animation here wants its next frame, so the caller can
    /// wake up in time instead of on its idle timer.
    pub fn next_animation_delay(&self, now: Instant) -> Option<Duration> {
        self.graphics.next_animation_delay(now)
    }

    fn place_at_cursor(&mut self, cmd: &GraphicsCommand, image_id: u32) {
        let (col, row) = (self.cursor.x as u16, self.cursor.y as u16);
        let cell_w = self.config.cell_width;
        let cell_h = self.config.cell_height;
        let Some(placement) = self.graphics.place(cmd, image_id, col, row, cell_w, cell_h) else {
            return;
        };
        let Some(p) = self.graphics.placement(placement) else {
            return;
        };
        let (cols, rows) = (p.cols, p.rows);

        // Tag the covered cells so the renderer knows to composite there.
        for dy in 0..rows {
            let y = row as usize + dy as usize;
            if y >= self.rows() {
                break;
            }
            for dx in 0..cols {
                let x = col as usize + dx as usize;
                if x >= self.cols() {
                    break;
                }
                if let Some(cell) = self.screen.cell_mut(x, y) {
                    cell.attrs.graphics = Some(GraphicsRef {
                        placement,
                        col: dx,
                        row: dy,
                    });
                }
            }
            self.damage.mark_row(y);
        }

        if !cmd.cursor_stays {
            // Kitty leaves the cursor just past the bottom right of the image.
            let target_y = row as usize + rows.saturating_sub(1) as usize;
            let target_x = col as usize + cols as usize;
            self.cursor.y = target_y.min(self.rows() - 1);
            self.cursor.x = target_x.min(self.cols() - 1);
            self.cursor.wrap_pending = false;
        }
    }

    /// Drop graphics references from cells whose placement no longer exists.
    fn clear_graphics_refs(&mut self) {
        let rows = self.rows();
        let cols = self.cols();
        for y in 0..rows {
            for x in 0..cols {
                let stale = match self.screen.cell(x, y).and_then(|c| c.attrs.graphics) {
                    Some(r) => self.graphics.placement(r.placement).is_none(),
                    None => false,
                };
                if stale {
                    if let Some(cell) = self.screen.cell_mut(x, y) {
                        cell.attrs.graphics = None;
                    }
                }
            }
        }
    }

    fn graphics_response(&mut self, cmd: &GraphicsCommand, result: Result<u32, &str>) {
        let quiet = cmd.quiet;
        let body = match result {
            Ok(_) => {
                if quiet >= 1 {
                    return;
                }
                "OK".to_string()
            }
            Err(err) => {
                if quiet >= 2 {
                    return;
                }
                err.to_string()
            }
        };
        // A response is only sent when the command identified itself.
        if cmd.image_id == 0 && cmd.image_number == 0 {
            return;
        }
        let response = format!("\x1b_G{};{}\x1b\\", cmd.response_id(), body);
        self.output.extend_from_slice(response.as_bytes());
    }

    /// Current pen rendered as SGR parameters, for DECRQSS.
    fn sgr_string(&self) -> String {
        let attrs = &self.cursor.attrs;
        let mut parts = vec!["0".to_string()];
        if attrs.flags.contains(Flags::BOLD) {
            parts.push("1".into());
        }
        if attrs.flags.contains(Flags::DIM) {
            parts.push("2".into());
        }
        if attrs.flags.contains(Flags::ITALIC) {
            parts.push("3".into());
        }
        if !attrs.underline.is_none() {
            parts.push("4".into());
        }
        if attrs.flags.contains(Flags::BLINK) {
            parts.push("5".into());
        }
        if attrs.flags.contains(Flags::REVERSE) {
            parts.push("7".into());
        }
        if attrs.flags.contains(Flags::HIDDEN) {
            parts.push("8".into());
        }
        if attrs.flags.contains(Flags::STRIKEOUT) {
            parts.push("9".into());
        }
        match attrs.fg {
            Color::Default => {}
            Color::Indexed(i) if i < 8 => parts.push((30 + i as u16).to_string()),
            Color::Indexed(i) if i < 16 => parts.push((90 + i as u16 - 8).to_string()),
            Color::Indexed(i) => parts.push(format!("38:5:{i}")),
            Color::Rgb(c) => parts.push(format!("38:2::{}:{}:{}", c.r, c.g, c.b)),
        }
        match attrs.bg {
            Color::Default => {}
            Color::Indexed(i) if i < 8 => parts.push((40 + i as u16).to_string()),
            Color::Indexed(i) if i < 16 => parts.push((100 + i as u16 - 8).to_string()),
            Color::Indexed(i) => parts.push(format!("48:5:{i}")),
            Color::Rgb(c) => parts.push(format!("48:2::{}:{}:{}", c.r, c.g, c.b)),
        }
        parts.join(";")
    }
}
