//! The compositor's own interface: pane dividers and the status bar.
//!
//! tOS draws its chrome out of the same cells and glyphs as everything else,
//! so it stays consistent at any font size and on any display.

use tos_font::{FontStack, RasterStyle};
use tos_render::{Rect, Surface};
use tos_session::Axis;
use tos_term::{Rgb, Terminal};

/// Colors used by the compositor's own interface.
#[derive(Debug, Clone, Copy)]
pub struct Chrome {
    pub background: Rgb,
    pub foreground: Rgb,
    pub dim: Rgb,
    pub accent: Rgb,
    pub accent_text: Rgb,
    pub divider: Rgb,
    pub divider_focused: Rgb,
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
        }
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

/// One segment of the status bar.
pub struct StatusItem {
    pub text: String,
    pub highlighted: bool,
}

impl StatusItem {
    pub fn new(text: impl Into<String>, highlighted: bool) -> Self {
        StatusItem {
            text: text.into(),
            highlighted,
        }
    }
}

/// Draw the status bar across `area`.
pub fn draw_status_bar(
    surface: &mut Surface<'_>,
    fonts: &mut FontStack,
    area: Rect,
    chrome: &Chrome,
    left: &[StatusItem],
    right: &str,
) {
    surface.fill(area, chrome.background);

    let mut x = area.x;
    for item in left {
        let label = format!(" {} ", item.text);
        let (fg, bg) = if item.highlighted {
            (chrome.accent_text, chrome.accent)
        } else {
            (chrome.dim, chrome.background)
        };
        x = draw_text(surface, fonts, x, area.y, &label, fg, Some(bg), item.highlighted);
    }

    if right.is_empty() {
        return;
    }
    // Right aligned, with a cell of margin, in whatever the workspace items
    // left. Text that does not fit is cut rather than dropped: the messages
    // too long for the bar are exactly the ones worth reading, because a
    // failure carries an `io::Error` and those are long.
    let metrics = fonts.metrics();
    let cw = metrics.cell_width.max(1);
    let room = (((area.right() - x).max(0) as u32) / cw).saturating_sub(1) as usize;
    if room == 0 {
        return;
    }
    let text = clip_marked(right, room);
    let width = tos_term::str_width(&text) as u32 * cw;
    let start = area.right() - width as i32 - cw as i32;
    draw_text(
        surface,
        fonts,
        start,
        area.y,
        &text,
        chrome.dim,
        Some(chrome.background),
        false,
    );
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
        let end = draw_text(&mut surface, &mut fonts, 0, 0, "漢", Rgb::WHITE, None, false);
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
    fn the_status_bar_fills_its_area() {
        let mut fonts = fonts();
        let chrome = Chrome::default();
        let metrics = fonts.metrics();
        let mut fb = OwnedFramebuffer::new(metrics.cell_width * 20, metrics.cell_height);
        {
            let mut surface = fb.surface();
            draw_status_bar(
                &mut surface,
                &mut fonts,
                Rect::new(0, 0, metrics.cell_width * 20, metrics.cell_height),
                &chrome,
                &[StatusItem::new("1", true), StatusItem::new("2", false)],
                "tOS",
            );
        }
        // The highlighted workspace uses the accent colour.
        assert!(fb.pixels().iter().any(|&px| px == chrome.accent.pack()));
        // And nothing is left transparent.
        assert!(!fb.pixels().iter().all(|&px| px == 0));
    }

    /// Draw a bar of `cols` cells with one highlighted workspace and this
    /// message, and say whether any of the message reached the surface. The
    /// workspace is highlighted so the only dim ink can be the message.
    fn bar_with_message(cols: u32, message: &str) -> bool {
        let mut fonts = fonts();
        let chrome = Chrome::default();
        let metrics = fonts.metrics();
        let (w, h) = (metrics.cell_width * cols, metrics.cell_height);
        let mut fb = OwnedFramebuffer::new(w, h);
        {
            let mut surface = fb.surface();
            draw_status_bar(
                &mut surface,
                &mut fonts,
                Rect::new(0, 0, w, h),
                &chrome,
                &[StatusItem::new("1", true)],
                message,
            );
        }
        fb.pixels().iter().any(|&px| px == chrome.dim.pack())
    }

    #[test]
    fn a_message_too_long_for_the_bar_is_clipped_rather_than_dropped() {
        // The message that does not fit is the one worth reading: this is
        // what an `io::Error` from a failed split looks like on a narrow bar.
        let long = "split failed: no such file or directory (os error 2)";
        assert!(bar_with_message(20, long), "the message was not drawn");
        assert!(bar_with_message(80, long));
        // Down to a bar with no room at all for it, which draws nothing and
        // does not panic working out where the text would start.
        assert!(!bar_with_message(3, long));
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
