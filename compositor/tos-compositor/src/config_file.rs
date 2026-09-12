//! The configuration file.
//!
//! tOS writes its own parser here rather than taking a TOML or YAML crate, for
//! the same reason it parses its own flags, PNGs and DEFLATE streams: the
//! compositor is what an installed machine runs as `/init`, and everything in
//! that path should be readable in one sitting. The format is the smallest
//! thing that covers the need — `key = value` lines, `[section]` headers and
//! `#` comments — which is also the shape most people already know from
//! `.gitconfig` and from `/etc`, so nobody has to learn it.
//!
//! Nothing in a file is allowed to stop the machine from booting into a
//! terminal. A line that makes no sense is collected as a problem, reported by
//! `main`, and then skipped; the rest of the file still applies.

use std::path::{Path, PathBuf};

use tos_term::{Palette, Rgb};

use crate::chrome::Chrome;
use crate::clock::Zone;
use crate::config::{
    parse_args, parse_args_over, parse_interval, parse_size, Backend, Config, ConfigSource,
};
use crate::status;

/// The file tOS looks for in each directory of the search path.
pub const FILE_NAME: &str = "tos.conf";

/// The configuration a run starts from, and everything wrong with the file it
/// came from.
pub struct Startup {
    pub config: Config,
    /// Problems worth telling the user about. They have already been survived.
    pub problems: Vec<String>,
}

/// Build the configuration for a run: the file underneath, the flags on top.
pub fn startup(args: &[String]) -> Result<Startup, String> {
    // The flags are read twice. They have to be, because which file to read is
    // itself a flag, and that file has to sit underneath the flags around it.
    // Parsing a handful of arguments a second time costs nothing.
    let source = parse_args(args)?.source;
    let mut problems = Vec::new();
    let mut base = Config::default();
    match &source {
        ConfigSource::None => {}
        ConfigSource::File(path) => read_into(&mut base, path, &mut problems),
        ConfigSource::Search => {
            // Only the first file found is read. Merging several would mean
            // explaining which one won every key, and someone who wants the
            // system file plus changes can copy it.
            if let Some(path) = search_path().into_iter().find(|p| p.is_file()) {
                read_into(&mut base, &path, &mut problems);
            }
        }
    }
    let config = parse_args_over(base, args)?;
    Ok(Startup { config, problems })
}

/// Where tOS looks for [`FILE_NAME`], best match first.
///
/// This is the XDG basedir order with one place added at the end: an installed
/// machine starts the compositor from `/init`, where there is no home
/// directory and often no environment at all, so `/etc/tos` is the last resort
/// that still lets such a machine be configured.
pub fn search_path() -> Vec<PathBuf> {
    // A non-UTF-8 value is read as if the variable were unset, which falls
    // back to the documented default rather than refusing to start.
    let config_home = std::env::var("XDG_CONFIG_HOME").ok();
    let home = std::env::var("HOME").ok();
    let config_dirs = std::env::var("XDG_CONFIG_DIRS").ok();
    search_path_from(
        config_home.as_deref(),
        home.as_deref(),
        config_dirs.as_deref(),
    )
}

/// The basedir spec says an empty variable counts as unset, and empty is
/// exactly what a stripped-down init environment tends to hand over.
fn nonempty(value: Option<&str>) -> Option<&str> {
    value.filter(|v| !v.is_empty())
}

fn search_path_from(
    config_home: Option<&str>,
    home: Option<&str>,
    config_dirs: Option<&str>,
) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    match nonempty(config_home) {
        Some(dir) => dirs.push(PathBuf::from(dir)),
        None => {
            if let Some(home) = nonempty(home) {
                dirs.push(Path::new(home).join(".config"));
            }
        }
    }
    for dir in nonempty(config_dirs).unwrap_or("/etc/xdg").split(':') {
        if !dir.is_empty() {
            dirs.push(PathBuf::from(dir));
        }
    }
    dirs.push(PathBuf::from("/etc"));

    let mut paths: Vec<PathBuf> = Vec::new();
    for dir in dirs {
        let path = dir.join("tos").join(FILE_NAME);
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    paths
}

