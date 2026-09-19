//! The pane: what is turned on at the start, off at the end, and drawn in
//! between.
//!
//! A program that takes the alternate screen, hides the cursor, turns on mouse
//! reporting and pushes keyboard flags has made four changes to a terminal
//! that belongs to somebody else. Every one of them has to be undone on every
//! way out — a clean quit, a `SIGTERM`, a panic, the engine dying — because
//! the thing left behind otherwise is a shell with no cursor that reports
//! every mouse move as garbage on the command line, and the person's next move
//! is to close the pane.
//!
//! Which is why the terminal state lives in a static here as well as in a
//! guard. A release build of tOS aborts on panic (`panic = "abort"` in the
//! workspace profile), so `Drop` is not a guarantee; [`emergency`] is the same
//! restoration written so it can run from a panic hook or a signal path, with
//! nothing borrowed and one `write(2)`.

use std::io::{self, Write};
use std::os::unix::io::RawFd;
use std::sync::Mutex;

use tos_preview::fit::Metrics;

/// The keyboard flags this program asks for.
///
/// Disambiguate so that `ctrl+i` is not `tab`; event types so that a page sees
/// key releases; alternate keys so that a shifted key reports what it made;
/// associated text so that a key that typed something says what. Not
/// `REPORT_ALL_KEYS_AS_ESCAPE`: it would make every printable key an escape
/// sequence, which is more parsing for no more information, since the text is
/// already reported.
pub const KEYBOARD_FLAGS: u8 = 1 | 2 | 4 | 16;

/// Everything turned on at the start.
pub fn enter_sequence() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"\x1b[?1049h"); // the alternate screen
    out.extend_from_slice(b"\x1b[?25l"); // no cursor
    out.extend_from_slice(b"\x1b[?1000h"); // report buttons
    out.extend_from_slice(b"\x1b[?1002h"); // and motion while one is held
    out.extend_from_slice(b"\x1b[?1006h"); // in SGR, which has no 223 limit
    out.extend_from_slice(b"\x1b[?1016h"); // in pixels, if the terminal can
    out.extend_from_slice(format!("\x1b[>{KEYBOARD_FLAGS}u").as_bytes());
    out.extend_from_slice(b"\x1b[2J"); // an empty screen to draw on
    out
}

/// The question that decides what mouse coordinates mean.
///
/// Asked after the mode is set, because DECRQM answers with the state as it is
/// now: a terminal that took `?1016h` answers 1, one that ignored it answers 2
/// if it knows the mode and 0 if it does not, and only the first is a licence
/// to treat coordinates as pixels.
pub const ASK_PIXEL_MOUSE: &[u8] = b"\x1b[?1016$p";

/// Everything turned off at the end, in the reverse order.
pub fn leave_sequence() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"\x1b[<u"); // pop the keyboard flags
    out.extend_from_slice(b"\x1b[?1016l");
    out.extend_from_slice(b"\x1b[?1006l");
    out.extend_from_slice(b"\x1b[?1002l");
    out.extend_from_slice(b"\x1b[?1000l");
    out.extend_from_slice(b"\x1b[?25h"); // the cursor comes back
    out.extend_from_slice(b"\x1b[?1049l"); // and so does the screen
    out
}

/// The terminal settings as they were before this program touched them.
///
/// A static rather than only a field, so that the panic hook can put them back
/// without a reference to anything.
static SAVED: Mutex<Option<libc::termios>> = Mutex::new(None);

/// Put the terminal back, from anywhere.
///
/// Deliberately not a method and deliberately not fallible: it runs where
/// there is nothing left to report an error to.
pub fn emergency() {
    let bytes = leave_sequence();
    unsafe {
        libc::write(1, bytes.as_ptr() as *const libc::c_void, bytes.len());
    }
    if let Ok(mut saved) = SAVED.lock() {
        if let Some(termios) = saved.take() {
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, &termios);
            }
        }
    }
}

/// The pane, in the state this program needs it, for as long as it is held.
pub struct Pane {
    input: RawFd,
    output: RawFd,
    restored: bool,
}

