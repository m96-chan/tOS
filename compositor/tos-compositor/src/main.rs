//! `tos`, the terminal compositor.
//!
//! ```text
//! Linux kernel -> DRM/KMS -> tOS compositor -> PTY -> shell
//! ```

use std::io::{self, Write};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};

use tos_input::host::HostInput;
use tos_platform::tty::ReadOutcome;
use tos_platform::{Display, HeadlessDisplay, NestedDisplay};

use tos_compositor::config::{usage, Backend, Config};
use tos_compositor::config_file;
use tos_compositor::power;
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
        libc::signal(
            libc::SIGTERM,
            on_terminate as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGINT,
            on_terminate as *const () as libc::sighandler_t,
        );
        // Writing to a PTY whose child has gone must return an error, not kill
        // the compositor.
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Everything after `-e` belongs to the child, so `tos -e git --help` must
    // run git's help rather than printing this one.
    let own_args = match args.iter().position(|a| a == "-e" || a == "--command") {
        Some(at) => &args[..at],
        None => &args[..],
    };
    if own_args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{}", usage());
        return ExitCode::SUCCESS;
    }
    if own_args.iter().any(|a| a == "-V" || a == "--version") {
        println!("tOS {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }

    let startup = match config_file::startup(&args) {
        Ok(startup) => startup,
        Err(message) => {
            eprintln!("tos: {message}");
            eprintln!("try 'tos --help'");
            return ExitCode::from(2);
        }
    };
    // A file this machine cannot read is not a reason to leave someone without
    // a terminal, any more than a missing display backend is. Say what was
    // wrong with it and carry on with the settings that did make sense.
    for problem in &startup.problems {
        eprintln!("tos: {problem}");
    }
    let config = startup.config;

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

/// Take the blank deadline away from a backend that has no panel to put to
/// sleep.
///
/// `Display::blank` is defaulted to doing nothing for exactly those backends,
/// and a session that believed it was dark when it was not would be a session
/// that swallows the keystroke waking a screen its user could see all along.
/// The lock deadline is untouched: locking means the same thing everywhere.
fn without_blanking(config: Config) -> Config {
    Config {
        idle_blank: None,
        ..config
    }
}

fn run_headless(config: Config) -> io::Result<()> {
    let config = without_blanking(config);
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
            // And the machine a chance to be read. The status bar's battery,
            // network and volume come from a poll on a timer rather than from
            // anything a pane does, so a picture taken without one is a
            // picture of a bar that has not been told what machine it is on.
            compositor.tick();
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
        sleep_if_asked(&mut compositor);
    }
    compositor.shut_down()
}

fn run_nested(config: Config) -> io::Result<()> {
    let config = without_blanking(config);
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
    let mut input_ended = false;

    while compositor.is_running() && !TERMINATE.load(Ordering::Relaxed) && !input_ended {
        if RESIZED.swap(false, Ordering::Relaxed) && display.refresh_size()? {
            compositor.resize(display.size());
        }
        // A bare escape is indistinguishable from the start of a sequence
        // until nothing follows it, so the idle pass is what delivers the key.
        if decoder.has_pending_escape() {
            for event in decoder.flush() {
                compositor.handle_input(event);
            }
        }
        compositor.run_once(&mut display, &[input_fd], |fd| {
            match tos_platform::tty::read_available(fd, &mut buf) {
                Ok(ReadOutcome::Data(n)) => decoder.feed(&buf[..n]),
                Ok(ReadOutcome::WouldBlock) => Vec::new(),
                // The host terminal hung up. `poll` would keep reporting the
                // descriptor readable, so the loop has to end here.
                Ok(ReadOutcome::Eof) => {
                    input_ended = true;
                    Vec::new()
                }
                Err(_) => {
                    input_ended = true;
                    Vec::new()
                }
            }
        })?;
        sleep_if_asked(&mut compositor);
    }
    display.release()?;
    io::stdout().flush()?;
    compositor.shut_down()
}

/// Suspend on behalf of a session that owns none of the machine it is drawn
/// on: the nested and headless backends.
///
/// There is no DRM master to give up and no device grabbed, because a session
/// inside somebody else's terminal holds neither, so the whole of the suspend
/// is the sleep itself. It is still offered rather than refused: it is still
/// the user's machine, and they still asked it to sleep.
fn sleep_if_asked(compositor: &mut Compositor) {
    if !compositor.take_suspend_request() {
        return;
    }
    let outcome = power::suspend(&mut power::Unowned, compositor.machine_mut().power());
    compositor.resumed(&outcome);
}

/// One thing the DRM loop does about a VT switch the kernel is asking about.
///
/// The kernel suspends the switch until tOS answers, so every path through
/// here has to end in an answer. Nothing may be silent: a switch left
/// unanswered stays pending for good.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SwitchStep {
    /// Drop DRM master, so whoever is taking the console can set a mode.
    ReleaseDisplay,
    /// `VT_RELDISP 1`: the switch goes ahead and tOS is no longer foreground.
    AllowSwitch,
    /// `VT_RELDISP 0`: the kernel abandons the switch and tOS stays where it
    /// is, still holding master.
    RefuseSwitch,
}

