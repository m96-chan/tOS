//! The compositor's own interface: pane dividers and the status bar.
//!
//! tOS draws its chrome out of the same cells and glyphs as everything else,
//! so it stays consistent at any font size and on any display.

use tos_font::{FontStack, RasterStyle};
use tos_render::{Rect, Surface};
use tos_session::Axis;
use tos_term::{Rgb, Terminal};

/// Colors used by the compositor's own interface.
///
/// Six of these are one colour each; the rest are overrides, and the
/// difference is deliberate. A person who sets `accent` means "the colour this
/// session is highlighted in", and every highlight in it — the workspace on
/// the bar, the row under the cursor in a menu, the text the mouse has
/// selected — should move together. A person who sets the status bar's own
/// background means that one strip and nothing else. So the general colours
/// are values and the particular ones are `Option`, resolved against the
/// general ones at the moment of drawing rather than at the moment of parsing:
/// resolving at parse time would make the answer depend on which order the two
/// lines appear in the file, which is not a thing anybody should have to know.
#[derive(Debug, Clone, Copy)]
pub struct Chrome {
    pub background: Rgb,
    pub foreground: Rgb,
    pub dim: Rgb,
    pub accent: Rgb,
    pub accent_text: Rgb,
    pub divider: Rgb,
    pub divider_focused: Rgb,
    /// The status bar's own strip, when it is not to be the same colour as
    /// everything else the compositor draws.
    pub status_background: Option<Rgb>,
    /// The text of a segment that is not highlighted. Falls back to [`dim`]
    /// rather than to [`foreground`], because the bar is meant to be read when
    /// looked at and ignored otherwise.
    ///
    /// [`dim`]: Chrome::dim
    /// [`foreground`]: Chrome::foreground
    pub status_foreground: Option<Rgb>,
    /// The block behind the active workspace and the focused pane.
    pub status_active: Option<Rgb>,
    /// The text inside that block.
    pub status_active_text: Option<Rgb>,
    /// The rule between two segments, which is what makes a row of unrelated
    /// facts read as a row of unrelated facts rather than as a sentence.
    pub status_divider: Option<Rgb>,
    /// The block behind text the mouse or the keyboard has selected in a pane.
    ///
    /// Separate from [`accent`] because it is the one highlight that sits on
    /// top of somebody else's colours: a palette whose own blue is close to
    /// the accent leaves a selection that cannot be seen, and there has to be
    /// a way to move one without moving the other.
    ///
    /// It is the one of these the default fills in rather than leaving to fall
    /// back, and the reason is the same one: the accent is a light green, and
    /// a light green block behind a program's own output is a block that hides
    /// what it is highlighting. The red it is instead is the other half of the
    /// pair — see the colours below.
    ///
    /// [`accent`]: Chrome::accent
    pub selection: Option<Rgb>,
}

/// The colour every highlight in a tOS session is drawn in.
///
/// The green of the `tOS` in the picture the machine opens with, and of the
/// prompt under it: a session that says what it is in the same colour twice
/// rather than in a blue nothing else on the machine uses. See
/// `docs/design/splash.md`.
pub const ACCENT: Rgb = Rgb::new(0x92, 0xf9, 0x80);

/// The other colour in that picture, and what a selection is drawn in.
///
/// Two colours rather than one because a highlight tOS draws on its own
/// chrome and a highlight it draws over a program's output are not the same
/// job: the first should be the brightest thing on the screen and the second
/// has to sit behind text without swallowing it.
pub const ATTENTION: Rgb = Rgb::new(0xcd, 0x3f, 0x73);

impl Default for Chrome {
    fn default() -> Self {
        Chrome {
            background: Rgb::new(0x18, 0x18, 0x1c),
            foreground: Rgb::new(0xc8, 0xc8, 0xd0),
            dim: Rgb::new(0x70, 0x70, 0x7c),
            accent: ACCENT,
            accent_text: Rgb::new(0x10, 0x10, 0x14),
            divider: Rgb::new(0x2c, 0x2c, 0x34),
            divider_focused: ACCENT,
            status_background: None,
            status_foreground: None,
            status_active: None,
            status_active_text: None,
            status_divider: None,
            selection: Some(ATTENTION),
        }
    }
}

/// The colours the status bar actually paints with, with every fallback
/// already taken.
///
/// Resolved into a struct of its own so that the drawing code never has to ask
/// whether a colour was configured — a question it would have to ask about
/// five separate fields, on every piece of every frame.
#[derive(Debug, Clone, Copy)]
pub struct BarColors {
    pub background: Rgb,
    pub foreground: Rgb,
    pub active: Rgb,
    pub active_text: Rgb,
    pub divider: Rgb,
}

