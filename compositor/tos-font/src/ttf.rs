//! TrueType/OpenType rasterization.
//!
//! The rootfs is expected to carry a real monospace font; this backend uses it
//! when present and lets the built-in bitmap face handle the case where it is
//! not. Faces for bold and italic are optional: when a cut is missing the
//! glyph is synthesized from the regular one, which is what terminals without
//! a full family do anyway.
//!
//! A Latin monospace face is not enough to read Japanese, so there is a second
//! search list for faces that carry kana and kanji. They are loaded as
//! fallbacks and rescaled to the cell the primary defined, because the two
//! faces otherwise disagree about how wide a wide character is.

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
    // Japanese faces that are genuinely fixed pitch, so they can carry the
    // whole terminal on their own. They come after the Latin ones because
    // their Latin cut is narrower than a terminal usually wants; the tOS ISO
    // ships the first of them and is the better for it, since halfwidth is
    // exactly half of fullwidth in these faces and a wide character then lands
    // on two cells with nothing rescaled.
    "/usr/share/fonts/truetype/vlgothic/VL-Gothic-Regular.ttf",
    "/usr/share/fonts/vlgothic/VL-Gothic-Regular.ttf",
    "/usr/share/fonts/truetype/ipafont-gothic/ipag.ttf",
    "/usr/share/fonts/opentype/noto/NotoSansMonoCJKjp-Regular.otf",
];

/// Faces that carry kana and kanji, most specific first.
///
/// These are only ever fallbacks, so a proportional face is acceptable here
/// even though it would be a poor primary: the terminal borrows nothing from
/// it but wide glyphs, and every wide glyph is one em regardless.
const CJK_SEARCH_PATHS: &[&str] = &[
    // What the tOS ISO ships; see iso/mkiso.sh.
    "/usr/share/fonts/truetype/vlgothic/VL-Gothic-Regular.ttf",
    "/usr/share/fonts/vlgothic/VL-Gothic-Regular.ttf",
    "/usr/share/fonts/truetype/ipafont-gothic/ipag.ttf",
    // Debian keeps an alternatives symlink here that points at whichever
    // Japanese gothic face is installed, which covers the ones not listed.
    "/usr/share/fonts/truetype/fonts-japanese-gothic.ttf",
    "/usr/share/fonts/opentype/noto/NotoSansMonoCJKjp-Regular.otf",
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/truetype/droid/DroidSansFallbackFull.ttf",
    "/usr/local/share/fonts/NotoSansCJK-Regular.ttc",
    // Present on developer machines, so nested mode works during development.
    "/System/Library/Fonts/ヒラギノ角ゴシック W3.ttc",
    "/System/Library/Fonts/Hiragino Sans GB.ttc",
];

/// Characters a face must have before it counts as CJK capable: a kanji, a
/// hiragana and a katakana. A face with kanji but no kana exists — some
/// Chinese faces are exactly that — and would still leave Japanese broken.
const CJK_PROBES: [char; 3] = ['漢', 'あ', 'ア'];

