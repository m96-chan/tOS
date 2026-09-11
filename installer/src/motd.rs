//! The message of the day.
//!
//! `.motd_art` is the tOS banner. It is compiled into the installer so the
//! welcome screen always has it, and it is also written into the live image so
//! that every shell prints it along with the one line that matters on a live
//! ISO: how to put tOS on the disk.

/// The banner, as it ships.
pub const ART: &str = include_str!("../../.motd_art");

/// Where the live image keeps the art, so it can be changed without a rebuild.
pub const ART_PATH: &str = "/etc/tos/motd_art";

/// The command that installs tOS.
pub const INSTALL_COMMAND: &str = "tos-install";

/// Load the banner, preferring the copy on this machine.
pub fn art() -> String {
    std::fs::read_to_string(ART_PATH).unwrap_or_else(|_| ART.to_string())
}

/// The banner as lines, with trailing blank lines removed.
pub fn art_lines(art: &str) -> Vec<String> {
    let mut lines: Vec<String> = art.lines().map(|line| line.to_string()).collect();
    while lines.last().map(|line| line.trim().is_empty()).unwrap_or(false) {
        lines.pop();
    }
    lines
}

/// How wide the banner is, in cells.
pub fn art_width(art: &str) -> usize {
    art_lines(art)
        .iter()
        .map(|line| tos_term::str_width(line))
        .max()
        .unwrap_or(0)
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
