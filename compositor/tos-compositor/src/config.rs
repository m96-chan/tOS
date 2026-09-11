//! Compositor configuration.
//!
//! This module holds the settings themselves and the command line that can set
//! any of them. [`crate::config_file`] fills the same struct in from a file
//! first, so a flag always lands on top of what the file said.

use std::path::PathBuf;

use tos_session::{describe, Keymap};
use tos_term::Palette;

use crate::chrome::Chrome;

/// Which display backend to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// Pick DRM/KMS if a display is available, otherwise run nested.
    Auto,
    /// Direct kernel mode setting: the real tOS.
    Drm,
    /// Inside another terminal, for development.
    Nested,
    /// Off screen, for screenshots and tests.
    Headless,
}

impl Backend {
    pub fn parse(name: &str) -> Option<Backend> {
        Some(match name {
            "auto" => Backend::Auto,
            "drm" | "kms" => Backend::Drm,
            "nested" => Backend::Nested,
            "headless" => Backend::Headless,
            _ => return None,
        })
    }
}

/// Which configuration file a run should read, if any.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ConfigSource {
    /// The XDG search path, where not finding anything is normal.
    #[default]
    Search,
    /// One file named by `--config`, where not finding it is worth saying.
    File(PathBuf),
    /// `--no-config`: defaults and flags, nothing else.
    None,
}

/// Everything the compositor reads at startup.
#[derive(Debug, Clone)]
pub struct Config {
    pub backend: Backend,
    /// Where the settings underneath the flags come from.
    pub source: ConfigSource,
    /// A font file to use instead of the built-in bitmap face.
    pub font: Option<PathBuf>,
    /// Faces consulted, in order, for glyphs the primary does not have.
    ///
    /// Naming faces here adds to the stack rather than replacing what the
    /// compositor finds on its own: if nothing in the list has kanji it still
    /// goes looking for a CJK face, because Japanese silently rendering as
    /// hollow boxes is never the outcome anyone was after.
    pub font_fallback: Vec<PathBuf>,
    /// Font size in pixels; `None` derives one from the display.
    pub font_size: Option<f32>,
    /// Integer scale for the built-in bitmap face.
    pub bitmap_scale: Option<u32>,
    pub scrollback: usize,
    /// The program each pane runs.
    pub command: Option<Vec<String>>,
    /// Draw the status bar along the bottom.
    pub status_bar: bool,
    /// Fade panes that do not have focus.
    pub inactive_fade: u8,
    /// Answer OSC 52 clipboard queries with the real selection. Off unless the
    /// user asks for it: anything that can write to a pane can send that query,
    /// so a `cat` of a hostile file, or a program on the far end of an ssh
    /// session, would otherwise read back whatever was last copied.
    pub allow_clipboard_read: bool,
    /// Headless size, and the fallback size when a backend cannot report one.
    pub size: (u32, u32),
    /// Write one frame here and exit.
    pub screenshot: Option<PathBuf>,
    /// Feed these bytes to the first pane before the first frame, so a
    /// screenshot can show something specific.
    pub preload: Option<String>,
    /// Frames to render before a screenshot is taken.
    pub warmup_frames: u32,
    /// The colours applications paint with, and what a palette reset returns
    /// to.
    pub palette: Palette,
    /// The colours the compositor paints its own dividers and bars with.
    pub chrome: Chrome,
    /// The file the screen lock checks a password against.
    ///
    /// A seam rather than a constant so that a test can point the whole state
    /// machine at a credential it wrote itself. Deliberately not a command
    /// line flag and not a configuration file setting: which file holds the
    /// password is not a preference, and a session that could be told to
    /// unlock against a file of the user's choosing would be a lock with a
    /// spare key printed on it.
    pub credential: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            backend: Backend::Auto,
            source: ConfigSource::Search,
            font: None,
            font_fallback: Vec::new(),
            font_size: None,
            bitmap_scale: None,
            scrollback: 10_000,
            command: None,
            status_bar: true,
            inactive_fade: 40,
            allow_clipboard_read: false,
            size: (1280, 720),
            screenshot: None,
            preload: None,
            warmup_frames: 1,
            palette: Palette::new(),
            chrome: Chrome::default(),
            credential: PathBuf::from(crate::lock::CREDENTIAL_PATH),
        }
    }
}