/// The character used to measure how wide a face draws a wide cell. Every
/// Japanese face has it and draws it one em wide, which is what `tos-term`'s
/// width table means by two cells.
pub const FULL_WIDTH_PROBE: char = '漢';

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

    /// Find a face with kana and kanji, if the system has one.
    ///
    /// The file is parsed to check its character map rather than trusted for
    /// its name: several of these paths are symlinks that a distribution can
    /// point at anything, and a face without kana is worse than none because
    /// it would silently win over the ones further down the list.
    pub fn find_system_cjk_font() -> Option<PathBuf> {
        CJK_SEARCH_PATHS
            .iter()
            .map(PathBuf::from)
            .filter(|path| path.is_file())
            .find(|path| {
                TtfFont::from_path(path, 16.0)
                    .map(|font| font.covers_cjk())
                    .unwrap_or(false)
            })
    }

    /// Load whichever CJK face the system has, at `px`.
    ///
    /// As with `system`, every candidate is tried: one that does not parse, or
    /// that turns out to have no kana, should not cost the ones below it.
    pub fn system_cjk(px: f32) -> Option<Self> {
        CJK_SEARCH_PATHS
            .iter()
            .map(PathBuf::from)
            .filter(|path| path.is_file())
            .find_map(|path| {
                TtfFont::from_path(path, px)
                    .ok()
                    .filter(TtfFont::covers_cjk)
            })
    }

    /// Whether this face can draw the scripts the width table gives two cells.
    pub fn covers_cjk(&self) -> bool {
        CJK_PROBES.iter().all(|&c| self.has_glyph(c))
    }

    /// Rescale this face so a wide character fills two cells of `cell`.
    ///
    /// A fallback is loaded at the primary's pixel size, but the two faces
    /// rarely agree on how much of the em a glyph uses: a Latin monospace
    /// advance is around 0.6em while a kanji is a full em, so at one pixel
    /// size the kanji comes out a fifth narrower than the two cells the width
    /// table promised, and a line of Japanese looks gappy. Measuring the
    /// advance and scaling by what it is off by closes that gap.
    ///
    /// The two clamps matter more than the target does: a face whose ink is
    /// taller than the primary's would otherwise climb into the row above or
    /// below, and a terminal grid has no room for that. Landing slightly
    /// under two cells is the price of staying inside the row.
    pub fn fit_wide_cell(mut self, cell: FontMetrics) -> Self {
        let Some(font) = self.faces[0].as_ref() else {
            return self;
        };
        let probe = font.metrics(FULL_WIDTH_PROBE, self.px);
        if probe.advance_width <= 0.0 || probe.height == 0 {
            return self;
        }
        let mut scale = (cell.cell_width * 2) as f32 / probe.advance_width;
        // `ymin` is the distance from the baseline to the bitmap's bottom, so
        // this is the ink above the baseline and what is below it.
        let above = probe.height as f32 + probe.ymin as f32;
        if above > 0.0 {
            scale = scale.min(cell.baseline as f32 / above);
        }
        let below = (-probe.ymin) as f32;
        if below > 0.0 {
            scale = scale.min(cell.cell_height.saturating_sub(cell.baseline) as f32 / below);
        }
        if !scale.is_finite() || scale <= 0.0 {
            return self;
        }
        let px = self.px * scale;
        let metrics = derive_metrics(font, px);
        self.px = px;
        self.metrics = metrics;
        self
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

        let glyph = if synthesize.bold {
            glyph.embolden()
        } else {
            glyph
        };
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

    /// The same for the CJK half: a machine with no Japanese face skips.
    fn cjk_font() -> Option<TtfFont> {
        TtfFont::system_cjk(16.0)
    }

    /// A cell shaped like the one a 16px Latin monospace face produces.
    fn latin_cell() -> FontMetrics {
        FontMetrics {
            cell_width: 10,
            cell_height: 19,
            baseline: 15,
            underline_position: 16,
            underline_thickness: 1,
            strikeout_position: 10,
        }
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
        let Some(mut font) = system_font() else {
            return;
        };
        let g = font.rasterize('A', RasterStyle::REGULAR).unwrap();
        assert!(g.width > 0 && g.height > 0);
        assert!(g.coverage.iter().any(|&v| v != 0));
    }

    #[test]
    fn synthetic_bold_is_wider() {
        let Some(mut font) = system_font() else {
            return;
        };
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

    #[test]
    fn a_cjk_face_has_kana_and_kanji() {
        let Some(font) = cjk_font() else { return };
        assert!(font.covers_cjk());
        assert!(font.has_glyph('日'));
        assert!(font.has_glyph('ー'));
    }

    #[test]
    fn the_cjk_search_finds_the_same_face_twice() {
        let Some(path) = TtfFont::find_system_cjk_font() else {
            return;
        };
        assert!(path.is_file());
        assert!(TtfFont::from_path(&path, 16.0).unwrap().covers_cjk());
        assert!(
            cjk_font().is_some(),
            "the loader must agree with the search"
        );
    }

    #[test]
    fn rasterizes_japanese() {
        let Some(mut font) = cjk_font() else { return };
        for c in ['漢', 'あ', 'ア', '日', '本'] {
            let g = font.rasterize(c, RasterStyle::REGULAR).unwrap();
            assert!(g.width > 0 && g.height > 0, "{c} rasterized to nothing");
            assert!(g.coverage.iter().any(|&v| v != 0), "{c} has no ink");
        }
    }

    #[test]
    fn fitting_widens_a_wide_glyph_towards_two_cells() {
        let Some(font) = cjk_font() else { return };
        let cell = latin_cell();
        let before = font.faces[0]
            .as_ref()
            .unwrap()
            .metrics(FULL_WIDTH_PROBE, font.px)
            .advance_width;
        let mut fitted = font.fit_wide_cell(cell);
        let after = fitted
            .rasterize(FULL_WIDTH_PROBE, RasterStyle::REGULAR)
            .unwrap();
        // A Latin monospace cell is about 0.6em, so two of them are wider than
        // the em the face drew the kanji in and the fit scales up.
        assert!(before < (cell.cell_width * 2) as f32);
        assert!(
            after.width > cell.cell_width,
            "a kanji must outgrow one cell"
        );
    }

    #[test]
    fn a_fitted_glyph_stays_inside_its_two_cells() {
        let Some(font) = cjk_font() else { return };
        let cell = latin_cell();
        let mut fitted = font.fit_wide_cell(cell);
        for c in ['漢', 'あ', 'ア', '髙', '＠'] {
            let Some(g) = fitted.rasterize(c, RasterStyle::REGULAR) else {
                continue;
            };
            assert!(
                g.left + g.width as i32 <= (cell.cell_width * 2) as i32,
                "{c} spills past the second cell"
            );
            assert!(
                g.top <= cell.baseline as i32,
                "{c} climbs into the row above"
            );
            assert!(
                g.height as i32 - g.top <= (cell.cell_height - cell.baseline) as i32,
                "{c} hangs below the row"
            );
        }
    }

    #[test]
    fn fitting_hits_the_target_when_the_row_is_tall() {
        let Some(font) = cjk_font() else { return };
        // Room to spare above and below the baseline, so only the width
        // target is in play and the kanji should nearly fill two cells.
        let cell = FontMetrics {
            cell_width: 20,
            cell_height: 60,
            baseline: 48,
            underline_position: 50,
            underline_thickness: 2,
            strikeout_position: 30,
        };
        let mut fitted = font.fit_wide_cell(cell);
        let g = fitted
            .rasterize(FULL_WIDTH_PROBE, RasterStyle::REGULAR)
            .unwrap();
        // The ink is inset from the em box, so this asks for most of the two
        // cells rather than all of them.
        assert!(g.width >= cell.cell_width * 3 / 2, "{} too narrow", g.width);
        assert!(g.width <= cell.cell_width * 2, "{} too wide", g.width);
    }

    #[test]
    fn fitting_shrinks_into_a_short_row() {
        let Some(font) = cjk_font() else { return };
        // Two cells of room across but almost none above the baseline: the
        // height clamp has to win, or the kanji would overdraw its neighbours.
        let cell = FontMetrics {
            cell_width: 20,
            cell_height: 12,
            baseline: 10,
            underline_position: 11,
            underline_thickness: 1,
            strikeout_position: 6,
        };
        let mut fitted = font.fit_wide_cell(cell);
        assert!(fitted.pixel_size() < 20.0);
        let g = fitted
            .rasterize(FULL_WIDTH_PROBE, RasterStyle::REGULAR)
            .unwrap();
        assert!(g.top <= cell.baseline as i32);
        assert!(g.height <= cell.cell_height);
    }
}
