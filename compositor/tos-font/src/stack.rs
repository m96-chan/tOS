//! Font fallback and glyph caching.
//!
//! A terminal needs one cell geometry but many glyph sources: a primary face,
//! fallbacks for scripts it lacks, and the procedural box drawing renderer
//! that always wins because it is the only source guaranteed to line up with
//! the cell grid.

use std::collections::HashMap;

use crate::boxdraw::BoxDrawing;
use crate::glyph::{FontMetrics, Glyph, GlyphSource, RasterStyle};

/// Beyond this many cached glyphs the cache is dropped wholesale; terminals
/// touch a small working set, so this is rare.
const MAX_CACHE: usize = 8192;

/// A primary face plus fallbacks, with a glyph cache.
pub struct FontStack {
    metrics: FontMetrics,
    sources: Vec<Box<dyn GlyphSource>>,
    boxdraw: BoxDrawing,
    cache: HashMap<(char, RasterStyle), Glyph>,
    missing: Glyph,
}

impl FontStack {
    /// Build a stack around a primary source, whose metrics define the cell.
    pub fn new(primary: Box<dyn GlyphSource>) -> Self {
        let metrics = primary.metrics();
        FontStack {
            metrics,
            sources: vec![primary],
            boxdraw: BoxDrawing::new(metrics),
            cache: HashMap::new(),
            missing: missing_glyph(metrics),
        }
    }

    /// Append a fallback, tried after every source already added.
    pub fn push_fallback(&mut self, source: Box<dyn GlyphSource>) {
        self.sources.push(source);
        // Characters already resolved to the missing-glyph box might be
        // covered by the new source, and a cached box would never be revisited.
        self.cache.clear();
    }

    /// Whether some source already draws `c`.
    ///
    /// Asked before loading a fallback: a CJK face is tens of megabytes and
    /// there is no sense parsing one when the primary already has kanji.
    pub fn covers(&self, c: char) -> bool {
        self.sources.iter().any(|source| source.has_glyph(c))
    }

    pub fn metrics(&self) -> FontMetrics {
        self.metrics
    }

    /// Override the cell geometry, for example to add letter spacing.
    pub fn set_metrics(&mut self, metrics: FontMetrics) {
        self.metrics = metrics;
        self.boxdraw.set_metrics(metrics);
        self.missing = missing_glyph(metrics);
        self.cache.clear();
    }

    pub fn cached_glyphs(&self) -> usize {
        self.cache.len()
    }

    /// Rasterize `c`, caching the result. Always returns a glyph: unknown
    /// characters render as a hollow box rather than a hole in the output.
    pub fn glyph(&mut self, c: char, style: RasterStyle) -> &Glyph {
        let key = (c, style);
        if !self.cache.contains_key(&key) {
            if self.cache.len() >= MAX_CACHE {
                self.cache.clear();
            }
            let glyph = self.render(c, style);
            self.cache.insert(key, glyph);
        }
        &self.cache[&key]
    }

    fn render(&mut self, c: char, style: RasterStyle) -> Glyph {
        // Box drawing is always drawn procedurally: a font's version would not
        // meet its neighbours at the cell edge.
        if BoxDrawing::covers(c) {
            if let Some(glyph) = self.boxdraw.rasterize(c, style) {
                return glyph;
            }
        }
        // Sources that claim the glyph come first.
        for source in &mut self.sources {
            if source.has_glyph(c) {
                if let Some(glyph) = source.rasterize(c, style) {
                    return glyph;
                }
            }
        }
        // Then anything that will render it at all.
        for source in &mut self.sources {
            if let Some(glyph) = source.rasterize(c, style) {
                return glyph;
            }
        }
        if c == ' ' {
            return Glyph::empty();
        }
        self.missing.clone()
    }
}