impl Chrome {
    /// What the status bar paints with.
    pub fn bar(&self) -> BarColors {
        BarColors {
            background: self.status_background.unwrap_or(self.background),
            foreground: self.status_foreground.unwrap_or(self.dim),
            active: self.status_active.unwrap_or(self.accent),
            active_text: self.status_active_text.unwrap_or(self.accent_text),
            divider: self.status_divider.unwrap_or(self.divider),
        }
    }

    /// What selected text sits on.
    pub fn selection(&self) -> Rgb {
        self.selection.unwrap_or(self.accent)
    }
}

/// Draw text starting at a pixel position, one cell per character.
///
/// Returns the x position just past the text.
// The arguments are the drawing context plus what to draw; bundling them into
// a struct would only move the same list somewhere else.
#[allow(clippy::too_many_arguments)]
pub fn draw_text(
    surface: &mut Surface<'_>,
    fonts: &mut FontStack,
    x: i32,
    y: i32,
    text: &str,
    fg: Rgb,
    bg: Option<Rgb>,
    bold: bool,
) -> i32 {
    let metrics = fonts.metrics();
    let (cw, ch) = (metrics.cell_width, metrics.cell_height);
    let style = RasterStyle::new(bold, false);
    let mut cursor = x;
    for c in text.chars() {
        let width = tos_term::char_width(c).max(1) as u32 * cw;
        if let Some(bg) = bg {
            surface.fill(Rect::new(cursor, y, width, ch), bg);
        }
        if c != ' ' {
            let glyph = fonts.glyph(c, style);
            if !glyph.is_empty() {
                surface.blend_mask(
                    cursor + glyph.left,
                    y + metrics.baseline as i32 - glyph.top,
                    glyph.width,
                    glyph.height,
                    &glyph.coverage,
                    fg,
                );
            }
        }
        cursor += width as i32;
    }
    cursor
}

/// Draw the line between two panes.
pub fn draw_divider(
    surface: &mut Surface<'_>,
    fonts: &mut FontStack,
    area: Rect,
    axis: Axis,
    color: Rgb,
    background: Rgb,
) {
    let metrics = fonts.metrics();
    let (cw, ch) = (metrics.cell_width, metrics.cell_height);
    // A divider between columns is a vertical line and vice versa.
    let glyph_char = match axis {
        Axis::Columns => '│',
        Axis::Rows => '─',
    };
    let cols = (area.width / cw.max(1)).max(1);
    let rows = (area.height / ch.max(1)).max(1);
    for row in 0..rows {
        for col in 0..cols {
            let x = area.x + (col * cw) as i32;
            let y = area.y + (row * ch) as i32;
            surface.fill(Rect::new(x, y, cw, ch), background);
            let glyph = fonts.glyph(glyph_char, RasterStyle::REGULAR);
            surface.blend_mask(
                x + glyph.left,
                y + metrics.baseline as i32 - glyph.top,
                glyph.width,
                glyph.height,
                &glyph.coverage,
                color,
            );
        }
    }
}

/// Cut text to `cols` cells, never slicing a double width character in half.
pub fn clip(text: &str, cols: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = tos_term::char_width(c).max(1) as usize;
        if used + w > cols {
            break;
        }
        out.push(c);
        used += w;
    }
    out
}

/// Like [`clip`], but says so. The last cell of a string that was cut is an
/// ellipsis, so a truncated message reads as truncated rather than as a
/// shorter message somebody meant to send.
pub fn clip_marked(text: &str, cols: usize) -> String {
    if tos_term::str_width(text) <= cols {
        return text.to_string();
    }
    if cols == 0 {
        return String::new();
    }
    let mut out = clip(text, cols - 1);
    out.push('…');
    out
}

/// Extend `text` with `fill` until it is `cols` cells wide.
///
/// Stops short rather than overshooting, so padding a row with a double width
/// character can leave it a cell narrower than asked; a box whose border ran
/// one cell past its corner would be worse than one a cell short.
pub fn pad_to(text: &mut String, cols: usize, fill: char) {
    let mut used = tos_term::str_width(text);
    let step = tos_term::char_width(fill).max(1) as usize;
    while used + step <= cols {
        text.push(fill);
        used += step;
    }
}