impl Pane {
    /// Take the terminal: raw mode, alternate screen, mouse, keyboard flags.
    ///
    /// Raw mode is done here rather than with [`tos_platform::tty::RawMode`]
    /// because the settings have to be saved somewhere a panic hook can reach
    /// them, and a guard that owns its copy cannot be that place.
    pub fn enter(input: RawFd, output: RawFd) -> io::Result<Pane> {
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(input, &mut saved) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut raw = saved;
        unsafe { libc::cfmakeraw(&mut raw) };
        raw.c_cc[libc::VMIN] = 0;
        raw.c_cc[libc::VTIME] = 0;
        if unsafe { libc::tcsetattr(input, libc::TCSANOW, &raw) } < 0 {
            return Err(io::Error::last_os_error());
        }
        if let Ok(mut slot) = SAVED.lock() {
            *slot = Some(saved);
        }

        let mut pane = Pane {
            input,
            output,
            restored: false,
        };
        pane.write(&enter_sequence())?;
        pane.write(ASK_PIXEL_MOUSE)?;
        Ok(pane)
    }

    /// Write bytes to the terminal.
    pub fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        let mut stdout = io::stdout().lock();
        stdout.write_all(bytes)?;
        stdout.flush()
    }

    pub fn input_fd(&self) -> RawFd {
        self.input
    }

    pub fn output_fd(&self) -> RawFd {
        self.output
    }

    /// Measure the pane again, which is what a `SIGWINCH` means.
    pub fn metrics(&self) -> io::Result<Metrics> {
        Metrics::probe(self.output)
    }

    /// Put everything back, now.
    pub fn leave(&mut self) {
        if self.restored {
            return;
        }
        self.restored = true;
        let _ = self.write(&leave_sequence());
        if let Ok(mut saved) = SAVED.lock() {
            if let Some(termios) = saved.take() {
                unsafe {
                    libc::tcsetattr(self.input, libc::TCSANOW, &termios);
                }
            }
        }
    }
}

impl Drop for Pane {
    fn drop(&mut self) {
        self.leave();
    }
}

/// The top row: what page this is, or what is being typed into the url bar.
///
/// Drawn in reverse video across the whole width so that the page below it
/// cannot be mistaken for part of it, and so that a picture that is one cell
/// too tall covers something that is already there rather than the first line
/// of the page.
pub fn status_line(cols: u32, text: &str, editing: Option<&str>) -> Vec<u8> {
    let cols = cols.max(1) as usize;
    let mut out = b"\x1b[1;1H\x1b[K\x1b[7m".to_vec();
    match editing {
        Some(url) => {
            // The tail of what is being typed, because the end of a url is
            // where the cursor is and what the person is looking at.
            let prompt = "url: ";
            let room = cols.saturating_sub(prompt.len());
            let shown = tail_to(url, room);
            out.extend_from_slice(prompt.as_bytes());
            out.extend_from_slice(shown.as_bytes());
            let used = prompt.len() + width(&shown);
            out.extend(std::iter::repeat(b' ').take(cols.saturating_sub(used)));
            out.extend_from_slice(b"\x1b[0m");
            // The cursor is put back where the typing is, and shown, because
            // this is the one moment the person is editing rather than
            // watching.
            out.extend_from_slice(format!("\x1b[1;{}H\x1b[?25h", used + 1).as_bytes());
        }
        None => {
            let shown = clip_to(text, cols);
            out.extend_from_slice(shown.as_bytes());
            out.extend(std::iter::repeat(b' ').take(cols.saturating_sub(width(&shown))));
            out.extend_from_slice(b"\x1b[0m\x1b[?25l");
        }
    }
    out
}

/// How wide a string is in cells.
///
/// The ranges are the East Asian wide and fullwidth blocks, which is what a
/// tOS pane draws at two cells. It is not the whole of UAX #11 — no combining
/// marks, no emoji sequences — because the cost of being wrong here is a
/// status line one cell short, and the cost of a table is a table.
pub fn width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

fn char_width(c: char) -> usize {
    let c = c as u32;
    let wide = (0x1100..=0x115f).contains(&c)
        || (0x2e80..=0xa4cf).contains(&c)
        || (0xac00..=0xd7a3).contains(&c)
        || (0xf900..=0xfaff).contains(&c)
        || (0xfe30..=0xfe6f).contains(&c)
        || (0xff00..=0xff60).contains(&c)
        || (0xffe0..=0xffe6).contains(&c)
        || (0x1f300..=0x1f9ff).contains(&c);
    if wide {
        2
    } else {
        1
    }
}

/// As much of the front of a string as fits, with an ellipsis when it does not.
pub fn clip_to(text: &str, cols: usize) -> String {
    if width(text) <= cols {
        return text.to_string();
    }
    if cols <= 1 {
        return "…".chars().take(cols).collect();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        if used + char_width(c) > cols - 1 {
            break;
        }
        used += char_width(c);
        out.push(c);
    }
    out.push('…');
    out
}

