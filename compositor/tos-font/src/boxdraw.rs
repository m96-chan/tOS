//! Procedural box drawing, block and braille glyphs.
//!
//! TUI applications lean heavily on U+2500..U+259F, and those characters look
//! wrong the moment a font's idea of a cell disagrees with the terminal's.
//! Drawing them from the cell geometry instead of a font file makes them join
//! up exactly at any size, and means the built-in bitmap face does not need
//! bitmaps for them.

use crate::glyph::{FontMetrics, Glyph, GlyphSource, RasterStyle};

/// Stroke weights, encoded one per direction.
const NONE: u8 = 0;
const HEAVY: u8 = 2;
const DOUBLE: u8 = 3;

/// `0xURDL`: one nibble of stroke weight per direction, for U+2500..U+257F.
#[rustfmt::skip]
const BOX: [u16; 0x80] = [
    // 2500..250F
    0x0101, 0x0202, 0x1010, 0x2020, 0x0101, 0x0202, 0x1010, 0x2020,
    0x0101, 0x0202, 0x1010, 0x2020, 0x0110, 0x0210, 0x0120, 0x0220,
    // 2510..251F
    0x0011, 0x0012, 0x0021, 0x0022, 0x1100, 0x1200, 0x2100, 0x2200,
    0x1001, 0x1002, 0x2001, 0x2002, 0x1110, 0x1210, 0x2110, 0x1120,
    // 2520..252F
    0x2120, 0x2210, 0x1220, 0x2220, 0x1011, 0x1012, 0x2011, 0x1021,
    0x2021, 0x2012, 0x1022, 0x2022, 0x0111, 0x0112, 0x0211, 0x0212,
    // 2530..253F
    0x0121, 0x0122, 0x0221, 0x0222, 0x1101, 0x1102, 0x1201, 0x1202,
    0x2101, 0x2102, 0x2201, 0x2202, 0x1111, 0x1112, 0x1211, 0x1212,
    // 2540..254F
    0x2111, 0x1121, 0x2121, 0x2112, 0x2211, 0x1122, 0x1221, 0x2212,
    0x1222, 0x2122, 0x2221, 0x2222, 0x0101, 0x0202, 0x1010, 0x2020,
    // 2550..255F
    0x0303, 0x3030, 0x0310, 0x0130, 0x0330, 0x0013, 0x0031, 0x0033,
    0x1300, 0x3100, 0x3300, 0x1003, 0x3001, 0x3003, 0x1310, 0x3130,
    // 2560..256F
    0x3330, 0x1013, 0x3031, 0x3033, 0x0313, 0x0131, 0x0333, 0x1303,
    0x3101, 0x3303, 0x1313, 0x3131, 0x3333, 0x0000, 0x0000, 0x0000,
    // 2570..257F: arcs and diagonals are drawn separately, halves follow
    0x0000, 0x0000, 0x0000, 0x0000, 0x0001, 0x1000, 0x0100, 0x0010,
    0x0002, 0x2000, 0x0200, 0x0020, 0x0201, 0x1020, 0x0102, 0x2010,
];

/// A grayscale drawing surface the size of one cell.
struct Canvas {
    width: u32,
    height: u32,
    data: Vec<u8>,
}

impl Canvas {
    fn new(width: u32, height: u32) -> Self {
        Canvas {
            width,
            height,
            data: vec![0; (width * height) as usize],
        }
    }

    fn set(&mut self, x: i64, y: i64, value: u8) {
        if x < 0 || y < 0 || x >= self.width as i64 || y >= self.height as i64 {
            return;
        }
        let slot = &mut self.data[(y as u32 * self.width + x as u32) as usize];
        *slot = (*slot).max(value);
    }

    fn fill(&mut self, x: i64, y: i64, w: i64, h: i64, value: u8) {
        for dy in 0..h {
            for dx in 0..w {
                self.set(x + dx, y + dy, value);
            }
        }
    }