/// Where a box goes: a corner in pixels and a size in cells.
///
/// The size is in cells because every decision about a box is made in cells —
/// how many rows the list gets, how many columns are left for its text — and
/// the corner is in pixels because that is what a [`Surface`] is painted in,
/// and because the thing a box is placed against need not sit on the screen's
/// own grid: an overlay is centred in whatever is left under the status bar,
/// and the IME's candidate window will be placed at a cursor inside a pane.
///
/// Whoever wants the box works the rectangle out. [`draw_box`] deliberately
/// does not centre one itself, because the second caller does not want a
/// centred box: a candidate list belongs at the cursor whose text it offers
/// replacements for, and above the cursor's row when there is no room below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoxRect {
    pub x: i32,
    pub y: i32,
    pub cols: usize,
    pub rows: usize,
}

impl BoxRect {
    pub fn new(x: i32, y: i32, cols: usize, rows: usize) -> Self {
        BoxRect { x, y, cols, rows }
    }
}

/// One row of a box's interior, borders excluded.
///
/// Borrowed rather than owned, so that a caller which already has the strings
/// — a candidate window holding a dictionary's entries — does not copy them
/// once per frame to say where they go.
#[derive(Debug, Clone, Copy)]
pub enum BoxLine<'a> {
    /// Text, clipped to the width that is left and padded out to it, so a row
    /// is opaque from border to border however short its text is.
    Text {
        text: &'a str,
        fg: Rgb,
        /// The row's own background, for the row a cursor is on. `None` means
        /// the box's, which is the usual answer.
        bg: Option<Rgb>,
        bold: bool,
    },
    /// A rule across the box, `├───┤`: what separates a query from the list it
    /// filters.
    Rule,
    /// Borders and background and nothing between them, for a row the caller
    /// paints itself. An overlay's query line is three colours and a block
    /// cursor, and a description of it in this enum would be a worse helper
    /// than an empty row and one piece of code that knows.
    Blank,
}

impl<'a> BoxLine<'a> {
    /// A plain row on the box's own background.
    pub fn text(text: &'a str, fg: Rgb) -> Self {
        BoxLine::Text {
            text,
            fg,
            bg: None,
            bold: false,
        }
    }
}