/// The glyph shown for characters no source can render: a hollow box, the
/// same convention other terminals use.
fn missing_glyph(metrics: FontMetrics) -> Glyph {
    let inset_x = (metrics.cell_width / 6).max(1);
    let width = metrics.cell_width.saturating_sub(inset_x * 2).max(1);
    let height = (metrics.baseline).max(1);
    let thickness = (metrics.cell_height / 16).max(1);
    let mut coverage = vec![0u8; (width * height) as usize];
    for y in 0..height {
        for x in 0..width {
            let edge = x < thickness
                || y < thickness
                || x + thickness >= width
                || y + thickness >= height;
            if edge {
                coverage[(y * width + x) as usize] = 0xff;
            }
        }
    }
    Glyph {
        width,
        height,
        left: inset_x as i32,
        top: height as i32,
        coverage,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bitmap::BitmapFont;

    fn stack() -> FontStack {
        FontStack::new(Box::new(BitmapFont::new(2)))
    }

    #[test]
    fn returns_a_glyph_for_ascii() {
        let mut stack = stack();
        let g = stack.glyph('A', RasterStyle::REGULAR);
        assert!(g.coverage.iter().any(|&v| v != 0));
    }

    #[test]
    fn caches_repeated_lookups() {
        let mut stack = stack();
        stack.glyph('A', RasterStyle::REGULAR);
        stack.glyph('A', RasterStyle::REGULAR);
        stack.glyph('B', RasterStyle::REGULAR);
        assert_eq!(stack.cached_glyphs(), 2);
    }

    #[test]
    fn styles_are_cached_separately() {
        let mut stack = stack();
        let plain = stack.glyph('A', RasterStyle::REGULAR).width;
        let bold = stack.glyph('A', RasterStyle::new(true, false)).width;
        assert!(bold > plain);
        assert_eq!(stack.cached_glyphs(), 2);
    }

    #[test]
    fn box_drawing_beats_the_font() {
        // The bitmap face has no box drawing glyphs, so this only works if the
        // procedural renderer is consulted.
        let mut stack = stack();
        let cell_width = stack.metrics().cell_width;
        let g = stack.glyph('┼', RasterStyle::REGULAR);
        assert!(g.coverage.iter().any(|&v| v != 0));
        assert_eq!(g.width, cell_width);
    }

    #[test]
    fn unknown_characters_render_as_a_box() {
        let mut stack = stack();
        let g = stack.glyph('漢', RasterStyle::REGULAR);
        assert!(g.coverage.iter().any(|&v| v != 0), "should draw something");
    }

    #[test]
    fn space_renders_as_nothing() {
        let mut stack = stack();
        assert!(stack.glyph(' ', RasterStyle::REGULAR).coverage.iter().all(|&v| v == 0));
    }

    #[test]
    fn fallbacks_are_consulted_in_order() {
        struct OnlyQ;
        impl GlyphSource for OnlyQ {
            fn metrics(&self) -> FontMetrics {
                BitmapFont::new(2).metrics()
            }
            fn has_glyph(&self, c: char) -> bool {
                c == '漢'
            }
            fn rasterize(&mut self, c: char, _style: RasterStyle) -> Option<Glyph> {
                if c != '漢' {
                    return None;
                }
                Some(Glyph {
                    width: 1,
                    height: 1,
                    left: 0,
                    top: 1,
                    coverage: vec![0x7f],
                })
            }
        }

        let mut stack = stack();
        stack.push_fallback(Box::new(OnlyQ));
        let g = stack.glyph('漢', RasterStyle::REGULAR);
        assert_eq!(g.coverage, vec![0x7f]);
    }

    #[test]
    fn adding_a_fallback_invalidates_the_cache() {
        struct OnlyCjk;
        impl GlyphSource for OnlyCjk {
            fn metrics(&self) -> FontMetrics {
                BitmapFont::new(2).metrics()
            }
            fn has_glyph(&self, c: char) -> bool {
                c == '漢'
            }
            fn rasterize(&mut self, c: char, _style: RasterStyle) -> Option<Glyph> {
                (c == '漢').then(|| Glyph {
                    width: 1,
                    height: 1,
                    left: 0,
                    top: 1,
                    coverage: vec![0x5a],
                })
            }
        }

        let mut stack = stack();
        // Resolved to the missing box while no source covers it.
        let before = stack.glyph('漢', RasterStyle::REGULAR).coverage.clone();
        stack.push_fallback(Box::new(OnlyCjk));
        let after = stack.glyph('漢', RasterStyle::REGULAR).coverage.clone();
        assert_ne!(before, after, "the stale box must not be cached forever");
        assert_eq!(after, vec![0x5a]);
    }

    #[test]
    fn coverage_is_reported_across_every_source() {
        struct OnlyKanji;
        impl GlyphSource for OnlyKanji {
            fn metrics(&self) -> FontMetrics {
                BitmapFont::new(2).metrics()
            }
            fn has_glyph(&self, c: char) -> bool {
                c == '漢'
            }
            fn rasterize(&mut self, _c: char, _style: RasterStyle) -> Option<Glyph> {
                None
            }
        }

        let mut stack = stack();
        assert!(stack.covers('A'), "the bitmap face has ASCII");
        assert!(!stack.covers('漢'), "and nothing else");
        stack.push_fallback(Box::new(OnlyKanji));
        assert!(stack.covers('漢'));
        assert!(stack.covers('A'), "a fallback does not hide the primary");
    }

    #[test]
    fn changing_metrics_invalidates_the_cache() {
        let mut stack = stack();
        stack.glyph('A', RasterStyle::REGULAR);
        assert_eq!(stack.cached_glyphs(), 1);
        let mut metrics = stack.metrics();
        metrics.cell_width += 2;
        stack.set_metrics(metrics);
        assert_eq!(stack.cached_glyphs(), 0);
    }
}
