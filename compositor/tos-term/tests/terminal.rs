//! Behavioural tests for the terminal: what a stream of bytes does to the grid.

use std::time::{Duration, Instant};

use tos_term::graphics::{encode_base64, Medium};
use tos_term::medium::MediumReader;
use tos_term::{Color, CursorShape, Flags, MouseTracking, TermEvent, Terminal, TerminalConfig};

fn term(cols: usize, rows: usize) -> Terminal {
    Terminal::new(cols, rows, TerminalConfig::default())
}

fn screen(t: &Terminal) -> Vec<String> {
    // `split` rather than `lines` so trailing blank rows stay in the vector.
    t.grid()
        .to_text()
        .split('\n')
        .map(|l| l.to_string())
        .collect()
}

#[test]
fn prints_plain_text() {
    let mut t = term(10, 2);
    t.advance(b"hello");
    assert_eq!(screen(&t)[0], "hello");
    assert_eq!(t.cursor().x, 5);
}

#[test]
fn wraps_at_the_right_margin() {
    let mut t = term(4, 3);
    t.advance(b"abcdef");
    assert_eq!(screen(&t)[0], "abcd");
    assert_eq!(screen(&t)[1], "ef");
    assert_eq!(t.cursor().y, 1);
}

#[test]
fn writing_the_last_column_defers_the_wrap() {
    let mut t = term(4, 3);
    t.advance(b"abcd");
    // The cursor stays on the last column until another character arrives.
    assert_eq!(t.cursor().x, 3);
    assert_eq!(t.cursor().y, 0);
    t.advance(b"e");
    assert_eq!(t.cursor().y, 1);
    assert_eq!(t.cursor().x, 1);
}

#[test]
fn no_wraparound_overwrites_last_column() {
    let mut t = term(4, 2);
    t.advance(b"\x1b[?7l");
    t.advance(b"abcdef");
    assert_eq!(screen(&t)[0], "abcf");
    assert_eq!(screen(&t)[1], "");
}

#[test]
fn carriage_return_and_linefeed() {
    let mut t = term(10, 3);
    t.advance(b"one\r\ntwo");
    assert_eq!(screen(&t)[0], "one");
    assert_eq!(screen(&t)[1], "two");
}

#[test]
fn scrolls_and_keeps_history() {
    let mut t = term(10, 2);
    t.advance(b"one\r\ntwo\r\nthree");
    assert_eq!(screen(&t)[0], "two");
    assert_eq!(screen(&t)[1], "three");
    assert_eq!(t.grid().scrollback_len(), 1);
    assert_eq!(t.grid().history_row(0).unwrap().to_text(), "one");
}

#[test]
fn wide_characters_occupy_two_cells() {
    let mut t = term(6, 1);
    t.advance("漢字".as_bytes());
    assert_eq!(t.cursor().x, 4);
    let row = t.grid().row(0);
    assert!(row.get(0).unwrap().attrs.flags.contains(Flags::WIDE));
    assert!(row.get(1).unwrap().attrs.flags.contains(Flags::WIDE_SPACER));
    assert_eq!(row.to_text(), "漢字");
}

#[test]
fn wide_character_never_straddles_the_margin() {
    let mut t = term(4, 2);
    t.advance("abc漢".as_bytes());
    // Only one column is left, so the glyph moves to the next line whole.
    assert_eq!(screen(&t)[0], "abc");
    assert_eq!(screen(&t)[1], "漢");
}

#[test]
fn overwriting_half_a_wide_glyph_erases_the_other_half() {
    let mut t = term(6, 1);
    t.advance("漢".as_bytes());
    t.advance(b"\x1b[1G");
    t.advance(b"x");
    assert_eq!(t.grid().row(0).to_text(), "x");
}

#[test]
fn combining_marks_attach_to_the_previous_cell() {
    let mut t = term(6, 1);
    t.advance("e\u{0301}".as_bytes());
    assert_eq!(t.cursor().x, 1);
    assert_eq!(t.grid().row(0).to_text(), "e\u{0301}");
}

#[test]
fn cursor_addressing_is_one_based() {
    let mut t = term(10, 5);
    t.advance(b"\x1b[3;5Hx");
    assert_eq!(screen(&t)[2], "    x");
}

#[test]
fn erase_in_display_below() {
    let mut t = term(5, 3);
    t.advance(b"aaaaa\r\nbbbbb\r\nccccc");
    t.advance(b"\x1b[2;3H\x1b[0J");
    assert_eq!(screen(&t)[0], "aaaaa");
    assert_eq!(screen(&t)[1], "bb");
    assert_eq!(screen(&t)[2], "");
}

