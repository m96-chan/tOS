//! End to end rendering: bytes in, pixels out.

use tos_font::{BitmapFont, FontStack};
use tos_render::{render, OwnedFramebuffer, Rect, RenderOptions, Selection, TextureCache};
use tos_term::{Terminal, TerminalConfig};

struct Harness {
    fb: OwnedFramebuffer,
    fonts: FontStack,
    term: Terminal,
    textures: TextureCache,
}

impl Harness {
    fn new(cols: usize, rows: usize) -> Self {
        let fonts = FontStack::new(Box::new(BitmapFont::new(1)));
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

    fn feed(&mut self, bytes: &[u8]) -> &mut Self {
        self.term.advance(bytes);
        self
    }

    fn draw(&mut self) -> &mut Self {
        self.draw_with(&RenderOptions {
            force: true,
            blink_visible: true,
            ..RenderOptions::default()
        })
    }

    fn draw_with(&mut self, options: &RenderOptions) -> &mut Self {
        let area = Rect::new(0, 0, self.fb.width(), self.fb.height());
        let mut surface = self.fb.surface();
        render(
            &mut surface,
            area,
            &self.term,
            &mut self.fonts,
            &mut self.textures,
            options,
        );
        self
    }

    /// Ink coverage of one cell, counting pixels that differ from the default
    /// background.
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

    fn cell_pixel(&self, col: usize, row: usize, dx: u32, dy: u32) -> u32 {
        let metrics = self.fonts.metrics();
        self.fb.pixel(
            col as u32 * metrics.cell_width + dx,
            row as u32 * metrics.cell_height + dy,
        )
    }
}

#[test]
fn blank_screen_is_the_background_color() {
    let mut h = Harness::new(8, 2);
    h.draw_with(&RenderOptions {
        force: true,
        draw_cursor: false,
        ..RenderOptions::default()
    });
    let background = h.term.palette().background.pack();
    assert!(h.fb.pixels().iter().all(|&px| px == background));
}

#[test]
fn text_puts_ink_in_the_right_cells() {
    let mut h = Harness::new(8, 2);
    // Cursor drawing would confuse the ink count, so it is turned off.
    h.feed(b"hi").draw_with(&RenderOptions {
        force: true,
        draw_cursor: false,
        ..RenderOptions::default()
    });
    assert!(h.ink(0, 0) > 0, "first cell should be inked");
    assert!(h.ink(1, 0) > 0, "second cell should be inked");
    assert_eq!(h.ink(2, 0), 0, "third cell should be empty");
    assert_eq!(h.ink(0, 1), 0, "second row should be empty");
}

#[test]
fn background_color_fills_the_whole_cell() {
    let mut h = Harness::new(4, 1);
    h.feed(b"\x1b[41m \x1b[0m").draw_with(&RenderOptions {
        force: true,
        draw_cursor: false,
        ..RenderOptions::default()
    });
    let red = h.term.palette().index(1).pack();
    let metrics = h.fonts.metrics();
    for y in 0..metrics.cell_height {
        for x in 0..metrics.cell_width {
            assert_eq!(h.cell_pixel(0, 0, x, y), red, "pixel {x},{y} not filled");
        }
    }
}

#[test]
fn truecolor_reaches_the_framebuffer() {
    let mut h = Harness::new(4, 1);
    h.feed(b"\x1b[48;2;10;20;30m ").draw_with(&RenderOptions {
        force: true,
        draw_cursor: false,
        ..RenderOptions::default()
    });
    assert_eq!(h.cell_pixel(0, 0, 1, 1), 0x0a141e);
}

#[test]
fn reverse_video_swaps_foreground_and_background() {
    let mut h = Harness::new(4, 1);
    h.feed(b"\x1b[7m ").draw_with(&RenderOptions {
        force: true,
        draw_cursor: false,
        ..RenderOptions::default()
    });
    let foreground = h.term.palette().foreground.pack();
    assert_eq!(h.cell_pixel(0, 0, 1, 1), foreground);
}

#[test]
fn blinking_text_disappears_on_the_off_phase() {
    let mut h = Harness::new(4, 1);
    h.feed(b"\x1b[5mX");
    h.draw_with(&RenderOptions {
        force: true,
        draw_cursor: false,
        blink_visible: true,
        ..RenderOptions::default()
    });
    assert!(h.ink(0, 0) > 0);
    h.draw_with(&RenderOptions {
        force: true,
        draw_cursor: false,
        blink_visible: false,
        ..RenderOptions::default()
    });
    assert_eq!(h.ink(0, 0), 0);
}

#[test]
fn underline_draws_below_the_glyph() {
    let mut h = Harness::new(4, 1);
    h.feed(b"\x1b[4m ").draw_with(&RenderOptions {
        force: true,
        draw_cursor: false,
        ..RenderOptions::default()
    });
    let metrics = h.fonts.metrics();
    let background = h.term.palette().background.pack();
    assert_ne!(
        h.cell_pixel(0, 0, 0, metrics.underline_position),
        background,
        "underline row should be inked"
    );
}

#[test]
fn block_cursor_paints_the_cell() {
    let mut h = Harness::new(4, 1);
    h.draw_with(&RenderOptions {
        force: true,
        draw_cursor: true,
        focused: true,
        blink_visible: true,
        ..RenderOptions::default()
    });
    let cursor = h.term.palette().cursor.pack();
    assert_eq!(h.cell_pixel(0, 0, 1, 1), cursor);
}

#[test]
fn unfocused_cursor_is_hollow() {
    let mut h = Harness::new(4, 1);
    h.draw_with(&RenderOptions {
        force: true,
        draw_cursor: true,
        focused: false,
        ..RenderOptions::default()
    });
    let cursor = h.term.palette().cursor.pack();
    let background = h.term.palette().background.pack();
    let metrics = h.fonts.metrics();
    // Edges drawn, middle left alone.
    assert_eq!(h.cell_pixel(0, 0, 0, 0), cursor);
    assert_eq!(
        h.cell_pixel(0, 0, metrics.cell_width / 2, metrics.cell_height / 2),
        background
    );
}

#[test]
fn hidden_cursor_draws_nothing() {
    let mut h = Harness::new(4, 1);
    h.feed(b"\x1b[?25l").draw_with(&RenderOptions {
        force: true,
        draw_cursor: true,
        ..RenderOptions::default()
    });
    let background = h.term.palette().background.pack();
    assert!(h.fb.pixels().iter().all(|&px| px == background));
}

#[test]
fn selection_repaints_the_background() {
    let mut h = Harness::new(6, 1);
    let options = RenderOptions {
        force: true,
        draw_cursor: false,
        selection: Some(Selection::new((1, 0), (2, 0), false)),
        ..RenderOptions::default()
    };
    h.feed(b"abcdef").draw_with(&options);
    let selection = options.selection_background.pack();
    assert_eq!(h.cell_pixel(1, 0, 0, 0), selection);
    assert_eq!(h.cell_pixel(2, 0, 0, 0), selection);
    assert_ne!(h.cell_pixel(3, 0, 0, 0), selection);
}

#[test]
fn damage_limits_what_is_repainted() {
    let mut h = Harness::new(6, 3);
    h.feed(b"\x1b[3;1Hbottom").draw();
    h.term.clear_damage();

    // Scribble directly into the framebuffer, then damage only row 0.
    let (width, height) = (h.fb.width(), h.fb.height());
    {
        let mut surface = h.fb.surface();
        surface.fill(Rect::new(0, 0, width, height), tos_term::Rgb::WHITE);
    }
    h.term.damage_mut().mark_row(0);
    h.draw_with(&RenderOptions {
        force: false,
        draw_cursor: false,
        ..RenderOptions::default()
    });

    let background = h.term.palette().background.pack();
    assert_eq!(h.cell_pixel(0, 0, 0, 0), background, "row 0 repainted");
    assert_eq!(h.cell_pixel(0, 2, 0, 0), 0xffffff, "row 2 left alone");
}

#[test]
fn wide_glyph_background_covers_two_cells() {
    let mut h = Harness::new(6, 1);
    h.feed("\x1b[41m漢".as_bytes()).draw_with(&RenderOptions {
        force: true,
        draw_cursor: false,
        ..RenderOptions::default()
    });
    let red = h.term.palette().index(1).pack();
    assert_eq!(h.cell_pixel(0, 0, 0, 0), red);
    assert_eq!(h.cell_pixel(1, 0, 0, 0), red);
}

#[test]
fn box_drawing_joins_across_cells() {
    let mut h = Harness::new(4, 1);
    h.feed("────".as_bytes()).draw_with(&RenderOptions {
        force: true,
        draw_cursor: false,
        ..RenderOptions::default()
    });
    // Every column across the full width must be inked on the centre row.
    let metrics = h.fonts.metrics();
    let background = h.term.palette().background.pack();
    let mid = metrics.cell_height / 2;
    for x in 0..h.fb.width() {
        let inked = (mid.saturating_sub(1)..=mid + 1)
            .any(|y| h.fb.pixel(x, y) != background);
        assert!(inked, "gap in the line at column {x}");
    }
}

#[test]
fn kitty_image_reaches_the_framebuffer() {
    let mut h = Harness::new(8, 4);
    let metrics = h.fonts.metrics();
    let (w, hgt) = (metrics.cell_width, metrics.cell_height);
    // A solid green rectangle exactly one cell in size.
    let pixels: Vec<u8> = (0..w * hgt).flat_map(|_| [0u8, 0xff, 0, 0xff]).collect();
    let payload = tos_term::graphics::encode_base64(&pixels);
    h.feed(format!("\x1b_Ga=T,f=32,s={w},v={hgt},i=1;{payload}\x1b\\").as_bytes());
    h.draw_with(&RenderOptions {
        force: true,
        draw_cursor: false,
        ..RenderOptions::default()
    });
    assert_eq!(h.cell_pixel(0, 0, 1, 1), 0x00ff00);
    assert_ne!(h.cell_pixel(2, 0, 1, 1), 0x00ff00);
}

#[test]
fn rendering_a_scrolled_view_shows_history() {
    let mut h = Harness::new(6, 2);
    h.feed(b"top\r\nmid\r\nbot");
    h.term.scroll_display(1);
    h.draw_with(&RenderOptions {
        force: true,
        draw_cursor: false,
        ..RenderOptions::default()
    });
    // The scrolled back view starts with "top", which inks the first cell.
    assert!(h.ink(0, 0) > 0);
    // And the cursor is suppressed while looking at history.
    let cursor = h.term.palette().cursor.pack();
    assert!(!h.fb.pixels().contains(&cursor));
}

#[test]
fn a_pane_only_paints_inside_its_area() {
    let mut h = Harness::new(8, 4);
    let metrics = h.fonts.metrics();
    h.feed(b"\x1b[41m\x1b[2J");
    // Render into the bottom right quadrant only.
    let area = Rect::new(
        (4 * metrics.cell_width) as i32,
        (2 * metrics.cell_height) as i32,
        4 * metrics.cell_width,
        2 * metrics.cell_height,
    );
    {
        let mut surface = h.fb.surface();
        render(
            &mut surface,
            area,
            &h.term,
            &mut h.fonts,
            &mut h.textures,
            &RenderOptions {
                force: true,
                draw_cursor: false,
                ..RenderOptions::default()
            },
        );
    }
    let red = h.term.palette().index(1).pack();
    assert_eq!(h.cell_pixel(5, 3, 0, 0), red, "inside the pane");
    assert_eq!(h.cell_pixel(1, 1, 0, 0), 0, "outside the pane untouched");
}

// ---------------------------------------------------------------------------
// Regressions found in review
// ---------------------------------------------------------------------------

#[test]
fn concealed_text_stays_concealed_when_selected() {
    // SGR 8 is used to echo passwords; a selection must not reveal them.
    let mut h = Harness::new(6, 1);
    let options = RenderOptions {
        force: true,
        draw_cursor: false,
        selection: Some(Selection::new((0, 0), (2, 0), false)),
        ..RenderOptions::default()
    };
    h.feed(b"\x1b[8mabc").draw_with(&options);
    let selection = options.selection_background.pack();
    // Every pixel of the selected cells is the selection colour: no glyph.
    let metrics = h.fonts.metrics();
    for col in 0..3 {
        for y in 0..metrics.cell_height {
            for x in 0..metrics.cell_width {
                assert_eq!(
                    h.cell_pixel(col, 0, x, y),
                    selection,
                    "glyph visible at {col} {x},{y}"
                );
            }
        }
    }
}

#[test]
fn blinking_text_stays_hidden_when_selected() {
    let mut h = Harness::new(6, 1);
    let options = RenderOptions {
        force: true,
        draw_cursor: false,
        blink_visible: false,
        selection: Some(Selection::new((0, 0), (0, 0), false)),
        ..RenderOptions::default()
    };
    h.feed(b"\x1b[5mX").draw_with(&options);
    let selection = options.selection_background.pack();
    let metrics = h.fonts.metrics();
    for y in 0..metrics.cell_height {
        for x in 0..metrics.cell_width {
            assert_eq!(h.cell_pixel(0, 0, x, y), selection);
        }
    }
}

/// Rows of the framebuffer that hold ink, relative to a cell's top edge.
fn inked_rows(h: &Harness, col: usize, row: usize) -> Vec<u32> {
    let metrics = h.fonts.metrics();
    let background = h.term.palette().background.pack();
    (0..metrics.cell_height)
        .filter(|&y| (0..metrics.cell_width).any(|x| h.cell_pixel(col, row, x, y) != background))
        .collect()
}

#[test]
fn a_double_underline_stays_inside_its_cell() {
    let mut h = Harness::new(4, 2);
    h.feed(b"\x1b[21m ").draw_with(&RenderOptions {
        force: true,
        draw_cursor: false,
        ..RenderOptions::default()
    });
    let metrics = h.fonts.metrics();
    let rows = inked_rows(&h, 0, 0);
    assert_eq!(rows.len(), 2, "a double underline has two strokes: {rows:?}");
    assert!(
        rows.iter().all(|&y| y < metrics.cell_height),
        "strokes must stay in the cell: {rows:?}"
    );
    // And nothing leaked into the row below.
    assert!(inked_rows(&h, 0, 1).is_empty(), "ink spilled into the next row");
}

#[test]
fn a_curly_underline_stays_inside_its_cell() {
    let mut h = Harness::new(4, 2);
    h.feed(b"\x1b[4:3m ").draw_with(&RenderOptions {
        force: true,
        draw_cursor: false,
        ..RenderOptions::default()
    });
    let metrics = h.fonts.metrics();
    let rows = inked_rows(&h, 0, 0);
    assert!(!rows.is_empty());
    assert!(rows.iter().all(|&y| y < metrics.cell_height), "{rows:?}");
    assert!(inked_rows(&h, 0, 1).is_empty(), "ink spilled into the next row");
}

#[test]
fn a_single_underline_is_still_one_stroke() {
    let mut h = Harness::new(4, 2);
    h.feed(b"\x1b[4m ").draw_with(&RenderOptions {
        force: true,
        draw_cursor: false,
        ..RenderOptions::default()
    });
    assert_eq!(inked_rows(&h, 0, 0).len(), 1);
}

#[test]
fn the_block_cursor_covers_a_wide_glyph() {
    let mut h = Harness::new(6, 1);
    // Put the cursor back on the wide character after writing it.
    h.feed("漢\x1b[1G".as_bytes()).draw_with(&RenderOptions {
        force: true,
        draw_cursor: true,
        focused: true,
        ..RenderOptions::default()
    });
    let cursor = h.term.palette().cursor.pack();
    // Both halves of the glyph sit on the cursor block.
    assert_eq!(h.cell_pixel(0, 0, 0, 0), cursor);
    assert_eq!(h.cell_pixel(1, 0, 0, 0), cursor);
    assert_ne!(h.cell_pixel(2, 0, 0, 0), cursor);
}

#[test]
fn images_scroll_with_the_text_they_sit_on() {
    let mut h = Harness::new(8, 4);
    let metrics = h.fonts.metrics();
    let (w, hgt) = (metrics.cell_width, metrics.cell_height);
    let pixels: Vec<u8> = (0..w * hgt).flat_map(|_| [0u8, 0xff, 0, 0xff]).collect();
    let payload = tos_term::graphics::encode_base64(&pixels);
    h.feed(format!("\x1b_Ga=T,f=32,s={w},v={hgt},i=1;{payload}\x1b\\").as_bytes());

    let options = RenderOptions {
        force: true,
        draw_cursor: false,
        ..RenderOptions::default()
    };
    h.draw_with(&options);
    assert_eq!(h.cell_pixel(0, 0, 1, 1), 0x00ff00, "image should be on row 0");

    // Push the image off the top of the screen, then look at history.
    h.feed(b"\r\n\r\n\r\n\r\n\r\n");
    h.term.scroll_display(2);
    h.draw_with(&options);
    // Wherever it is now, it must not still be painted on screen row 0.
    let top_row_is_image = h.cell_pixel(0, 0, 1, 1) == 0x00ff00;
    assert!(!top_row_is_image, "the image stayed pinned to the display");
}

// ---------------------------------------------------------------------------
// Texture cache
// ---------------------------------------------------------------------------

/// A scene with an image scaled to a size it was not transmitted at, whose
/// alpha covers the transparent, blended and opaque cases. Those three are
/// exactly what the cached blit has to reproduce.
fn scaled_image_over_a_backdrop(h: &mut Harness) {
    let pixels: Vec<u8> = (0..3u32 * 5)
        .flat_map(|i| {
            let alpha = match i % 3 {
                0 => 0x00,
                1 => 0x80,
                _ => 0xff,
            };
            [(0x20 * (i % 8)) as u8, 0xff, 0x40, alpha]
        })
        .collect();
    let payload = tos_term::graphics::encode_base64(&pixels);
    h.feed(b"\x1b[44mbackdrop\r\n");
    h.feed(format!("\x1b_Ga=T,f=32,s=3,v=5,c=3,r=2,i=1;{payload}\x1b\\").as_bytes());
}

#[test]
fn the_texture_cache_changes_no_pixels() {
    let options = RenderOptions {
        force: true,
        draw_cursor: false,
        ..RenderOptions::default()
    };

    let mut cached = Harness::new(8, 4);
    scaled_image_over_a_backdrop(&mut cached);
    cached.draw_with(&options);
    // The second frame is the one that comes out of the cache.
    cached.draw_with(&options);

    let mut uncached = Harness::new(8, 4);
    // A budget nothing fits in forces every frame down the scaling path.
    uncached.textures = TextureCache::new(0);
    scaled_image_over_a_backdrop(&mut uncached);
    uncached.draw_with(&options);
    uncached.draw_with(&options);

    assert!(cached.textures.hits() > 0, "the scene never hit the cache");
    assert!(uncached.textures.is_empty(), "nothing should have been kept");
    assert_eq!(cached.fb.pixels(), uncached.fb.pixels());
}

#[test]
fn repeat_frames_do_not_rescale() {
    let options = RenderOptions {
        force: true,
        draw_cursor: false,
        ..RenderOptions::default()
    };
    let mut h = Harness::new(8, 4);
    scaled_image_over_a_backdrop(&mut h);
    for _ in 0..5 {
        h.draw_with(&options);
    }
    assert_eq!(h.textures.misses(), 1, "the image was scaled more than once");
    assert_eq!(h.textures.hits(), 4);
}

#[test]
fn retransmitting_an_image_invalidates_its_texture() {
    let options = RenderOptions {
        force: true,
        draw_cursor: false,
        ..RenderOptions::default()
    };
    let mut h = Harness::new(8, 4);
    let green: Vec<u8> = (0..4u32).flat_map(|_| [0, 0xff, 0, 0xff]).collect();
    let red: Vec<u8> = (0..4u32).flat_map(|_| [0xff, 0, 0, 0xff]).collect();

    let payload = tos_term::graphics::encode_base64(&green);
    h.feed(format!("\x1b[H\x1b_Ga=T,f=32,s=2,v=2,c=2,r=2,i=1;{payload}\x1b\\").as_bytes());
    h.draw_with(&options);
    assert_eq!(h.cell_pixel(0, 0, 1, 1), 0x00ff00);

    // The same image id, the same placement size: only the pixels changed.
    let payload = tos_term::graphics::encode_base64(&red);
    h.feed(format!("\x1b[H\x1b_Ga=T,f=32,s=2,v=2,c=2,r=2,i=1;{payload}\x1b\\").as_bytes());
    h.draw_with(&options);
    assert_eq!(h.cell_pixel(0, 0, 1, 1), 0xff0000, "stale texture on screen");
}
