//! A filtered list on top of the panes: the compositor's one modal surface.
//!
//! The launcher is the first thing to use it, but nothing here knows what a
//! program is. An overlay is a title, a list of items, a query typed against
//! them and a cursor; it reports which item was chosen and lets the caller
//! decide what choosing means. The power, network, Bluetooth and audio menus
//! are the same surface over a different list, so they belong here too rather
//! than each growing their own box.
//!
//! Drop the list and the same box is a prompt: a title and one line to type
//! into, which is all renaming a workspace needs. Everything about typing —
//! which keys are text and which are bindings to swallow, where the cursor is
//! drawn, how a line too long for the box is shown — is the same question in
//! both, and worth answering once.
//!
//! The box itself is no longer the overlay's own. It is
//! [`chrome::draw_box`](crate::chrome::draw_box), because the IME's candidate
//! window needs the same border, the same clipping and the same padding, and
//! must not be an overlay to get them: an overlay owns the keyboard, and a
//! candidate window that took every key would be an input method that stops
//! you typing. What is left here is what is the overlay's own — where the box
//! goes, what the query line looks like and what the list says.

use tos_font::FontStack;
use tos_input::{KeyCode, KeyEvent, Modifiers, MouseAction, MouseButton};
use tos_render::{Rect, Surface};

use crate::chrome::{draw_box, draw_text, BoxLine, BoxRect, Chrome};

/// The two pieces the overlay's box was made of, re-exported under the names
/// they had here. The lock screen still draws its box by hand out of them and
/// reaches for them through this module — it is the next caller [`draw_box`]
/// should take — and the tests at the bottom of this file check the behaviour
/// the box relies on.
pub use crate::chrome::{clip, pad_to};

/// The widest the overlay grows, however wide the display is. A launcher that
/// spans a 4K screen is harder to read, not easier.
const MAX_WIDTH: usize = 64;
/// The most list rows shown at once; beyond this it stops being a menu.
const MAX_LIST_ROWS: usize = 14;
/// What a secret prompt draws instead of the character that was typed.
///
/// The same bullet [`crate::lock`] masks a password with, so that the two
/// places in tOS where something is typed that nobody else should read look
/// like the same thing, because they are.
const MASK: char = '\u{2022}';

/// Rows the overlay spends on itself: two borders, the query and its divider.
const CHROME_ROWS: usize = 4;
/// Which row of the box the list starts on: the top border, the query line
/// and the rule under it come first.
const FIRST_LIST_ROW: usize = 3;
/// How far one notch of the wheel moves a list. Three is what the scrollback
/// uses, and a menu that jumped a whole page per notch would be a menu nobody
/// can stop in the middle of.
const WHEEL_ROWS: isize = 3;

/// One row of an overlay's list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayItem {
    /// Drawn on the left, and the only text the query is matched against.
    pub label: String,
    /// Drawn dim on the right when it fits: a path, a signal strength, a
    /// volume. Never matched against, so it can say anything.
    pub detail: String,
}

impl OverlayItem {
    pub fn new(label: impl Into<String>) -> Self {
        OverlayItem {
            label: label.into(),
            detail: String::new(),
        }
    }

    pub fn with_detail(label: impl Into<String>, detail: impl Into<String>) -> Self {
        OverlayItem {
            label: label.into(),
            detail: detail.into(),
        }
    }
}

/// What the overlay did with a key.
///
/// Every variant means the key was taken: while an overlay is open it owns the
/// keyboard, so nothing here ever hands a key back to the pane underneath.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayOutcome {
    /// Taken, but nothing on screen changed.
    Consumed,
    /// Taken, and the overlay needs repainting.
    Changed,
    /// This index into [`Overlay::items`] was chosen; the overlay is finished.
    Chosen(usize),
    /// A prompt's line was accepted; the text is [`Overlay::query`] and the
    /// overlay is finished. An empty line is accepted like any other, since
    /// only the caller knows whether clearing it means something.
    Accepted,
    /// Escape: the overlay is finished and nothing should happen.
    Cancelled,
}

/// Where an overlay's box landed, and what is drawn in each of its rows.
///
/// The box is centred in whatever area the frame hands over, so where it is
/// depends on the display size, the cell size and how long the list is — none
/// of which the overlay is told until it is asked to draw. It is worked out
/// once, by [`Overlay::placement`], and both the drawing and the mouse read
/// the answer, the way [`crate::status::Bar`] places its pieces once and lets
/// `draw` and `hit` share them. Two copies of this arithmetic would be two
/// copies that agree until one of the numbers above changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    /// Top left corner of the box, in pixels.
    pub x: i32,
    pub y: i32,
    /// The box in cells, borders counted.
    pub cols: usize,
    pub rows: usize,
    /// How many of those rows the list got.
    pub list_rows: usize,
    /// The position within [`Overlay::matches`] drawn on the first list row,
    /// already pulled back far enough to keep the cursor on screen.
    pub scroll: usize,
    /// The cell size the box was laid out for.
    pub cell: (u32, u32),
}

impl Placement {
    /// The top of one row of the box, in pixels, counting the top border as
    /// row zero.
    fn row_y(&self, row: usize) -> i32 {
        self.y + (row as u32 * self.cell.1) as i32
    }

    /// Whether the cell at `col`, `row` of the display is inside the box.
    ///
    /// In pixels rather than cells, because that is what the box was placed
    /// in: a cell counts as inside when the pixel it starts at is.
    pub fn contains(&self, col: u32, row: u32) -> bool {
        let (cw, ch) = self.cell;
        let (x, y) = ((col * cw) as i32, (row * ch) as i32);
        x >= self.x
            && y >= self.y
            && x < self.x + (self.cols as u32 * cw) as i32
            && y < self.y + (self.rows as u32 * ch) as i32
    }

    /// The position within [`Overlay::matches`] drawn at a cell, if the cell
    /// is on a list row at all. The borders, the title, the query line and
    /// the rule are all `None`, and so is every cell of a prompt, which has
    /// no list.
    ///
    /// The position can still be past the end of the match list: the row an
    /// empty list says so on is a list row like any other.
    pub fn row_at(&self, col: u32, row: u32) -> Option<usize> {
        if !self.contains(col, row) {
            return None;
        }
        let (cw, ch) = self.cell;
        // The two columns the frame is drawn in are inside the box, which is
        // all `contains` is asked, and they are not the row beside them: the
        // item's text starts one column in. Pressing the `│` at the edge of a
        // row launched what that row named, which is a click nobody aimed.
        let column = (((col * cw) as i32 - self.x) / cw as i32) as usize;
        if column == 0 || column + 1 >= self.cols {
            return None;
        }
        let within = (((row * ch) as i32 - self.y) / ch as i32) as usize;
        let list = within.checked_sub(FIRST_LIST_ROW)?;
        (list < self.list_rows).then_some(self.scroll + list)
    }
}