#[test]
fn erase_in_line_variants() {
    let mut t = term(5, 1);
    t.advance(b"abcde\x1b[3G\x1b[1K");
    assert_eq!(screen(&t)[0], "   de");
}

#[test]
fn insert_and_delete_characters() {
    let mut t = term(6, 1);
    t.advance(b"abcdef\x1b[1G\x1b[2@");
    assert_eq!(screen(&t)[0], "  abcd");
    t.advance(b"\x1b[2P");
    assert_eq!(screen(&t)[0], "abcd");
}

#[test]
fn delete_line_does_not_enter_scrollback() {
    let mut t = term(5, 3);
    t.advance(b"aaa\r\nbbb\r\nccc");
    t.advance(b"\x1b[1;1H\x1b[1M");
    assert_eq!(screen(&t)[0], "bbb");
    assert_eq!(t.grid().scrollback_len(), 0);
}

#[test]
fn scroll_region_confines_scrolling() {
    let mut t = term(5, 4);
    t.advance(b"aaa\r\nbbb\r\nccc\r\nddd");
    // Region covers rows 2..3 (one based, inclusive).
    t.advance(b"\x1b[2;3r");
    t.advance(b"\x1b[3;1H\r\nxxx");
    let s = screen(&t);
    assert_eq!(s[0], "aaa");
    assert_eq!(s[1], "ccc");
    assert_eq!(s[2], "xxx");
    assert_eq!(s[3], "ddd");
}

#[test]
fn origin_mode_is_relative_to_the_region() {
    let mut t = term(5, 4);
    t.advance(b"\x1b[2;3r\x1b[?6h");
    t.advance(b"\x1b[1;1Hx");
    assert_eq!(screen(&t)[1], "x");
}

#[test]
fn sgr_sets_colors_and_attributes() {
    let mut t = term(10, 1);
    t.advance(b"\x1b[1;31;48;5;12mA");
    let cell = t.grid().cell(0, 0).unwrap();
    assert!(cell.attrs.flags.contains(Flags::BOLD));
    assert_eq!(cell.attrs.fg, Color::Indexed(1));
    assert_eq!(cell.attrs.bg, Color::Indexed(12));
}

#[test]
fn sgr_truecolor_both_separator_styles() {
    let mut t = term(10, 1);
    t.advance(b"\x1b[38;2;10;20;30mA");
    assert_eq!(
        t.grid().cell(0, 0).unwrap().attrs.fg,
        Color::Rgb(tos_term::Rgb::new(10, 20, 30))
    );
    t.advance(b"\x1b[38:2::40:50:60mB");
    assert_eq!(
        t.grid().cell(1, 0).unwrap().attrs.fg,
        Color::Rgb(tos_term::Rgb::new(40, 50, 60))
    );
}

#[test]
fn sgr_reset_clears_the_pen() {
    let mut t = term(10, 1);
    t.advance(b"\x1b[1;31mA\x1b[0mB");
    assert_eq!(t.grid().cell(1, 0).unwrap().attrs.fg, Color::Default);
    assert!(!t
        .grid()
        .cell(1, 0)
        .unwrap()
        .attrs
        .flags
        .contains(Flags::BOLD));
}

#[test]
fn background_color_erase_keeps_the_background() {
    let mut t = term(5, 1);
    t.advance(b"\x1b[41m\x1b[2K");
    assert_eq!(t.grid().cell(2, 0).unwrap().attrs.bg, Color::Indexed(1));
}

#[test]
fn alternate_screen_swaps_and_restores() {
    let mut t = term(10, 2);
    t.advance(b"primary");
    t.advance(b"\x1b[?1049h");
    assert_eq!(screen(&t)[0], "");
    // 1049 saves the cursor and clears, but does not home it, so the app has
    // to position itself just as it does on a real terminal.
    t.advance(b"\x1b[1;1Halt");
    assert_eq!(screen(&t)[0], "alt");
    t.advance(b"\x1b[?1049l");
    assert_eq!(screen(&t)[0], "primary");
}

#[test]
fn alternate_screen_has_no_scrollback() {
    let mut t = term(10, 2);
    t.advance(b"\x1b[?1049h");
    t.advance(b"a\r\nb\r\nc\r\nd");
    assert_eq!(t.grid().scrollback_len(), 0);
}

#[test]
fn save_and_restore_cursor() {
    let mut t = term(10, 3);
    t.advance(b"\x1b[2;4H\x1b7\x1b[1;1H\x1b8x");
    assert_eq!(screen(&t)[1], "   x");
}

