//! TrueType/OpenType rasterization.
//!
//! The rootfs is expected to carry a real monospace font; this backend uses it
//! when present and lets the built-in bitmap face handle the case where it is
//! not. Faces for bold and italic are optional: when a cut is missing the
//! glyph is synthesized from the regular one, which is what terminals without
//! a full family do anyway.

use std::path::{Path, PathBuf};

use fontdue::{Font, FontSettings};

use crate::glyph::{FontMetrics, Glyph, GlyphSource, RasterStyle};

/// Places a Linux rootfs usually keeps monospace fonts, most specific first.
const FONT_SEARCH_PATHS: &[&str] = &[
    "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationMono-Regular.ttf",
    "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
    "/usr/share/fonts/dejavu/DejaVuSansMono.ttf",
    "/usr/share/fonts/liberation-mono/LiberationMono-Regular.ttf",
    "/usr/share/fonts/noto/NotoSansMono-Regular.ttf",
    "/usr/local/share/fonts/DejaVuSansMono.ttf",
    // Present on developer machines, so nested mode works during development.
    "/System/Library/Fonts/Menlo.ttc",
    "/System/Library/Fonts/SFNSMono.ttf",
];

fn style_index(style: RasterStyle) -> usize {
    match (style.bold, style.italic) {
        (false, false) => 0,
        (true, false) => 1,
        (false, true) => 2,
        (true, true) => 3,
    }
}

/// A scalable font family.
pub struct TtfFont {
    /// Regular, bold, italic, bold italic. Index 0 is always present.
    faces: [Option<Font>; 4],
    px: f32,
    metrics: FontMetrics,
}

impl TtfFont {
    /// Load a regular face from memory and derive cell metrics at `px`.
    pub fn from_bytes(data: &[u8], px: f32) -> Result<Self, String> {
        let settings = FontSettings {
            scale: px,
            ..FontSettings::default()
        };
        let font = Font::from_bytes(data, settings)?;
        let metrics = derive_metrics(&font, px);
        Ok(TtfFont {
            faces: [Some(font), None, None, None],
            px,
            metrics,
        })
    }

    pub fn from_path(path: impl AsRef<Path>, px: f32) -> Result<Self, String> {
        let data = std::fs::read(path.as_ref())
            .map_err(|e| format!("{}: {e}", path.as_ref().display()))?;
        TtfFont::from_bytes(&data, px)
    }

    /// Add a bold, italic or bold italic cut.
    pub fn with_face(mut self, style: RasterStyle, data: &[u8]) -> Result<Self, String> {
        let settings = FontSettings {
            scale: self.px,
            ..FontSettings::default()
        };
        self.faces[style_index(style)] = Some(Font::from_bytes(data, settings)?);
        Ok(self)
    }

    /// Find a monospace font on this system, if there is one.
    pub fn find_system_font() -> Option<PathBuf> {
        FONT_SEARCH_PATHS
            .iter()
            .map(PathBuf::from)
            .find(|p| p.is_file())
    }

    /// Load whichever monospace font the system has.
    ///
    /// Every candidate is tried: a font file that exists but does not parse
    /// should not cost the user the ones further down the list.
    pub fn system(px: f32) -> Option<Self> {
        FONT_SEARCH_PATHS
            .iter()
            .map(PathBuf::from)
            .filter(|path| path.is_file())
            .find_map(|path| TtfFont::from_path(path, px).ok())
    }

    pub fn pixel_size(&self) -> f32 {
        self.px
    }

    fn face(&self, style: RasterStyle) -> (&Font, RasterStyle) {
        let wanted = style_index(style);
        if let Some(font) = &self.faces[wanted] {
            return (font, RasterStyle::REGULAR);
        }
        // Fall back to the regular cut and record what has to be synthesized.
        let regular = self.faces[0].as_ref().expect("regular face is required");
        (regular, style)
    }
}

/// Derive terminal cell metrics from a font's own line metrics.
fn derive_metrics(font: &Font, px: f32) -> FontMetrics {
    let line = font.horizontal_line_metrics(px);
    let (ascent, descent) = match line {
        Some(line) => (line.ascent, -line.descent),
        None => (px * 0.8, px * 0.2),
    };
    let cell_height = (ascent + descent).ceil().max(1.0) as u32;
    // Monospace advance: 'M' is the conventional probe.
    let advance = font.metrics('M', px).advance_width;
    let cell_width = advance.ceil().max(1.0) as u32;
    let baseline = ascent.ceil().max(1.0) as u32;
    let thickness = (px / 14.0).round().max(1.0) as u32;

    FontMetrics {
        cell_width,
        cell_height,
        baseline,
        underline_position: (baseline + thickness).min(cell_height - 1),
        underline_thickness: thickness,
        strikeout_position: baseline - (ascent * 0.3).round() as u32,
    }
}

impl GlyphSource for TtfFont {
    fn metrics(&self) -> FontMetrics {
        self.metrics
    }

    fn has_glyph(&self, c: char) -> bool {
        self.faces[0]
            .as_ref()
            .map(|f| f.lookup_glyph_index(c) != 0)
            .unwrap_or(false)
    }

    fn rasterize(&mut self, c: char, style: RasterStyle) -> Option<Glyph> {
        let px = self.px;
        let (font, synthesize) = self.face(style);
        if font.lookup_glyph_index(c) == 0 && c != ' ' {
            return None;
        }
        let (metrics, coverage) = font.rasterize(c, px);
        let glyph = Glyph {
            width: metrics.width as u32,
            height: metrics.height as u32,
            left: metrics.xmin,
            // `ymin` is the distance from the baseline to the bitmap's bottom.
            top: metrics.height as i32 + metrics.ymin,
            coverage,
        };

        let glyph = if synthesize.bold { glyph.embolden() } else { glyph };
        let glyph = if synthesize.italic {
            glyph.slant((px / 8.0).round().max(1.0) as u32)
        } else {
            glyph
        };
        Some(glyph)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests need a real font; skip cleanly on machines without one.
    fn system_font() -> Option<TtfFont> {
        TtfFont::system(16.0)
    }

    #[test]
    fn derives_sane_cell_metrics() {
        let Some(font) = system_font() else { return };
        let m = font.metrics();
        assert!(m.cell_width > 0 && m.cell_height > 0);
        assert!(m.baseline > 0 && m.baseline < m.cell_height);
        assert!(m.underline_position < m.cell_height);
    }

    #[test]
    fn rasterizes_ascii() {
        let Some(mut font) = system_font() else { return };
        let g = font.rasterize('A', RasterStyle::REGULAR).unwrap();
        assert!(g.width > 0 && g.height > 0);
        assert!(g.coverage.iter().any(|&v| v != 0));
    }

    #[test]
    fn synthetic_bold_is_wider() {
        let Some(mut font) = system_font() else { return };
        let regular = font.rasterize('l', RasterStyle::REGULAR).unwrap();
        let bold = font.rasterize('l', RasterStyle::new(true, false)).unwrap();
        assert!(bold.width > regular.width);
    }

    #[test]
    fn missing_glyphs_report_absent() {
        let Some(font) = system_font() else { return };
        // A private use codepoint that no standard font covers.
        assert!(!font.has_glyph('\u{f8ff0}'));
    }
}