/// Read one file into `config`, naming it in anything that goes wrong.
fn read_into(config: &mut Config, path: &Path, problems: &mut Vec<String>) {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) => {
            problems.push(format!("{}: {e}", path.display()));
            return;
        }
    };
    for problem in apply(config, &text) {
        problems.push(format!("{}:{problem}", path.display()));
    }
}

/// Apply the settings in `text` to `config`, collecting what it got wrong.
///
/// Problems are prefixed with the line they came from, so that a report can
/// put a file name in front and read the way a compiler does.
pub fn apply(config: &mut Config, text: &str) -> Vec<String> {
    let mut problems = Vec::new();
    let mut section = String::new();
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        // Comments are whole lines only. Values begin with `#` all the time --
        // every colour does -- and a rule that ate the rest of a line after a
        // hash would eat those.
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fail = |message: String| problems.push(format!("{}: {message}", index + 1));
        if let Some(rest) = line.strip_prefix('[') {
            match rest.strip_suffix(']') {
                // `[general]` names the same place as the top of the file, so
                // a file can be written with every setting under a heading.
                Some(name) => {
                    section = match name.trim() {
                        "general" => String::new(),
                        other => other.to_string(),
                    }
                }
                None => fail(format!("section is missing its ]: {line}")),
            }
            continue;
        }
        let (key, value) = match line.split_once('=') {
            Some(pair) => pair,
            None => {
                fail(format!("expected key = value: {line}"));
                continue;
            }
        };
        let (key, value) = (key.trim(), value.trim());
        if key.is_empty() {
            fail(format!("setting has no name: {line}"));
            continue;
        }
        if let Err(message) = set(config, &section, key, value) {
            fail(message);
        }
    }
    problems
}

/// Apply one `key = value`.
///
/// Adding a setting is one arm of this match, which is the point of the shape:
/// the sections the format reserves but cannot answer yet — `[keys]` for
/// bindings, `[fonts]` for the fallback list — each become an arm when the
/// code behind them lands, the way `[status]` did. Until then an unknown key
/// is reported rather than ignored, because a setting that is silently dropped
/// looks exactly like a setting that does not work.
fn set(config: &mut Config, section: &str, key: &str, value: &str) -> Result<(), String> {
    match (section, key) {
        ("", "backend") => {
            config.backend =
                Backend::parse(value).ok_or_else(|| format!("unknown backend: {value}"))?
        }
        ("", "font") => config.font = Some(PathBuf::from(value)),
        ("", "font-size") => config.font_size = Some(number(value)?),
        ("", "bitmap-scale") => config.bitmap_scale = Some(number(value)?),
        ("", "scrollback") => config.scrollback = number(value)?,
        ("", "size") => config.size = parse_size(value)?,
        ("", "status-bar") => config.status_bar = boolean(value)?,
        ("", "inactive-fade") => config.inactive_fade = number(value)?,
        // The shell is the command every pane runs, which is what `-e` names
        // as well, so they are one setting and the flag lands on top of it.
        ("", "shell") => {
            let words: Vec<String> = value.split_whitespace().map(String::from).collect();
            if words.is_empty() {
                return Err("shell needs a program".into());
            }
            config.command = Some(words);
        }
        // What the session does when nobody is touching it. A section of its
        // own because the two settings are one subject and are read together:
        // which comes first is the whole of what an idle machine does.
        ("idle", "lock-after") => config.idle_lock = parse_interval(value)?,
        ("idle", "blank-after") => config.idle_blank = parse_interval(value)?,
        ("idle", other) => return Err(format!("unknown setting: [idle] {other}")),
        // Japanese input. One setting, because there is one question a person
        // has about it: which dictionary. Everything else about the IME is a
        // key binding, and bindings are `[keys]`, which is still to come.
        ("ime", "dictionary") => config.ime_dictionary = Some(PathBuf::from(value)),
        ("ime", other) => return Err(format!("unknown setting: [ime] {other}")),
        ("colors", key) => set_palette(&mut config.palette, key, value)?,
        ("chrome", key) => set_chrome(&mut config.chrome, key, value)?,
        // The status bar is two subjects that belong together: what it says,
        // and what colour it says it in. They share a section because the
        // person writing one is the person writing the other, and the colours
        // land in `Chrome` anyway — this is a heading, not a second home.
        ("status", key) => set_status(&mut config.status, &mut config.chrome, key, value)?,
        ("", key) => return Err(format!("unknown setting: {key}")),
        (section, key) => return Err(format!("unknown setting: [{section}] {key}")),
    }
    Ok(())
}