#[test]
fn tabs_move_to_multiples_of_eight() {
    let mut t = term(20, 1);
    t.advance(b"a\tb");
    assert_eq!(screen(&t)[0], "a       b");
}

#[test]
fn dec_special_graphics_draws_lines() {
    let mut t = term(5, 1);
    t.advance(b"\x1b(0qqq\x1b(B");
    assert_eq!(screen(&t)[0], "───");
}

#[test]
fn device_status_report_answers_cursor_position() {
    let mut t = term(10, 5);
    t.advance(b"\x1b[3;7H\x1b[6n");
    assert_eq!(t.take_output(), b"\x1b[3;7R".to_vec());
}

#[test]
fn device_attributes_identify_the_terminal() {
    let mut t = term(10, 2);
    t.advance(b"\x1b[c");
    assert_eq!(t.take_output(), b"\x1b[?62;22c".to_vec());
}

#[test]
fn mode_query_reports_state() {
    let mut t = term(10, 2);
    t.advance(b"\x1b[?1049h\x1b[?1049$p");
    assert_eq!(t.take_output(), b"\x1b[?1049;1$y".to_vec());
}

#[test]
fn title_is_reported_as_an_event() {
    let mut t = term(10, 2);
    t.advance(b"\x1b]0;tOS\x07");
    assert_eq!(t.title(), "tOS");
    assert!(t
        .take_events()
        .contains(&TermEvent::TitleChanged("tOS".into())));
}

#[test]
fn clipboard_write_is_decoded() {
    let mut t = term(10, 2);
    let payload = encode_base64(b"copied");
    t.advance(format!("\x1b]52;c;{payload}\x07").as_bytes());
    let events = t.take_events();
    assert!(events.contains(&TermEvent::ClipboardStore {
        selection: 'c',
        data: b"copied".to_vec(),
    }));
}

#[test]
fn mouse_tracking_modes_are_tracked() {
    let mut t = term(10, 2);
    assert_eq!(t.mouse().tracking, MouseTracking::None);
    t.advance(b"\x1b[?1002h\x1b[?1006h");
    assert_eq!(t.mouse().tracking, MouseTracking::ButtonEvent);
    assert_eq!(t.mouse().encoding, tos_term::MouseEncoding::Sgr);
    t.advance(b"\x1b[?1002l");
    assert_eq!(t.mouse().tracking, MouseTracking::None);
}

#[test]
fn cursor_style_is_settable() {
    let mut t = term(10, 2);
    t.advance(b"\x1b[4 q");
    assert_eq!(t.cursor_style().shape, CursorShape::Underline);
    assert!(!t.cursor_style().blinking);
}

#[test]
fn hyperlinks_are_interned() {
    let mut t = term(10, 1);
    t.advance(b"\x1b]8;;https://tos.example\x1b\\link\x1b]8;;\x1b\\");
    let id = t.grid().cell(0, 0).unwrap().attrs.hyperlink.unwrap();
    assert_eq!(t.hyperlink(id), Some("https://tos.example"));
    // The trailing empty URI ends the link.
    assert!(t.cursor().attrs.hyperlink.is_none());
}

#[test]
fn repeat_duplicates_the_last_character() {
    let mut t = term(10, 1);
    t.advance(b"x\x1b[4b");
    assert_eq!(screen(&t)[0], "xxxxx");
}

#[test]
fn resize_preserves_content_and_clamps_cursor() {
    let mut t = term(10, 3);
    t.advance(b"hello\r\nworld");
    t.resize(6, 2);
    assert_eq!(t.cols(), 6);
    assert_eq!(t.rows(), 2);
    assert_eq!(screen(&t)[0], "hello");
    assert!(t.cursor().x < 6);
    assert!(t.cursor().y < 2);
}

#[test]
fn full_reset_restores_defaults() {
    let mut t = term(10, 2);
    t.advance(b"\x1b[?1049h\x1b[31mjunk\x1bc");
    assert_eq!(screen(&t)[0], "");
    assert_eq!(t.cursor().attrs.fg, Color::Default);
    assert!(!t.modes.alt_screen);
}

#[test]
fn synchronized_output_mode_round_trips() {
    let mut t = term(10, 2);
    t.advance(b"\x1b[?2026h");
    assert!(t.modes.synchronized_output);
    t.advance(b"\x1b[?2026l");
    assert!(!t.modes.synchronized_output);
}

