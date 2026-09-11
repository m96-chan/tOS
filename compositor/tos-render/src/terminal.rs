//! Painting a terminal into a surface.
//!
//! Everything here is CPU work on a plain pixel buffer. The README's first
//! renderer is deliberately unsophisticated; the point is that the terminal,
//! session and protocol model above it never has to change when a GPU path is
//! added underneath.

use tos_font::{FontStack, RasterStyle};
use tos_term::cell::{Cell, Flags, Underline};
use tos_term::{Color, CursorShape, Palette, Rgb, Terminal};

use crate::surface::{Rect, Surface};

/// A selected region of the grid, in cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    /// Inclusive start, in (column, row) of the displayed grid.
    pub start: (usize, usize),
    /// Inclusive end.
    pub end: (usize, usize),
    /// Rectangular rather than line-wise selection.
    pub block: bool,
}

impl Selection {
    pub fn new(start: (usize, usize), end: (usize, usize), block: bool) -> Self {
        Selection { start, end, block }
    }

    /// Normalize so that `start` precedes `end`.
    fn ordered(&self) -> ((usize, usize), (usize, usize)) {
        let (a, b) = (self.start, self.end);
        if (a.1, a.0) <= (b.1, b.0) {
            (a, b)
        } else {
            (b, a)
        }
    }

    pub fn contains(&self, col: usize, row: usize) -> bool {
        let (start, end) = self.ordered();
        if self.block {
            let (left, right) = (start.0.min(end.0), start.0.max(end.0));
            return row >= start.1 && row <= end.1 && col >= left && col <= right;
        }
        if row < start.1 || row > end.1 {
            return false;
        }
        if row == start.1 && col < start.0 {
            return false;
        }
        if row == end.1 && col > end.0 {
            return false;
        }
        true
    }
}

/// Per frame rendering choices the compositor makes.
#[derive(Debug, Clone)]
pub struct RenderOptions {
    /// Phase of the blink cycle; false hides blinking text and cursors.
    pub blink_visible: bool,
    /// Whether this pane has keyboard focus.
    pub focused: bool,
    pub draw_cursor: bool,
    pub selection: Option<Selection>,
    pub selection_background: Rgb,
    /// Repaint every row, ignoring damage.
    pub force: bool,
    /// Fade unfocused panes by this amount, 0 for none.
    pub inactive_fade: u8,
}

impl Default for RenderOptions {
    fn default() -> Self {
        RenderOptions {
            blink_visible: true,
            focused: true,
            draw_cursor: true,
            selection: None,
            selection_background: Rgb::new(0x33, 0x44, 0x66),
            force: false,
            inactive_fade: 0,
        }
    }
}

/// Resolved colors for one cell.
struct CellColors {
    fg: Rgb,
    bg: Rgb,
    underline: Rgb,
}

fn resolve_colors(
    cell: &Cell,
    palette: &Palette,
    reverse_screen: bool,
    selected: bool,
    options: &RenderOptions,
) -> CellColors {
    let attrs = &cell.attrs;
    let mut fg = attrs.fg;
    let bg = attrs.bg;

    // Bold text with a palette color uses the bright half of the palette, the
    // convention every terminal inherited from the VGA days.
    if attrs.flags.contains(Flags::BOLD) {
        fg = Palette::brighten(fg);
    }

    let mut fg_rgb = palette.resolve(fg, true);
    let mut bg_rgb = palette.resolve(bg, false);

    if attrs.flags.contains(Flags::DIM) {
        fg_rgb = fg_rgb.scale(2, 3);
    }

    let reverse = attrs.flags.contains(Flags::REVERSE) != reverse_screen;
    if reverse {
        std::mem::swap(&mut fg_rgb, &mut bg_rgb);
    }

    if attrs.flags.contains(Flags::HIDDEN) {
        fg_rgb = bg_rgb;
    }
    if attrs.flags.contains(Flags::BLINK) && !options.blink_visible {
        fg_rgb = bg_rgb;
    }

    if selected {
        bg_rgb = options.selection_background;
    }

    if options.inactive_fade > 0 && !options.focused {
        let target = palette.background;
        fg_rgb = fg_rgb.blend(target, options.inactive_fade);
        bg_rgb = bg_rgb.blend(target, options.inactive_fade);
    }

    let underline = match attrs.underline_color {
        Color::Default => fg_rgb,
        other => palette.resolve(other, true),
    };

    CellColors {
        fg: fg_rgb,
        bg: bg_rgb,
        underline,
    }
}