/// What a switch away costs, given whether the session is locked.
///
/// A list rather than a boolean because the order is the whole trap. Giving
/// up the display comes first and agreeing to the switch second, and a locked
/// session has to skip *both*. Skipping only the second would be the worst of
/// the three outcomes: master is gone, so any process that opens the card can
/// set its own mode over a locked screen, while the kernel still believes tOS
/// is the foreground terminal and leaves it there.
///
/// Refusing is decided afresh every time rather than armed once, because the
/// kernel puts no timeout on the answer. Each Ctrl+Alt+Fn on a locked session
/// costs one signal and one ioctl; the kernel keeps no queue of them, so
/// holding the key down cannot build anything up, and `VT_RELDISP 0` leaves
/// the terminal in the state it was in before the key — a switch asked for
/// after the password is accepted is answered like any other.
///
/// A blanked screen needs nothing extra on either side. Refusing touches the
/// CRTC not at all, so a session that switched its panel off while locked
/// stays dark through the attempt, and an unlocked one releases exactly as it
/// does today — `drm.rs` gives the device up as it stands and remembers the
/// blank for the way back.
fn switch_away_plan(locked: bool) -> &'static [SwitchStep] {
    if locked {
        return &[SwitchStep::RefuseSwitch];
    }
    &[SwitchStep::ReleaseDisplay, SwitchStep::AllowSwitch]
}