/// The half of `tos --help` that describes the command line.
const OPTIONS: &str = "\
tOS, a terminal-native compositor

usage: tos [options] [-e command [args...]]

options:
  --backend <auto|drm|nested|headless>   display backend (default: auto)
  --font <path>                          TrueType font file
  --font-fallback <path>                 extra face for missing glyphs (repeatable)
  --font-size <pixels>                   font size in pixels
  --bitmap-scale <n>                     scale for the built-in bitmap font
  --scrollback <lines>                   scrollback per pane (default: 10000)
  --size <WxH>                           framebuffer size for headless mode
  --screenshot <path.ppm>                render one frame, save it and exit
  --preload <text>                       feed text to the first pane first
  --no-status-bar                        hide the status bar
  --allow-clipboard-read                 let programs read the clipboard (OSC 52)
  --config <path>                        read this file instead of searching
  --no-config                            ignore the configuration file
  -e, --command <program> [args...]      program to run instead of the shell
  -h, --help                             show this message
  -V, --version                          show the version

configuration file, first one found wins:
  $XDG_CONFIG_HOME/tos/tos.conf, or ~/.config/tos/tos.conf
  $XDG_CONFIG_DIRS/tos/tos.conf, or /etc/xdg/tos/tos.conf
  /etc/tos/tos.conf
Options given here always win over the file. A line the file gets wrong is
reported and skipped; see the README for the settings it understands.
";

/// The usage text shown by `tos --help`.
///
/// The bindings half is generated from the keymap the compositor is about to
/// run rather than written out here, so `--help` and the sheet `leader ?` puts
/// over the panes cannot come to disagree about what a key does. A list of
/// bindings kept by hand is wrong within a release of being written.
pub fn usage() -> String {
    let mut text = String::from(OPTIONS);
    text.push('\n');
    text.push_str(&bindings(&Keymap::default_bindings()));
    text
}

/// The `key bindings` section, for the map it describes.
pub fn bindings(keymap: &Keymap) -> String {
    let mut text = String::from("key bindings");
    if let Some(leader) = describe::leader_name(keymap) {
        text.push_str(&format!(" (leader is {leader}"));
        // Worth mentioning only when there are super bindings to mention; a
        // keymap that binds none would be claiming something untrue.
        if describe::super_works_alone(keymap) {
            text.push_str("; super works without the leader");
        }
        text.push(')');
    }
    text.push_str(":\n");
    for row in describe::cheat_sheet(keymap) {
        text.push_str(&format!("  {:<30} {}\n", row.keys, row.action));
    }
    text
}

/// Parse command line arguments.
pub fn parse_args(args: &[String]) -> Result<Config, String> {
    parse_args_over(Config::default(), args)
}

