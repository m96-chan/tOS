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

/// One tab, as the strip needs it: a name and whether it is the one in front.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TabLabel<'a> {
    pub title: &'a str,
    pub active: bool,
}

/// How much room a url has to be worth putting after the strip.
///
/// Less than this and what is shown is a scheme and an ellipsis, which says
/// nothing and takes the space the titles wanted.
const URL_MINIMUM: usize = 12;

/// The top row when there is more than one tab: `1 title  2 title  3 title`,
/// and the active tab's url after them if anything is left over.
///
/// The whole row is reverse video, as [`status_line`] draws it, so the active
/// tab cannot be marked by reversing it again — it is *un*-reversed instead
/// (`\x1b[27m`), which against a reversed row is the same emphasis the other
/// way round and needs no colour. Colour would have to be chosen against a
/// theme this program cannot see.
pub fn tab_line(cols: u32, tabs: &[TabLabel], url: &str) -> Vec<u8> {
    let cols = cols.max(1) as usize;
    let mut out = b"\x1b[1;1H\x1b[K\x1b[7m".to_vec();
    let mut used = 0;
    for (text, active) in strip(cols, tabs, url) {
        used += width(&text);
        if active {
            out.extend_from_slice(b"\x1b[27m");
            out.extend_from_slice(text.as_bytes());
            out.extend_from_slice(b"\x1b[7m");
        } else {
            out.extend_from_slice(text.as_bytes());
        }
    }
    out.extend(std::iter::repeat(b' ').take(cols.saturating_sub(used)));
    out.extend_from_slice(b"\x1b[0m\x1b[?25l");
    out
}

/// The strip as the runs it is drawn in: the text, and whether it is the
/// active tab and so emphasised.
///
/// Separate from the escapes so that what fits can be tested as what fits.
fn strip(cols: usize, tabs: &[TabLabel], url: &str) -> Vec<(String, bool)> {
    // What every tab costs before its title: its number, a space, and the two
    // spaces that separate it from the one before.
    let fixed: usize = tabs
        .iter()
        .enumerate()
        .map(|(index, _)| number_width(index + 1) + 1 + if index == 0 { 0 } else { 2 })
        .sum();
    let budget = cols.saturating_sub(fixed);
    let wanted: Vec<usize> = tabs.iter().map(|tab| width(tab.title)).collect();
    let given = shares(budget, &wanted);

    let mut runs: Vec<(String, bool)> = Vec::new();
    let mut used = 0;
    for (index, tab) in tabs.iter().enumerate() {
        if index > 0 {
            runs.push(("  ".to_string(), false));
            used += 2;
        }
        let text = format!("{} {}", index + 1, clip_to(tab.title, given[index]));
        used += width(&text);
        runs.push((text, tab.active));
    }

    // The url gets whatever the titles did not want, and only if that is
    // enough to read: the strip is what this row is for now.
    let left = cols.saturating_sub(used);
    if !url.is_empty() && left >= URL_MINIMUM + 2 {
        runs.push((format!("  {}", clip_to(url, left - 2)), false));
    }

    // Titles can be clipped to nothing but numbers cannot, so a pane narrower
    // than `1  2  3 …` still has more strip than row. The row wins: what is
    // past the edge is cut, and the numbers that are left still say which tab
    // is which, because the order never changes.
    let mut fitted = Vec::with_capacity(runs.len());
    let mut used = 0;
    for (text, active) in runs {
        let room = cols - used;
        if width(&text) <= room {
            used += width(&text);
            fitted.push((text, active));
            continue;
        }
        if room > 0 {
            fitted.push((clip_to(&text, room), active));
        }
        break;
    }
    fitted
}

/// Share `budget` cells between titles that want `wanted`.
///
/// Equally, except that a title which wants less than its share gives the rest
/// back to the ones that want more — so two short titles and one long one show
/// the long one rather than three equal stumps. What nobody gets is a share of
/// zero while somebody else has room to spare.
fn shares(mut budget: usize, wanted: &[usize]) -> Vec<usize> {
    let mut given = vec![0usize; wanted.len()];
    let mut open: Vec<usize> = (0..wanted.len()).collect();
    while !open.is_empty() {
        let each = budget / open.len();
        if each == 0 {
            break;
        }
        let modest: Vec<usize> = open
            .iter()
            .copied()
            .filter(|&i| wanted[i] <= each)
            .collect();
        if modest.is_empty() {
            let spare = budget - each * open.len();
            for (rank, &i) in open.iter().enumerate() {
                given[i] = each + usize::from(rank < spare);
            }
            break;
        }
        for i in modest {
            given[i] = wanted[i];
            budget -= wanted[i];
            open.retain(|&open| open != i);
        }
    }
    given
}

