//! Compositor configuration.

use std::path::PathBuf;

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

/// Everything the compositor reads at startup.
#[derive(Debug, Clone)]
pub struct Config {
    pub backend: Backend,
    /// A font file to use instead of the built-in bitmap face.
    pub font: Option<PathBuf>,
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
    /// Headless size, and the fallback size when a backend cannot report one.
    pub size: (u32, u32),
    /// Write one frame here and exit.
    pub screenshot: Option<PathBuf>,
    /// Feed these bytes to the first pane before the first frame, so a
    /// screenshot can show something specific.
    pub preload: Option<String>,
    /// Frames to render before a screenshot is taken.
    pub warmup_frames: u32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            backend: Backend::Auto,
            font: None,
            font_size: None,
            bitmap_scale: None,
            scrollback: 10_000,
            command: None,
            status_bar: true,
            inactive_fade: 40,
            size: (1280, 720),
            screenshot: None,
            preload: None,
            warmup_frames: 1,
        }
    }
}

/// The usage text shown by `tos --help`.
pub const USAGE: &str = "\
tOS, a terminal-native compositor

usage: tos [options] [-e command [args...]]

options:
  --backend <auto|drm|nested|headless>   display backend (default: auto)
  --font <path>                          TrueType font file
  --font-size <pixels>                   font size in pixels
  --bitmap-scale <n>                     scale for the built-in bitmap font
  --scrollback <lines>                   scrollback per pane (default: 10000)
  --size <WxH>                           framebuffer size for headless mode
  --screenshot <path.ppm>                render one frame, save it and exit
  --preload <text>                       feed text to the first pane first
  --no-status-bar                        hide the status bar
  -e, --command <program> [args...]      program to run instead of the shell
  -h, --help                             show this message
  -V, --version                          show the version

key bindings (leader is ctrl+a; super works without the leader):
  ctrl+shift+enter  split the focused pane
  ctrl+shift+t      new workspace
  leader d / s      split into columns / rows
  leader h j k l    move focus
  leader z          zoom the focused pane
  leader x          close the focused pane
  leader c / n / p  new / next / previous workspace
  leader 1..9       select a workspace
  leader q          quit
  shift+pageup      scroll back
";

/// Parse command line arguments.
pub fn parse_args(args: &[String]) -> Result<Config, String> {
    let mut config = Config::default();
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
                config.backend = Backend::parse(&name)
                    .ok_or_else(|| format!("unknown backend: {name}"))?;
            }
            "--font" => config.font = Some(PathBuf::from(value("--font")?)),
            "--font-size" => {
                let text = value("--font-size")?;
                config.font_size = Some(
                    text.parse()
                        .map_err(|_| format!("not a number: {text}"))?,
                );
            }
            "--bitmap-scale" => {
                let text = value("--bitmap-scale")?;
                config.bitmap_scale = Some(
                    text.parse()
                        .map_err(|_| format!("not a number: {text}"))?,
                );
            }
            "--scrollback" => {
                let text = value("--scrollback")?;
                config.scrollback = text
                    .parse()
                    .map_err(|_| format!("not a number: {text}"))?;
            }
            "--size" => {
                let text = value("--size")?;
                let (w, h) = text
                    .split_once(['x', 'X'])
                    .ok_or_else(|| format!("expected WxH, got {text}"))?;
                config.size = (
                    w.parse().map_err(|_| format!("not a number: {w}"))?,
                    h.parse().map_err(|_| format!("not a number: {h}"))?,
                );
            }
            "--screenshot" => {
                config.screenshot = Some(PathBuf::from(value("--screenshot")?));
                // A screenshot is inherently off screen.
                if config.backend == Backend::Auto {
                    config.backend = Backend::Headless;
                }
            }
            "--preload" => config.preload = Some(value("--preload")?),
            "--warmup" => {
                let text = value("--warmup")?;
                config.warmup_frames =
                    text.parse().map_err(|_| format!("not a number: {text}"))?;
            }
            "--no-status-bar" => config.status_bar = false,
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
    fn missing_values_are_errors() {
        assert!(parse_args(&args(&["--font"])).is_err());
        assert!(parse_args(&args(&["-e"])).is_err());
    }

    #[test]
    fn unknown_options_are_errors() {
        assert!(parse_args(&args(&["--wayland"])).is_err());
    }
}
