//! The message of the day.
//!
//! `.motd_art` is the tOS banner. It is compiled into the installer so the
//! welcome screen always has it, and it is also written into the live image so
//! that every shell prints it along with the one line that matters on a live
//! ISO: how to put tOS on the disk.
//!
//! The banner on a machine does not have to be the one tOS ships. A shell can
//! print whatever is in the file, escape sequences and all, but the installer
//! draws into a cell buffer of its own, so it has to understand the colours
//! rather than pass them through. [`art_runs`] is that: it turns a banner into
//! styled runs, which is what makes a picture produced by something like
//! `chafa` usable as the banner and not just the text of its escapes.
//!
//! ## The picture
//!
//! A tOS pane can do better than a banner drawn in cells, and since #132 there
//! is a picture on the machine to show it: `/etc/tos/splash.png`, the same file
//! the login screen draws. So the greeting asks the terminal what it is, and a
//! pane gets the picture itself over the graphics protocol while everything
//! else goes on getting the drawn banner.
//!
//! The picture is sent as a *path* (`t=f`) rather than as a payload, which is
//! what makes this affordable: the escape is a hundred bytes however large the
//! picture is, and the compositor reads the file it already has open on the
//! login screen. `tos-preview` takes the same route for the same reason, and
//! [`place`] is deliberately the sequence its `transmit::sequence` builds —
//! room scrolled up first, because `a=T` clips at the bottom of the screen
//! rather than scrolling, and the cursor walked back down afterwards.
//!
//! ## And the picture drawn in cells
//!
//! A terminal at the far end of an `ssh` cannot be sent the picture — that is
//! what [`Screen::probe`] refuses — but it can draw one, in the half blocks
//! `chafa` produces and [`art_runs`] has always parsed. So there is a third
//! rung between the two (#162): [`ASCII`], the same splash rendered into
//! cells once and checked in, printed by any terminal the whole greeting fits
//! on. The ladder is the picture, then this, then the drawn banner, then
//! [`ART_SMALL`]: each rung is what the one above gives way to, and the
//! bottom one always arrives.

use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::RawFd;
use std::path::Path;

use tos_platform::tty;
use tos_term::graphics::encode_base64;
use tos_term::Palette;

use crate::ui::{Color, Style};

/// The banner, as it ships.
pub const ART: &str = include_str!("../../.motd_art");

/// The banner for a screen too small for a picture: one line that still says
/// what this machine is.
pub const ART_SMALL: &str = "tOS — the terminal is the desktop";

/// Where the live image keeps the art, so it can be changed without a rebuild.
pub const ART_PATH: &str = "/etc/tos/motd_art";

/// The picture drawn in cells, for a terminal that cannot be sent the file.
///
/// Half blocks with a true colour foreground and background: two rows of
/// picture in every row of cells, which is what `chafa` produces and what
/// [`art_runs`] was written to read. It is checked in rather than rendered
/// during the build because rendering it wants `chafa`, and a banner is not
/// worth a build dependency — `docs/design/splash.md` says the command that
/// makes it again when the picture changes.
pub const ASCII: &str = include_str!("../../.motd_ascii");

/// Where the live image keeps that render, so it too can be changed without a
/// rebuild. The same door [`ART_PATH`] opens, for the same reason.
pub const ASCII_PATH: &str = "/etc/tos/motd_ascii";

/// The picture a pane is shown instead of the drawn banner.
///
/// The file the compositor's login screen draws, put on the image by
/// `iso/mkiso.sh` beside the banner and carried onto a disk by the installer
/// with the rest of `/etc`. A machine that replaced it replaced both screens
/// at once, which is the point of there being one file.
pub const PICTURE_PATH: &str = "/etc/tos/splash.png";

/// What the banner says in words, kept under the picture, which does not.
///
/// A copy of the last line of [`ART`], and the test below is what keeps the
/// two saying the same thing.
const TAGLINE: &str = "the terminal is the desktop";

/// The colour that line is drawn in, which is the colour it has in the art.
const TAGLINE_COLOUR: &str = "\x1b[38;2;127;138;154m";

/// The environment variable that says this shell is inside a tOS session.
///
/// Exported by `iso/live-session`, which is what starts every tOS session.
const SESSION_MARK: &str = "TOS";

/// The smallest picture worth showing, in cells. Below this the drawn banner
/// says more, and it is what a terminal this small gets.
const MIN_CELLS: (u32, u32) = (16, 4);

/// The command that installs tOS.
pub const INSTALL_COMMAND: &str = "tos-install";

/// Written by the installer onto the disk, and never present on the live
/// image. Its existence is the whole of how a shell tells the two apart.
///
/// A machine cannot work this out for itself: an installed tOS and a live one
/// run the same binaries out of the same layout, and by the time a shell asks,
/// the only difference left is that somebody chose to put this one on a disk.
/// So the installer records that it did.
pub const INSTALLED_PATH: &str = "/etc/tos/installed";

