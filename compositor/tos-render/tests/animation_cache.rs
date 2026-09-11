//! The interaction the two branches create: an animated image's pixels change
//! without a re-transmission, so a cache keyed only on the image id would show
//! a frozen picture forever.

use std::time::{Duration, Instant};

use tos_font::{BitmapFont, FontStack};
use tos_render::{render, OwnedFramebuffer, Rect, RenderOptions, TextureCache};
use tos_term::graphics::encode_base64;
use tos_term::{Terminal, TerminalConfig};

fn solid(w: u32, h: u32, rgba: [u8; 4]) -> String {
    let pixels: Vec<u8> = (0..w * h).flat_map(|_| rgba).collect();
    encode_base64(&pixels)
}

#[test]
fn an_animation_keeps_moving_under_the_texture_cache() {
    let fonts_owner = BitmapFont::new(1);
    let mut fonts = FontStack::new(Box::new(fonts_owner));
    let metrics = fonts.metrics();
    let mut fb = OwnedFramebuffer::new(metrics.cell_width * 8, metrics.cell_height * 4);
    let config = TerminalConfig {
        cell_width: metrics.cell_width,
        cell_height: metrics.cell_height,
        ..TerminalConfig::default()
    };
    let mut term = Terminal::new(8, 4, config);
    let mut textures = TextureCache::default();

    // A red root frame, then a blue second frame with a 40ms gap, playing.
    let red = solid(4, 4, [0xff, 0x00, 0x00, 0xff]);
    term.advance(format!("\x1b_Ga=T,f=32,s=4,v=4,c=4,r=2,i=7;{red}\x1b\\").as_bytes());
    let blue = solid(4, 4, [0x00, 0x00, 0xff, 0xff]);
    term.advance(format!("\x1b_Ga=f,f=32,s=4,v=4,i=7,z=40,X=1;{blue}\x1b\\").as_bytes());
    term.advance(b"\x1b_Ga=a,i=7,s=3,v=1\x1b\\");

    let options = RenderOptions {
        force: true,
        draw_cursor: false,
        ..RenderOptions::default()
    };
    let area = Rect::new(0, 0, fb.width(), fb.height());

    let mut draw = |fb: &mut OwnedFramebuffer, term: &Terminal, textures: &mut TextureCache| {
        let mut surface = fb.surface();
        render(&mut surface, area, term, &mut fonts, textures, &options);
    };

    let start = Instant::now();
    term.advance_animations(start);
    draw(&mut fb, &term, &mut textures);
    let first = fb.pixels().to_vec();

    // Past the gap: frame two is what should be on screen now.
    term.advance_animations(start + Duration::from_millis(60));
    draw(&mut fb, &term, &mut textures);
    let second = fb.pixels().to_vec();

    assert_ne!(
        first, second,
        "the animation is frozen: the cache served the first frame again"
    );
}