/// Parse command line arguments over settings that came from somewhere else.
///
/// Taking the starting point as an argument is the whole of "flags win": the
/// file is read into `base` and every flag then overwrites what it named.
pub fn parse_args_over(base: Config, args: &[String]) -> Result<Config, String> {
    let mut config = base;
    // Whether the backend was chosen on the command line rather than by the
    // file, which decides whether `--screenshot` may claim it.
    let mut backend_from_flag = false;
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        let mut value = |name: &str| -> Result<String, String> {
            index += 1;
            args.get(index)
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match arg {
            "--backend" => {
                let name = value("--backend")?;
                config.backend =
                    Backend::parse(&name).ok_or_else(|| format!("unknown backend: {name}"))?;
                backend_from_flag = true;
            }
            "--config" => config.source = ConfigSource::File(PathBuf::from(value("--config")?)),
            "--no-config" => config.source = ConfigSource::None,
            "--font" => config.font = Some(PathBuf::from(value("--font")?)),
            // Repeating the flag appends, so the order on the command line is
            // the order the faces are tried in.
            "--font-fallback" => config
                .font_fallback
                .push(PathBuf::from(value("--font-fallback")?)),
            "--font-size" => {
                let text = value("--font-size")?;
                config.font_size = Some(text.parse().map_err(|_| format!("not a number: {text}"))?);
            }
            "--bitmap-scale" => {
                let text = value("--bitmap-scale")?;
                config.bitmap_scale =
                    Some(text.parse().map_err(|_| format!("not a number: {text}"))?);
            }
            "--scrollback" => {
                let text = value("--scrollback")?;
                config.scrollback = text.parse().map_err(|_| format!("not a number: {text}"))?;
            }
            "--size" => config.size = parse_size(&value("--size")?)?,
            "--screenshot" => {
                config.screenshot = Some(PathBuf::from(value("--screenshot")?));
                // A screenshot is inherently off screen. A backend the file
                // asked for is a standing preference rather than a decision
                // about this run, so it does not stand in the way.
                if !backend_from_flag {
                    config.backend = Backend::Headless;
                }
            }
            "--preload" => config.preload = Some(value("--preload")?),
            "--warmup" => {
                let text = value("--warmup")?;
                config.warmup_frames = text.parse().map_err(|_| format!("not a number: {text}"))?;
            }
            "--no-status-bar" => config.status_bar = false,
            "--allow-clipboard-read" => config.allow_clipboard_read = true,
            "-e" | "--command" => {
                // Everything after this is the command and its arguments.
                let rest: Vec<String> = args[index + 1..].to_vec();
                if rest.is_empty() {
                    return Err("-e needs a program".into());
                }
                config.command = Some(rest);
                return Ok(config);
            }
            other => return Err(format!("unknown option: {other}")),
        }
        index += 1;
    }
    Ok(config)
}