/// Whether this machine was installed rather than booted from a medium.
pub fn is_installed() -> bool {
    std::path::Path::new(INSTALLED_PATH).exists()
}

/// Load the banner, preferring the copy on this machine.
pub fn art() -> String {
    std::fs::read_to_string(ART_PATH).unwrap_or_else(|_| ART.to_string())
}

/// Load the picture drawn in cells, preferring the copy on this machine.
pub fn ascii() -> String {
    std::fs::read_to_string(ASCII_PATH).unwrap_or_else(|_| ASCII.to_string())
}

/// A run of characters in the banner that share one style.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub text: String,
    pub style: Style,
}

/// The banner as lines of styled runs, with trailing blank lines removed.
///
/// A banner with no escape sequences in it comes back as one default-styled
/// run per line, which is the shape the tOS banner has always had.
pub fn art_runs(art: &str) -> Vec<Vec<Run>> {
    let palette = Palette::new();
    let mut lines: Vec<Vec<Run>> = Vec::new();
    let mut line: Vec<Run> = Vec::new();
    let mut style = Style::default();
    let mut chars = art.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\n' => lines.push(std::mem::take(&mut line)),
            '\r' => {}
            '\x1b' => {
                if chars.peek() != Some(&'[') {
                    continue;
                }
                chars.next();
                let mut params = String::new();
                let mut final_byte = None;
                for c in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&c) {
                        final_byte = Some(c);
                        break;
                    }
                    params.push(c);
                }
                // Only SGR says how a cell looks. Everything else in a banner
                // is something a shell would act on and a cell buffer cannot:
                // chafa, for one, hides the cursor around its output.
                if final_byte == Some('m') && !params.starts_with('?') {
                    apply_sgr(&params, &palette, &mut style);
                }
            }
            _ => match line.last_mut() {
                Some(run) if run.style == style => run.text.push(c),
                _ => line.push(Run {
                    text: c.to_string(),
                    style,
                }),
            },
        }
    }
    lines.push(line);

    while lines
        .last()
        .map(|line| line.iter().all(|run| run.text.trim().is_empty()))
        .unwrap_or(false)
    {
        lines.pop();
    }
    lines
}

/// The banner as plain lines, with the styling and the trailing blanks gone.
pub fn art_lines(art: &str) -> Vec<String> {
    art_runs(art)
        .iter()
        .map(|line| line.iter().map(|run| run.text.as_str()).collect())
        .collect()
}

/// How wide the banner is, in cells. Escape sequences take up none of them.
pub fn art_width(art: &str) -> usize {
    art_lines(art)
        .iter()
        .map(|line| tos_term::str_width(line))
        .max()
        .unwrap_or(0)
}

/// Apply one SGR sequence's parameters to `style`.
fn apply_sgr(params: &str, palette: &Palette, style: &mut Style) {
    // An omitted parameter means zero, which is the reset.
    let numbers: Vec<u16> = params.split(';').map(|p| p.parse().unwrap_or(0)).collect();
    let mut i = 0;
    while i < numbers.len() {
        match numbers[i] {
            0 => *style = Style::default(),
            1 => style.bold = true,
            2 => style.dim = true,
            7 => style.reverse = true,
            22 => {
                style.bold = false;
                style.dim = false;
            }
            27 => style.reverse = false,
            n @ 30..=37 => style.fg = Color::Rgb(palette.index((n - 30) as u8)),
            39 => style.fg = Color::Default,
            n @ 40..=47 => style.bg = Color::Rgb(palette.index((n - 40) as u8)),
            49 => style.bg = Color::Default,
            n @ 90..=97 => style.fg = Color::Rgb(palette.index((n - 90) as u8 + 8)),
            n @ 100..=107 => style.bg = Color::Rgb(palette.index((n - 100) as u8 + 8)),
            n @ (38 | 48) => {
                let (color, eaten) = extended_color(&numbers[i + 1..], palette);
                if let Some(color) = color {
                    if n == 38 {
                        style.fg = color;
                    } else {
                        style.bg = color;
                    }
                }
                i += eaten;
            }
            // An attribute this screen cannot draw is not a reason to lose the
            // rest of the sequence.
            _ => {}
        }
        i += 1;
    }
}

/// The colour after a `38` or `48`, and how many parameters it used.
fn extended_color(rest: &[u16], palette: &Palette) -> (Option<Color>, usize) {
    match rest.first() {
        Some(2) if rest.len() >= 4 => (
            Some(Color::rgb(rest[1] as u8, rest[2] as u8, rest[3] as u8)),
            4,
        ),
        Some(5) if rest.len() >= 2 => (Some(Color::Rgb(palette.index(rest[1] as u8))), 2),
        // Malformed: give up on this sequence rather than read its tail as
        // attributes of its own.
        _ => (None, rest.len()),
    }
}

