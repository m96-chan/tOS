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

use tos_term::Palette;

use crate::ui::{Color, Style};

/// The banner, as it ships.
pub const ART: &str = include_str!("../../.motd_art");

/// The banner for a screen too small for a picture: one line that still says
/// what this machine is.
pub const ART_SMALL: &str = "tOS — the terminal is the desktop";

/// Where the live image keeps the art, so it can be changed without a rebuild.
pub const ART_PATH: &str = "/etc/tos/motd_art";

/// The command that installs tOS.
pub const INSTALL_COMMAND: &str = "tos-install";

/// Load the banner, preferring the copy on this machine.
pub fn art() -> String {
    std::fs::read_to_string(ART_PATH).unwrap_or_else(|_| ART.to_string())
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
    let numbers: Vec<u16> = params
        .split(';')
        .map(|p| p.parse().unwrap_or(0))
        .collect();
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

/// The message a shell prints when it starts on the live image.
///
/// This is the whole reason the art exists in a file rather than in the
/// compositor: a person who boots the ISO should not have to be told
/// separately how to install it.
pub fn live_message() -> String {
    let mut out = art();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push('\n');
    out.push_str(&format!(
        "  This is a live session: nothing is written to disk.\n\
         \n\
         \x20 Type \x1b[1m{INSTALL_COMMAND}\x1b[0m to install tOS on this machine.\n\
         \x20 Type \x1b[1mexit\x1b[0m to close this pane.\n\
         \n\
         \x20 ctrl+a d  split beside     ctrl+a s  split below\n\
         \x20 ctrl+a h/j/k/l  move focus ctrl+a q  quit\n\
         \n"
    ));
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
        assert!(!plain.contains("25"), "{plain:?} kept a sequence's parameters");
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