/// Parse a `WxH` size, which is spelled the same way on the command line and
/// in the file.
pub fn parse_size(text: &str) -> Result<(u32, u32), String> {
    let (w, h) = text
        .split_once(['x', 'X'])
        .ok_or_else(|| format!("expected WxH, got {text}"))?;
    Ok((
        w.parse().map_err(|_| format!("not a number: {w}"))?,
        h.parse().map_err(|_| format!("not a number: {h}"))?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn defaults_are_sensible() {
        let config = parse_args(&[]).unwrap();
        assert_eq!(config.backend, Backend::Auto);
        assert!(config.status_bar);
        assert!(config.command.is_none());
    }

    #[test]
    fn backend_is_parsed() {
        assert_eq!(
            parse_args(&args(&["--backend", "drm"])).unwrap().backend,
            Backend::Drm
        );
        assert!(parse_args(&args(&["--backend", "opengl"])).is_err());
    }

    #[test]
    fn size_is_parsed() {
        let config = parse_args(&args(&["--size", "800x600"])).unwrap();
        assert_eq!(config.size, (800, 600));
        assert!(parse_args(&args(&["--size", "800"])).is_err());
    }

    #[test]
    fn a_screenshot_implies_headless() {
        let config = parse_args(&args(&["--screenshot", "out.ppm"])).unwrap();
        assert_eq!(config.backend, Backend::Headless);
        assert_eq!(config.screenshot.unwrap().to_str().unwrap(), "out.ppm");
    }

    #[test]
    fn an_explicit_backend_survives_a_screenshot() {
        let config =
            parse_args(&args(&["--backend", "nested", "--screenshot", "out.ppm"])).unwrap();
        assert_eq!(config.backend, Backend::Nested);
    }

    #[test]
    fn everything_after_dash_e_is_the_command() {
        let config = parse_args(&args(&["-e", "sh", "-c", "echo hi"])).unwrap();
        assert_eq!(
            config.command.unwrap(),
            vec!["sh".to_string(), "-c".into(), "echo hi".into()]
        );
    }

    #[test]
    fn font_fallbacks_accumulate_in_order() {
        let config = parse_args(&args(&[
            "--font-fallback",
            "/a.ttf",
            "--font-fallback",
            "/b.ttf",
        ]))
        .unwrap();
        let paths: Vec<&str> = config
            .font_fallback
            .iter()
            .map(|p| p.to_str().unwrap())
            .collect();
        assert_eq!(paths, vec!["/a.ttf", "/b.ttf"]);
    }

    #[test]
    fn no_font_fallbacks_by_default() {
        assert!(parse_args(&[]).unwrap().font_fallback.is_empty());
    }

    #[test]
    fn missing_values_are_errors() {
        assert!(parse_args(&args(&["--font"])).is_err());
        assert!(parse_args(&args(&["--font-fallback"])).is_err());
        assert!(parse_args(&args(&["-e"])).is_err());
    }

    #[test]
    fn clipboard_reads_are_off_until_asked_for() {
        assert!(!parse_args(&[]).unwrap().allow_clipboard_read);
        let config = parse_args(&args(&["--allow-clipboard-read"])).unwrap();
        assert!(config.allow_clipboard_read);
    }

    #[test]
    fn unknown_options_are_errors() {
        assert!(parse_args(&args(&["--wayland"])).is_err());
    }

    #[test]
    fn the_config_file_can_be_named_or_refused() {
        assert_eq!(parse_args(&[]).unwrap().source, ConfigSource::Search);
        assert_eq!(
            parse_args(&args(&["--config", "/tmp/tos.conf"]))
                .unwrap()
                .source,
            ConfigSource::File(PathBuf::from("/tmp/tos.conf"))
        );
        assert_eq!(
            parse_args(&args(&["--no-config"])).unwrap().source,
            ConfigSource::None
        );
        assert!(parse_args(&args(&["--config"])).is_err());
    }

    #[test]
    fn flags_land_on_top_of_the_file() {
        let from_file = Config {
            scrollback: 500,
            font_size: Some(12.0),
            ..Config::default()
        };
        let config = parse_args_over(from_file, &args(&["--scrollback", "9"])).unwrap();
        assert_eq!(config.scrollback, 9);
        // Nothing the flags did not mention is disturbed.
        assert_eq!(config.font_size, Some(12.0));
    }

    #[test]
    fn a_screenshot_beats_a_backend_the_file_asked_for() {
        let from_file = Config {
            backend: Backend::Drm,
            ..Config::default()
        };
        let config = parse_args_over(from_file, &args(&["--screenshot", "out.ppm"])).unwrap();
        assert_eq!(config.backend, Backend::Headless);
    }

    #[test]
    fn the_help_lists_the_bindings_the_compositor_will_run() {
        // The reason the section is generated: no line of it can survive a
        // binding being moved, because no line of it is written down.
        let text = usage();
        for row in describe::cheat_sheet(&Keymap::default_bindings()) {
            assert!(
                text.contains(&row.keys),
                "{:?} missing from --help",
                row.keys
            );
            assert!(text.contains(&row.action), "{:?} missing", row.action);
        }
        assert!(text.contains("key bindings (leader is ctrl+a"));
        assert!(text.contains("super works without the leader"));
    }

    #[test]
    fn the_help_still_describes_the_options() {
        let text = usage();
        assert!(text.starts_with("tOS, a terminal-native compositor"));
        assert!(text.contains("--backend"));
        assert!(text.contains("-e, --command"));
    }

    #[test]
    fn a_map_with_no_leader_and_no_super_says_neither() {
        use tos_input::{KeyCode, Modifiers};
        use tos_session::{Action, Binding};

        let mut keymap = Keymap::empty();
        keymap.bind(
            Binding::new(KeyCode::Function(1), Modifiers::NONE),
            Action::Quit,
        );
        let text = bindings(&keymap);
        assert!(text.starts_with("key bindings:\n"), "{text:?}");
        assert!(text.contains("f1"));
        assert!(text.contains("quit tOS"));
    }
}