/// As much of the end of a string as fits, which is where a url is typed.
pub fn tail_to(text: &str, cols: usize) -> String {
    if width(text) <= cols {
        return text.to_string();
    }
    let mut kept: Vec<char> = Vec::new();
    let mut used = 0;
    for c in text.chars().rev() {
        if used + char_width(c) > cols {
            break;
        }
        used += char_width(c);
        kept.push(c);
    }
    kept.into_iter().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).to_string()
    }

    #[test]
    fn everything_turned_on_is_turned_off_again() {
        let on = text(&enter_sequence());
        let off = text(&leave_sequence());
        for (set, reset) in [
            ("?1049h", "?1049l"),
            ("?25l", "?25h"),
            ("?1000h", "?1000l"),
            ("?1002h", "?1002l"),
            ("?1006h", "?1006l"),
            ("?1016h", "?1016l"),
        ] {
            assert!(on.contains(set), "{set} is never set");
            assert!(off.contains(reset), "{set} is never unset");
        }
        assert!(on.contains("\x1b[>23u"), "the flags are pushed: {on:?}");
        assert!(off.contains("\x1b[<u"), "and popped: {off:?}");
    }

    #[test]
    fn the_flags_are_the_four_that_were_asked_for() {
        // DISAMBIGUATE | REPORT_EVENT_TYPES | REPORT_ALTERNATE_KEYS |
        // REPORT_ASSOCIATED_TEXT, and not REPORT_ALL_KEYS_AS_ESCAPE.
        assert_eq!(KEYBOARD_FLAGS, 23);
        assert_eq!(KEYBOARD_FLAGS & 8, 0);
    }

    /// The terminal has to make of these bytes what they were meant as.
    #[test]
    fn a_terminal_reads_the_setup_the_way_it_was_written() {
        let mut terminal = tos_term::Terminal::new(80, 24, tos_term::TerminalConfig::default());
        terminal.advance(&enter_sequence());
        assert!(terminal.modes.alt_screen);
        assert!(!terminal.modes.cursor_visible);
        assert_eq!(terminal.keyboard_flags().0, KEYBOARD_FLAGS);

        terminal.advance(ASK_PIXEL_MOUSE);
        let answer = String::from_utf8(terminal.take_output()).expect("ascii");
        // Today's tOS does not know the mode, which is the answer this
        // program has to cope with: cells, not pixels.
        assert!(
            answer == "\x1b[?1016;0$y" || answer == "\x1b[?1016;1$y",
            "unexpected answer {answer:?}"
        );

        terminal.advance(&leave_sequence());
        assert!(!terminal.modes.alt_screen);
        assert!(terminal.modes.cursor_visible);
        assert_eq!(terminal.keyboard_flags().0, 0);
    }

    #[test]
    fn the_status_line_fills_the_width_and_no_more() {
        let line = text(&status_line(20, "tOS — a title", None));
        assert!(line.starts_with("\x1b[1;1H\x1b[K\x1b[7m"));
        let body = line
            .trim_start_matches("\x1b[1;1H\x1b[K\x1b[7m")
            .trim_end_matches("\x1b[0m\x1b[?25l");
        assert_eq!(width(body), 20, "{body:?}");
    }

    #[test]
    fn a_long_title_is_cut_and_says_it_was() {
        assert_eq!(clip_to("abcdef", 10), "abcdef");
        assert_eq!(clip_to("abcdef", 6), "abcdef");
        assert_eq!(clip_to("abcdef", 4), "abc…");
        assert_eq!(clip_to("abcdef", 1), "…");
        // A wide character is two cells and is never cut in half.
        assert_eq!(width("\u{65e5}\u{672c}\u{8a9e}"), 6);
        assert_eq!(clip_to("\u{65e5}\u{672c}\u{8a9e}", 4), "\u{65e5}…");
    }

    #[test]
    fn a_url_being_typed_shows_its_end_and_the_cursor() {
        let line = text(&status_line(
            20,
            "",
            Some("https://example.com/a/very/long/path"),
        ));
        assert!(line.contains("url: "));
        assert!(line.contains("long/path"), "{line:?}");
        assert!(line.ends_with("\x1b[?25h"), "the cursor is shown: {line:?}");
        assert!(line.contains("\x1b[1;21H"), "at the end of what was typed");
    }

    #[test]
    fn a_short_url_keeps_its_whole_self() {
        assert_eq!(tail_to("abc", 10), "abc");
        assert_eq!(tail_to("abcdef", 3), "def");
        assert_eq!(tail_to("\u{65e5}\u{672c}\u{8a9e}", 5), "\u{672c}\u{8a9e}");
    }
}
