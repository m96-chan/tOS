//! Glyph representation shared by every font backend.

/// Font geometry, in pixels. The renderer lays the grid out from this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FontMetrics {
    pub cell_width: u32,
    pub cell_height: u32,
    /// Distance from the top of the cell to the baseline.
    pub baseline: u32,
    /// Distance from the top of the cell to the underline.
    pub underline_position: u32,
    pub underline_thickness: u32,
    /// Distance from the top of the cell to the strikeout line.
    pub strikeout_position: u32,
}

impl FontMetrics {
    /// Scale metrics by an integer factor, used by the bitmap font.
    pub fn scaled(self, factor: u32) -> FontMetrics {
        let f = factor.max(1);
        FontMetrics {
            cell_width: self.cell_width * f,
            cell_height: self.cell_height * f,
            baseline: self.baseline * f,
            underline_position: self.underline_position * f,
            underline_thickness: self.underline_thickness * f,
            strikeout_position: self.strikeout_position * f,
        }
    }
}

/// Which face to rasterize with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct RasterStyle {
    pub bold: bool,
    pub italic: bool,
}

impl RasterStyle {
    pub const REGULAR: RasterStyle = RasterStyle {
        bold: false,
        italic: false,
    };

    pub fn new(bold: bool, italic: bool) -> Self {
        RasterStyle { bold, italic }
    }
}

/// A rasterized glyph: an 8 bit coverage mask plus its placement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Glyph {
    pub width: u32,
    pub height: u32,
    /// Horizontal offset from the cell's left edge.
    pub left: i32,
    /// Vertical offset from the baseline, positive upwards.
    pub top: i32,
    /// `width * height` coverage values.
    pub coverage: Vec<u8>,
}

impl Glyph {
    pub fn empty() -> Self {
        Glyph {
            width: 0,
            height: 0,
            left: 0,
            top: 0,
            coverage: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    pub fn coverage_at(&self, x: u32, y: u32) -> u8 {
        if x >= self.width || y >= self.height {
            return 0;
        }
        self.coverage[(y * self.width + x) as usize]
    }

    /// Smear horizontally by one pixel to fake a bold face.
    pub fn embolden(&self) -> Glyph {
        if self.is_empty() {
            return self.clone();
        }
        let width = self.width + 1;
        let mut coverage = vec![0u8; (width * self.height) as usize];
        for y in 0..self.height {
            for x in 0..self.width {
                let v = self.coverage_at(x, y);
                let a = &mut coverage[(y * width + x) as usize];
                *a = (*a).max(v);
                let b = &mut coverage[(y * width + x + 1) as usize];
                *b = (*b).max(v);
            }
        }
        Glyph {
            width,
            height: self.height,
            left: self.left,
            top: self.top,
            coverage,
        }
    }

    /// Shear to fake an italic face. `slant` is the shift in pixels at the top.
    pub fn slant(&self, slant: u32) -> Glyph {
        if self.is_empty() || slant == 0 {
            return self.clone();
        }
        let width = self.width + slant;
        let mut coverage = vec![0u8; (width * self.height) as usize];
        for y in 0..self.height {
            // Rows near the top shift right the most; the bottom row does not
            // move at all, which keeps the baseline where the renderer expects.
            let shift = slant * (self.height - 1 - y) / (self.height - 1).max(1);
            for x in 0..self.width {
                let v = self.coverage_at(x, y);
                if v != 0 {
                    coverage[(y * width + x + shift) as usize] = v;
                }
            }
        }
        Glyph {
            width,
            height: self.height,
            left: self.left,
            top: self.top,
            coverage,
        }
    }

    /// Render as ASCII art, which is how the built-in font is reviewed.
    pub fn debug_art(&self) -> String {
        let mut out = String::new();
        for y in 0..self.height {
            for x in 0..self.width {
                out.push(match self.coverage_at(x, y) {
                    0..=63 => ' ',
                    64..=127 => '.',
                    128..=191 => '+',
                    _ => '#',
                });
            }
            out.push('\n');
        }
        out
    }
}

/// A source of glyphs: a font file, the built-in bitmap font, or a procedural
/// generator such as the box drawing renderer.
pub trait GlyphSource {
    fn metrics(&self) -> FontMetrics;
    /// Whether this source can render `c` without falling back.
    fn has_glyph(&self, c: char) -> bool;
    /// Rasterize `c`. Returning `None` means the caller should fall back.
    fn rasterize(&mut self, c: char, style: RasterStyle) -> Option<Glyph>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dot() -> Glyph {
        Glyph {
            width: 1,
            height: 2,
            left: 0,
            top: 2,
            coverage: vec![255, 255],
        }
    }

    #[test]
    fn embolden_widens_by_one() {
        let bold = dot().embolden();
        assert_eq!(bold.width, 2);
        assert_eq!(bold.coverage, vec![255, 255, 255, 255]);
    }

    #[test]
    fn slant_shifts_the_top_row() {
        let italic = dot().slant(1);
        assert_eq!(italic.width, 2);
        // Top row moved right, bottom row stayed.
        assert_eq!(italic.coverage_at(1, 0), 255);
        assert_eq!(italic.coverage_at(0, 1), 255);
    }

    #[test]
    fn metrics_scale_uniformly() {
        let m = FontMetrics {
            cell_width: 6,
            cell_height: 11,
            baseline: 8,
            underline_position: 9,
            underline_thickness: 1,
            strikeout_position: 5,
        };
        let s = m.scaled(2);
        assert_eq!(s.cell_width, 12);
        assert_eq!(s.baseline, 16);
        assert_eq!(s.underline_thickness, 2);
    }
}