/// The terminal's own colours, which are what an application paints with.
fn set_palette(palette: &mut Palette, key: &str, value: &str) -> Result<(), String> {
    if let Some(digits) = key.strip_prefix("color") {
        let index: u8 = digits
            .parse()
            .map_err(|_| format!("colour index must be 0 to 255: {key}"))?;
        palette.set_index(index, color(value)?);
        return Ok(());
    }
    match key {
        "background" => palette.background = color(value)?,
        "foreground" => palette.foreground = color(value)?,
        "cursor" => palette.cursor = color(value)?,
        "cursor-text" => palette.cursor_text = color(value)?,
        other => return Err(format!("unknown setting: [colors] {other}")),
    }
    Ok(())
}

/// The compositor's own colours: dividers, the status bar and the launcher.
fn set_chrome(chrome: &mut Chrome, key: &str, value: &str) -> Result<(), String> {
    match key {
        "background" => chrome.background = color(value)?,
        "foreground" => chrome.foreground = color(value)?,
        "dim" => chrome.dim = color(value)?,
        "accent" => chrome.accent = color(value)?,
        "accent-text" => chrome.accent_text = color(value)?,
        "divider" => chrome.divider = color(value)?,
        "divider-focused" => chrome.divider_focused = color(value)?,
        // Here rather than under `[status]` because selected text is in a
        // pane, not on the bar. It is separate from `accent` at all because it
        // is the one highlight drawn over somebody else's palette, and a
        // palette whose own blue is near the accent leaves a selection that
        // cannot be made out.
        "selection" => chrome.selection = Some(color(value)?),
        other => return Err(format!("unknown setting: [chrome] {other}")),
    }
    Ok(())
}

/// What the status bar shows, and what it shows it in.
///
/// The colours are `Option` in [`Chrome`] and are set here, which is what
/// makes `[chrome] accent` move the bar's highlight along with every other
/// highlight while `[status] active` moves only the bar's. Somebody who wants
/// one colour scheme writes one line; somebody who wants the bar to stand
/// apart from the panes writes the other.
fn set_status(
    status: &mut status::Settings,
    chrome: &mut Chrome,
    key: &str,
    value: &str,
) -> Result<(), String> {
    match key {
        "left" => status.left = status::Settings::parse_list(value)?,
        "right" => status.right = status::Settings::parse_list(value)?,
        "clock" => status.clock_format = value.to_string(),
        "timezone" => status.zone = Zone::parse(value),
        "background" => chrome.status_background = Some(color(value)?),
        "foreground" => chrome.status_foreground = Some(color(value)?),
        "active" => chrome.status_active = Some(color(value)?),
        "active-text" => chrome.status_active_text = Some(color(value)?),
        "divider" => chrome.status_divider = Some(color(value)?),
        other => return Err(format!("unknown setting: [status] {other}")),
    }
    Ok(())
}

fn number<T: std::str::FromStr>(value: &str) -> Result<T, String> {
    value.parse().map_err(|_| format!("not a number: {value}"))
}

fn boolean(value: &str) -> Result<bool, String> {
    match value {
        "true" | "yes" | "on" | "1" => Ok(true),
        "false" | "no" | "off" | "0" => Ok(false),
        other => Err(format!("expected true or false, got {other}")),
    }
}