/// A titled list with a query line.
///
/// Build it with the items already gathered — the overlay filters what it is
/// given and never goes looking for more, which is what keeps a directory scan
/// or a D-Bus round trip off the keystroke path.
#[derive(Debug, Clone)]
pub struct Overlay {
    title: String,
    query: String,
    items: Vec<OverlayItem>,
    /// Indices into `items`, best match first, for the current query.
    matches: Vec<usize>,
    /// Position within `matches`, not within `items`.
    cursor: usize,
    /// First visible row, kept in range by [`Overlay::placement`], which is
    /// the only place the number of visible rows is known.
    scroll: usize,
    /// Whether the line itself is the answer rather than a filter over the
    /// list. A prompt has no list to choose from, so enter takes the text.
    prompt: bool,
    /// Whether the line is drawn as bullets rather than as itself. What is
    /// accepted is still the real text: this changes what is on the screen and
    /// nothing else.
    secret: bool,
}

impl Overlay {
    pub fn new(title: impl Into<String>, items: Vec<OverlayItem>) -> Self {
        let mut overlay = Overlay {
            title: title.into(),
            query: String::new(),
            items,
            matches: Vec::new(),
            cursor: 0,
            scroll: 0,
            prompt: false,
            secret: false,
        };
        overlay.refilter();
        overlay
    }

    /// The same box with one line to type into and nothing underneath it.
    ///
    /// The line starts on `initial` so that the usual edit — fixing a name
    /// that is nearly right — is a few keys rather than retyping it, and
    /// because it is the only place the current value is shown. Enter reports
    /// [`OverlayOutcome::Accepted`] whatever is on the line, empty included.
    pub fn prompt(title: impl Into<String>, initial: impl Into<String>) -> Self {
        let mut overlay = Overlay::new(title, Vec::new());
        overlay.query = initial.into();
        overlay.prompt = true;
        overlay
    }

    /// The same prompt with the line drawn as one bullet per character.
    ///
    /// For a passphrase, which `docs/design/wifi.md` (#137) asks for masked
    /// the way the lock screen masks a password: a passphrase typed in front
    /// of somebody is a passphrase. Only the drawing changes — enter still
    /// reports [`OverlayOutcome::Accepted`] with the real text on it, which is
    /// the whole point of it being typed.
    pub fn secret_prompt(title: impl Into<String>, initial: impl Into<String>) -> Self {
        let mut overlay = Overlay::prompt(title, initial);
        overlay.secret = true;
        overlay
    }

    /// Replace the list, keeping the query.
    ///
    /// For a menu whose contents arrive late or change under it: a network
    /// scan finding another access point should not throw away what the user
    /// has typed.
    pub fn set_items(&mut self, items: Vec<OverlayItem>) {
        self.items = items;
        self.refilter();
    }