/// How many cells a tab's number takes.
fn number_width(n: usize) -> usize {
    if n < 10 {
        1
    } else {
        n.to_string().len()
    }
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

    fn labels<'a>(titles: &[&'a str], active: usize) -> Vec<TabLabel<'a>> {
        titles
            .iter()
            .enumerate()
            .map(|(index, title)| TabLabel {
                title,
                active: index == active,
            })
            .collect()
    }

    /// What the strip reads as, with the emphasis written as brackets.
    fn strip_text(cols: usize, tabs: &[TabLabel], url: &str) -> String {
        strip(cols, tabs, url)
            .into_iter()
            .map(
                |(text, active)| {
                    if active {
                        format!("[{text}]")
                    } else {
                        text
                    }
                },
            )
            .collect()
    }

    #[test]
    fn the_strip_numbers_the_tabs_and_marks_the_one_in_front() {
        let tabs = labels(&["One", "Two", "Three"], 1);
        assert_eq!(strip_text(80, &tabs, ""), "1 One  [2 Two]  3 Three");
        // The number is part of what is emphasised: it is how the tab is
        // selected, and a number outside the mark would read as a separator.
        let line = String::from_utf8(tab_line(80, &tabs, "")).expect("ascii");
        assert!(line.contains("\x1b[27m2 Two\x1b[7m"), "{line:?}");
        assert!(line.contains("1 One"), "{line:?}");
    }

    #[test]
    fn the_strip_fills_the_width_and_no_more() {
        for cols in [8u32, 13, 20, 40, 80] {
            for count in 2..=9usize {
                let titles: Vec<String> =
                    (1..=count).map(|n| format!("Title number {n}")).collect();
                let refs: Vec<&str> = titles.iter().map(String::as_str).collect();
                let tabs = labels(&refs, count - 1);
                let line = String::from_utf8(tab_line(cols, &tabs, "https://example.com/page"))
                    .expect("ascii");
                let body = line
                    .trim_start_matches("\x1b[1;1H\x1b[K\x1b[7m")
                    .trim_end_matches("\x1b[0m\x1b[?25l")
                    .replace("\x1b[27m", "")
                    .replace("\x1b[7m", "");
                assert_eq!(
                    width(&body),
                    cols as usize,
                    "{cols} cols, {count} tabs: {body:?}"
                );
            }
        }
    }

    #[test]
    fn a_long_title_is_clipped_and_a_short_one_gives_its_room_away() {
        // Three tabs, forty cells: the numbers and separators cost 10, so 30
        // is shared. "ok" wants two and gives back the rest.
        let tabs = labels(
            &["ok", "a title that will not fit in ten cells", "also ok"],
            0,
        );
        let text = strip_text(40, &tabs, "");
        assert_eq!(
            width(&text) - 2,
            40,
            "the brackets are the test's, not the row's"
        );
        assert!(text.starts_with("[1 ok]  2 a title that"), "{text:?}");
        assert!(text.contains('…'), "the long one says it was cut: {text:?}");
        assert!(text.ends_with("3 also ok"), "{text:?}");
    }

    #[test]
    fn the_url_comes_after_the_strip_when_there_is_room_for_it() {
        let tabs = labels(&["A", "B"], 0);
        let wide = strip_text(60, &tabs, "https://example.com/a");
        assert!(wide.ends_with("  https://example.com/a"), "{wide:?}");
        // And not when there is not: half a url is not worth a title.
        let narrow = strip_text(12, &tabs, "https://example.com/a");
        assert_eq!(narrow, "[1 A]  2 B");
    }

    #[test]
    fn more_tabs_than_the_pane_is_wide_is_still_one_row() {
        let titles: Vec<String> = (1..=9).map(|n| format!("Tab {n}")).collect();
        let refs: Vec<&str> = titles.iter().map(String::as_str).collect();
        let tabs = labels(&refs, 0);
        let text = strip_text(10, &tabs, "https://example.com");
        assert!(width(&text) <= 10 + 2, "{text:?}");
        assert!(text.starts_with("[1"), "{text:?}");
    }

    #[test]
    fn the_shares_go_to_the_titles_that_want_them() {
        // Nobody wants more than a third: everybody gets what they asked for.
        assert_eq!(shares(30, &[5, 5, 5]), vec![5, 5, 5]);
        // One wants everything: it gets what the other two left.
        assert_eq!(shares(30, &[2, 100, 3]), vec![2, 25, 3]);
        // Everybody wants more than there is: it is split, and the odd cell
        // goes to the left.
        assert_eq!(shares(10, &[100, 100, 100]), vec![4, 3, 3]);
        // Nothing to share.
        assert_eq!(shares(1, &[10, 10, 10]), vec![0, 0, 0]);
    }

    #[test]
    fn a_short_url_keeps_its_whole_self() {
        assert_eq!(tail_to("abc", 10), "abc");
        assert_eq!(tail_to("abcdef", 3), "def");
        assert_eq!(tail_to("\u{65e5}\u{672c}\u{8a9e}", 5), "\u{672c}\u{8a9e}");
    }
}
