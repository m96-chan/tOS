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
    /// the accent leaves a selection that cannot be seen, and until now there
    /// was no way to move one without moving the other.
    ///
    /// [`accent`]: Chrome::accent
    pub selection: Option<Rgb>,
}

impl Default for Chrome {
    fn default() -> Self {
        Chrome {
            background: Rgb::new(0x18, 0x18, 0x1c),
            foreground: Rgb::new(0xc8, 0xc8, 0xd0),
            dim: Rgb::new(0x70, 0x70, 0x7c),
            accent: Rgb::new(0x5f, 0x87, 0xd7),
            accent_text: Rgb::new(0x10, 0x10, 0x14),
            divider: Rgb::new(0x2c, 0x2c, 0x34),
            divider_focused: Rgb::new(0x5f, 0x87, 0xd7),
            status_background: None,
            status_foreground: None,
            status_active: None,
            status_active_text: None,
            status_divider: None,
            selection: None,
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