    /// A straight line between two points, with no anti-aliasing so that
    /// adjacent cells always join cleanly.
    fn line(&mut self, x0: i64, y0: i64, x1: i64, y1: i64, thickness: i64) {
        let dx = (x1 - x0).abs();
        let dy = -(y1 - y0).abs();
        let sx = if x0 < x1 { 1 } else { -1 };
        let sy = if y0 < y1 { 1 } else { -1 };
        let mut err = dx + dy;
        let (mut x, mut y) = (x0, y0);
        loop {
            self.fill(x, y, thickness, thickness, 0xff);
            if x == x1 && y == y1 {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x += sx;
            }
            if e2 <= dx {
                err += dx;
                y += sy;
            }
        }
    }
}

/// Draws the procedural glyph ranges.
#[derive(Debug, Clone)]
pub struct BoxDrawing {
    metrics: FontMetrics,
}

impl BoxDrawing {
    pub fn new(metrics: FontMetrics) -> Self {
        BoxDrawing { metrics }
    }

    pub fn set_metrics(&mut self, metrics: FontMetrics) {
        self.metrics = metrics;
    }

    /// Whether this module draws `c` itself.
    pub fn covers(c: char) -> bool {
        matches!(c as u32,
            0x2500..=0x259f | 0x2800..=0x28ff | 0xe0b0..=0xe0b3)
    }

    fn light(&self) -> i64 {
        (self.metrics.cell_height / 12).max(1) as i64
    }

    fn stroke(&self, weight: u8) -> i64 {
        match weight {
            HEAVY => self.light() * 2,
            _ => self.light(),
        }
    }

    fn draw_box(&self, c: char) -> Canvas {
        let (w, h) = (self.metrics.cell_width as i64, self.metrics.cell_height as i64);
        let mut canvas = Canvas::new(self.metrics.cell_width, self.metrics.cell_height);
        let cp = c as u32;

        match cp {
            // Rounded corners.
            0x256d..=0x2570 => {
                self.draw_arc(&mut canvas, cp);
                return canvas;
            }
            // Diagonals.
            0x2571 => {
                canvas.line(w - 1, 0, 0, h - 1, self.light());
                return canvas;
            }
            0x2572 => {
                canvas.line(0, 0, w - 1, h - 1, self.light());
                return canvas;
            }
            0x2573 => {
                canvas.line(0, 0, w - 1, h - 1, self.light());
                canvas.line(w - 1, 0, 0, h - 1, self.light());
                return canvas;
            }
            _ => {}
        }

        let entry = BOX[(cp - 0x2500) as usize];
        let up = ((entry >> 12) & 0xf) as u8;
        let right = ((entry >> 8) & 0xf) as u8;
        let down = ((entry >> 4) & 0xf) as u8;
        let left = (entry & 0xf) as u8;

        let cx = (w - self.light()) / 2;
        let cy = (h - self.light()) / 2;
        let gap = self.light();

        // Each arm runs from the edge to the centre, so neighbouring cells meet.
        let mut arm = |weight: u8, dir: usize| {
            if weight == NONE {
                return;
            }
            let t = self.stroke(weight);
            if weight == DOUBLE {
                // Two light strokes straddling the centre line.
                for offset in [-gap, gap] {
                    match dir {
                        0 => canvas.fill(cx + offset, 0, gap, cy + gap + offset.max(0), 0xff),
                        1 => canvas.fill(cx - offset.min(0), cy + offset, w - cx, gap, 0xff),
                        2 => canvas.fill(cx + offset, cy - offset.min(0), gap, h - cy, 0xff),
                        _ => canvas.fill(0, cy + offset, cx + gap + offset.max(0), gap, 0xff),
                    }
                }
                return;
            }
            // Centre the stroke on the cell centre.
            let ox = cx - (t - self.light()) / 2;
            let oy = cy - (t - self.light()) / 2;
            match dir {
                0 => canvas.fill(ox, 0, t, cy + t, 0xff),
                1 => canvas.fill(cx, oy, w - cx, t, 0xff),
                2 => canvas.fill(ox, cy, t, h - cy, 0xff),
                _ => canvas.fill(0, oy, cx + t, t, 0xff),
            }
        };

        arm(up, 0);
        arm(right, 1);
        arm(down, 2);
        arm(left, 3);
        canvas
    }