/// Paint `term` into `area`.
///
/// Only rows the terminal marked as damaged are repainted unless
/// [`RenderOptions::force`] is set. The caller clears the damage once the
/// frame has been presented.
pub fn render(
    surface: &mut Surface<'_>,
    area: Rect,
    term: &Terminal,
    fonts: &mut FontStack,
    options: &RenderOptions,
) {
    let metrics = fonts.metrics();
    let (cw, ch) = (metrics.cell_width, metrics.cell_height);
    let palette = term.palette();
    let grid = term.grid();
    let rows = grid.rows().min((area.height / ch.max(1)) as usize);
    let cols = grid.cols().min((area.width / cw.max(1)) as usize);

    let background = if term.modes.reverse_video {
        palette.foreground
    } else {
        palette.background
    };

    surface.with_clip(area, |surface| {
        // Any part of the pane the grid does not cover, such as a few pixels
        // left over from an uneven division, still belongs to this pane.
        if options.force {
            surface.fill(area, background);
        }

        for y in 0..rows {
            if !options.force && !term.damage().is_row_dirty(y) {
                continue;
            }
            let row = grid.display_row(y);
            let py = area.y + (y as u32 * ch) as i32;

            // Background first, merging runs of identical color.
            let mut run_start = 0usize;
            let mut run_color: Option<Rgb> = None;
            for x in 0..cols {
                let cell = &row.cells()[x];
                let selected = options
                    .selection
                    .map(|s| s.contains(x, y))
                    .unwrap_or(false);
                let colors =
                    resolve_colors(cell, palette, term.modes.reverse_video, selected, options);
                match run_color {
                    Some(color) if color == colors.bg => {}
                    Some(color) => {
                        let px = area.x + (run_start as u32 * cw) as i32;
                        let width = (x - run_start) as u32 * cw;
                        surface.fill(Rect::new(px, py, width, ch), color);
                        run_start = x;
                        run_color = Some(colors.bg);
                    }
                    None => {
                        run_start = x;
                        run_color = Some(colors.bg);
                    }
                }
            }
            if let Some(color) = run_color {
                let px = area.x + (run_start as u32 * cw) as i32;
                let width = (cols - run_start) as u32 * cw;
                surface.fill(Rect::new(px, py, width, ch), color);
            }
            // Trailing pixels when the pane is not an exact number of cells.
            let used = cols as u32 * cw;
            if used < area.width {
                surface.fill(
                    Rect::new(area.x + used as i32, py, area.width - used, ch),
                    background,
                );
            }

            // Then glyphs and decorations.
            for x in 0..cols {
                let cell = &row.cells()[x];
                if cell.attrs.flags.contains(Flags::WIDE_SPACER) {
                    continue;
                }
                let selected = options
                    .selection
                    .map(|s| s.contains(x, y))
                    .unwrap_or(false);
                let colors =
                    resolve_colors(cell, palette, term.modes.reverse_video, selected, options);
                let px = area.x + (x as u32 * cw) as i32;
                draw_cell(surface, px, py, cell, &colors, fonts, cw, ch);
            }
        }

        // Images sit above the text layer.
        draw_graphics(surface, area, term, cw, ch, options);

        if options.draw_cursor {
            draw_cursor(surface, area, term, fonts, options);
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn draw_cell(
    surface: &mut Surface<'_>,
    px: i32,
    py: i32,
    cell: &Cell,
    colors: &CellColors,
    fonts: &mut FontStack,
    cell_width: u32,
    cell_height: u32,
) {
    let metrics = fonts.metrics();
    let baseline = py + metrics.baseline as i32;

    if cell.ch != ' ' {
        let style = RasterStyle::new(
            cell.attrs.flags.contains(Flags::BOLD),
            cell.attrs.flags.contains(Flags::ITALIC),
        );
        let glyph = fonts.glyph(cell.ch, style);
        if !glyph.is_empty() {
            surface.blend_mask(
                px + glyph.left,
                baseline - glyph.top,
                glyph.width,
                glyph.height,
                &glyph.coverage,
                colors.fg,
            );
        }
        // Combining marks are drawn on top of the base glyph.
        if let Some(marks) = &cell.zerowidth {
            for &mark in marks.iter() {
                let glyph = fonts.glyph(mark, style);
                if !glyph.is_empty() {
                    surface.blend_mask(
                        px + glyph.left,
                        baseline - glyph.top,
                        glyph.width,
                        glyph.height,
                        &glyph.coverage,
                        colors.fg,
                    );
                }
            }
        }
    }

    let span = if cell.attrs.flags.contains(Flags::WIDE) {
        cell_width * 2
    } else {
        cell_width
    };
    let thickness = metrics.underline_thickness.max(1);

    if !cell.attrs.underline.is_none() {
        draw_underline(
            surface,
            px,
            py + metrics.underline_position as i32,
            span,
            thickness,
            cell.attrs.underline,
            colors.underline,
        );
    }
    if cell.attrs.flags.contains(Flags::STRIKEOUT) {
        surface.fill(
            Rect::new(px, py + metrics.strikeout_position as i32, span, thickness),
            colors.fg,
        );
    }
    if cell.attrs.flags.contains(Flags::OVERLINE) {
        surface.fill(Rect::new(px, py, span, thickness), colors.fg);
    }
    let _ = cell_height;
}

fn draw_underline(
    surface: &mut Surface<'_>,
    x: i32,
    y: i32,
    width: u32,
    thickness: u32,
    style: Underline,
    color: Rgb,
) {
    match style {
        Underline::None => {}
        Underline::Single => surface.fill(Rect::new(x, y, width, thickness), color),
        Underline::Double => {
            surface.fill(Rect::new(x, y, width, thickness), color);
            surface.fill(
                Rect::new(x, y + (thickness * 2) as i32, width, thickness),
                color,
            );
        }
        Underline::Curly => {
            // A small triangle wave, one period every four pixels.
            let amplitude = thickness.max(1) as i32;
            for dx in 0..width as i32 {
                let phase = dx % 4;
                let offset = match phase {
                    0 | 2 => 0,
                    1 => -amplitude,
                    _ => amplitude,
                };
                surface.fill(
                    Rect::new(x + dx, y + offset, 1, thickness),
                    color,
                );
            }
        }
        Underline::Dotted => {
            for dx in (0..width as i32).step_by(2) {
                surface.fill(Rect::new(x + dx, y, 1, thickness), color);
            }
        }
        Underline::Dashed => {
            let dash = (width / 4).max(1);
            let mut dx = 0;
            while dx < width {
                surface.fill(
                    Rect::new(x + dx as i32, y, dash.min(width - dx), thickness),
                    color,
                );
                dx += dash * 2;
            }
        }
    }
}

fn draw_graphics(
    surface: &mut Surface<'_>,
    area: Rect,
    term: &Terminal,
    cell_width: u32,
    cell_height: u32,
    options: &RenderOptions,
) {
    let store = term.graphics();
    if store.is_empty() {
        return;
    }
    // Placements are drawn back to front so z-index is respected.
    let mut placements: Vec<_> = store.placements().collect();
    placements.sort_by_key(|p| p.z_index);

    for placement in placements {
        let Some(image) = store.image(placement.image_id) else {
            continue;
        };
        let dest = Rect::new(
            area.x + (placement.col as u32 * cell_width) as i32,
            area.y + (placement.row as u32 * cell_height) as i32,
            placement.cols as u32 * cell_width,
            placement.rows as u32 * cell_height,
        );
        // Repaint an image only when a row it covers was damaged.
        if !options.force {
            let touched = (0..placement.rows as usize)
                .any(|dy| term.damage().is_row_dirty(placement.row as usize + dy));
            if !touched {
                continue;
            }
        }
        let region = Rect::new(
            placement.src_x as i32,
            placement.src_y as i32,
            placement.src_w,
            placement.src_h,
        );
        surface.blit_rgba_region(dest, &image.data, image.width, image.height, region);
    }
}

fn draw_cursor(
    surface: &mut Surface<'_>,
    area: Rect,
    term: &Terminal,
    fonts: &mut FontStack,
    options: &RenderOptions,
) {
    if !term.modes.cursor_visible || term.display_offset() != 0 {
        return;
    }
    let style = term.cursor_style();
    if style.blinking && !options.blink_visible && options.focused {
        return;
    }

    let metrics = fonts.metrics();
    let (cw, ch) = (metrics.cell_width, metrics.cell_height);
    let cursor = term.cursor();
    let px = area.x + (cursor.x as u32 * cw) as i32;
    let py = area.y + (cursor.y as u32 * ch) as i32;
    let palette = term.palette();
    let color = palette.cursor;

    // An unfocused pane shows where its cursor is without pretending to own
    // the keyboard, so it is always drawn as an outline.
    let shape = if options.focused {
        style.shape
    } else {
        CursorShape::Hollow
    };
    let thickness = (ch / 8).max(1);

    match shape {
        CursorShape::Block => {
            surface.fill(Rect::new(px, py, cw, ch), color);
            // Redraw the character on top in the cursor's text color.
            if let Some(cell) = term.grid().cell(cursor.x, cursor.y) {
                if cell.ch != ' ' {
                    let style = RasterStyle::new(
                        cell.attrs.flags.contains(Flags::BOLD),
                        cell.attrs.flags.contains(Flags::ITALIC),
                    );
                    let glyph = fonts.glyph(cell.ch, style);
                    surface.blend_mask(
                        px + glyph.left,
                        py + metrics.baseline as i32 - glyph.top,
                        glyph.width,
                        glyph.height,
                        &glyph.coverage,
                        palette.cursor_text,
                    );
                }
            }
        }
        CursorShape::Underline => {
            surface.fill(
                Rect::new(px, py + (ch - thickness) as i32, cw, thickness),
                color,
            );
        }
        CursorShape::Beam => surface.fill(Rect::new(px, py, thickness, ch), color),
        CursorShape::Hollow => {
            surface.fill(Rect::new(px, py, cw, thickness), color);
            surface.fill(Rect::new(px, py + (ch - thickness) as i32, cw, thickness), color);
            surface.fill(Rect::new(px, py, thickness, ch), color);
            surface.fill(Rect::new(px + (cw - thickness) as i32, py, thickness, ch), color);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linewise_selection_spans_rows() {
        let sel = Selection::new((2, 0), (1, 2), false);
        assert!(!sel.contains(1, 0));
        assert!(sel.contains(2, 0));
        assert!(sel.contains(9, 1));
        assert!(sel.contains(1, 2));
        assert!(!sel.contains(2, 2));
    }

    #[test]
    fn block_selection_is_rectangular() {
        let sel = Selection::new((2, 0), (4, 2), true);
        assert!(sel.contains(3, 1));
        assert!(!sel.contains(5, 1));
        assert!(!sel.contains(1, 1));
    }

    #[test]
    fn selection_is_direction_agnostic() {
        let forward = Selection::new((1, 0), (3, 1), false);
        let backward = Selection::new((3, 1), (1, 0), false);
        for row in 0..2 {
            for col in 0..5 {
                assert_eq!(
                    forward.contains(col, row),
                    backward.contains(col, row),
                    "mismatch at {col},{row}"
                );
            }
        }
    }
}