#[cfg(target_os = "linux")]
fn run_drm(config: Config) -> io::Result<()> {
    use tos_input::evdev::InputBackend;
    use tos_platform::{DrmDisplay, VirtualTerminal};

    /// The screen, the keyboard and the console, for the one thing that takes
    /// all three away underneath a running session.
    ///
    /// This is the only place in tOS that holds all three at once, which is
    /// why the order they are given up and taken back in lives in
    /// `tos_compositor::power` and only the calls themselves live here.
    struct Console<'a> {
        display: &'a mut DrmDisplay,
        input: &'a mut InputBackend,
        /// `None` on a machine where the VT could not be taken over, which is
        /// every nested and containerised one. There is then nothing to
        /// reclaim, and the suspend is not worth refusing over it.
        vt: Option<&'a mut VirtualTerminal>,
    }

    impl power::Hardware for Console<'_> {
        fn release_display(&mut self) -> io::Result<()> {
            // Drop DRM master. The mode on the far side is set from nothing by
            // `restore_display` below, which is the only honest thing to do
            // with a CRTC that a driver has been resetting while tOS was not
            // running to watch it.
            self.display.release()
        }

        fn ungrab_input(&mut self) -> io::Result<()> {
            self.input.ungrab_all()
        }

        fn reclaim_terminal(&mut self) -> io::Result<()> {
            let Some(vt) = self.vt.as_mut() else {
                return Ok(());
            };
            // The same call that took the terminal at startup, and it is
            // deliberately the same call: `KDSETMODE`, `KDSKBMODE` and
            // `VT_SETMODE` are all idempotent, the saved console state was
            // captured when the terminal was opened and is not touched again,
            // and a resume is exactly when the kernel's own console has woken
            // up and may start drawing text over the session.
            vt.take_over(libc::SIGUSR1, libc::SIGUSR2)
        }

        fn restore_display(&mut self) -> io::Result<()> {
            // Master again, and the next frame is a mode set rather than a
            // page flip — which is `Display::restore`, the same path a VT
            // switch back takes.
            self.display.restore()
        }

        fn grab_input(&mut self) -> io::Result<()> {
            // The devices opened at startup, and only those. tOS has no
            // hotplug, so a keyboard the kernel re-enumerates under a
            // different `/dev/input/eventN` across the resume is gone until
            // the session is restarted, and no amount of re-grabbing here
            // would find it. What this reclaims is the ordinary case: the
            // node survived the sleep and only the exclusive grab did not.
            self.input.grab_all()
        }

        fn drain_input(&mut self) -> io::Result<()> {
            // Read and throw away, in that order: the queue has to be emptied
            // through the translator or the events would simply be waiting on
            // the next pass, and the modifier state has to be cleared after
            // that or the translator would have put back the very keys this is
            // forgetting.
            let _ = self.input.poll()?;
            self.input.forget_held_keys();
            Ok(())
        }
    }

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
    // The switch signals need handlers first: their default action is to
    // terminate, which would leave the console in graphics mode with no
    // keyboard and no chance to put it back.
    let mut vt = VirtualTerminal::current().ok();
    if let Some(vt) = vt.as_mut() {
        // Clear `vt_dont_switch` before anything else, whoever set it. The
        // kernel does not clear it when the process that took it dies — that
        // was measured, in `docs/design/vt-lockswitch.md` — so a machine whose
        // terminals cannot be switched stays that way until something calls
        // this. tOS never takes the flag, and an installed system respawns
        // `tos` from `/etc/inittab`, so one ioctl here turns a stuck flag into
        // something a restart undoes. Failing means this is not a console or
        // tOS lacks CAP_SYS_TTY_CONFIG, and neither is worth a word about.
        let _ = vt.unlock_switching();
        let armed = tos_platform::install_switch_handlers(libc::SIGUSR1, libc::SIGUSR2)
            .and_then(|()| vt.take_over(libc::SIGUSR1, libc::SIGUSR2));
        if let Err(e) = armed {
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

    let mut suspended = false;
    while compositor.is_running() && !TERMINATE.load(Ordering::Relaxed) {
        // A VT switch is acted on here rather than in the signal handler,
        // where releasing the display would not be safe.
        if tos_platform::take_switch_away() && !suspended {
            // The plan rather than an `if` here so that there is exactly one
            // place that decides what a switch costs, and it is a place a
            // test can read.
            for step in switch_away_plan(compositor.is_locked()) {
                match step {
                    SwitchStep::ReleaseDisplay => {
                        let _ = display.release();
                    }
                    SwitchStep::AllowSwitch => {
                        if let Some(vt) = vt.as_ref() {
                            let _ = vt.allow_switch_away();
                        }
                        suspended = true;
                    }
                    SwitchStep::RefuseSwitch => {
                        // Nothing else happens: the screen stays tOS's, the
                        // panes keep running, and the next iteration draws the
                        // lock again as though no key had been pressed. If
                        // even the refusal fails the kernel is left holding a
                        // switch it will never complete, which on a locked
                        // session is the same answer by a worse road.
                        if let Some(vt) = vt.as_ref() {
                            let _ = vt.refuse_switch_away();
                        }
                    }
                }
            }
        }
        if tos_platform::take_switch_back() && suspended {
            if let Some(vt) = vt.as_ref() {
                let _ = vt.acknowledge_switch_back();
            }
            let _ = display.restore();
            compositor.perform(tos_session::Action::Refresh);
            suspended = false;
        }
        if suspended {
            // Another VT owns the screen; stay out of its way but keep the
            // panes running so their output is there on the way back.
            compositor.pump_panes();
            std::thread::sleep(std::time::Duration::from_millis(50));
            continue;
        }

        // Draining every device on the first ready descriptor is harmless:
        // later calls in the same iteration simply find nothing left.
        compositor.run_once(&mut display, &input_fds, |_fd| {
            input.poll().unwrap_or_default()
        })?;

        // After the frame, so that what is on the panel while the machine
        // sleeps is the screen the session meant to leave behind — the lock,
        // on a machine that has a password.
        if compositor.take_suspend_request() {
            let outcome = {
                let mut console = Console {
                    display: &mut display,
                    input: &mut input,
                    vt: vt.as_mut(),
                };
                power::suspend(&mut console, compositor.machine_mut().power())
            };
            compositor.resumed(&outcome);
        }
    }

    if let Some(vt) = vt.as_mut() {
        vt.restore();
    }
    // Everything goes back before the machine is asked to stop: the console to
    // text mode, the CRTC to the mode it was found in, the keyboards to
    // whoever else wants them. A `reboot(2)` from a session still holding all
    // three would say whatever the kernel has to say on the way down onto a
    // screen in graphics mode that nobody can read — and if the syscall is
    // refused, it would leave the user in front of exactly that.
    drop(display);
    drop(input);
    compositor.shut_down()
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
    fn help_flags_after_dash_e_belong_to_the_child() {
        let args: Vec<String> = ["-e", "git", "--help"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let at = args.iter().position(|a| a == "-e" || a == "--command");
        assert_eq!(at, Some(0));
        let own = &args[..at.unwrap()];
        assert!(!own.iter().any(|a| a == "--help"));
    }

    #[test]
    fn help_flags_before_dash_e_are_ours() {
        let args: Vec<String> = ["--help", "-e", "sh"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let at = args.iter().position(|a| a == "-e" || a == "--command");
        let own = &args[..at.unwrap()];
        assert!(own.iter().any(|a| a == "--help"));
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
    fn a_backend_with_no_panel_does_not_pretend_to_blank() {
        let config = Config::default();
        assert!(config.idle_blank.is_some(), "the default blanks");
        let nested = without_blanking(config.clone());
        assert_eq!(nested.idle_blank, None);
        // Locking is not a property of the screen, so it survives.
        assert_eq!(nested.idle_lock, config.idle_lock);
    }

    #[test]
    fn auto_never_resolves_to_auto() {
        let config = Config::default();
        assert_ne!(choose_backend(&config), Backend::Auto);
    }

    #[test]
    fn an_unlocked_session_gives_up_the_display_before_it_agrees_to_the_switch() {
        // Both, and in this order: a process that answered the switch while
        // still holding DRM master would hand the console to a terminal that
        // cannot put anything on the screen.
        assert_eq!(
            switch_away_plan(false),
            &[SwitchStep::ReleaseDisplay, SwitchStep::AllowSwitch]
        );
    }

    #[test]
    fn a_locked_session_skips_the_release_as_well_as_the_switch() {
        // The trap the whole change exists for. Refusing the switch while
        // still calling `display.release()` reads like a lock and is not one:
        // master would already be gone by the time the answer was given, so
        // anything that opened the card could draw over the locked screen.
        let plan = switch_away_plan(true);
        assert_eq!(plan, &[SwitchStep::RefuseSwitch]);
        assert!(!plan.contains(&SwitchStep::ReleaseDisplay));
    }

    #[test]
    fn every_plan_answers_the_kernel_exactly_once() {
        // The kernel holds the switch until it is answered and never times
        // out, so a plan that answered twice, or not at all, would be a
        // console that cannot be switched by anyone.
        for locked in [false, true] {
            let answers = switch_away_plan(locked)
                .iter()
                .filter(|step| matches!(step, SwitchStep::AllowSwitch | SwitchStep::RefuseSwitch))
                .count();
            assert_eq!(answers, 1, "locked = {locked}");
        }
    }
}