/// Draw a bordered box of cells, with `title` set into its top border and one
/// [`BoxLine`] per interior row.
///
/// This is the compositor's one box. The overlay drew it by hand and was the
/// only thing in the tree that knew how, which was fine while the overlay was
/// the only modal surface; the IME's candidate window needs the same box and
/// must not be an `Overlay`, because an overlay owns the keyboard and a
/// candidate window that took every key would be an input method that stops
/// you typing. So the border, the clipping and the padding live here, where
/// something that is not modal can reach them.
///
/// Rows past the end of `lines` are drawn empty and lines past the end of the
/// box are not drawn at all: the caller chose the rectangle, so the rectangle
/// wins.
pub fn draw_box(
    surface: &mut Surface<'_>,
    fonts: &mut FontStack,
    rect: BoxRect,
    title: Option<&str>,
    lines: &[BoxLine<'_>],
    chrome: &Chrome,
) {
    // Two borders and a cell between them is the smallest box there is; below
    // that there is nothing honest to draw, and a border drawn over itself
    // would read as a stray glyph rather than as a box.
    if rect.cols < 3 || rect.rows < 2 {
        return;
    }
    let metrics = fonts.metrics();
    let (cw, ch) = (metrics.cell_width.max(1), metrics.cell_height.max(1));
    let inner = rect.cols - 2;
    let row_y = |row: usize| rect.y + (row as u32 * ch) as i32;
    let border = chrome.divider_focused;
    let background = Some(chrome.background);

    // Whatever is underneath must not show through. A box is a surface in its
    // own right, and one with the panes visible through it is unreadable.
    surface.fill(
        Rect::new(rect.x, rect.y, rect.cols as u32 * cw, rect.rows as u32 * ch),
        chrome.background,
    );

    // The title sits in the top border rather than on a row of its own, which
    // is a whole row saved on a box that is mostly border already.
    let mut top = match title {
        Some(title) => format!("┌─ {} ", clip(title, inner.saturating_sub(4))),
        None => "┌".to_string(),
    };
    pad_to(&mut top, rect.cols - 1, '─');
    top.push('┐');
    draw_text(
        surface,
        fonts,
        rect.x,
        row_y(0),
        &top,
        border,
        background,
        false,
    );

    // A row the caller said nothing about is a row with borders and nothing
    // in it, which is what [`BoxLine::Blank`] asks for anyway.
    for row in 0..rect.rows - 2 {
        let y = row_y(row + 1);
        let line = lines.get(row).copied().unwrap_or(BoxLine::Blank);
        if let BoxLine::Rule = line {
            let mut rule = "├".to_string();
            pad_to(&mut rule, rect.cols - 1, '─');
            rule.push('┤');
            draw_text(surface, fonts, rect.x, y, &rule, border, background, false);
            continue;
        }
        draw_text(surface, fonts, rect.x, y, "│", border, background, false);
        let right = rect.x + ((rect.cols - 1) as u32 * cw) as i32;
        draw_text(surface, fonts, right, y, "│", border, background, false);
        if let BoxLine::Text { text, fg, bg, bold } = line {
            // The clipping and the padding together are what make a row a
            // row: cut to the cells there are, and filled out to them so the
            // border has an unbroken run of background to sit at the end of.
            let mut text = clip(text, inner);
            pad_to(&mut text, inner, ' ');
            draw_text(
                surface,
                fonts,
                rect.x + cw as i32,
                y,
                &text,
                fg,
                Some(bg.unwrap_or(chrome.background)),
                bold,
            );
        }
    }

    let mut bottom = "└".to_string();
    pad_to(&mut bottom, rect.cols - 1, '─');
    bottom.push('┘');
    draw_text(
        surface,
        fonts,
        rect.x,
        row_y(rect.rows - 1),
        &bottom,
        border,
        background,
        false,
    );
}

/// The label a pane shows in the status bar.
pub fn pane_label(index: usize, terminal: &Terminal, title: &str) -> String {
    let title = if title.is_empty() {
        // Without a title from the application, say something truthful.
        if terminal.modes.alt_screen {
            "application"
        } else {
            "shell"
        }
    } else {
        title
    };
    let title: String = title.chars().take(24).collect();
    format!("{}:{}", index + 1, title)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tos_font::BitmapFont;
    use tos_render::OwnedFramebuffer;
    use tos_term::TerminalConfig;

    fn fonts() -> FontStack {
        FontStack::new(Box::new(BitmapFont::new(1)))
    }

    #[test]
    fn text_advances_by_one_cell_per_character() {
        let mut fonts = fonts();
        let cw = fonts.metrics().cell_width as i32;
        let mut fb = OwnedFramebuffer::new(64, 16);
        let mut surface = fb.surface();
        let end = draw_text(
            &mut surface,
            &mut fonts,
            0,
            0,
            "abc",
            Rgb::WHITE,
            None,
            false,
        );
        assert_eq!(end, cw * 3);
    }

    #[test]
    fn wide_characters_take_two_cells() {
        let mut fonts = fonts();
        let cw = fonts.metrics().cell_width as i32;
        let mut fb = OwnedFramebuffer::new(64, 16);
        let mut surface = fb.surface();
        let end = draw_text(
            &mut surface,
            &mut fonts,
            0,
            0,
            "漢",
            Rgb::WHITE,
            None,
            false,
        );
        assert_eq!(end, cw * 2);
    }

    #[test]
    fn text_puts_ink_on_the_surface() {
        let mut fonts = fonts();
        let mut fb = OwnedFramebuffer::new(64, 16);
        {
            let mut surface = fb.surface();
            draw_text(
                &mut surface,
                &mut fonts,
                0,
                0,
                "X",
                Rgb::WHITE,
                Some(Rgb::BLACK),
                false,
            );
        }
        assert!(fb.pixels().contains(&0xffffff));
    }

    #[test]
    fn dividers_ink_every_cell() {
        let mut fonts = fonts();
        let metrics = fonts.metrics();
        let mut fb = OwnedFramebuffer::new(metrics.cell_width, metrics.cell_height * 3);
        {
            let mut surface = fb.surface();
            draw_divider(
                &mut surface,
                &mut fonts,
                Rect::new(0, 0, metrics.cell_width, metrics.cell_height * 3),
                Axis::Columns,
                Rgb::WHITE,
                Rgb::BLACK,
            );
        }
        // A vertical divider inks something on every row.
        for row in 0..3 {
            let y = row * metrics.cell_height + metrics.cell_height / 2;
            let inked = (0..metrics.cell_width).any(|x| fb.pixel(x, y) != 0);
            assert!(inked, "divider missing on row {row}");
        }
    }

    /// The colour nothing in a box is allowed to be, so that a pixel still
    /// wearing it is a pixel the box did not touch.
    const UNTOUCHED: Rgb = Rgb::new(0xff, 0x00, 0xff);

    #[test]
    fn a_box_is_drawn_where_it_was_put_rather_than_centred() {
        let mut fonts = fonts();
        let metrics = fonts.metrics();
        let (cw, ch) = (metrics.cell_width, metrics.cell_height);
        let chrome = Chrome::default();
        let mut fb = OwnedFramebuffer::new(cw * 20, ch * 10);
        {
            let mut surface = fb.surface();
            surface.clear(UNTOUCHED);
            draw_box(
                &mut surface,
                &mut fonts,
                BoxRect::new((cw * 3) as i32, (ch * 2) as i32, 8, 4),
                Some("t"),
                &[BoxLine::text("hi", chrome.foreground), BoxLine::Rule],
                &chrome,
            );
        }
        // Nothing outside the rectangle it was handed. This is the property
        // the candidate window needs and an overlay cannot give it: a box
        // that centred itself would be four cells to the right of here.
        for y in 0..ch * 10 {
            for x in 0..cw * 20 {
                let inside = (cw * 3..cw * 11).contains(&x) && (ch * 2..ch * 6).contains(&y);
                if !inside {
                    assert_eq!(fb.pixel(x, y), UNTOUCHED.pack(), "painted at {x},{y}");
                }
            }
        }
        // And inside it is opaque, so the panes do not show through.
        assert!(fb.pixel(cw * 3 + 1, ch * 2 + 1) != UNTOUCHED.pack());
    }

    #[test]
    fn a_box_pads_a_short_line_and_clips_a_long_one_to_the_same_cells() {
        let metrics = fonts().metrics();
        let (cw, ch) = (metrics.cell_width, metrics.cell_height);
        // A highlight no other part of the box shares. The default accent and
        // the focused divider are deliberately the same blue, so a border
        // painted in it would count as a row that reached too far.
        let highlight = Rgb::new(0x00, 0xff, 0x00);
        let chrome = Chrome::default();
        // Six cells wide is two borders and four to write in.
        let row_is = |text: &str| {
            let mut fonts = fonts();
            let mut fb = OwnedFramebuffer::new(cw * 6, ch * 3);
            {
                let mut surface = fb.surface();
                surface.clear(UNTOUCHED);
                draw_box(
                    &mut surface,
                    &mut fonts,
                    BoxRect::new(0, 0, 6, 3),
                    None,
                    &[BoxLine::Text {
                        text,
                        fg: chrome.foreground,
                        // A row with a background of its own is the row a
                        // cursor is on, and it is also the only way to see
                        // from the outside where a row stopped.
                        bg: Some(highlight),
                        bold: false,
                    }],
                    &chrome,
                );
            }
            (0..6)
                .map(|col| {
                    (0..ch).any(|y| {
                        (0..cw).any(|x| fb.pixel(col * cw + x, ch + y) == highlight.pack())
                    })
                })
                .collect::<Vec<bool>>()
        };
        // The row is highlighted up to each border and into neither, whether
        // the text ran out early or was cut short.
        let expected = vec![false, true, true, true, true, false];
        assert_eq!(row_is("a"), expected);
        assert_eq!(row_is("abcdefghij"), expected);
        // A double width character is not cut in half to make it fit.
        assert_eq!(row_is("漢字です"), expected);
    }

    #[test]
    fn a_box_with_no_room_between_its_borders_draws_nothing() {
        let mut fonts = fonts();
        let metrics = fonts.metrics();
        let (cw, ch) = (metrics.cell_width, metrics.cell_height);
        let mut fb = OwnedFramebuffer::new(cw * 4, ch * 4);
        {
            let mut surface = fb.surface();
            // Two columns is two borders with nothing between them, which is
            // not a box; better nothing than a pair of stray glyphs.
            draw_box(
                &mut surface,
                &mut fonts,
                BoxRect::new(0, 0, 2, 4),
                Some("t"),
                &[BoxLine::Blank],
                &Chrome::default(),
            );
        }
        assert!(fb.pixels().iter().all(|&px| px == 0));
    }

    #[test]
    fn clipping_marks_what_it_cut_and_leaves_what_fits_alone() {
        assert_eq!(clip_marked("hello", 10), "hello");
        assert_eq!(clip_marked("hello", 5), "hello");
        assert_eq!(clip_marked("hello", 4), "hel…");
        assert_eq!(clip_marked("hello", 1), "…");
        assert_eq!(clip_marked("hello", 0), "");
        // A double width character is never cut in half, so the result can
        // come out a cell narrower than it was allowed.
        assert_eq!(clip_marked("漢字です", 4), "漢…");
        assert_eq!(clip("漢字", 3), "漢");
    }

    #[test]
    fn pane_labels_fall_back_to_something_truthful() {
        let terminal = Terminal::new(10, 5, TerminalConfig::default());
        assert_eq!(pane_label(0, &terminal, ""), "1:shell");
        assert_eq!(pane_label(2, &terminal, "vim"), "3:vim");
    }

    #[test]
    fn long_titles_are_clipped() {
        let terminal = Terminal::new(10, 5, TerminalConfig::default());
        let label = pane_label(0, &terminal, &"x".repeat(100));
        assert!(label.len() < 40);
    }
}