#[test]
fn kitty_graphics_places_an_image_and_tags_cells() {
    let mut t = term(10, 4);
    // A 16x32 RGB image over 8x16 cells covers two columns and two rows.
    let pixels = vec![0x40u8; 16 * 32 * 3];
    let payload = encode_base64(&pixels);
    t.advance(format!("\x1b_Ga=T,f=24,s=16,v=32,i=5;{payload}\x1b\\").as_bytes());

    let cell = t.grid().cell(0, 0).unwrap();
    let reference = cell.attrs.graphics.expect("cell should reference graphics");
    let placement = t.graphics().placement(reference.placement).unwrap();
    assert_eq!(placement.cols, 2);
    assert_eq!(placement.rows, 2);
    assert_eq!(t.graphics().image(5).unwrap().data.len(), 16 * 32 * 4);
    assert_eq!(t.take_output(), b"\x1b_Gi=5;OK\x1b\\".to_vec());
}

/// A 2x2 RGBA PNG: red, green on the top row, blue and half-transparent
/// white on the bottom. Written by CPython's zlib rather than by anything in
/// this repository, so the decoder is checked against a real encoder's
/// output: `IDAT` here is a genuine compressed deflate block, not stored
/// bytes. IHDR says 8-bit colour type 6, no interlacing.
const PNG_2X2: [u8; 76] = [
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x02, 0x08, 0x06, 0x00, 0x00, 0x00, 0x72, 0xb6, 0x0d,
    0x24, 0x00, 0x00, 0x00, 0x13, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0xf8, 0xcf, 0xc0, 0xf0,
    0x1f, 0x0c, 0x81, 0x34, 0x08, 0x34, 0x00, 0x00, 0x49, 0x49, 0x09, 0x78, 0x9c, 0x51, 0x17, 0x92,
    0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

#[test]
fn kitty_graphics_transmits_a_png() {
    let mut t = term(10, 4);
    let payload = encode_base64(&PNG_2X2);
    t.advance(format!("\x1b_Ga=t,f=100,i=9;{payload}\x1b\\").as_bytes());

    let image = t.graphics().image(9).expect("the PNG should be stored");
    assert_eq!((image.width, image.height), (2, 2));
    assert_eq!(
        image.data,
        vec![255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 128]
    );
    assert_eq!(t.take_output(), b"\x1b_Gi=9;OK\x1b\\".to_vec());
}

#[test]
fn kitty_graphics_transmits_a_zlib_compressed_image() {
    let mut t = term(10, 4);
    // A PNG is already deflated, so `o=z` on top of it is the doubly
    // compressed shape a file manager can send. Deflate is content-agnostic,
    // so a stored block is a legitimate stream to send here.
    let mut compressed = vec![0x78u8, 0x01, 0x01];
    compressed.extend_from_slice(&(PNG_2X2.len() as u16).to_le_bytes());
    compressed.extend_from_slice(&(!(PNG_2X2.len() as u16)).to_le_bytes());
    compressed.extend_from_slice(&PNG_2X2);
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in PNG_2X2.iter() {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    compressed.extend_from_slice(&(((b << 16) | a).to_be_bytes()));

    let payload = encode_base64(&compressed);
    t.advance(format!("\x1b_Ga=t,f=100,o=z,i=11;{payload}\x1b\\").as_bytes());
    assert_eq!(t.graphics().image(11).unwrap().data.len(), 2 * 2 * 4);
    assert_eq!(t.take_output(), b"\x1b_Gi=11;OK\x1b\\".to_vec());
}

#[test]
fn kitty_graphics_reports_undecodable_payloads() {
    let mut t = term(10, 4);
    t.advance(b"\x1b_Ga=T,f=100,s=1,v=1,i=9;AAAA\x1b\\");
    let out = String::from_utf8(t.take_output()).unwrap();
    assert!(
        out.contains("i=9"),
        "response should identify the image: {out}"
    );
    assert!(out.contains("EINVAL"), "response should be an error: {out}");
    assert!(t.graphics().image(9).is_none());
}

/// A reader that holds one file in memory.
///
/// The terminal's half of a named transmission is all that is tested here:
/// that the name is decoded, handed over whole, and answered either with an
/// image or with the reader's own error. What tOS is willing to open on a
/// program's say-so belongs to the compositor and is tested there, because
/// this crate has no filesystem to be tested against.
struct OneFile {
    medium: Medium,
    name: &'static str,
    data: Vec<u8>,
}

impl MediumReader for OneFile {
    fn read(
        &mut self,
        medium: Medium,
        name: &[u8],
        max_bytes: usize,
    ) -> Result<Vec<u8>, &'static str> {
        if medium != self.medium || name != self.name.as_bytes() {
            return Err("EBADF:no such file");
        }
        if self.data.len() > max_bytes {
            return Err("EINVAL:file exceeds the image budget");
        }
        Ok(self.data.clone())
    }
}

fn term_with_file(medium: Medium, name: &'static str, data: &[u8]) -> Terminal {
    let mut t = term(10, 4);
    t.set_medium_reader(Box::new(OneFile {
        medium,
        name,
        data: data.to_vec(),
    }));
    t
}

#[test]
fn kitty_graphics_transmits_a_file_by_name() {
    let mut t = term_with_file(Medium::File, "/tmp/tos-preview.png", &PNG_2X2);
    let payload = encode_base64(b"/tmp/tos-preview.png");
    t.advance(format!("\x1b_Ga=t,f=100,t=f,i=7;{payload}\x1b\\").as_bytes());

    let image = t
        .graphics()
        .image(7)
        .expect("the named file should be stored");
    assert_eq!((image.width, image.height), (2, 2));
    assert_eq!(t.take_output(), b"\x1b_Gi=7;OK\x1b\\".to_vec());
}

#[test]
fn kitty_graphics_reads_a_name_that_arrived_in_chunks() {
    let mut t = term_with_file(Medium::TempFile, "/tmp/tos-preview.png", &PNG_2X2);
    // A name is base64 like any other payload and can be split like one.
    // Reading has to wait for the last chunk: opening the first half of a
    // path is opening a file nobody named.
    let whole = encode_base64(b"/tmp/tos-preview.png");
    let (first, rest) = whole.split_at(8);
    t.advance(format!("\x1b_Ga=t,f=100,t=t,i=8,m=1;{first}\x1b\\").as_bytes());
    assert!(t.graphics().image(8).is_none());
    t.advance(format!("\x1b_Gm=0;{rest}\x1b\\").as_bytes());
    assert_eq!(t.graphics().image(8).unwrap().width, 2);
}

#[test]
fn kitty_graphics_tells_the_reader_which_medium_asked() {
    // The same name under another medium is a different request, because
    // `t=t` is the one that also deletes what it read.
    let mut t = term_with_file(Medium::TempFile, "/tmp/tos-preview.png", &PNG_2X2);
    let payload = encode_base64(b"/tmp/tos-preview.png");
    t.advance(format!("\x1b_Ga=t,f=100,t=f,i=9;{payload}\x1b\\").as_bytes());
    assert_eq!(
        t.take_output(),
        b"\x1b_Gi=9;EBADF:no such file\x1b\\".to_vec()
    );
    assert!(t.graphics().image(9).is_none());
}

#[test]
fn kitty_graphics_reads_an_animation_frame_from_a_file() {
    let mut t = term_with_file(Medium::File, "/tmp/tos-frame.rgba", &[0, 255, 0, 255]);
    let base = encode_base64(&[255, 0, 0, 255]);
    t.advance(format!("\x1b_Ga=t,f=32,s=1,v=1,i=5;{base}\x1b\\").as_bytes());
    t.take_output();

    // A frame is transmitted the same way an image is, so it reaches a file
    // the same way: the name is resolved once, before either is stored.
    let payload = encode_base64(b"/tmp/tos-frame.rgba");
    t.advance(format!("\x1b_Ga=f,f=32,t=f,s=1,v=1,i=5,z=40;{payload}\x1b\\").as_bytes());
    assert_eq!(t.take_output(), b"\x1b_Gi=5;OK\x1b\\".to_vec());
    assert_eq!(t.graphics().image(5).unwrap().frame_count(), 2);
}

#[test]
fn kitty_graphics_refuses_a_file_when_nothing_can_read_one() {
    let mut t = term(10, 4);
    let payload = encode_base64(b"/tmp/tos-preview.png");
    t.advance(format!("\x1b_Ga=T,f=100,t=f,i=3;{payload}\x1b\\").as_bytes());
    assert_eq!(
        t.take_output(),
        b"\x1b_Gi=3;ENOSUP:this terminal cannot read files\x1b\\".to_vec()
    );
}

#[test]
fn damage_is_limited_to_touched_rows() {
    let mut t = term(10, 4);
    t.advance(b"\x1b[3;1H");
    t.clear_damage();
    t.advance(b"x");
    assert!(!t.damage().is_row_dirty(0));
    assert!(!t.damage().is_row_dirty(1));
    assert!(t.damage().is_row_dirty(2));
}

#[test]
fn scrollback_viewport_snaps_back_on_output() {
    let mut t = term(10, 2);
    t.advance(b"a\r\nb\r\nc\r\nd");
    assert!(t.scroll_display(2));
    assert_eq!(t.display_offset(), 2);
    t.advance(b"e");
    assert_eq!(t.display_offset(), 0);
}

// ---------------------------------------------------------------------------
// Regressions found in review
// ---------------------------------------------------------------------------

#[test]
fn a_malformed_color_spec_does_not_panic() {
    // The spec arrives through `from_utf8_lossy`, so a stray byte becomes a
    // multi-byte replacement character; slicing it by byte offset used to
    // abort the whole compositor.
    let mut t = term(10, 2);
    t.advance(b"\x1b]11;#\xff\x07");
    t.advance(b"\x1b]4;1;#\xff\x07");
    t.advance("\x1b]11;#a\u{20ac}\x07".as_bytes());
    t.advance(b"\x1b]10;#\x07");
    // The terminal is still usable afterwards.
    t.advance(b"ok");
    assert_eq!(screen(&t)[0], "ok");
}

#[test]
fn valid_color_specs_still_parse() {
    let mut t = term(10, 2);
    t.advance(b"\x1b]11;#ff0000\x07");
    assert_eq!(t.palette().background, tos_term::Rgb::new(0xff, 0, 0));
    t.advance(b"\x1b]11;rgb:00/ff/00\x07");
    assert_eq!(t.palette().background, tos_term::Rgb::new(0, 0xff, 0));
}

#[test]
fn a_configured_palette_survives_a_reset() {
    let mut t = term(10, 2);
    let mut palette = tos_term::Palette::new();
    palette.background = tos_term::Rgb::new(0x10, 0x20, 0x30);
    palette.set_index(1, tos_term::Rgb::new(0x40, 0x50, 0x60));
    t.set_palette(palette);

    // An application changes the colours and then puts them back. Back means
    // what the machine was configured with, not what tOS ships with.
    t.advance(b"\x1b]11;#ff0000\x07\x1b]4;1;#00ff00\x07");
    t.advance(b"\x1b]111\x07\x1b]104;1\x07");
    assert_eq!(t.palette().background, tos_term::Rgb::new(0x10, 0x20, 0x30));
    assert_eq!(t.palette().index(1), tos_term::Rgb::new(0x40, 0x50, 0x60));
}

#[test]
fn an_enormous_parameter_does_not_wrap_around() {
    let mut t = term(10, 5);
    // Saturating rather than wrapping keeps this a clamp to the last row.
    t.advance(b"\x1b[99999999999;1Hx");
    assert_eq!(screen(&t)[4], "x");
}

#[test]
fn newline_mode_does_not_swallow_a_line_after_a_full_row() {
    let mut t = term(4, 4);
    t.advance(b"\x1b[20h");
    t.advance(b"abcd\ne");
    assert_eq!(screen(&t)[0], "abcd");
    assert_eq!(screen(&t)[1], "e", "the deferred wrap must be cleared");
}

#[test]
fn vertical_position_absolute_respects_origin_mode() {
    let mut t = term(10, 10);
    t.advance(b"\x1b[4;8r\x1b[?6h");
    t.advance(b"\x1b[1dX");
    // Row 1 of the region is absolute row 3.
    assert_eq!(screen(&t)[3], "X");
    assert_eq!(t.cursor().y, 3);
}

#[test]
fn a_scroll_region_past_the_last_row_is_clamped_not_ignored() {
    let mut t = term(10, 10);
    t.advance(b"\x1b[3;6r");
    // A stale region from before a resize must not stick.
    t.advance(b"\x1b[1;40r");
    t.advance(b"\x1b[10;1Ha\n b");
    // With the full screen region restored, the last line scrolls the screen.
    assert_eq!(t.grid().scrollback_len(), 1);
}

#[test]
fn del_and_c1_bytes_are_discarded() {
    let mut t = term(10, 1);
    t.advance(b"ab\x7f\x7f\x7f");
    let cell = t.grid().cell(1, 0).unwrap();
    assert_eq!(cell.ch, 'b');
    assert!(
        cell.zerowidth.is_none(),
        "DEL must not become a combining mark"
    );
    assert_eq!(t.cursor().x, 2);
}

#[test]
fn combining_marks_on_one_cell_are_bounded() {
    let mut t = term(10, 1);
    t.advance(b"a");
    for _ in 0..1000 {
        t.advance("\u{0301}".as_bytes());
    }
    let marks = t.grid().cell(0, 0).unwrap().zerowidth.clone().unwrap();
    assert!(marks.len() <= tos_term::Cell::MAX_ZEROWIDTH);
}

#[test]
fn resizing_on_the_alternate_screen_keeps_the_newest_primary_output() {
    let mut t = term(10, 4);
    t.advance(b"l1\r\nl2\r\nl3\r\nl4");
    t.advance(b"\x1b[?1049h");
    t.resize(10, 2);
    t.advance(b"\x1b[?1049l");
    // The shell's most recent output survives; the oldest is what goes.
    assert_eq!(screen(&t)[0], "l3");
    assert_eq!(screen(&t)[1], "l4");
}

#[test]
fn resizing_on_the_primary_screen_still_keeps_the_newest_output() {
    let mut t = term(10, 4);
    t.advance(b"l1\r\nl2\r\nl3\r\nl4");
    t.resize(10, 2);
    assert_eq!(screen(&t)[0], "l3");
    assert_eq!(screen(&t)[1], "l4");
}

#[test]
fn kitty_graphics_plays_an_animation_and_damages_the_rows_it_covers() {
    let mut t = term(10, 4);
    // An 8x16 image is one cell, placed on row zero.
    let red = encode_base64(&[255, 0, 0, 255].repeat(8 * 16));
    t.advance(format!("\x1b_Ga=T,f=32,s=8,v=16,i=5;{red}\x1b\\").as_bytes());
    let green = encode_base64(&[0, 255, 0, 255].repeat(8 * 16));
    t.advance(format!("\x1b_Ga=f,f=32,s=8,v=16,i=5,z=40;{green}\x1b\\").as_bytes());
    t.advance(b"\x1b_Ga=a,i=5,r=1,z=40,s=3\x1b\\");
    assert!(String::from_utf8(t.take_output()).unwrap().contains("OK"));

    let start = Instant::now();
    assert!(!t.advance_animations(start));
    t.clear_damage();

    assert!(!t.advance_animations(start + Duration::from_millis(20)));
    assert!(!t.damage().is_row_dirty(0));

    assert!(t.advance_animations(start + Duration::from_millis(40)));
    assert!(t.damage().is_row_dirty(0));
    assert!(!t.damage().is_row_dirty(2));
    assert_eq!(t.graphics().image(5).unwrap().data[..4], [0, 255, 0, 255]);
}

#[test]
fn kitty_graphics_composes_one_frame_onto_another() {
    let mut t = term(10, 4);
    let red = encode_base64(&[255, 0, 0, 255].repeat(8 * 16));
    t.advance(format!("\x1b_Ga=T,f=32,s=8,v=16,i=5;{red}\x1b\\").as_bytes());
    let green = encode_base64(&[0, 255, 0, 255].repeat(8 * 16));
    t.advance(format!("\x1b_Ga=f,f=32,s=8,v=16,i=5,z=40;{green}\x1b\\").as_bytes());
    t.take_output();
    t.clear_damage();

    // The left half of frame two onto the left half of frame one, which is
    // the frame on screen, so the row it covers has to repaint.
    t.advance(b"\x1b_Ga=c,i=5,r=2,c=1,w=4,h=16\x1b\\");
    let out = String::from_utf8(t.take_output()).unwrap();
    assert!(
        out.contains("i=5"),
        "response should identify the image: {out}"
    );
    assert!(
        out.contains("OK"),
        "composition should have been accepted: {out}"
    );
    assert!(t.damage().is_row_dirty(0));

    let image = t.graphics().image(5).unwrap();
    assert_eq!(image.data[..4], [0, 255, 0, 255]);
    // Past the rectangle the destination frame is untouched.
    assert_eq!(image.data[16..20], [255, 0, 0, 255]);
}

#[test]
fn kitty_graphics_rejects_composing_a_frame_that_was_never_sent() {
    let mut t = term(10, 4);
    let red = encode_base64(&[255, 0, 0, 255].repeat(8 * 16));
    t.advance(format!("\x1b_Ga=T,f=32,s=8,v=16,i=5;{red}\x1b\\").as_bytes());
    t.take_output();

    // The image is a still, so it has a frame one and nothing else.
    t.advance(b"\x1b_Ga=c,i=5,r=2,c=1\x1b\\");
    let out = String::from_utf8(t.take_output()).unwrap();
    assert!(out.contains("EINVAL"), "response should be an error: {out}");
}

#[test]
fn an_animation_scrolled_out_of_view_asks_for_no_repaint() {
    // A playing animation off the top of the viewport is still playing, but
    // nothing about the screen changes. Calling that a repaint would flip the
    // whole page every gap for a picture nobody can see.
    //
    // It has to be a tall image to get into that state at all: a placement one
    // row high is dropped the moment it scrolls off, so it is gone before
    // there is any history to look back through.
    let mut t = term(10, 6);
    let red = encode_base64(&[255, 0, 0, 255].repeat(8 * 48));
    t.advance(format!("\x1b_Ga=T,f=32,s=8,v=48,i=5;{red}\x1b\\").as_bytes());
    let green = encode_base64(&[0, 255, 0, 255].repeat(8 * 48));
    t.advance(format!("\x1b_Ga=f,f=32,s=8,v=48,i=5,z=40;{green}\x1b\\").as_bytes());
    t.advance(b"\x1b_Ga=a,i=5,r=1,z=40,s=3\x1b\\");
    t.take_output();

    for _ in 0..6 {
        t.advance(b"\r\n");
    }
    assert!(t.scroll_display(3), "the test needs scrollback to look at");
    let placed: Vec<i64> = t
        .graphics()
        .placements()
        .map(|p| p.row as i64 - t.display_offset() as i64)
        .collect();
    assert_eq!(placed, vec![-3], "the image should be just off the top");

    let start = Instant::now();
    t.advance_animations(start);
    t.clear_damage();
    assert!(
        !t.advance_animations(start + Duration::from_millis(40)),
        "an animation nobody can see asked for a repaint"
    );
    assert!(!t.damage().is_dirty());
    // It is still playing; only the repaint was declined.
    assert_eq!(t.graphics().image(5).unwrap().current_frame(), 2);
}

#[test]
fn an_animation_half_on_screen_still_repaints() {
    // The other side of the same rule: one row of it showing is still showing.
    let mut t = term(10, 6);
    let red = encode_base64(&[255, 0, 0, 255].repeat(8 * 48));
    t.advance(format!("\x1b_Ga=T,f=32,s=8,v=48,i=5;{red}\x1b\\").as_bytes());
    let green = encode_base64(&[0, 255, 0, 255].repeat(8 * 48));
    t.advance(format!("\x1b_Ga=f,f=32,s=8,v=48,i=5,z=40;{green}\x1b\\").as_bytes());
    t.advance(b"\x1b_Ga=a,i=5,r=1,z=40,s=3\x1b\\");
    t.take_output();

    for _ in 0..4 {
        t.advance(b"\r\n");
    }
    assert!(t.scroll_display(1));

    let start = Instant::now();
    t.advance_animations(start);
    t.clear_damage();
    assert!(t.advance_animations(start + Duration::from_millis(40)));
    assert!(t.damage().is_row_dirty(0));
}

#[test]
fn kitty_graphics_rejects_frames_for_images_that_were_never_sent() {
    let mut t = term(10, 4);
    t.advance(b"\x1b_Ga=f,f=32,s=1,v=1,i=9;AAAAAA==\x1b\\");
    let out = String::from_utf8(t.take_output()).unwrap();
    assert!(
        out.contains("i=9"),
        "response should identify the image: {out}"
    );
    assert!(out.contains("ENOENT"), "response should be an error: {out}");
}

#[test]
fn an_unplaced_animation_asks_for_no_repaint() {
    let mut t = term(10, 4);
    let pixels = encode_base64(&[0u8; 4]);
    // a=t stores without placing, so nothing on screen shows these frames.
    t.advance(format!("\x1b_Ga=t,f=32,s=1,v=1,i=6;{pixels}\x1b\\").as_bytes());
    t.advance(format!("\x1b_Ga=f,f=32,s=1,v=1,i=6,z=40;{pixels}\x1b\\").as_bytes());
    t.advance(b"\x1b_Ga=a,i=6,r=1,z=40,s=3\x1b\\");

    let start = Instant::now();
    t.advance_animations(start);
    t.clear_damage();
    assert!(!t.advance_animations(start + Duration::from_millis(40)));
    assert!(!t.damage().is_dirty());
    // The frame still moved on, so the next placement shows the right one.
    assert_eq!(t.graphics().image(6).unwrap().current_frame(), 2);
}

#[test]
fn kitty_graphics_repaints_a_placement_whose_image_was_retransmitted() {
    let mut t = term(10, 4);
    let red = encode_base64(&[255, 0, 0, 255].repeat(8 * 16));
    t.advance(format!("\x1b_Ga=T,f=32,s=8,v=16,i=5;{red}\x1b\\").as_bytes());
    t.take_output();
    t.clear_damage();

    // Sending the image again under the placement that is already showing it
    // is how a program streams: place once, then transmit a frame at a time.
    // The pixels on screen have changed, so the rows they cover have to
    // repaint — nothing else in the session is going to notice that they did.
    let green = encode_base64(&[0, 255, 0, 255].repeat(8 * 16));
    t.advance(format!("\x1b_Ga=t,f=32,s=8,v=16,i=5;{green}\x1b\\").as_bytes());

    assert_eq!(t.graphics().image(5).unwrap().data[..4], [0, 255, 0, 255]);
    assert!(
        t.damage().is_row_dirty(0),
        "retransmitting an image under a live placement must repaint it"
    );
}
