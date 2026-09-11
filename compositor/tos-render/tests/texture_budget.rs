use std::time::{Duration, Instant};
use tos_font::{BitmapFont, FontStack};
use tos_render::{render, OwnedFramebuffer, Rect, RenderOptions, TextureCache};
use tos_term::graphics::encode_base64;
use tos_term::{Terminal, TerminalConfig};

fn setup(cols: usize, rows: usize) -> (OwnedFramebuffer, FontStack, Terminal, TextureCache) {
    let fonts = FontStack::new(Box::new(BitmapFont::new(1)));
    let m = fonts.metrics();
    let fb = OwnedFramebuffer::new(m.cell_width * cols as u32, m.cell_height * rows as u32);
    let config = TerminalConfig { cell_width: m.cell_width, cell_height: m.cell_height, ..TerminalConfig::default() };
    (fb, fonts, Terminal::new(cols, rows, config), TextureCache::default())
}

/// FINDING 1: a placement far bigger than the pane is scaled in full before
/// anyone asks whether it fits the budget.
#[test]
fn a_huge_placement_costs_a_huge_scale_every_frame() {
    let (mut fb, mut fonts, mut term, mut textures) = setup(8, 4);
    let pixels: Vec<u8> = (0..4u32 * 4).flat_map(|_| [0xff, 0, 0, 0xff]).collect();
    let payload = encode_base64(&pixels);
    // 1000 cells wide by 500 tall, on an 8x4 pane.
    term.advance(format!("\x1b_Ga=T,f=32,s=4,v=4,c=1000,r=500,i=1;{payload}\x1b\\").as_bytes());

    let area = Rect::new(0, 0, fb.width(), fb.height());
    let options = RenderOptions { force: true, draw_cursor: false, ..RenderOptions::default() };
    let start = Instant::now();
    for _ in 0..3 {
        let mut s = fb.surface();
        render(&mut s, area, &term, &mut fonts, &mut textures, &options);
    }
    let each = start.elapsed() / 3;
    eprintln!("per frame: {each:?}   cache entries: {}", textures.len());
    assert!(
        each < Duration::from_millis(5),
        "a placement off the edge of an 8x4 pane cost {each:?} a frame"
    );
}

/// FINDING 2: a looping animation should reuse its textures, not fill the
/// cache with one dead entry per frame shown.
#[test]
fn a_looping_animation_reuses_its_textures() {
    let (mut fb, mut fonts, mut term, mut textures) = setup(8, 4);
    let red = encode_base64(&(0..16u32).flat_map(|_| [0xff, 0u8, 0, 0xff]).collect::<Vec<u8>>());
    term.advance(format!("\x1b_Ga=T,f=32,s=4,v=4,c=4,r=2,i=7;{red}\x1b\\").as_bytes());
    let blue = encode_base64(&(0..16u32).flat_map(|_| [0u8, 0, 0xff, 0xff]).collect::<Vec<u8>>());
    term.advance(format!("\x1b_Ga=f,f=32,s=4,v=4,i=7,z=40,X=1;{blue}\x1b\\").as_bytes());
    term.advance(b"\x1b_Ga=a,i=7,s=3,v=1\x1b\\");

    let area = Rect::new(0, 0, fb.width(), fb.height());
    let options = RenderOptions { force: true, draw_cursor: false, ..RenderOptions::default() };
    let t0 = Instant::now();
    for step in 0..40u64 {
        term.advance_animations(t0 + Duration::from_millis(step * 50));
        let mut s = fb.surface();
        render(&mut s, area, &term, &mut fonts, &mut textures, &options);
    }
    eprintln!("hits={} misses={} entries={}", textures.hits(), textures.misses(), textures.len());
    assert!(
        textures.len() <= 4,
        "two frames left {} cache entries behind", textures.len()
    );
}
