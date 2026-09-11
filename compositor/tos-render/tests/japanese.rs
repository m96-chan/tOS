//! Japanese on the grid: a wide character has to cover both of its cells.
//!
//! `tos-term`'s width table gives a kanji two columns, so the renderer owes it
//! two columns of ink. The hollow box that a missing glyph draws is one cell
//! wide, which is exactly how this used to fail; these tests tell the two
//! apart by looking at the second cell.

use tos_font::{FontStack, TtfFont};
use tos_render::{render, OwnedFramebuffer, Rect, RenderOptions, TextureCache};
use tos_term::{Terminal, TerminalConfig};

const PX: f32 = 16.0;

/// Build a stack the way the compositor does: a monospace primary plus a CJK
/// face fitted to the cell it defines. `None` on a machine with no fonts,
/// which is where this test has nothing to say.
fn japanese_stack() -> Option<FontStack> {
    let mut stack = FontStack::new(Box::new(TtfFont::system(PX)?));
    let cell = stack.metrics();
    if !stack.covers('漢') {
        stack.push_fallback(Box::new(TtfFont::system_cjk(PX)?.fit_wide_cell(cell)));
    }
    Some(stack)
}

struct Harness {
    fb: OwnedFramebuffer,
    fonts: FontStack,
    term: Terminal,
    textures: TextureCache,
}

impl Harness {
    fn new(fonts: FontStack, cols: usize, rows: usize) -> Self {
        let metrics = fonts.metrics();
        let fb = OwnedFramebuffer::new(
            metrics.cell_width * cols as u32,
            metrics.cell_height * rows as u32,
        );
        let config = TerminalConfig {
            cell_width: metrics.cell_width,
            cell_height: metrics.cell_height,
            ..TerminalConfig::default()
        };
        Harness {
            fb,
            fonts,
            term: Terminal::new(cols, rows, config),
            textures: TextureCache::default(),
        }
    }

    fn draw(&mut self, text: &str) -> &mut Self {
        self.term.advance(text.as_bytes());
        let area = Rect::new(0, 0, self.fb.width(), self.fb.height());
        let mut surface = self.fb.surface();
        render(
            &mut surface,
            area,
            &self.term,
            &mut self.fonts,
            &mut self.textures,
            // The cursor would count as ink, and it sits on the cell after the
            // text, which is one of the cells these tests read.
            &RenderOptions {
                force: true,
                draw_cursor: false,
                ..RenderOptions::default()
            },
        );
        self
    }

    /// Pixels in one cell that are not the background.
    fn ink(&self, col: usize, row: usize) -> usize {
        let metrics = self.fonts.metrics();
        let background = self.term.palette().background.pack();
        let mut count = 0;
        for y in 0..metrics.cell_height {
            for x in 0..metrics.cell_width {
                let px = self.fb.pixel(
                    col as u32 * metrics.cell_width + x,
                    row as u32 * metrics.cell_height + y,
                );
                if px != background {
                    count += 1;
                }
            }
        }
        count
    }
}

#[test]
fn a_kanji_inks_both_of_its_cells() {
    let Some(fonts) = japanese_stack() else {
        return;
    };
    let mut h = Harness::new(fonts, 10, 2);
    h.draw("日");
    assert!(h.ink(0, 0) > 0, "the first cell is blank");
    assert!(
        h.ink(1, 0) > 0,
        "the second cell is blank: a one cell box, not a kanji"
    );
}

#[test]
fn a_kanji_does_not_spill_into_the_next_cell() {
    let Some(fonts) = japanese_stack() else {
        return;
    };
    let mut h = Harness::new(fonts, 10, 2);
    h.draw("日");
    assert_eq!(h.ink(2, 0), 0, "the glyph overran its two cells");
    assert_eq!(h.ink(0, 1), 0, "the glyph overran its row");
}

#[test]
fn kana_and_latin_share_a_line() {
    let Some(fonts) = japanese_stack() else {
        return;
    };
    let mut h = Harness::new(fonts, 12, 2);
    // One column of Latin, then two per kana: cells 0, 1-2, 3-4, and 5 clear.
    h.draw("aあい");
    assert!(h.ink(0, 0) > 0);
    assert!(h.ink(1, 0) > 0 && h.ink(2, 0) > 0, "あ lost a cell");
    assert!(h.ink(3, 0) > 0 && h.ink(4, 0) > 0, "い lost a cell");
    assert_eq!(h.ink(5, 0), 0, "the line ran long");
}

#[test]
fn a_kanji_is_not_the_missing_box() {
    let Some(fonts) = japanese_stack() else {
        return;
    };
    let boxed = {
        let Some(fonts) = japanese_stack() else {
            return;
        };
        // A private use codepoint no face covers, so this is the hollow box,
        // and the width table gives it a single cell.
        let mut h = Harness::new(fonts, 10, 2);
        h.draw("\u{f8ff0}");
        (h.ink(0, 0), h.ink(1, 0))
    };
    let mut h = Harness::new(fonts, 10, 2);
    h.draw("日");
    assert_eq!(boxed.1, 0, "the box was expected to occupy one cell");
    assert_ne!(h.ink(0, 0), boxed.0, "the kanji rendered as the box");
}

#[test]
fn a_full_line_of_japanese_reaches_the_last_column() {
    let Some(fonts) = japanese_stack() else {
        return;
    };
    // Twelve wide characters in twenty four columns: the grid should be full
    // to the edge, with nothing wrapped onto the second row.
    let mut h = Harness::new(fonts, 24, 2);
    h.draw("日本語のテキストが読める");
    for col in 0..24 {
        assert!(h.ink(col, 0) > 0, "column {col} is blank");
    }
    for col in 0..24 {
        assert_eq!(h.ink(col, 1), 0, "column {col} wrapped onto the next row");
    }
}