    /// Quarter circle corners, U+256D..U+2570.
    fn draw_arc(&self, canvas: &mut Canvas, cp: u32) {
        let (w, h) = (self.metrics.cell_width as i64, self.metrics.cell_height as i64);
        let t = self.light();
        let cx = (w - t) / 2;
        let cy = (h - t) / 2;
        let radius = cx.min(cy).max(1);

        // Which way the two arms leave the cell.
        let (horizontal_right, vertical_down) = match cp {
            0x256d => (true, true),   // down and right
            0x256e => (false, true),  // down and left
            0x256f => (false, false), // up and left
            _ => (true, false),       // up and right
        };

        // Straight parts beyond the arc.
        if horizontal_right {
            canvas.fill(cx + radius, cy, w - cx - radius, t, 0xff);
        } else {
            canvas.fill(0, cy, cx - radius + t, t, 0xff);
        }
        if vertical_down {
            canvas.fill(cx, cy + radius, t, h - cy - radius, 0xff);
        } else {
            canvas.fill(cx, 0, t, cy - radius + t, 0xff);
        }

        // The arc itself, stepped in integer positions.
        let steps = (radius * 4).max(8);
        for i in 0..=steps {
            let angle = std::f64::consts::FRAC_PI_2 * i as f64 / steps as f64;
            let dx = (radius as f64 * angle.cos()).round() as i64;
            let dy = (radius as f64 * angle.sin()).round() as i64;
            let x = if horizontal_right { cx + dx } else { cx - dx };
            let y = if vertical_down { cy + dy } else { cy - dy };
            canvas.fill(x, y, t, t, 0xff);
        }
    }

    fn draw_block(&self, c: char) -> Canvas {
        let (w, h) = (self.metrics.cell_width as i64, self.metrics.cell_height as i64);
        let mut canvas = Canvas::new(self.metrics.cell_width, self.metrics.cell_height);
        let eighth_h = |n: i64| (h * n + 4) / 8;
        let eighth_w = |n: i64| (w * n + 4) / 8;

        match c as u32 {
            0x2580 => canvas.fill(0, 0, w, h / 2, 0xff),
            cp @ 0x2581..=0x2588 => {
                let n = (cp - 0x2580) as i64;
                let filled = eighth_h(n);
                canvas.fill(0, h - filled, w, filled, 0xff);
            }
            cp @ 0x2589..=0x258f => {
                // 2589 is seven eighths wide, 258f is one eighth.
                let n = 8 - (cp - 0x2588) as i64;
                canvas.fill(0, 0, eighth_w(n), h, 0xff);
            }
            0x2590 => canvas.fill(w - w / 2, 0, w / 2, h, 0xff),
            0x2591 => canvas.fill(0, 0, w, h, 0x40),
            0x2592 => canvas.fill(0, 0, w, h, 0x80),
            0x2593 => canvas.fill(0, 0, w, h, 0xc0),
            0x2594 => canvas.fill(0, 0, w, eighth_h(1), 0xff),
            0x2595 => canvas.fill(w - eighth_w(1), 0, eighth_w(1), h, 0xff),
            cp @ 0x2596..=0x259f => {
                // Bits: upper left, upper right, lower left, lower right.
                const QUADRANTS: [u8; 10] =
                    [0b0100, 0b1000, 0b0001, 0b1101, 0b1001, 0b0111, 0b1011, 0b0010, 0b0110, 0b1110];
                let mask = QUADRANTS[(cp - 0x2596) as usize];
                let (hw, hh) = (w / 2, h / 2);
                if mask & 0b0001 != 0 {
                    canvas.fill(0, 0, hw, hh, 0xff);
                }
                if mask & 0b0010 != 0 {
                    canvas.fill(hw, 0, w - hw, hh, 0xff);
                }
                if mask & 0b0100 != 0 {
                    canvas.fill(0, hh, hw, h - hh, 0xff);
                }
                if mask & 0b1000 != 0 {
                    canvas.fill(hw, hh, w - hw, h - hh, 0xff);
                }
            }
            _ => {}
        }
        canvas
    }

