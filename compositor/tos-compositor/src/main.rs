//! `tos`, the terminal compositor.
//!
//! ```text
//! Linux kernel -> DRM/KMS -> tOS compositor -> PTY -> shell
//! ```

use std::io::{self, Write};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};

use tos_input::host::HostInput;
use tos_platform::{Display, HeadlessDisplay, NestedDisplay};

use tos_compositor::config::{parse_args, Backend, Config, USAGE};
use tos_compositor::Compositor;

/// Set from a signal handler when the host terminal changes size.
static RESIZED: AtomicBool = AtomicBool::new(false);
/// Set when the compositor is asked to stop.
static TERMINATE: AtomicBool = AtomicBool::new(false);

extern "C" fn on_winch(_: libc::c_int) {
    RESIZED.store(true, Ordering::Relaxed);
}

extern "C" fn on_terminate(_: libc::c_int) {
    TERMINATE.store(true, Ordering::Relaxed);
}

fn install_signal_handlers() {
    unsafe {
        libc::signal(libc::SIGWINCH, on_winch as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_terminate as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_terminate as *const () as libc::sighandler_t);
        // Writing to a PTY whose child has gone must return an error, not kill
        // the compositor.
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    if args.iter().any(|a| a == "-V" || a == "--version") {
        println!("tOS {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }

    let config = match parse_args(&args) {
        Ok(config) => config,
        Err(message) => {
            eprintln!("tos: {message}");
            eprintln!("try 'tos --help'");
            return ExitCode::from(2);
        }
    };

    install_signal_handlers();
    match run(config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // The display backends restore the terminal on drop, so by the
            // time this prints the screen is usable again.
            eprintln!("tos: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(config: Config) -> io::Result<()> {
    match choose_backend(&config) {
        Backend::Headless => run_headless(config),
        Backend::Nested => run_nested(config),
        Backend::Drm => run_drm(config),
        Backend::Auto => unreachable!("auto is resolved by choose_backend"),
    }
}

/// Decide what to run on when the user did not say.
fn choose_backend(config: &Config) -> Backend {
    match config.backend {
        Backend::Auto => {
            #[cfg(target_os = "linux")]
            {
                // A DRM device tOS can become master of means real hardware.
                if std::path::Path::new("/dev/dri/card0").exists()
                    && std::env::var_os("WAYLAND_DISPLAY").is_none()
                    && std::env::var_os("DISPLAY").is_none()
                {
                    return Backend::Drm;
                }
            }
            Backend::Nested
        }
        explicit => explicit,
    }
}

fn run_headless(config: Config) -> io::Result<()> {
    let (width, height) = config.size;
    let mut display = HeadlessDisplay::new(width, height);
    let screenshot = config.screenshot.clone();
    let warmup = config.warmup_frames.max(1);
    let preload = config.preload.clone();
    let mut compositor = Compositor::new(config, (width, height), None)?;

    if let Some(text) = &preload {
        compositor.inject(unescape(text).as_bytes());
    }

    if let Some(path) = screenshot {
        // Give the shell a moment to draw its prompt before the picture.
        for _ in 0..warmup {
            compositor.pump_panes();
            let retained = display.retains_contents();
            display.frame(&mut |surface| compositor.render_frame(surface, retained))?;
            std::thread::sleep(std::time::Duration::from_millis(60));
        }
        compositor.pump_panes();
        let retained = display.retains_contents();
        display.frame(&mut |surface| compositor.render_frame(surface, retained))?;
        display.save(&path)?;
        println!("wrote {}", path.display());
        return Ok(());
    }

    // Without a screenshot, headless mode is only useful as a soak test.
    while compositor.is_running() && !TERMINATE.load(Ordering::Relaxed) {
        compositor.run_once(&mut display, &[], |_| Vec::new())?;
    }
    Ok(())
}

fn run_nested(config: Config) -> io::Result<()> {
    let mut display = NestedDisplay::acquire().map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("cannot use the host terminal ({e}); try --backend headless"),
        )
    })?;
    let size = display.size();
    let input_fd = display.input_fd();
    let preload = config.preload.clone();
    let mut compositor = Compositor::new(config, size, None)?;
    if let Some(text) = &preload {
        compositor.inject(unescape(text).as_bytes());
    }

    let mut decoder = HostInput::new();
    let mut buf = vec![0u8; 8192];

    while compositor.is_running() && !TERMINATE.load(Ordering::Relaxed) {
        if RESIZED.swap(false, Ordering::Relaxed) && display.refresh_size()? {
            compositor.resize(display.size());
        }
        compositor.run_once(&mut display, &[input_fd], |fd| {
            match tos_platform::tty::read_available(fd, &mut buf) {
                Ok(0) => Vec::new(),
                Ok(n) => decoder.feed(&buf[..n]),
                Err(_) => Vec::new(),
            }
        })?;
    }
    display.release()?;
    io::stdout().flush()?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn run_drm(config: Config) -> io::Result<()> {
    use tos_input::evdev::InputBackend;
    use tos_platform::{DrmDisplay, VirtualTerminal};

    let mut display = DrmDisplay::open().map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("cannot take over the display ({e}); try --backend nested"),
        )
    })?;
    let size = display.size();
    let physical = display.physical_size();
    eprintln!("tos: {}", display.name());

    // Take the virtual terminal so the kernel stops drawing and reading keys.
    let mut vt = VirtualTerminal::current().ok();
    if let Some(vt) = vt.as_mut() {
        if let Err(e) = vt.take_over(libc::SIGUSR1, libc::SIGUSR2) {
            eprintln!("tos: continuing without VT ownership: {e}");
        }
    }

    let mut input = InputBackend::open_all(size.0, size.1)?;
    // Without grabbing, every keystroke would also reach the kernel console.
    if let Err(e) = input.grab_all() {
        eprintln!("tos: could not grab input devices: {e}");
    }
    let input_fds = input.fds();

    let preload = config.preload.clone();
    let mut compositor = Compositor::new(config, size, physical)?;
    if let Some(text) = &preload {
        compositor.inject(unescape(text).as_bytes());
    }

    while compositor.is_running() && !TERMINATE.load(Ordering::Relaxed) {
        // Draining every device on the first ready descriptor is harmless:
        // later calls in the same iteration simply find nothing left.
        compositor.run_once(&mut display, &input_fds, |_fd| {
            input.poll().unwrap_or_default()
        })?;
    }

    if let Some(vt) = vt.as_mut() {
        vt.restore();
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn run_drm(_config: Config) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "the DRM backend needs Linux; use --backend nested or --backend headless",
    ))
}

/// Expand the escapes `--preload` accepts, so shell quoting stays simple.
fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('e') => out.push('\x1b'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_are_expanded() {
        assert_eq!(unescape("a\\nb"), "a\nb");
        assert_eq!(unescape("\\e[31m"), "\x1b[31m");
        assert_eq!(unescape("\\\\"), "\\");
    }

    #[test]
    fn unknown_escapes_are_left_alone() {
        assert_eq!(unescape("\\q"), "\\q");
        assert_eq!(unescape("trailing\\"), "trailing\\");
    }

    #[test]
    fn an_explicit_backend_is_respected() {
        let config = Config {
            backend: Backend::Headless,
            ..Config::default()
        };
        assert_eq!(choose_backend(&config), Backend::Headless);
    }

    #[test]
    fn auto_never_resolves_to_auto() {
        let config = Config::default();
        assert_ne!(choose_backend(&config), Backend::Auto);
    }
}
