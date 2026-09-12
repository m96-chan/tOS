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
use tos_input::{KeyCode, KeyEvent, Modifiers};
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
/// Rows the overlay spends on itself: two borders, the query and its divider.
const CHROME_ROWS: usize = 4;

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
    /// First visible row, kept in range by [`Overlay::draw`], which is the
    /// only place the number of visible rows is known.
    scroll: usize,
    /// Whether the line itself is the answer rather than a filter over the
    /// list. A prompt has no list to choose from, so enter takes the text.
    prompt: bool,
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

    /// Replace the list, keeping the query.
    ///
    /// For a menu whose contents arrive late or change under it: a network
    /// scan finding another access point should not throw away what the user
    /// has typed.
    pub fn set_items(&mut self, items: Vec<OverlayItem>) {
        self.items = items;
        self.refilter();
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

    /// Indices into [`Overlay::items`] that the query matches, best first.
    pub fn matches(&self) -> &[usize] {
        &self.matches
    }

    /// The index into [`Overlay::items`] under the cursor, if anything matches.
    pub fn selected(&self) -> Option<usize> {
        self.matches.get(self.cursor).copied()
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

    /// Draw the overlay centred in `area`, which is in pixels.
    ///
    /// Takes `&mut self` because this is where the number of visible rows is
    /// known, and so where the scroll offset can be brought back into range.
    pub fn draw(
        &mut self,
        surface: &mut Surface<'_>,
        fonts: &mut FontStack,
        area: Rect,
        chrome: &Chrome,
    ) {
        let metrics = fonts.metrics();
        let (cw, ch) = (metrics.cell_width.max(1), metrics.cell_height.max(1));
        let cols = (area.width / cw) as usize;
        let rows = (area.height / ch) as usize;

        // A list row plus the overlay's own rows, and a cell of margin on
        // every side. Below that there is no honest way to draw this, and a
        // box with no room inside it is worse than none.
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
            return;
        }
        let box_rows = list_rows + chrome_rows;
        let inner = box_cols - 2;
        let x0 = area.x + (((cols - box_cols) / 2) * cw as usize) as i32;
        let y0 = area.y + (((rows - box_rows) / 2) * ch as usize) as i32;

        // Keep the cursor on screen now that the row count is known.
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if list_rows > 0 && self.cursor >= self.scroll + list_rows {
            self.scroll = self.cursor + 1 - list_rows;
        }

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

        let row_y = |row: usize| y0 + (row as u32 * ch) as i32;

        // The query line, ending in a block cursor so it is obvious where the
        // keyboard is going. It starts a cell in, because the box has already
        // drawn the border it starts after.
        let y = row_y(1);
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
        let shown = clip_end(&self.query, inner.saturating_sub(5));
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
                row_y(3 + row),
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