    /// Braille patterns: a 2x4 dot matrix in the low eight bits.
    fn draw_braille(&self, c: char) -> Canvas {
        let (w, h) = (self.metrics.cell_width as i64, self.metrics.cell_height as i64);
        let mut canvas = Canvas::new(self.metrics.cell_width, self.metrics.cell_height);
        let bits = (c as u32 - 0x2800) as u8;
        let dot = (w / 4).max(1).min((h / 8).max(1));

        // Dot order is 1,2,3,7 down the left column then 4,5,6,8 down the right.
        const POSITIONS: [(i64, i64); 8] = [
            (0, 0), (0, 1), (0, 2), (1, 0), (1, 1), (1, 2), (0, 3), (1, 3),
        ];
        for (i, (col, row)) in POSITIONS.iter().enumerate() {
            if bits & (1 << i) == 0 {
                continue;
            }
            let x = w * (1 + 2 * col) / 4 - dot / 2;
            let y = h * (1 + 2 * row) / 8 - dot / 2;
            canvas.fill(x, y, dot, dot, 0xff);
        }
        canvas
    }

    /// Powerline separators, U+E0B0..U+E0B3.
    fn draw_powerline(&self, c: char) -> Canvas {
        let (w, h) = (self.metrics.cell_width as i64, self.metrics.cell_height as i64);
        let mut canvas = Canvas::new(self.metrics.cell_width, self.metrics.cell_height);
        let t = self.light();
        match c as u32 {
            // Filled triangles.
            0xe0b0 | 0xe0b2 => {
                let pointing_right = c as u32 == 0xe0b0;
                for y in 0..h {
                    // Width of the filled span narrows toward the tip.
                    let distance = (y - h / 2).abs();
                    let span = w - distance * 2 * w / h;
                    if span <= 0 {
                        continue;
                    }
                    if pointing_right {
                        canvas.fill(0, y, span, 1, 0xff);
                    } else {
                        canvas.fill(w - span, y, span, 1, 0xff);
                    }
                }
            }
            // Outlined chevrons.
            _ => {
                let pointing_right = c as u32 == 0xe0b1;
                if pointing_right {
                    canvas.line(0, 0, w - 1, h / 2, t);
                    canvas.line(w - 1, h / 2, 0, h - 1, t);
                } else {
                    canvas.line(w - 1, 0, 0, h / 2, t);
                    canvas.line(0, h / 2, w - 1, h - 1, t);
                }
            }
        }
        canvas
    }
}

impl GlyphSource for BoxDrawing {
    fn metrics(&self) -> FontMetrics {
        self.metrics
    }

    fn has_glyph(&self, c: char) -> bool {
        BoxDrawing::covers(c)
    }