/// The keys the greeting names, and the one thing in tOS that says what a
/// binding is without asking the keymap.
///
/// Everything else — the sheet over the panes, `tos --help` — is generated
/// from the map that is resolving keys, and cannot come to disagree with it.
/// This cannot be: it is printed by a shell profile on a machine where the
/// compositor is not the thing running, so it is a copy, and a copy has to be
/// kept. It leads with the ctrl+shift combinations because they are the ones a
/// person arrives already knowing from a terminal emulator, and names the
/// leader underneath because that is what still works in a nested session,
/// which is most of what anybody develops in.
const KEYS: &str = "\x20 ctrl+shift+enter  split     ctrl+shift+w  close the pane\n\
                    \x20 ctrl+shift+t  new workspace ctrl+shift+]  the next pane\n\
                    \x20 ctrl+a d  split beside      ctrl+a s  split below\n\
                    \x20 ctrl+a h/j/k/l  move focus  ctrl+a q  quit\n";

/// The message a shell prints when it starts on the live image.
///
/// This is the whole reason the art exists in a file rather than in the
/// compositor: a person who boots the ISO should not have to be told
/// separately how to install it.
pub fn live_message() -> String {
    live_message_under(art())
}

fn live_message_under(banner: String) -> String {
    let mut out = under(banner);
    out.push_str(&format!(
        "  This is a live session: nothing is written to disk.\n\
         \n\
         \x20 Type \x1b[1m{INSTALL_COMMAND}\x1b[0m to install tOS on this machine.\n\
         \x20 Type \x1b[1mexit\x1b[0m to close this pane.\n\
         \n\
         {KEYS}\n"
    ));
    out
}

/// The greeting for a machine that has been installed.
///
/// The banner and the keys, and nothing about installing: the disk is already
/// the answer to that question, and inviting somebody to install the machine
/// they are standing in is worse than saying nothing.
pub fn installed_message() -> String {
    installed_message_under(art())
}

fn installed_message_under(banner: String) -> String {
    let mut out = under(banner);
    out.push_str(&format!(
        "\x20 Type \x1b[1mexit\x1b[0m to close this pane.\n\
         \n\
         {KEYS}\n"
    ));
    out
}

/// A banner with the blank line after it that everything else is printed
/// below.
fn under(mut banner: String) -> String {
    if !banner.ends_with('\n') {
        banner.push('\n');
    }
    banner.push('\n');
    banner
}

/// The greeting for whichever machine this is.
pub fn message() -> String {
    greeting_under(art())
}

/// The greeting for whichever machine this is, on whichever terminal this is.
///
/// A pane gets the picture. A terminal that cannot be sent one but has room to
/// draw it — an `ssh` from anywhere, most often — gets the same picture in
/// cells. Everything left over gets the banner, which fits anything and is
/// certain to arrive: `size` is `None` for a terminal that could not even be
/// asked how big it is.
pub fn greeting(screen: Option<Screen>, size: Option<(u32, u32)>) -> String {
    greeting_from(screen, size, Path::new(PICTURE_PATH))
}

/// The same, with the picture named rather than assumed.
///
/// A seam for the reason the compositor's paths are seams: a test that could
/// not say where the picture is would have to put one in `/etc` on the machine
/// running it. Not a setting — the greeting and the login screen draw the same
/// file on purpose, and a second way to name it would be a way for them to
/// disagree.
pub fn greeting_from(screen: Option<Screen>, size: Option<(u32, u32)>, path: &Path) -> String {
    if let Some(sent) = screen.and_then(|screen| picture(screen, path)) {
        return greeting_under(sent);
    }
    if let Some((cols, rows)) = size {
        let drawn = greeting_under(ascii_banner());
        if fits(&drawn, cols, rows) {
            return drawn;
        }
    }
    message()
}

/// The picture drawn in cells, with the line it does not say underneath it.
fn ascii_banner() -> String {
    let mut art = ascii();
    if !art.ends_with('\n') {
        art.push('\n');
    }
    let width = art_width(&art) as u32;
    art.push_str(&tagline(width));
    art
}

/// Whether a greeting reaches a terminal this size as it was drawn.
///
/// Width is the question that matters. One cell too wide and every line wraps,
/// and a picture whose every other row starts a column further along is not a
/// picture — where the banner that fits is at least what it was meant to be.
///
/// Height is asked too, and not because text cannot scroll. The rows above the
/// prompt are all anybody sees without reaching for the scrollback, and a
/// greeting with three rows of somebody's hair at the top of it says less than
/// the small banner it gave way to. Both are the same rule the picture already
/// follows for the same reason (`fit`): the banner is the part that gives way,
/// because the ten lines under it are the part being read.
fn fits(greeting: &str, cols: u32, rows: u32) -> bool {
    art_width(greeting) as u32 <= cols && art_lines(greeting).len() as u32 <= rows
}