    /// Rename the box without disturbing what is in it.
    ///
    /// The wireless menu's title carries whether the radio is still listening,
    /// and it stops being true while the menu is open. Rebuilding the overlay
    /// to say so would throw away the query and the cursor, which is the thing
    /// [`Overlay::set_items`] exists to avoid.
    pub fn set_title(&mut self, title: impl Into<String>) {
        self.title = title.into();
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn items(&self) -> &[OverlayItem] {
        &self.items
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    /// The query as it is drawn: bullets for a secret prompt, itself
    /// otherwise.
    ///
    /// The renderer measures and clips this rather than [`Overlay::query`], so
    /// that the cursor lands after the last bullet and a long passphrase
    /// scrolls the line by the same arithmetic every other prompt uses. A
    /// multi-byte character is one bullet, because it is one thing that was
    /// typed and one press of backspace takes it away.
    pub fn shown_query(&self) -> String {
        match self.secret {
            true => MASK.to_string().repeat(self.query.chars().count()),
            false => self.query.clone(),
        }
    }

    /// Indices into [`Overlay::items`] that the query matches, best first.
    pub fn matches(&self) -> &[usize] {
        &self.matches
    }

    /// The index into [`Overlay::items`] under the cursor, if anything matches.
    pub fn selected(&self) -> Option<usize> {
        self.matches.get(self.cursor).copied()
    }

    /// The position within [`Overlay::matches`] on the first visible row, as
    /// of the last frame. Only the wheel moves this on its own, which is what
    /// there is to look at from outside.
    pub fn scroll(&self) -> usize {
        self.scroll
    }

    pub fn selected_item(&self) -> Option<&OverlayItem> {
        self.selected().map(|index| &self.items[index])
    }

    /// Offer a key to the overlay.
    pub fn handle_key(&mut self, key: &KeyEvent) -> OverlayOutcome {
        // Releases and the modifier keys themselves are still swallowed: the
        // pane must not see half of a keypress the overlay took.
        if !key.is_press() || matches!(key.code, KeyCode::ModifierKey(_)) {
            return OverlayOutcome::Consumed;
        }
        let modifiers = key.modifiers.effective();
        let ctrl = modifiers.contains(Modifiers::CTRL);

        match key.code {
            KeyCode::Escape => return OverlayOutcome::Cancelled,
            KeyCode::Enter => {
                if self.prompt {
                    return OverlayOutcome::Accepted;
                }
                return match self.selected() {
                    Some(index) => OverlayOutcome::Chosen(index),
                    // Nothing matches, so there is nothing to choose. The
                    // overlay stays open rather than closing on an empty list.
                    None => OverlayOutcome::Consumed,
                };
            }
            KeyCode::Backspace => {
                return match self.query.pop() {
                    Some(_) => {
                        self.refilter();
                        OverlayOutcome::Changed
                    }
                    None => OverlayOutcome::Consumed,
                }
            }
            KeyCode::Up => return self.move_cursor(-1),
            KeyCode::Down => return self.move_cursor(1),
            KeyCode::Char('p') if ctrl => return self.move_cursor(-1),
            KeyCode::Char('n') if ctrl => return self.move_cursor(1),
            _ => {}
        }

        // Anything else with a modifier that is not shift is a binding
        // attempt, not typing, and must not end up in the query.
        if !modifiers.without(Modifiers::SHIFT).is_empty() {
            return OverlayOutcome::Consumed;
        }
        match key.text {
            Some(c) if !c.is_control() => {
                self.query.push(c);
                self.refilter();
                OverlayOutcome::Changed
            }
            _ => OverlayOutcome::Consumed,
        }
    }

    /// Move the cursor, stopping at the ends rather than wrapping: a held
    /// arrow key should settle on the last row, not cycle forever.
    fn move_cursor(&mut self, delta: isize) -> OverlayOutcome {
        if self.matches.is_empty() {
            return OverlayOutcome::Consumed;
        }
        let last = self.matches.len() - 1;
        let next = (self.cursor as isize + delta).clamp(0, last as isize) as usize;
        if next == self.cursor {
            return OverlayOutcome::Consumed;
        }
        self.cursor = next;
        OverlayOutcome::Changed
    }

    /// Offer a mouse event to the overlay, in display cells.
    ///
    /// `area` and `cell` are the ones the frame draws with, so that the row
    /// this answers for is the row under the pointer rather than the row
    /// under where the box was last time the list was this long.
    ///
    /// The outcomes are the ones [`Overlay::handle_key`] already reports: a
    /// click on a row is the same event as moving there and pressing enter,
    /// and a press outside is escape. Nothing here is a second kind of
    /// answer, so nothing downstream needs a second way to handle one.
    pub fn handle_mouse(
        &mut self,
        col: u32,
        row: u32,
        button: Option<MouseButton>,
        action: MouseAction,
        area: Rect,
        cell: (u32, u32),
    ) -> OverlayOutcome {
        let Some(placement) = self.placement(area, cell) else {
            // A display too small to draw the box on still has an overlay
            // open on it, eating every key. A press is the only evidence
            // anybody is trying to get out of a menu they cannot see, so it
            // is taken as one rather than swallowed.
            return match action {
                MouseAction::Press => OverlayOutcome::Cancelled,
                _ => OverlayOutcome::Consumed,
            };
        };

        // The wheel goes to the list wherever the pointer is. An overlay is
        // modal: there is nothing else on screen for a notch to mean, and
        // asking people to aim at a fourteen row box before they can scroll
        // it is a rule that only ever costs them a notch.
        if button.is_some_and(MouseButton::is_wheel) {
            if action != MouseAction::Press {
                return OverlayOutcome::Consumed;
            }
            return match button {
                Some(MouseButton::WheelUp) => self.scroll_list(-WHEEL_ROWS, &placement),
                Some(MouseButton::WheelDown) => self.scroll_list(WHEEL_ROWS, &placement),
                _ => OverlayOutcome::Consumed,
            };
        }

        match action {
            MouseAction::Press => {
                if !placement.contains(col, row) {
                    // A list costs nothing to dismiss by accident: the same
                    // rows are there again the moment it is reopened. A
                    // half-typed name is not, so a prompt keeps what has been
                    // typed and waits for an answer aimed at the question —
                    // enter or escape. Clicking away from a box is also how
                    // somebody moves the pointer off text they are reading,
                    // and that gesture must not be able to rename a
                    // workspace or throw the new name away.
                    return if self.prompt {
                        OverlayOutcome::Consumed
                    } else {
                        OverlayOutcome::Cancelled
                    };
                }
                if button != Some(MouseButton::Left) {
                    return OverlayOutcome::Consumed;
                }
                match placement.row_at(col, row) {
                    Some(position) if position < self.matches.len() => {
                        self.cursor = position;
                        OverlayOutcome::Chosen(self.matches[position])
                    }
                    _ => OverlayOutcome::Consumed,
                }
            }
            // Hover highlights, on bare motion and not only while a button is
            // held: a menu whose rows light up only once you are already
            // pressing is a menu that tells you what you chose after you have
            // chosen it. The repaint that costs is bounded by the number of
            // rows crossed rather than the number of events, because moving
            // within one row reports `Consumed` and asks for no frame.
            MouseAction::Motion | MouseAction::Drag => match placement.row_at(col, row) {
                Some(position) if position < self.matches.len() => self.hover(position),
                // Off the list, including off the box: the highlight stays
                // where it was rather than clearing, so that the row the
                // keyboard is on is still shown while the pointer is parked
                // somewhere else.
                _ => OverlayOutcome::Consumed,
            },
            MouseAction::Release => OverlayOutcome::Consumed,
        }
    }

    /// Put the cursor on a row the pointer is over.
    fn hover(&mut self, position: usize) -> OverlayOutcome {
        if position == self.cursor {
            return OverlayOutcome::Consumed;
        }
        self.cursor = position;
        OverlayOutcome::Changed
    }

    /// Scroll the list, taking the cursor with it.
    ///
    /// The cursor moves because [`Overlay::placement`] pulls the scroll back
    /// to wherever the cursor is: a wheel that moved the view alone would be
    /// undone by the very next frame, and the list would sit still however
    /// hard it was spun. Keeping the highlight on the same screen row is also
    /// what the pointer expects, since the pointer has not moved either.
    fn scroll_list(&mut self, delta: isize, placement: &Placement) -> OverlayOutcome {
        let rows = placement.list_rows;
        if rows == 0 || self.matches.len() <= rows {
            return OverlayOutcome::Consumed;
        }
        let last = self.matches.len() - rows;
        let next = (placement.scroll as isize + delta).clamp(0, last as isize) as usize;
        if next == placement.scroll {
            return OverlayOutcome::Consumed;
        }
        let offset = self.cursor.saturating_sub(placement.scroll);
        self.scroll = next;
        self.cursor = (next + offset).min(self.matches.len() - 1);
        OverlayOutcome::Changed
    }

    /// Rebuild the match list for the current query.
    fn refilter(&mut self) {
        let mut scored: Vec<(i32, usize)> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| score(&self.query, &item.label).map(|s| (s, index)))
            .collect();
        // A stable sort leaves equally good matches in the order the caller
        // supplied, which for the launcher is alphabetical.
        scored.sort_by_key(|&(score, _)| std::cmp::Reverse(score));
        self.matches = scored.into_iter().map(|(_, index)| index).collect();
        self.cursor = 0;
        self.scroll = 0;
    }

    /// Where the box goes inside `area`, which is in pixels, when it is drawn
    /// with cells of size `cell`.
    ///
    /// `None` when there is no honest way to draw it: a box needs a list row
    /// plus the overlay's own rows, and a cell of margin on every side, and
    /// one with no room inside it is worse than none.
    ///
    /// This takes `&self` and reports the scroll offset it would use rather
    /// than storing it, so that a hit test costs the caller nothing and
    /// cannot move the list out from under the frame that is about to draw
    /// it. [`Overlay::draw`] is where the answer is kept.
    pub fn placement(&self, area: Rect, cell: (u32, u32)) -> Option<Placement> {
        let (cw, ch) = (cell.0.max(1), cell.1.max(1));
        let cols = (area.width / cw) as usize;
        let rows = (area.height / ch) as usize;

        // An empty list still gets a row, so that it has somewhere to say so.
        // A prompt has no list at all, and so no divider either.
        let chrome_rows = if self.prompt {
            CHROME_ROWS - 1
        } else {
            CHROME_ROWS
        };
        let wanted = match (self.prompt, self.matches.is_empty()) {
            (true, _) => 0,
            (false, true) => 1,
            (false, false) => self.matches.len(),
        };
        let list_rows = wanted
            .min(MAX_LIST_ROWS)
            .min(rows.saturating_sub(chrome_rows + 2));
        let box_cols = cols.saturating_sub(2).min(MAX_WIDTH);
        if (list_rows == 0 && !self.prompt) || rows < chrome_rows + 2 || box_cols < 12 {
            return None;
        }
        let box_rows = list_rows + chrome_rows;

        // Keep the cursor on screen now that the row count is known, and the
        // list itself with it. Pulling back is not enough on its own: a box
        // that grew — a console that gained rows, a host terminal resized —
        // keeps a scroll taken at the bottom of a shorter window and draws
        // the tail of the list against a full height of blank rows.
        let scroll = if self.cursor < self.scroll {
            self.cursor
        } else if list_rows > 0 && self.cursor >= self.scroll + list_rows {
            self.cursor + 1 - list_rows
        } else {
            self.scroll
        };
        // The last window that still ends on the last match. Clamping to it
        // cannot hide the cursor, because that window covers the whole tail.
        let scroll = scroll.min(self.matches.len().saturating_sub(list_rows));

        Some(Placement {
            x: area.x + (((cols - box_cols) / 2) * cw as usize) as i32,
            y: area.y + (((rows - box_rows) / 2) * ch as usize) as i32,
            cols: box_cols,
            rows: box_rows,
            list_rows,
            scroll,
            cell: (cw, ch),
        })
    }

    /// Draw the overlay centred in `area`, which is in pixels.
    ///
    /// Takes `&mut self` because the scroll offset [`Overlay::placement`]
    /// worked out is kept here: the frame that has just been drawn is what
    /// the next keystroke and the next click are answered against.
    pub fn draw(
        &mut self,
        surface: &mut Surface<'_>,
        fonts: &mut FontStack,
        area: Rect,
        chrome: &Chrome,
    ) {
        let metrics = fonts.metrics();
        let cell = (metrics.cell_width.max(1), metrics.cell_height.max(1));
        let Some(placement) = self.placement(area, cell) else {
            return;
        };
        self.scroll = placement.scroll;
        let (cw, ch) = placement.cell;
        let (x0, y0) = (placement.x, placement.y);
        let (box_cols, box_rows) = (placement.cols, placement.rows);
        let list_rows = placement.list_rows;
        let inner = box_cols - 2;

        // The interior, row by row. The query line is left empty for the
        // code below: it is three colours and a block cursor, and no
        // description of it belongs in the box helper.
        let mut texts: Vec<String> = Vec::with_capacity(list_rows);
        for row in 0..list_rows {
            texts.push(match self.matches.get(self.scroll + row) {
                Some(&index) => format!(" {}", self.items[index].label),
                // An empty list still gets a row, so the box does not collapse
                // to nothing while the query is being corrected.
                None if self.matches.is_empty() && row == 0 => " (no matches)".to_string(),
                None => String::new(),
            });
        }
        let mut lines = Vec::with_capacity(list_rows + 2);
        lines.push(BoxLine::Blank);
        // With no list under it there is nothing for a divider to divide.
        if list_rows > 0 {
            lines.push(BoxLine::Rule);
        }
        for (row, text) in texts.iter().enumerate() {
            let position = self.scroll + row;
            lines.push(match self.matches.get(position) {
                Some(_) if position == self.cursor => BoxLine::Text {
                    text,
                    fg: chrome.accent_text,
                    bg: Some(chrome.accent),
                    bold: true,
                },
                Some(_) => BoxLine::text(text, chrome.foreground),
                None => BoxLine::text(text, chrome.dim),
            });
        }
        draw_box(
            surface,
            fonts,
            BoxRect::new(x0, y0, box_cols, box_rows),
            Some(&self.title),
            &lines,
            chrome,
        );

        // The query line, ending in a block cursor so it is obvious where the
        // keyboard is going. It starts a cell in, because the box has already
        // drawn the border it starts after.
        let y = placement.row_y(1);
        let mut x = draw_text(
            surface,
            fonts,
            x0 + cw as i32,
            y,
            " > ",
            chrome.accent,
            Some(chrome.background),
            true,
        );
        let shown = clip_end(&self.shown_query(), inner.saturating_sub(5));
        x = draw_text(
            surface,
            fonts,
            x,
            y,
            &shown,
            chrome.foreground,
            Some(chrome.background),
            false,
        );
        surface.fill(Rect::new(x, y, cw, ch), chrome.accent);
        // Nothing after the cursor: the box filled the row with its own
        // background before handing it over, and the query is clipped short
        // enough that the cursor never reaches the border.

        // The detail is drawn over the padding of the row it belongs to,
        // right aligned, and only when there is room for it and a gap after
        // the label. It stays here rather than becoming part of a box line
        // because a line is one run of text, and a second column that appears
        // only sometimes is the overlay's own idea.
        for row in 0..list_rows {
            let position = self.scroll + row;
            let Some(&index) = self.matches.get(position) else {
                continue;
            };
            let item = &self.items[index];
            let detail_width = width_of(&item.detail);
            let label_width = width_of(&item.label) + 1;
            if detail_width == 0 || label_width + detail_width + 2 > inner {
                continue;
            }
            let selected = position == self.cursor;
            let (fg, bg) = if selected {
                (chrome.accent_text, chrome.accent)
            } else {
                (chrome.dim, chrome.background)
            };
            // A cell for the border, then as far right as the detail goes
            // while leaving the last interior cell clear.
            let offset = inner - detail_width;
            draw_text(
                surface,
                fonts,
                x0 + (offset as u32 * cw) as i32,
                placement.row_y(FIRST_LIST_ROW + row),
                &item.detail,
                fg,
                Some(bg),
                false,
            );
        }
    }
}

/// How well `query` matches `text`, or `None` when it does not match at all.
///
/// The match is a case-insensitive subsequence, so "gcc" finds "gcc" and
/// "grep-cc" alike. Bigger is better. The score prefers a match that starts
/// early, then one whose characters sit close together, then a shorter
/// candidate, which between them put the obvious answer first: typing "ls"
/// offers `ls` before `lsof` before `less`.
///
/// Finding the characters is greedy from the left and then tightened from the
/// right, which gives the tightest run ending where the greedy pass ended.
/// That is not the globally best run in every case, but it is linear and never
/// scans a name twice.
pub fn score(query: &str, text: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let hay: Vec<char> = text.chars().map(lower).collect();
    let needle: Vec<char> = query.chars().map(lower).collect();
    if needle.len() > hay.len() {
        return None;
    }

    let mut at = 0;
    let mut end = 0;
    for &c in &needle {
        let found = hay[at..].iter().position(|&h| h == c)? + at;
        end = found;
        at = found + 1;
    }
    // Walk back from the last match to pull the earlier ones as close to it
    // as they will go.
    let mut start = end;
    for &c in needle[..needle.len() - 1].iter().rev() {
        start = hay[..start].iter().rposition(|&h| h == c)?;
    }

    let span = (end - start + 1) as i32;
    let gaps = span - needle.len() as i32;
    Some(1000 - start as i32 * 4 - gaps * 8 - hay.len() as i32)
}

/// Lowercase a character without letting it turn into several of them, so a
/// character in the query still lines up with one in the name.
fn lower(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

fn width_of(text: &str) -> usize {
    tos_term::str_width(text)
}

/// Like [`clip`] but keeps the end, for a query that has outgrown its line:
/// what was typed last is what needs to be visible.
fn clip_end(text: &str, cols: usize) -> String {
    if width_of(text) <= cols {
        return text.to_string();
    }
    let mut out: Vec<char> = Vec::new();
    let mut used = 0;
    for c in text.chars().rev() {
        let w = tos_term::char_width(c).max(1) as usize;
        if used + w > cols {
            break;
        }
        out.push(c);
        used += w;
    }
    out.into_iter().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tos_font::BitmapFont;
    use tos_input::{KeyState, ModifierKey};
    use tos_render::OwnedFramebuffer;

    fn overlay(labels: &[&str]) -> Overlay {
        let items = labels.iter().map(|l| OverlayItem::new(*l)).collect();
        Overlay::new("run a program", items)
    }

    fn type_text(overlay: &mut Overlay, text: &str) {
        for c in text.chars() {
            overlay.handle_key(&KeyEvent::new(KeyCode::Char(c), Modifiers::NONE));
        }
    }

    fn labels(overlay: &Overlay) -> Vec<&str> {
        overlay
            .matches()
            .iter()
            .map(|&i| overlay.items()[i].label.as_str())
            .collect()
    }

    #[test]
    fn everything_matches_an_empty_query() {
        let overlay = overlay(&["ls", "vim", "cat"]);
        assert_eq!(labels(&overlay), ["ls", "vim", "cat"]);
    }

    #[test]
    fn typing_narrows_the_list() {
        let mut overlay = overlay(&["ls", "vim", "cat", "vimdiff"]);
        type_text(&mut overlay, "vi");
        assert_eq!(labels(&overlay), ["vim", "vimdiff"]);
    }

    #[test]
    fn matching_skips_characters_in_between() {
        let mut overlay = overlay(&["git-rebase", "grep"]);
        type_text(&mut overlay, "gre");
        assert_eq!(labels(&overlay), ["grep", "git-rebase"]);
    }

    #[test]
    fn matching_ignores_case() {
        let mut overlay = overlay(&["Xorg"]);
        type_text(&mut overlay, "xo");
        assert_eq!(labels(&overlay), ["Xorg"]);
    }

    #[test]
    fn the_shorter_and_earlier_match_comes_first() {
        assert!(score("ls", "ls").unwrap() > score("ls", "lsof").unwrap());
        assert!(score("ls", "lsof").unwrap() > score("ls", "less").unwrap());
        assert!(score("vim", "vim").unwrap() > score("vim", "gvim").unwrap());
        assert_eq!(score("zz", "ls"), None);
    }

    #[test]
    fn a_match_at_the_front_beats_the_same_match_further_in() {
        // Same length, same tightness: where the match starts is the only
        // thing between these two, and a name that begins with what was typed
        // is the one that was meant.
        let front = score("ab", "abzz").unwrap();
        let back = score("ab", "zzab").unwrap();
        assert!(front > back, "{front} should beat {back}");
    }

    #[test]
    fn backspace_widens_the_list_again() {
        let mut overlay = overlay(&["ls", "vim"]);
        type_text(&mut overlay, "vi");
        assert_eq!(labels(&overlay), ["vim"]);
        overlay.handle_key(&KeyEvent::new(KeyCode::Backspace, Modifiers::NONE));
        overlay.handle_key(&KeyEvent::new(KeyCode::Backspace, Modifiers::NONE));
        assert_eq!(labels(&overlay), ["ls", "vim"]);
        assert_eq!(overlay.query(), "");
    }

    #[test]
    fn backspace_on_an_empty_query_does_nothing() {
        let mut overlay = overlay(&["ls"]);
        assert_eq!(
            overlay.handle_key(&KeyEvent::new(KeyCode::Backspace, Modifiers::NONE)),
            OverlayOutcome::Consumed
        );
    }

    #[test]
    fn the_arrows_and_ctrl_p_and_n_move_the_cursor() {
        let mut overlay = overlay(&["one", "two", "three"]);
        assert_eq!(overlay.selected_item().unwrap().label, "one");
        overlay.handle_key(&KeyEvent::new(KeyCode::Down, Modifiers::NONE));
        assert_eq!(overlay.selected_item().unwrap().label, "two");
        overlay.handle_key(&KeyEvent::new(KeyCode::Char('n'), Modifiers::CTRL));
        assert_eq!(overlay.selected_item().unwrap().label, "three");
        overlay.handle_key(&KeyEvent::new(KeyCode::Char('p'), Modifiers::CTRL));
        assert_eq!(overlay.selected_item().unwrap().label, "two");
        overlay.handle_key(&KeyEvent::new(KeyCode::Up, Modifiers::NONE));
        assert_eq!(overlay.selected_item().unwrap().label, "one");
    }

    #[test]
    fn the_cursor_stops_at_the_ends() {
        let mut overlay = overlay(&["one", "two"]);
        for _ in 0..5 {
            overlay.handle_key(&KeyEvent::new(KeyCode::Up, Modifiers::NONE));
        }
        assert_eq!(overlay.selected_item().unwrap().label, "one");
        for _ in 0..5 {
            overlay.handle_key(&KeyEvent::new(KeyCode::Down, Modifiers::NONE));
        }
        assert_eq!(overlay.selected_item().unwrap().label, "two");
    }

    #[test]
    fn ctrl_n_does_not_type_an_n() {
        let mut overlay = overlay(&["one"]);
        overlay.handle_key(&KeyEvent::new(KeyCode::Char('n'), Modifiers::CTRL));
        assert_eq!(overlay.query(), "");
    }

    #[test]
    fn a_binding_combination_is_swallowed_rather_than_typed() {
        // Super+d splits a pane when no overlay is open; here it must neither
        // split nor land in the query.
        let mut overlay = overlay(&["one"]);
        assert_eq!(
            overlay.handle_key(&KeyEvent::new(KeyCode::Char('d'), Modifiers::SUPER)),
            OverlayOutcome::Consumed
        );
        assert_eq!(overlay.query(), "");
    }

    #[test]
    fn shifted_characters_still_type() {
        let mut overlay = overlay(&["Make"]);
        let key = KeyEvent::new(KeyCode::Char('m'), Modifiers::SHIFT).with_text(Some('M'));
        overlay.handle_key(&key);
        assert_eq!(overlay.query(), "M");
    }

    #[test]
    fn typing_resets_the_cursor_to_the_best_match() {
        let mut overlay = overlay(&["one", "two", "three"]);
        overlay.handle_key(&KeyEvent::new(KeyCode::Down, Modifiers::NONE));
        type_text(&mut overlay, "t");
        assert_eq!(overlay.selected_item().unwrap().label, "two");
    }

    #[test]
    fn enter_chooses_the_selected_item() {
        let mut overlay = overlay(&["ls", "vim"]);
        type_text(&mut overlay, "vim");
        let index = overlay.selected().unwrap();
        assert_eq!(
            overlay.handle_key(&KeyEvent::new(KeyCode::Enter, Modifiers::NONE)),
            OverlayOutcome::Chosen(index)
        );
        assert_eq!(overlay.items()[index].label, "vim");
    }

    #[test]
    fn escape_cancels() {
        let mut overlay = overlay(&["ls"]);
        assert_eq!(
            overlay.handle_key(&KeyEvent::new(KeyCode::Escape, Modifiers::NONE)),
            OverlayOutcome::Cancelled
        );
    }

    #[test]
    fn an_empty_list_is_still_navigable_and_cancellable() {
        let mut overlay = Overlay::new("empty", Vec::new());
        assert!(overlay.selected().is_none());
        for code in [KeyCode::Up, KeyCode::Down, KeyCode::Enter] {
            assert_eq!(
                overlay.handle_key(&KeyEvent::new(code, Modifiers::NONE)),
                OverlayOutcome::Consumed
            );
        }
        assert_eq!(
            overlay.handle_key(&KeyEvent::new(KeyCode::Escape, Modifiers::NONE)),
            OverlayOutcome::Cancelled
        );
    }

    #[test]
    fn a_query_that_matches_nothing_leaves_the_overlay_usable() {
        let mut overlay = overlay(&["ls", "vim"]);
        type_text(&mut overlay, "zzz");
        assert!(overlay.matches().is_empty());
        assert_eq!(
            overlay.handle_key(&KeyEvent::new(KeyCode::Enter, Modifiers::NONE)),
            OverlayOutcome::Consumed
        );
        overlay.handle_key(&KeyEvent::new(KeyCode::Backspace, Modifiers::NONE));
        overlay.handle_key(&KeyEvent::new(KeyCode::Backspace, Modifiers::NONE));
        overlay.handle_key(&KeyEvent::new(KeyCode::Backspace, Modifiers::NONE));
        assert_eq!(labels(&overlay), ["ls", "vim"]);
    }

    #[test]
    fn releases_and_modifier_presses_change_nothing() {
        let mut overlay = overlay(&["ls"]);
        let release =
            KeyEvent::new(KeyCode::Char('l'), Modifiers::NONE).with_state(KeyState::Release);
        assert_eq!(overlay.handle_key(&release), OverlayOutcome::Consumed);
        let shift = KeyEvent::new(
            KeyCode::ModifierKey(ModifierKey::LeftShift),
            Modifiers::SHIFT,
        );
        assert_eq!(overlay.handle_key(&shift), OverlayOutcome::Consumed);
        assert_eq!(overlay.query(), "");
    }

    #[test]
    fn replacing_the_items_keeps_the_query() {
        let mut overlay = overlay(&["one"]);
        type_text(&mut overlay, "t");
        overlay.set_items(vec![OverlayItem::new("two"), OverlayItem::new("one")]);
        assert_eq!(overlay.query(), "t");
        assert_eq!(labels(&overlay), ["two"]);
    }

    #[test]
    fn a_prompt_starts_on_the_text_it_was_given_and_hands_back_what_was_typed() {
        let mut overlay = Overlay::prompt("rename workspace", "2");
        assert_eq!(overlay.query(), "2");
        assert!(overlay.items().is_empty(), "a prompt has no list");
        overlay.handle_key(&KeyEvent::new(KeyCode::Backspace, Modifiers::NONE));
        type_text(&mut overlay, "build");
        assert_eq!(
            overlay.handle_key(&KeyEvent::new(KeyCode::Enter, Modifiers::NONE)),
            OverlayOutcome::Accepted
        );
        assert_eq!(overlay.query(), "build");
    }

    #[test]
    fn an_empty_prompt_is_accepted_rather_than_swallowed() {
        // A list with nothing selected has nothing to choose, but an empty
        // line is a thing to say, so the caller gets to hear it.
        let mut overlay = Overlay::prompt("rename workspace", "");
        assert_eq!(
            overlay.handle_key(&KeyEvent::new(KeyCode::Enter, Modifiers::NONE)),
            OverlayOutcome::Accepted
        );
        assert_eq!(overlay.query(), "");
    }

    #[test]
    fn a_secret_prompt_shows_bullets_and_hands_back_the_real_text() {
        let mut overlay = Overlay::secret_prompt("kitchen-table — passphrase", "");
        type_text(&mut overlay, "correct horse");
        assert_eq!(overlay.shown_query(), "•••••••••••••");
        assert_eq!(
            overlay.handle_key(&KeyEvent::new(KeyCode::Enter, Modifiers::NONE)),
            OverlayOutcome::Accepted
        );
        assert_eq!(
            overlay.query(),
            "correct horse",
            "the masking reached the answer and not only the screen"
        );
    }

    #[test]
    fn a_secret_prompt_draws_one_bullet_per_character_and_not_per_byte() {
        // A character that is three bytes of UTF-8 and one press of backspace
        // is one bullet, or the line would grow by three every time somebody
        // pasted a passphrase with anything but ASCII in it.
        let mut overlay = Overlay::secret_prompt("passphrase", "");
        type_text(&mut overlay, "あい");
        assert_eq!(overlay.shown_query(), "••");
        overlay.handle_key(&KeyEvent::new(KeyCode::Backspace, Modifiers::NONE));
        assert_eq!(overlay.shown_query(), "•");
    }

    #[test]
    fn an_ordinary_prompt_and_an_ordinary_list_are_shown_as_themselves() {
        let prompt = Overlay::prompt("rename workspace", "build");
        assert_eq!(prompt.shown_query(), "build");
        let mut list = overlay(&["ls", "less"]);
        type_text(&mut list, "le");
        assert_eq!(list.shown_query(), "le");
    }

    #[test]
    fn a_secret_prompt_starts_on_the_text_it_was_given() {
        // Which is how a refused passphrase keeps what was typed: the prompt
        // goes back up with the line still on it and the message beside it.
        let overlay = Overlay::secret_prompt("kitchen-table — passphrase", "short");
        assert_eq!(overlay.query(), "short");
        assert_eq!(overlay.shown_query(), "•••••");
    }

    #[test]
    fn the_title_can_be_renamed_without_disturbing_the_line() {
        let mut overlay = overlay(&["kitchen-table", "cafe"]);
        type_text(&mut overlay, "ca");
        overlay.set_title("wireless");
        assert_eq!(overlay.title(), "wireless");
        assert_eq!(overlay.query(), "ca");
        assert_eq!(
            overlay.selected_item().map(|item| item.label.as_str()),
            Some("cafe")
        );
    }

    #[test]
    fn a_prompt_cancels_like_a_list_does() {
        let mut overlay = Overlay::prompt("rename workspace", "1");
        type_text(&mut overlay, "half typed");
        assert_eq!(
            overlay.handle_key(&KeyEvent::new(KeyCode::Escape, Modifiers::NONE)),
            OverlayOutcome::Cancelled
        );
    }

    #[test]
    fn a_prompt_still_swallows_bindings_and_arrows() {
        let mut overlay = Overlay::prompt("rename workspace", "");
        for key in [
            KeyEvent::new(KeyCode::Char('d'), Modifiers::SUPER),
            KeyEvent::new(KeyCode::Up, Modifiers::NONE),
            KeyEvent::new(KeyCode::Down, Modifiers::NONE),
        ] {
            assert_eq!(overlay.handle_key(&key), OverlayOutcome::Consumed);
        }
        assert_eq!(overlay.query(), "");
    }

    fn fonts() -> FontStack {
        FontStack::new(Box::new(BitmapFont::new(1)))
    }

    #[test]
    fn drawing_puts_the_overlay_on_the_surface() {
        let mut fonts = fonts();
        let chrome = Chrome::default();
        let metrics = fonts.metrics();
        let (w, h) = (metrics.cell_width * 60, metrics.cell_height * 20);
        let mut fb = OwnedFramebuffer::new(w, h);
        let mut overlay = overlay(&["ls", "vim", "cat"]);
        {
            let mut surface = fb.surface();
            surface.clear(tos_term::Rgb::new(0xff, 0x00, 0xff));
            overlay.draw(&mut surface, &mut fonts, Rect::new(0, 0, w, h), &chrome);
        }
        // The selected row is highlighted, and the box covers what was under
        // it rather than letting the panes show through.
        assert!(fb.pixels().iter().any(|&px| px == chrome.accent.pack()));
        assert!(fb.pixels().iter().any(|&px| px == chrome.background.pack()));
    }

    #[test]
    fn a_long_list_scrolls_to_keep_the_cursor_visible() {
        let names: Vec<String> = (0..40).map(|i| format!("program-{i}")).collect();
        let items = names.iter().map(OverlayItem::new).collect();
        let mut overlay = Overlay::new("many", items);
        let mut fonts = fonts();
        let metrics = fonts.metrics();
        let (w, h) = (metrics.cell_width * 60, metrics.cell_height * 24);
        let mut fb = OwnedFramebuffer::new(w, h);
        for _ in 0..30 {
            overlay.handle_key(&KeyEvent::new(KeyCode::Down, Modifiers::NONE));
        }
        {
            let mut surface = fb.surface();
            overlay.draw(
                &mut surface,
                &mut fonts,
                Rect::new(0, 0, w, h),
                &Chrome::default(),
            );
        }
        assert!(overlay.scroll > 0, "the list should have scrolled");
        assert!(overlay.scroll <= overlay.cursor);
    }

    #[test]
    fn a_prompt_is_drawn_in_the_rows_a_list_would_not_fit_in() {
        let mut fonts = fonts();
        let metrics = fonts.metrics();
        // Five rows: the borders, the line, and a row of margin either side.
        // A list needs one more for its own row and one for the divider.
        let (w, h) = (metrics.cell_width * 40, metrics.cell_height * 5);
        let chrome = Chrome::default();
        let area = Rect::new(0, 0, w, h);

        let mut list = overlay(&["ls"]);
        let mut fb = OwnedFramebuffer::new(w, h);
        {
            let mut surface = fb.surface();
            list.draw(&mut surface, &mut fonts, area, &chrome);
        }
        assert!(
            fb.pixels().iter().all(|&px| px == 0),
            "the list should not fit"
        );

        let mut prompt = Overlay::prompt("rename workspace", "build");
        let mut fb = OwnedFramebuffer::new(w, h);
        {
            let mut surface = fb.surface();
            prompt.draw(&mut surface, &mut fonts, area, &chrome);
        }
        // The block cursor at the end of the line is accent coloured, so the
        // line was drawn and the box under it was filled.
        assert!(fb.pixels().iter().any(|&px| px == chrome.accent.pack()));
        assert!(fb.pixels().iter().any(|&px| px == chrome.background.pack()));
    }

    #[test]
    fn a_display_too_small_for_the_overlay_draws_nothing() {
        let mut fonts = fonts();
        let metrics = fonts.metrics();
        let (w, h) = (metrics.cell_width * 6, metrics.cell_height * 3);
        let mut fb = OwnedFramebuffer::new(w, h);
        let mut overlay = overlay(&["ls"]);
        {
            let mut surface = fb.surface();
            overlay.draw(
                &mut surface,
                &mut fonts,
                Rect::new(0, 0, w, h),
                &Chrome::default(),
            );
        }
        assert!(
            fb.pixels().iter().all(|&px| px == 0),
            "nothing should be drawn"
        );
    }

    /// The cell size the bitmap face is drawn at, which is what turns the
    /// pixels a placement is in into the cells a mouse event arrives in.
    fn cell(fonts: &mut FontStack) -> (u32, u32) {
        let metrics = fonts.metrics();
        (metrics.cell_width.max(1), metrics.cell_height.max(1))
    }

    /// The cell one row of the list is drawn in, a little way in from the
    /// left border so that the answer does not depend on the label's width.
    fn row_cell(placement: &Placement, row: usize) -> (u32, u32) {
        let (cw, ch) = placement.cell;
        let x = placement.x as u32 / cw + 2;
        let y = placement.row_y(FIRST_LIST_ROW + row) as u32 / ch;
        (x, y)
    }

    fn press(
        overlay: &mut Overlay,
        at: (u32, u32),
        area: Rect,
        cell: (u32, u32),
    ) -> OverlayOutcome {
        overlay.handle_mouse(
            at.0,
            at.1,
            Some(MouseButton::Left),
            MouseAction::Press,
            area,
            cell,
        )
    }

    fn motion(
        overlay: &mut Overlay,
        at: (u32, u32),
        area: Rect,
        cell: (u32, u32),
    ) -> OverlayOutcome {
        overlay.handle_mouse(at.0, at.1, None, MouseAction::Motion, area, cell)
    }

    fn wheel(
        overlay: &mut Overlay,
        button: MouseButton,
        area: Rect,
        cell: (u32, u32),
    ) -> OverlayOutcome {
        overlay.handle_mouse(0, 0, Some(button), MouseAction::Press, area, cell)
    }

    #[test]
    fn a_press_on_a_row_chooses_what_is_drawn_there() {
        let mut fonts = fonts();
        let cell = cell(&mut fonts);
        let area = Rect::new(0, 0, cell.0 * 60, cell.1 * 20);
        let mut overlay = overlay(&["ls", "vim", "cat"]);
        let placement = overlay.placement(area, cell).unwrap();
        let outcome = press(&mut overlay, row_cell(&placement, 1), area, cell);
        assert_eq!(outcome, OverlayOutcome::Chosen(1));
        assert_eq!(overlay.items()[1].label, "vim");
    }

    #[test]
    fn a_press_on_the_query_line_or_a_border_chooses_nothing() {
        let mut fonts = fonts();
        let cell = cell(&mut fonts);
        let area = Rect::new(0, 0, cell.0 * 60, cell.1 * 20);
        let mut overlay = overlay(&["ls", "vim"]);
        let placement = overlay.placement(area, cell).unwrap();
        let (cw, ch) = cell;
        let left = placement.x as u32 / cw;
        for row in [0, 1, 2] {
            let at = (left + 2, placement.row_y(row) as u32 / ch);
            assert_eq!(
                press(&mut overlay, at, area, cell),
                OverlayOutcome::Consumed,
                "row {row} of the box is not a list row"
            );
        }
        // And the columns the border is drawn in, on a row that does hold an
        // item: they are inside the box, which is all `contains` asks, and
        // they are not the row beside them.
        let list_row = placement.row_y(FIRST_LIST_ROW) as u32 / ch;
        for col in [left, left + placement.cols as u32 - 1] {
            assert_eq!(
                press(&mut overlay, (col, list_row), area, cell),
                OverlayOutcome::Consumed,
                "column {col} of the box is a border, not the row it runs beside"
            );
        }
    }

    #[test]
    fn a_press_outside_the_box_cancels_a_list_and_is_swallowed_by_a_prompt() {
        let mut fonts = fonts();
        let cell = cell(&mut fonts);
        let area = Rect::new(0, 0, cell.0 * 60, cell.1 * 20);
        let mut list = overlay(&["ls"]);
        assert_eq!(
            press(&mut list, (0, 0), area, cell),
            OverlayOutcome::Cancelled
        );

        let mut prompt = Overlay::prompt("rename workspace", "build");
        assert_eq!(
            press(&mut prompt, (0, 0), area, cell),
            OverlayOutcome::Consumed
        );
        assert_eq!(prompt.query(), "build");
    }

    #[test]
    fn the_pointer_highlights_the_row_it_is_over_and_repaints_only_when_it_changes() {
        let mut fonts = fonts();
        let cell = cell(&mut fonts);
        let area = Rect::new(0, 0, cell.0 * 60, cell.1 * 20);
        let mut overlay = overlay(&["one", "two", "three"]);
        let placement = overlay.placement(area, cell).unwrap();

        let at = row_cell(&placement, 2);
        assert_eq!(
            motion(&mut overlay, at, area, cell),
            OverlayOutcome::Changed
        );
        assert_eq!(overlay.selected_item().unwrap().label, "three");
        assert_eq!(
            motion(&mut overlay, (at.0 + 1, at.1), area, cell),
            OverlayOutcome::Consumed,
            "still the same row"
        );
        // Off the box entirely: the keyboard's row is still the row shown.
        assert_eq!(
            motion(&mut overlay, (0, 0), area, cell),
            OverlayOutcome::Consumed
        );
        assert_eq!(overlay.selected_item().unwrap().label, "three");
    }

    #[test]
    fn the_wheel_scrolls_a_long_list_and_the_row_under_the_pointer_is_what_it_chooses() {
        let names: Vec<String> = (0..40).map(|i| format!("program-{i}")).collect();
        let items = names.iter().map(OverlayItem::new).collect();
        let mut overlay = Overlay::new("many", items);
        let mut fonts = fonts();
        let cell = cell(&mut fonts);
        let area = Rect::new(0, 0, cell.0 * 60, cell.1 * 24);
        let placement = overlay.placement(area, cell).unwrap();
        let at = row_cell(&placement, 1);

        assert_eq!(
            wheel(&mut overlay, MouseButton::WheelDown, area, cell),
            OverlayOutcome::Changed
        );
        assert_eq!(overlay.scroll(), WHEEL_ROWS as usize);
        // The scroll is what the next frame would draw, so the row under a
        // pointer that has not moved is a different program now.
        let placement = overlay.placement(area, cell).unwrap();
        assert_eq!(placement.scroll, WHEEL_ROWS as usize);
        assert_eq!(
            press(&mut overlay, at, area, cell),
            OverlayOutcome::Chosen(WHEEL_ROWS as usize + 1)
        );
    }

    #[test]
    fn the_wheel_stops_at_the_ends_and_a_list_that_fits_does_not_scroll() {
        let mut short = overlay(&["ls", "vim"]);
        let names: Vec<String> = (0..20).map(|i| format!("program-{i}")).collect();
        let items = names.iter().map(OverlayItem::new).collect();
        let mut long = Overlay::new("many", items);
        let mut fonts = fonts();
        let cell = cell(&mut fonts);
        let area = Rect::new(0, 0, cell.0 * 60, cell.1 * 24);
        assert_eq!(
            wheel(&mut long, MouseButton::WheelUp, area, cell),
            OverlayOutcome::Consumed,
            "already at the top"
        );
        for _ in 0..20 {
            wheel(&mut long, MouseButton::WheelDown, area, cell);
        }
        let placement = long.placement(area, cell).unwrap();
        assert_eq!(long.scroll(), 20 - placement.list_rows);

        assert_eq!(
            wheel(&mut short, MouseButton::WheelDown, area, cell),
            OverlayOutcome::Consumed
        );
        assert_eq!(short.scroll(), 0);
    }

    #[test]
    fn a_list_scrolled_to_its_end_does_not_stay_there_when_the_box_grows_taller() {
        // The window a list is read through is worked out afresh every frame,
        // from a display size that changes: a host terminal is resized, a
        // console gains rows on a mode set. Keeping the cursor on screen is
        // only half of staying in range — a scroll left behind by a box that
        // grew draws a full height frame with three items in it.
        let mut fonts = fonts();
        let cell = cell(&mut fonts);
        let names: Vec<String> = (0..20).map(|i| format!("program-{i}")).collect();
        let items = names.iter().map(OverlayItem::new).collect();
        let mut overlay = Overlay::new("many", items);

        let small = Rect::new(0, 0, cell.0 * 60, cell.1 * 9);
        let placement = overlay.placement(small, cell).unwrap();
        assert_eq!(placement.list_rows, 3, "a window worth scrolling");
        for _ in 0..20 {
            wheel(&mut overlay, MouseButton::WheelDown, small, cell);
        }
        assert_eq!(overlay.scroll(), 20 - 3, "the wheel should reach the end");

        let grown = Rect::new(0, 0, cell.0 * 60, cell.1 * 24);
        let placement = overlay.placement(grown, cell).unwrap();
        assert_eq!(
            placement.scroll + placement.list_rows,
            20,
            "the taller box drew blank rows under the end of the list"
        );
    }

    #[test]
    fn the_row_the_hit_test_names_is_the_row_that_was_drawn_highlighted() {
        let mut fonts = fonts();
        let cell = cell(&mut fonts);
        let (cw, ch) = cell;
        let (w, h) = (cw * 60, ch * 20);
        let chrome = Chrome::default();
        let area = Rect::new(0, 0, w, h);
        let mut overlay = overlay(&["one", "two", "three", "four"]);

        // One placement, asked for once: the click below and the assertions
        // about the pixels are both answered out of it, so a box drawn
        // somewhere other than where the hit test looks fails here rather
        // than agreeing with a second copy of the same arithmetic.
        let placement = overlay.placement(area, cell).unwrap();
        let at = row_cell(&placement, 2);
        assert_eq!(
            motion(&mut overlay, at, area, cell),
            OverlayOutcome::Changed
        );

        let mut fb = OwnedFramebuffer::new(w, h);
        {
            let mut surface = fb.surface();
            overlay.draw(&mut surface, &mut fonts, area, &chrome);
        }
        // The interior of the row, not its borders: the box is drawn in the
        // same blue the highlight is, so a scan that took the border in would
        // find every row highlighted.
        let accent = chrome.accent.pack();
        let painted = |row: usize| -> bool {
            let y = placement.row_y(FIRST_LIST_ROW + row) as u32;
            let left = placement.x as u32 + cw;
            (0..ch).any(|dy| {
                (0..(placement.cols as u32 - 2) * cw)
                    .any(|dx| fb.pixel(left + dx, y + dy) == accent)
            })
        };
        assert!(
            painted(2),
            "the row the pointer is on is the highlighted one"
        );
        for row in [0, 1, 3] {
            assert!(!painted(row), "row {row} should not be highlighted");
        }
    }

    #[test]
    fn a_label_wider_than_the_box_is_clipped_not_wrapped() {
        assert_eq!(clip("abcdef", 3), "abc");
        assert_eq!(clip("漢字", 3), "漢");
        assert_eq!(clip_end("abcdef", 3), "def");
        let mut text = "ab".to_string();
        pad_to(&mut text, 5, '─');
        assert_eq!(text, "ab───");
    }
}