    fn rasterize(&mut self, c: char, _style: RasterStyle) -> Option<Glyph> {
        if !BoxDrawing::covers(c) {
            return None;
        }
        let canvas = match c as u32 {
            0x2500..=0x257f => self.draw_box(c),
            0x2580..=0x259f => self.draw_block(c),
            0x2800..=0x28ff => self.draw_braille(c),
            _ => self.draw_powerline(c),
        };
        Some(Glyph {
            width: canvas.width,
            height: canvas.height,
            left: 0,
            // Procedural glyphs fill the cell, so the top is the whole ascent.
            top: self.metrics.baseline as i32,
            coverage: canvas.data,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics() -> FontMetrics {
        FontMetrics {
            cell_width: 12,
            cell_height: 24,
            baseline: 18,
            underline_position: 21,
            underline_thickness: 2,
            strikeout_position: 11,
        }
    }

    fn render(c: char) -> Glyph {
        BoxDrawing::new(metrics())
            .rasterize(c, RasterStyle::REGULAR)
            .unwrap()
    }

    #[test]
    fn covers_the_expected_ranges() {
        assert!(BoxDrawing::covers('─'));
        assert!(BoxDrawing::covers('█'));
        assert!(BoxDrawing::covers('⠿'));
        assert!(!BoxDrawing::covers('a'));
    }

    #[test]
    fn horizontal_line_spans_the_whole_cell() {
        let g = render('─');
        let mid = metrics().cell_height / 2;
        // Every column on the centre row is inked, so neighbours join up.
        for x in 0..g.width {
            assert!(
                (mid - 1..=mid + 1).any(|y| g.coverage_at(x, y) != 0),
                "gap at column {x}"
            );
        }
    }

    #[test]
    fn vertical_line_spans_the_whole_cell() {
        let g = render('│');
        let mid = metrics().cell_width / 2;
        for y in 0..g.height {
            assert!((mid - 1..=mid + 1).any(|x| g.coverage_at(x, y) != 0), "gap at row {y}");
        }
    }

    #[test]
    fn corner_only_inks_two_arms() {
        let g = render('┌');
        let (w, h) = (g.width, g.height);
        let inked = |x: u32, y: u32| g.coverage_at(x, y) != 0;
        // Right and down are drawn; left and up are not.
        assert!((0..h).any(|y| inked(w - 1, y)), "right arm missing");
        assert!((0..w).any(|x| inked(x, h - 1)), "down arm missing");
        assert!(!(0..h).any(|y| inked(0, y)), "left arm should be absent");
        assert!(!(0..w).any(|x| inked(x, 0)), "up arm should be absent");
    }

    #[test]
    fn heavy_lines_are_thicker_than_light() {
        let light = render('─');
        let heavy = render('━');
        let ink = |g: &Glyph| g.coverage.iter().filter(|&&v| v != 0).count();
        assert!(ink(&heavy) > ink(&light));
    }

    #[test]
    fn full_block_fills_everything() {
        let g = render('█');
        assert!(g.coverage.iter().all(|&v| v == 0xff));
    }

    #[test]
    fn half_blocks_fill_half() {
        let g = render('▀');
        let total = (g.width * g.height) as usize;
        let inked = g.coverage.iter().filter(|&&v| v != 0).count();
        assert_eq!(inked, total / 2);
    }

    #[test]
    fn shades_are_partially_transparent() {
        assert!(render('░').coverage.iter().all(|&v| v == 0x40));
        assert!(render('▒').coverage.iter().all(|&v| v == 0x80));
    }

    #[test]
    fn braille_dot_count_matches_the_codepoint() {
        // U+2800 is blank, U+28FF has all eight dots.
        assert!(render('\u{2800}').coverage.iter().all(|&v| v == 0));
        let full = render('\u{28ff}');
        assert!(full.coverage.iter().filter(|&&v| v != 0).count() > 0);
        let one = render('\u{2801}');
        let two = render('\u{2803}');
        let ink = |g: &Glyph| g.coverage.iter().filter(|&&v| v != 0).count();
        assert!(ink(&two) > ink(&one));
    }

    #[test]
    fn every_box_codepoint_renders_without_panicking() {
        let mut font = BoxDrawing::new(metrics());
        for cp in 0x2500u32..=0x259f {
            let c = char::from_u32(cp).unwrap();
            assert!(font.rasterize(c, RasterStyle::REGULAR).is_some(), "{c:?}");
        }
        for cp in 0x2800u32..=0x28ff {
            let c = char::from_u32(cp).unwrap();
            assert!(font.rasterize(c, RasterStyle::REGULAR).is_some(), "{c:?}");
        }
    }

    #[test]
    fn odd_cell_sizes_still_join() {
        // Cell sizes are whatever the font gives; drawing must not assume even.
        let odd = FontMetrics {
            cell_width: 7,
            cell_height: 15,
            baseline: 11,
            underline_position: 13,
            underline_thickness: 1,
            strikeout_position: 7,
        };
        let mut font = BoxDrawing::new(odd);
        let g = font.rasterize('┼', RasterStyle::REGULAR).unwrap();
        assert!((0..g.height).any(|y| g.coverage_at(0, y) != 0));
        assert!((0..g.width).any(|x| g.coverage_at(x, 0) != 0));
    }
}