/// How big the terminal on `fd` is, in cells, whatever terminal it is.
///
/// [`Screen::probe`] answers for the one terminal a picture can be sent to and
/// says nothing about any other, because that is all a picture needs to know.
/// This is the question every terminal answers: an `ssh`, a serial line and a
/// kernel VT all fill in `ws_col` and `ws_row` where they leave the pixel
/// fields zero. `None` is something that is not a terminal at all — a pipe
/// into `less`, or a shell started with no tty — which gets the banner.
pub fn cells(fd: RawFd) -> Option<(u32, u32)> {
    let size = tty::terminal_size(fd).ok()?;
    if size.cols == 0 || size.rows == 0 {
        return None;
    }
    Some((size.cols as u32, size.rows as u32))
}

fn greeting_under(banner: String) -> String {
    if is_installed() {
        installed_message_under(banner)
    } else {
        live_message_under(banner)
    }
}

/// What a terminal has said about itself, when it has said enough to be sent
/// a picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Screen {
    pub cols: u32,
    pub rows: u32,
    /// The pixel size of one cell.
    pub cell: (u32, u32),
}

impl Screen {
    /// The terminal on `fd`, if a picture can be put on it.
    ///
    /// Two questions, and both have to be answered yes.
    ///
    /// **Is this a tOS session?** `TOS` is exported by `iso/live-session`, so
    /// it is set for everything a tOS machine starts and for nothing somebody
    /// brought with them. Without it this could be any terminal at the far end
    /// of an `ssh`, and a graphics command such a terminal does not know is
    /// not ignored — it is printed, as the text of its own escape.
    ///
    /// **Did the terminal say how big a cell is?** `TIOCGWINSZ` carries pixel
    /// fields, and tOS fills them in for every pane it spawns
    /// (`tos_compositor::pane::winsize_for`) where the kernel's own VT leaves
    /// them zero. So this is what tells a pane from the console the rescue
    /// session lands on, which is inside a tOS session and cannot draw a
    /// thing. It is also the number the picture has to be sized against, so
    /// it would have had to be asked for anyway.
    pub fn probe(fd: RawFd) -> Option<Screen> {
        std::env::var_os(SESSION_MARK)?;
        let size = tty::terminal_size(fd).ok()?;
        if size.width_px == 0 || size.height_px == 0 || size.cols == 0 || size.rows == 0 {
            return None;
        }
        Some(Screen {
            cols: size.cols as u32,
            rows: size.rows as u32,
            cell: (
                (size.width_px as u32 / size.cols as u32).max(1),
                (size.height_px as u32 / size.rows as u32).max(1),
            ),
        })
    }
}

/// The picture at `path` as the bytes that put it on `screen`, with the line
/// the banner would have said underneath it.
///
/// `None` when there is no picture there, when what is there does not have a
/// PNG header, or when the terminal is too small to be worth one — each of
/// which is the drawn banner instead, and none of which is an error.
///
/// Only the header is read, because only its two numbers are needed and the
/// greeting is printed by every shell that starts. That leaves one case this
/// cannot see: a file whose header is good and whose pixels are not, which is
/// a picture the compositor refuses and so a greeting with a gap where the
/// picture was. Decoding a hundred kilobytes at every prompt to rule it out
/// would cost every shell something to save a broken file from looking broken.
pub fn picture(screen: Screen, path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let size = tos_term::png::dimensions(&bytes).ok()?;
    let cells = fit(screen, size)?;
    Some(format!("{}{}", place(path, cells), tagline(cells.0)))
}

/// What the banner says in words, centred under something that says it in
/// pixels: the picture, and the picture drawn in cells.
fn tagline(width: u32) -> String {
    let indent = " ".repeat(((width.saturating_sub(TAGLINE.len() as u32)) / 2) as usize);
    format!("{indent}{TAGLINE_COLOUR}{TAGLINE}\x1b[0m\n")
}

/// How many cells to give the picture.
///
/// Its own size where there is room for it, so that one picture pixel is one
/// screen pixel and the pixel art it is stays pixel art. Narrower where the
/// pane is narrower, and never more than half the pane tall, because the
/// greeting has ten more lines to print underneath and a picture that pushed
/// them off the top would be a picture instead of a greeting.
fn fit(screen: Screen, picture: (u32, u32)) -> Option<(u32, u32)> {
    let (cw, ch) = (screen.cell.0.max(1), screen.cell.1.max(1));
    let mut cols = picture.0.div_ceil(cw).max(1);
    let mut rows = picture.1.div_ceil(ch).max(1);
    if cols > screen.cols {
        rows = (rows * screen.cols / cols).max(1);
        cols = screen.cols;
    }
    let tall = screen.rows / 2;
    if rows > tall {
        cols = (cols * tall / rows).max(1);
        rows = tall;
    }
    if cols < MIN_CELLS.0 || rows < MIN_CELLS.1 {
        return None;
    }
    Some((cols, rows))
}