/// Parse `#rrggbb`, or the same six digits without the hash, or the three
/// digit shorthand every stylesheet uses.
///
/// Hex only. Colour names would need a table that is never quite the one the
/// person writing the file has in mind, and `[colors]` is itself where this
/// machine decides what red is.
fn color(value: &str) -> Result<Rgb, String> {
    let digits = value.strip_prefix('#').unwrap_or(value);
    let bad = || format!("expected a colour like #5f87d7, got {value}");
    let pair = |at: usize| u8::from_str_radix(&digits[at..at + 2], 16).map_err(|_| bad());
    match digits.len() {
        // In the shorthand each digit stands for both halves of a byte, so
        // #abc is #aabbcc.
        3 => {
            let mut bytes = [0u8; 3];
            for (out, digit) in bytes.iter_mut().zip(digits.chars()) {
                let half = digit.to_digit(16).ok_or_else(bad)? as u8;
                *out = (half << 4) | half;
            }
            Ok(Rgb::new(bytes[0], bytes[1], bytes[2]))
        }
        6 => Ok(Rgb::new(pair(0)?, pair(2)?, pair(4)?)),
        _ => Err(bad()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply_to(config: &mut Config, text: &str) {
        let problems = apply(config, text);
        assert!(problems.is_empty(), "unexpected problems: {problems:?}");
    }

    #[test]
    fn settings_reach_the_config() {
        let mut config = Config::default();
        apply_to(
            &mut config,
            "# a comment\n\
             \n\
             scrollback = 500\n\
             font = /usr/share/fonts/tos.ttf\n\
             font-size = 18.5\n\
             status-bar = off\n\
             size = 800x600\n\
             shell = /bin/dash -l\n",
        );
        assert_eq!(config.scrollback, 500);
        assert_eq!(
            config.font.unwrap().to_str().unwrap(),
            "/usr/share/fonts/tos.ttf"
        );
        assert_eq!(config.font_size, Some(18.5));
        assert!(!config.status_bar);
        assert_eq!(config.size, (800, 600));
        assert_eq!(
            config.command.unwrap(),
            vec!["/bin/dash".to_string(), "-l".into()]
        );
    }

    #[test]
    fn a_general_section_is_the_top_of_the_file() {
        let mut config = Config::default();
        apply_to(&mut config, "[general]\nbackend = headless\n");
        assert_eq!(config.backend, Backend::Headless);
    }

    #[test]
    fn colors_are_parsed_in_both_lengths() {
        let mut config = Config::default();
        apply_to(
            &mut config,
            "[colors]\n\
             background = #102030\n\
             foreground = #abc\n\
             color1 = ff0000\n\
             [chrome]\n\
             accent = #5f87d7\n",
        );
        assert_eq!(config.palette.background, Rgb::new(0x10, 0x20, 0x30));
        assert_eq!(config.palette.foreground, Rgb::new(0xaa, 0xbb, 0xcc));
        assert_eq!(config.palette.index(1), Rgb::new(0xff, 0, 0));
        assert_eq!(config.chrome.accent, Rgb::new(0x5f, 0x87, 0xd7));
    }

    #[test]
    fn a_hash_inside_a_value_is_not_a_comment() {
        let mut config = Config::default();
        apply_to(&mut config, "[chrome]\ndim = #70707c\n");
        assert_eq!(config.chrome.dim, Rgb::new(0x70, 0x70, 0x7c));
    }

    #[test]
    fn a_bad_line_is_reported_and_the_rest_still_applies() {
        let mut config = Config::default();
        let problems = apply(
            &mut config,
            "scrollback = plenty\n\
             typo-here = 1\n\
             this line has no equals sign\n\
             scrollback = 42\n",
        );
        assert_eq!(problems.len(), 3);
        assert!(problems[0].starts_with("1: "), "{:?}", problems[0]);
        assert!(problems[1].contains("unknown setting"), "{:?}", problems[1]);
        assert!(problems[2].starts_with("3: "), "{:?}", problems[2]);
        assert_eq!(config.scrollback, 42);
    }

    #[test]
    fn the_idle_deadlines_come_from_the_file() {
        let mut config = Config::default();
        apply_to(
            &mut config,
            "[idle]\nlock-after = 90\nblank-after = never\n",
        );
        assert_eq!(config.idle_lock, Some(std::time::Duration::from_secs(90)));
        assert_eq!(config.idle_blank, None);
        let problems = apply(&mut config, "[idle]\nlock-after = soon\nsleep = 10\n");
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems[1].contains("[idle] sleep"), "{:?}", problems[1]);
    }

    #[test]
    fn the_dictionary_comes_from_the_file_and_a_typo_in_the_section_is_reported() {
        let mut config = Config::default();
        assert_eq!(config.ime_dictionary, None, "the default is to search");
        apply_to(&mut config, "[ime]\ndictionary = /srv/SKK-JISYO.mine\n");
        assert_eq!(
            config.ime_dictionary,
            Some(PathBuf::from("/srv/SKK-JISYO.mine"))
        );
        // Reported rather than ignored, because a setting that silently does
        // nothing looks exactly like one that is broken.
        let problems = apply(&mut config, "[ime]\ndictionery = /srv/typo\n");
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].contains("[ime] dictionery"),
            "{:?}",
            problems[0]
        );
    }

    #[test]
    fn an_idle_flag_wins_over_the_file() {
        let path = scratch("idle", "[idle]\nlock-after = 90\nblank-after = 120\n");
        let flags = args(&["--config", path.to_str().unwrap(), "--idle-lock", "never"]);
        let startup = startup(&flags).expect("startup");
        assert!(startup.problems.is_empty(), "{:?}", startup.problems);
        assert_eq!(startup.config.idle_lock, None);
        // And leaves alone what it did not mention.
        assert_eq!(
            startup.config.idle_blank,
            Some(std::time::Duration::from_secs(120))
        );
    }

    #[test]
    fn the_status_bar_takes_its_segments_and_its_order_from_the_file() {
        let mut config = Config::default();
        apply_to(
            &mut config,
            "[status]\n\
             left = workspaces panes\n\
             right = volume, bluetooth, clock\n\
             clock = %a %H:%M\n\
             timezone = Asia/Tokyo\n",
        );
        assert_eq!(
            config.status.left,
            vec![status::Segment::Workspaces, status::Segment::Panes]
        );
        assert_eq!(
            config.status.right,
            vec![
                status::Segment::Volume,
                status::Segment::Bluetooth,
                status::Segment::Clock
            ]
        );
        assert_eq!(config.status.clock_format, "%a %H:%M");
        assert_eq!(config.status.zone, Zone::Named("Asia/Tokyo".to_string()));
    }

    #[test]
    fn a_segment_that_does_not_exist_is_reported_and_the_side_is_left_alone() {
        let mut config = Config::default();
        let before = config.status.right.clone();
        let problems = apply(
            &mut config,
            "[status]\nright = clock batery\nleft = title\n",
        );
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("batery"), "{:?}", problems[0]);
        assert_eq!(config.status.right, before, "a bad word took the good ones");
        // And the line after it still applies, the way every other bad line
        // in this file does.
        assert_eq!(config.status.left, vec![status::Segment::Title]);
    }

    #[test]
    fn the_bar_can_be_coloured_apart_from_the_rest_of_the_chrome() {
        let mut config = Config::default();
        apply_to(
            &mut config,
            "[chrome]\n\
             accent = #ff0000\n\
             selection = #00ff00\n\
             [status]\n\
             background = #101010\n\
             active = #0000ff\n",
        );
        let bar = config.chrome.bar();
        assert_eq!(bar.background, Rgb::new(0x10, 0x10, 0x10));
        assert_eq!(bar.active, Rgb::new(0, 0, 0xff), "the bar's own highlight");
        // What the bar was not given follows the chrome, so one `accent` line
        // still moves everything that was not singled out.
        assert_eq!(bar.active_text, config.chrome.accent_text);
        assert_eq!(config.chrome.selection(), Rgb::new(0, 0xff, 0));
    }

    #[test]
    fn a_chrome_colour_set_after_a_status_colour_still_reaches_the_bar() {
        // The fallback is taken when the bar is drawn rather than when the
        // file is read, so the order of the two sections cannot matter.
        let mut config = Config::default();
        apply_to(
            &mut config,
            "[status]\nforeground = #123456\n[chrome]\naccent = #abcdef\n",
        );
        assert_eq!(config.chrome.bar().active, Rgb::new(0xab, 0xcd, 0xef));
        assert_eq!(config.chrome.bar().foreground, Rgb::new(0x12, 0x34, 0x56));
    }

    #[test]
    fn a_section_that_is_not_here_yet_names_itself() {
        let mut config = Config::default();
        let problems = apply(&mut config, "[keys]\nleader = ctrl+a\n");
        assert_eq!(
            problems,
            vec!["2: unknown setting: [keys] leader".to_string()]
        );
    }

    #[test]
    fn a_malformed_section_header_is_reported() {
        let mut config = Config::default();
        let problems = apply(&mut config, "[colors\nbackground = #000\n");
        assert_eq!(problems.len(), 2);
        assert!(problems[0].contains("missing its ]"));
        // The header never took effect, so the line under it was read as a
        // general setting, where there is no such name.
        assert!(problems[1].contains("unknown setting: background"));
    }

    #[test]
    fn bad_colors_are_refused() {
        let mut config = Config::default();
        let problems = apply(
            &mut config,
            "[colors]\ncursor = blue\ncolor300 = #fff\nbackground = #12345\n",
        );
        assert_eq!(problems.len(), 3, "{problems:?}");
    }

    #[test]
    fn booleans_take_the_usual_spellings() {
        assert_eq!(boolean("yes"), Ok(true));
        assert_eq!(boolean("0"), Ok(false));
        assert!(boolean("maybe").is_err());
    }

    #[test]
    fn the_search_path_follows_xdg() {
        let paths = search_path_from(None, Some("/home/tos"), None);
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/home/tos/.config/tos/tos.conf"),
                PathBuf::from("/etc/xdg/tos/tos.conf"),
                PathBuf::from("/etc/tos/tos.conf"),
            ]
        );
    }

    #[test]
    fn xdg_config_home_replaces_the_home_directory() {
        let paths = search_path_from(Some("/cfg"), Some("/home/tos"), Some("/a:/b"));
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/cfg/tos/tos.conf"),
                PathBuf::from("/a/tos/tos.conf"),
                PathBuf::from("/b/tos/tos.conf"),
                PathBuf::from("/etc/tos/tos.conf"),
            ]
        );
    }

    #[test]
    fn an_empty_variable_counts_as_unset() {
        let paths = search_path_from(Some(""), Some(""), Some(""));
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/etc/xdg/tos/tos.conf"),
                PathBuf::from("/etc/tos/tos.conf"),
            ]
        );
    }

    #[test]
    fn etc_is_never_listed_twice() {
        let paths = search_path_from(None, None, Some("/etc"));
        assert_eq!(paths, vec![PathBuf::from("/etc/tos/tos.conf")]);
    }

    /// Write a file somewhere private to this test and return its path.
    fn scratch(name: &str, text: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tos-config-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch directory");
        let path = dir.join(FILE_NAME);
        std::fs::write(&path, text).expect("write");
        path
    }

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_named_file_is_read_and_the_flags_still_win() {
        let path = scratch(
            "named",
            "scrollback = 500\nbackend = headless\nsize = 640x400\n",
        );
        let flags = args(&["--config", path.to_str().unwrap(), "--scrollback", "7"]);
        let startup = startup(&flags).expect("startup");
        assert!(startup.problems.is_empty(), "{:?}", startup.problems);
        assert_eq!(startup.config.scrollback, 7);
        assert_eq!(startup.config.backend, Backend::Headless);
        assert_eq!(startup.config.size, (640, 400));
    }

    #[test]
    fn a_file_that_is_not_there_is_reported_and_survived() {
        let startup = startup(&args(&["--config", "/nonexistent/tos.conf"])).expect("startup");
        assert_eq!(startup.problems.len(), 1);
        assert!(startup.problems[0].contains("/nonexistent/tos.conf"));
        assert_eq!(startup.config.scrollback, Config::default().scrollback);
    }

    #[test]
    fn no_config_leaves_the_file_unread() {
        let path = scratch("refused", "scrollback = 500\n");
        let flags = args(&["--config", path.to_str().unwrap(), "--no-config"]);
        let startup = startup(&flags).expect("startup");
        assert!(startup.problems.is_empty());
        assert_eq!(startup.config.scrollback, Config::default().scrollback);
    }
}