/// The graphics command that puts the file at `path` on the screen, and the
/// cursor movement around it.
///
/// `a=T` places at the cursor and clips at the bottom of the screen rather
/// than scrolling to make room, so the room is scrolled up first and the
/// cursor walked back into it; `C=1` keeps the terminal from moving the
/// cursor itself, and the walk back down leaves it on the row below the
/// picture. `q=2` asks for no reply: nothing is reading this program's input,
/// and an answer would be collected by the shell as typing.
fn place(path: &Path, (cols, rows): (u32, u32)) -> String {
    let name = encode_base64(path.as_os_str().as_bytes());
    let mut out = String::from("\r");
    for _ in 0..rows {
        out.push('\n');
    }
    out.push_str(&format!("\x1b[{rows}A"));
    out.push_str(&format!(
        "\x1b_Ga=T,f=100,t=f,c={cols},r={rows},C=1,q=2;{name}\x1b\\"
    ));
    out.push_str(&format!("\x1b[{rows}B\r"));
    out
}

/// The same message with no escape sequences, for a console that has none.
pub fn live_message_plain() -> String {
    strip_sgr(&live_message())
}

/// Remove SGR sequences from text.
fn strip_sgr(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        if chars.peek() == Some(&'[') {
            chars.next();
            // Everything up to and including the final byte belongs to the
            // sequence.
            for c in chars.by_ref() {
                if ('\x40'..='\x7e').contains(&c) {
                    break;
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_art_is_compiled_in() {
        assert!(!ART.trim().is_empty());
        assert!(ART.contains('█'), "the banner should be drawn in blocks");
    }

    #[test]
    fn the_art_says_what_tos_is() {
        assert!(ART.contains("the terminal is the desktop"));
    }

    #[test]
    fn the_art_fits_a_narrow_console() {
        // Eighty columns is the floor for any terminal, and the banner has to
        // fit inside a pane that is narrower than the screen.
        assert!(
            art_width(ART) <= 60,
            "the banner is {} cells wide",
            art_width(ART)
        );
    }

    /// A banner in the shape `chafa` produces: block characters carrying a
    /// true colour foreground and background, wrapped in the cursor hiding it
    /// puts around its output.
    const PICTURE: &str = "\x1b[?25l\x1b[0m\x1b[38;2;10;20;30m\u{2584}\u{2584}\x1b[38;2;1;2;3;48;2;4;5;6m\u{2580}\x1b[0m\n\
                           \x1b[7m\u{2588}\x1b[0m \x1b[38;5;196m\u{2588}\x1b[0m\n\x1b[?25h";

    #[test]
    fn a_picture_keeps_its_colours() {
        let lines = art_runs(PICTURE);
        assert_eq!(lines.len(), 2);
        // Two colours on the first line, so two runs.
        assert_eq!(lines[0].len(), 2);
        assert_eq!(lines[0][0].text, "\u{2584}\u{2584}");
        assert_eq!(lines[0][0].style.fg, Color::rgb(10, 20, 30));
        assert_eq!(lines[0][1].style.fg, Color::rgb(1, 2, 3));
        assert_eq!(lines[0][1].style.bg, Color::rgb(4, 5, 6));
    }

    #[test]
    fn a_reset_ends_a_colour_and_reverse_is_kept() {
        let lines = art_runs(PICTURE);
        let reversed = &lines[1][0];
        assert!(reversed.style.reverse);
        assert_eq!(reversed.style.fg, Color::Default, "\\x1b[7m sets no colour");
        // The space after it carries nothing at all.
        assert_eq!(lines[1][1].text, " ");
        assert_eq!(lines[1][1].style, Style::default());
    }

    #[test]
    fn an_indexed_colour_resolves_through_the_palette() {
        let lines = art_runs(PICTURE);
        let indexed = lines[1].last().unwrap();
        assert_eq!(
            indexed.style.fg,
            Color::Rgb(Palette::new().index(196)),
            "38;5;196 should be the palette entry"
        );
    }

    #[test]
    fn the_sequences_a_cell_buffer_cannot_act_on_are_dropped() {
        // Hiding the cursor is a shell's business; the text must not keep it.
        let plain: String = art_lines(PICTURE).join("");
        assert!(!plain.contains('\x1b'), "{plain:?} still has an escape");
        assert!(
            !plain.contains("25"),
            "{plain:?} kept a sequence's parameters"
        );
    }

    #[test]
    fn a_picture_is_measured_in_cells_not_bytes() {
        // Counting the escapes would make this banner dozens of cells wide.
        assert_eq!(art_width(PICTURE), 3);
    }

    #[test]
    fn art_with_no_escapes_in_it_carries_no_style() {
        let lines = art_runs("one\ntwo\n");
        assert_eq!(lines.len(), 2);
        for line in &lines {
            assert_eq!(line.len(), 1, "plain art is one run per line");
            assert_eq!(line[0].style, Style::default());
        }
    }

    // ---- the picture (#132) ---------------------------------------------

    /// A terminal that says it is a tOS pane 100 cells wide, with the cell
    /// size tOS gives a pane at its default font.
    fn pane(cols: u32, rows: u32) -> Screen {
        Screen {
            cols,
            rows,
            cell: (8, 16),
        }
    }

    /// A file holding a PNG of this size, and its path.
    fn picture_file(name: &str, width: u32, height: u32) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("tos-motd-{}-{name}.png", std::process::id()));
        std::fs::write(&path, png_of(width, height)).expect("picture file");
        path
    }

    #[test]
    fn the_line_under_the_picture_is_the_one_the_banner_draws() {
        // The picture has no words in it, so the greeting says them. Two
        // copies of one sentence, and this is what keeps them one sentence.
        assert!(
            ART.contains(TAGLINE),
            "the art no longer says {TAGLINE:?}, so the picture should not either"
        );
    }

    #[test]
    fn a_picture_gets_its_own_size_where_there_is_room() {
        // 512x170 at an 8x16 cell is 64 cells by 11, which is one picture
        // pixel per screen pixel — the whole reason to ask the terminal how
        // big a cell is.
        assert_eq!(fit(pane(100, 40), (512, 170)), Some((64, 11)));
    }

    #[test]
    fn a_picture_is_never_wider_than_the_pane() {
        let (cols, rows) = fit(pane(40, 40), (512, 170)).expect("a picture");
        assert_eq!(cols, 40);
        assert!(rows < 11, "a narrowed picture kept its height: {rows}");
    }

    #[test]
    fn a_picture_never_takes_more_than_half_the_pane() {
        // Ten more lines are printed under it, and a greeting whose picture
        // pushed them off the top would be a picture instead of a greeting.
        let (_, rows) = fit(pane(100, 12), (512, 170)).expect("a picture");
        assert_eq!(rows, 6);
    }

    #[test]
    fn a_terminal_too_small_for_a_picture_gets_none() {
        assert_eq!(fit(pane(10, 40), (512, 170)), None);
        assert_eq!(fit(pane(100, 4), (512, 170)), None);
    }

    #[test]
    fn the_picture_is_sent_as_a_path_and_not_as_a_payload() {
        // The whole point: the escape is the same hundred bytes whatever the
        // picture weighs, because the compositor opens the file itself.
        let path = picture_file("path", 512, 170);
        let sent = picture(pane(100, 40), &path).expect("a picture");
        assert!(sent.contains("t=f"), "{sent:?} is not a file transmission");
        assert!(sent.contains("f=100"), "{sent:?} does not say it is a PNG");
        assert!(sent.contains("c=64,r=11"), "{sent:?} is the wrong size");
        assert!(
            sent.contains(&encode_base64(path.as_os_str().as_bytes())),
            "{sent:?} does not name the file"
        );
        assert!(
            !sent.contains(&encode_base64(&png_of(512, 170))),
            "the picture itself went through the terminal"
        );
        assert!(sent.len() < 400, "{} bytes for one picture", sent.len());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_picture_is_given_room_before_it_is_drawn_and_left_below() {
        // `a=T` clips at the bottom of the screen rather than scrolling, so
        // the rows are scrolled up first and the cursor walked back into
        // them; afterwards it has to be below the picture, or the next prompt
        // prints over it.
        let path = picture_file("room", 512, 170);
        let sent = picture(pane(100, 40), &path).expect("a picture");
        assert!(sent.starts_with("\r\n\n\n\n\n\n\n\n\n\n\n\x1b[11A"));
        let after = sent
            .split("\x1b\\")
            .nth(1)
            .expect("something after the command");
        assert!(after.starts_with("\x1b[11B\r"), "{after:?}");
        assert!(after.contains(TAGLINE));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_file_that_is_not_a_picture_is_no_picture_rather_than_an_error() {
        let path = std::env::temp_dir().join(format!("tos-motd-bad-{}.png", std::process::id()));
        std::fs::write(&path, b"this is not a PNG").expect("write");
        assert_eq!(picture(pane(100, 40), &path), None);
        assert_eq!(
            picture(
                pane(100, 40),
                std::path::Path::new("/nonexistent/splash.png")
            ),
            None
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_terminal_that_cannot_be_asked_is_not_sent_a_picture() {
        // Nothing is on file descriptor -1, and a greeting has to come out
        // anyway.
        assert_eq!(Screen::probe(-1), None);
        assert_eq!(greeting(None, None), message());
    }

    #[test]
    fn the_greeting_with_a_picture_still_says_everything_it_said() {
        let path = picture_file("greeting", 512, 170);
        let sent = picture(pane(100, 40), &path).expect("a picture");
        let greeting = greeting_under(sent);
        assert!(greeting.contains(TAGLINE));
        assert!(greeting.contains("ctrl+shift+enter"));
        assert!(greeting.contains("Type"));
        assert!(
            !greeting.contains('\u{2588}'),
            "the drawn banner was printed under the picture as well"
        );
        let _ = std::fs::remove_file(&path);
    }

    // ---- the picture drawn in cells (#162) -------------------------------

    /// The smallest terminal the render is chosen on, worked out from the
    /// greeting itself: the tests below are then the rule rather than a copy
    /// of whatever the file in the tree happens to measure today.
    fn smallest() -> (u32, u32) {
        let greeting = greeting_under(ascii_banner());
        (
            art_width(&greeting) as u32,
            art_lines(&greeting).len() as u32,
        )
    }

    #[test]
    fn the_render_is_compiled_in() {
        assert!(!ASCII.trim().is_empty());
        assert!(
            ASCII.contains('\u{2584}'),
            "the render should be drawn in half blocks"
        );
        assert!(
            ASCII.contains("\x1b[38;2;"),
            "the render should carry true colour"
        );
    }

    #[test]
    fn the_render_is_a_rectangle() {
        // A ragged line is a line that was measured wrong, and the fit is
        // decided on the widest one.
        let widths: Vec<usize> = art_lines(ASCII)
            .iter()
            .map(|line| tos_term::str_width(line))
            .collect();
        assert!(
            widths.windows(2).all(|pair| pair[0] == pair[1]),
            "the render is ragged: {widths:?}"
        );
    }

    #[test]
    fn a_terminal_with_room_for_it_is_shown_the_picture_in_cells() {
        let (cols, rows) = smallest();
        let greeting = greeting(None, Some((cols, rows)));
        assert!(
            greeting.contains('\u{2584}'),
            "a terminal with room got no render"
        );
        assert!(
            greeting.contains(TAGLINE),
            "the render has no words of its own, so the greeting says them"
        );
        assert!(
            greeting.contains("ctrl+shift+enter"),
            "the keys are part of the greeting whatever is above them"
        );
        assert!(
            !greeting.contains('\u{2588}'),
            "the drawn banner was printed as well"
        );
    }

    #[test]
    fn one_cell_too_narrow_or_too_short_is_the_drawn_banner() {
        // Wrapping is what this is avoiding, and a picture mostly above the
        // screen says less than a small one wholly on it.
        let (cols, rows) = smallest();
        assert_eq!(greeting(None, Some((cols - 1, rows))), message());
        assert_eq!(greeting(None, Some((cols, rows - 1))), message());
    }

    #[test]
    fn a_terminal_that_cannot_be_asked_its_size_gets_the_drawn_banner() {
        // Nothing is on file descriptor -1, and a greeting has to come out.
        assert_eq!(cells(-1), None);
        assert_eq!(greeting(None, None), message());
    }

    #[test]
    fn an_eighty_column_console_gets_the_banner_it_always_got() {
        // The floor for any terminal, and under the render's width: a serial
        // line and a kernel VT are exactly where the small banner has to win.
        assert_eq!(greeting(None, Some((80, 24))), message());
    }

    #[test]
    fn a_pane_is_still_sent_the_picture_rather_than_the_render() {
        // Both rungs are available on a pane this size. The picture is real
        // pixels and wins.
        let path = picture_file("cells", 512, 170);
        let (cols, rows) = smallest();
        let sent = greeting_from(Some(pane(cols, rows)), Some((cols, rows)), &path);
        assert!(sent.contains("t=f"), "the pane lost its picture");
        assert!(
            !sent.contains('\u{2584}'),
            "the render was sent to the pane as well"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A PNG of this size: one stored deflate block of opaque white, written
    /// out by hand so the tests depend on no encoder.
    fn png_of(width: u32, height: u32) -> Vec<u8> {
        let mut raw = Vec::new();
        for _ in 0..height {
            raw.push(0u8);
            raw.extend(std::iter::repeat_n(0xffu8, width as usize * 4));
        }
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&width.to_be_bytes());
        ihdr.extend_from_slice(&height.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);

        let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        png.extend_from_slice(&chunk(b"IHDR", &ihdr));
        png.extend_from_slice(&chunk(b"IDAT", &zlib_stored(&raw)));
        png.extend_from_slice(&chunk(b"IEND", &[]));
        png
    }

    fn chunk(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = (body.len() as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        let mut checked = kind.to_vec();
        checked.extend_from_slice(body);
        out.extend_from_slice(&crc32(&checked).to_be_bytes());
        out
    }

    /// Deflate that compresses nothing: stored blocks, which is all a test
    /// needs and the one encoding that can be written in ten lines.
    fn zlib_stored(data: &[u8]) -> Vec<u8> {
        let mut out = vec![0x78, 0x01];
        let mut blocks = data.chunks(0xffff).peekable();
        while let Some(block) = blocks.next() {
            out.push(u8::from(blocks.peek().is_none()));
            out.extend_from_slice(&(block.len() as u16).to_le_bytes());
            out.extend_from_slice(&(!(block.len() as u16)).to_le_bytes());
            out.extend_from_slice(block);
        }
        out.extend_from_slice(&adler32(data).to_be_bytes());
        out
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut value = 0xffff_ffffu32;
        for &byte in data {
            value ^= byte as u32;
            for _ in 0..8 {
                value = if value & 1 != 0 {
                    (value >> 1) ^ 0xedb8_8320
                } else {
                    value >> 1
                };
            }
        }
        value ^ 0xffff_ffff
    }

    fn adler32(data: &[u8]) -> u32 {
        let (mut a, mut b) = (1u32, 0u32);
        for &byte in data {
            a = (a + byte as u32) % 65521;
            b = (b + a) % 65521;
        }
        (b << 16) | a
    }

    #[test]
    fn the_art_that_ships_is_a_picture() {
        // The banner is drawn rather than typed, so it has colours of its own.
        let coloured = art_runs(ART)
            .iter()
            .flatten()
            .filter(|run| run.style.fg != Color::Default)
            .count();
        assert!(coloured > 0, "the banner should bring its own colours");
    }

    #[test]
    fn a_malformed_colour_does_not_derail_the_rest() {
        // `38;2` with no colour after it must not make `1` a red channel.
        let lines = art_runs("\x1b[38;2m\x1b[1mx");
        assert_eq!(lines[0][0].text, "x");
        assert!(lines[0][0].style.bold);
    }

    #[test]
    fn art_lines_drop_the_trailing_blanks() {
        let lines = art_lines("a\nb\n\n\n");
        assert_eq!(lines, vec!["a", "b"]);
    }

    #[test]
    fn art_lines_keep_the_blank_in_the_middle() {
        let lines = art_lines("a\n\nb\n");
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn the_installed_message_does_not_offer_to_install_the_disk_it_is_on() {
        let message = installed_message();
        assert!(
            !message.contains(INSTALL_COMMAND),
            "an installed machine should not be told to install: {message}"
        );
        assert!(!message.contains("live session"));
        assert!(!message.contains("nothing is written to disk"));
    }

    #[test]
    fn the_installed_message_still_carries_the_banner_and_the_keys() {
        let message = installed_message();
        assert!(message.contains("the terminal is the desktop"));
        assert!(message.contains("ctrl+shift+enter"));
        assert!(message.contains("ctrl+shift+t"));
        assert!(message.contains("ctrl+a d"));
        assert!(message.contains("ctrl+a q"));
    }

    #[test]
    fn the_keys_lead_with_the_ones_a_terminal_user_already_has() {
        // The greeting is the first thing a booted machine says, and the first
        // keys in it should be the ones somebody would have tried anyway.
        let message = installed_message();
        let kitty = message.find("ctrl+shift+enter").expect("the Kitty keys");
        let leader = message.find("ctrl+a d").expect("the leader keys");
        assert!(kitty < leader, "the leader should be the second mention");
    }

    #[test]
    fn the_keys_fit_the_console_the_banner_fits() {
        // Wrapping a two column list turns it into four ragged lines, and the
        // pane this is printed into is narrower than the screen.
        for line in KEYS.lines() {
            assert!(line.chars().count() <= 60, "{line:?} is too wide");
        }
    }

    #[test]
    fn the_live_message_says_how_to_install() {
        let message = live_message();
        assert!(
            message.contains(INSTALL_COMMAND),
            "a live session has to say how to install"
        );
        assert!(message.contains("nothing is written to disk"));
    }

    #[test]
    fn the_live_message_carries_the_banner() {
        assert!(live_message().contains("the terminal is the desktop"));
    }

    #[test]
    fn the_live_message_lists_the_keys_that_are_not_obvious() {
        let message = live_message();
        assert!(message.contains("ctrl+shift+w"));
        assert!(message.contains("ctrl+a"));
        assert!(message.contains("split"));
    }

    #[test]
    fn the_plain_message_has_no_escape_sequences() {
        let plain = live_message_plain();
        assert!(!plain.contains('\x1b'), "found an escape sequence");
        // Stripping must not eat the words.
        assert!(plain.contains(INSTALL_COMMAND));
        assert!(plain.contains("the terminal is the desktop"));
    }

    #[test]
    fn stripping_leaves_ordinary_text_alone() {
        assert_eq!(strip_sgr("plain text"), "plain text");
        assert_eq!(strip_sgr("\x1b[1mbold\x1b[0m"), "bold");
        assert_eq!(strip_sgr("a\x1b[38;2;1;2;3mb"), "ab");
    }

    #[test]
    fn a_file_on_this_machine_wins_over_the_compiled_copy() {
        // The installed system can be rebranded without rebuilding anything.
        // There is no such file on a developer machine, so this checks the
        // fallback rather than the override.
        let loaded = art();
        if std::path::Path::new(ART_PATH).exists() {
            return;
        }
        assert_eq!(loaded, ART);
    }
}
