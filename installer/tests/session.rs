//! Drive the real `tos-install` binary on a pseudoterminal.
//!
//! The unit tests cover the state machine; this covers the thing a user
//! actually runs, including that it takes over the terminal and gives it back.

use std::time::{Duration, Instant};

use tos_pty::{Pty, PtyConfig, Winsize};

/// A prepared `/sys` tree, so the installer sees disks on any host.
struct FakeMachine {
    root: std::path::PathBuf,
}

impl FakeMachine {
    fn new(name: &str) -> FakeMachine {
        let root = std::env::temp_dir().join(format!("tos-install-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("proc")).unwrap();
        std::fs::write(root.join("proc/mounts"), "").unwrap();
        // A live session, which since GRUB left the initramfs means one with
        // a grub-install in it. Written here rather than left to the host:
        // whether the installer offers to install at all now depends on this
        // file, and a test that asks the developer's laptop for it passes or
        // fails for a reason that has nothing to do with the change.
        std::fs::create_dir_all(root.join("usr/sbin")).unwrap();
        std::fs::write(root.join("usr/sbin/grub-install"), "").unwrap();
        FakeMachine { root }
    }

    /// The session that runs when the squashfs will not mount, which has no
    /// GRUB and so cannot make anything bootable.
    fn rescue(self) -> Self {
        std::fs::remove_file(self.root.join("usr/sbin/grub-install")).unwrap();
        self
    }

    /// Add a disk of `gib` gibibytes.
    fn disk(self, name: &str, gib: u64) -> Self {
        let device = self.root.join("sys/block").join(name);
        std::fs::create_dir_all(device.join("device")).unwrap();
        let sectors = gib * (1 << 30) / 512;
        std::fs::write(device.join("size"), format!("{sectors}\n")).unwrap();
        std::fs::write(device.join("device/model"), "TEST DISK\n").unwrap();
        self
    }

    fn path(&self) -> String {
        self.root.to_string_lossy().into_owned()
    }
}

impl Drop for FakeMachine {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn spawn(machine: &FakeMachine, args: &[&str]) -> Pty {
    let mut config = PtyConfig::command(
        env!("CARGO_BIN_EXE_tos-install"),
        args.iter().map(|s| s.to_string()).collect(),
        Winsize::new(100, 32, 800, 512),
    );
    config
        .env
        .push(("TOS_INSTALL_SYSROOT".into(), machine.path()));
    Pty::spawn(&config).expect("spawn tos-install")
}

/// A terminal that the installer's output is played into.
///
/// Matching on the raw byte stream does not work: the installer only repaints
/// the cells that changed, so a line of text arrives split across cursor
/// moves. Interpreting the output with tOS's own terminal emulator is both
/// correct and the same thing the compositor would do with it.
struct Session {
    pty: Pty,
    terminal: tos_term::Terminal,
}

impl Session {
    fn new(machine: &FakeMachine, args: &[&str]) -> Session {
        Session {
            pty: spawn(machine, args),
            terminal: tos_term::Terminal::new(100, 32, tos_term::TerminalConfig::default()),
        }
    }

    /// Read until the screen shows `needle`, or time runs out.
    fn wait_for(&mut self, needle: &str) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut buf = [0u8; 65536];
        while Instant::now() < deadline {
            if self.screen().contains(needle) {
                return true;
            }
            if !self.pty.poll_readable(50).unwrap_or(false) {
                continue;
            }
            match self.pty.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => self.terminal.advance(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(_) => break,
            }
        }
        self.screen().contains(needle)
    }

    /// What is on screen right now.
    fn screen(&self) -> String {
        self.terminal.grid().to_text()
    }

    fn type_keys(&mut self, keys: &[u8]) {
        self.pty.write(keys).expect("write");
    }

    /// Wait for the child to exit and report its status.
    fn wait_for_exit(&mut self) -> Option<i32> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut buf = [0u8; 65536];
        while Instant::now() < deadline {
            if let Ok(Some(code)) = self.pty.try_wait() {
                return Some(code);
            }
            if self.pty.poll_readable(20).unwrap_or(false) {
                if let Ok(n) = self.pty.read(&mut buf) {
                    self.terminal.advance(&buf[..n]);
                }
            }
        }
        None
    }

    /// Walk from the welcome screen to the confirmation.
    fn reach_confirmation(&mut self, disk: &str) {
        assert!(self.wait_for("Enter to begin"), "{}", self.screen());
        self.type_keys(b"\r");
        assert!(self.wait_for("GiB"), "{}", self.screen());
        self.type_keys(b"\r");
        assert!(self.wait_for("Tab switches fields"), "{}", self.screen());
        self.type_keys(b"\r");
        assert!(
            self.wait_for(&format!("Type {disk} to confirm")),
            "{}",
            self.screen()
        );
    }
}

#[test]
fn the_welcome_screen_carries_the_banner_and_the_disk_count() {
    let machine = FakeMachine::new("welcome").disk("vda", 64).disk("sdb", 32);
    let mut session = Session::new(&machine, &["--dry-run"]);

    assert!(session.wait_for("Enter to begin"), "{}", session.screen());
    let screen = session.screen();
    assert!(
        screen.contains("the terminal is the desktop"),
        "the banner should greet the user:\n{screen}"
    );
    assert!(screen.contains("Install tOS on this machine"));
    assert!(screen.contains("2 disks can be installed onto"));
    // The banner is the art file, not something the installer made up. The art
    // carries its own colours, so it is the drawn text that has to match.
    let art = tos_install::motd::art_lines(tos_install::motd::ART);
    assert!(screen.contains(art.last().unwrap().trim()));
}

#[test]
fn the_disk_picker_lists_what_was_found() {
    let machine = FakeMachine::new("picker").disk("vda", 64);
    let mut session = Session::new(&machine, &["--dry-run"]);
    assert!(session.wait_for("Enter to begin"));

    session.type_keys(b"\r");
    // Wait for the row itself: the frame arrives before what is inside it.
    assert!(session.wait_for("64 GiB"), "{}", session.screen());
    let screen = session.screen();
    assert!(screen.contains("Where should tOS go?"), "{screen}");
    assert!(screen.contains("/dev/vda"), "{screen}");
}

#[test]
fn the_confirmation_demands_the_disk_name() {
    let machine = FakeMachine::new("confirm").disk("vda", 64);
    let mut session = Session::new(&machine, &["--dry-run"]);
    session.reach_confirmation("vda");

    let screen = session.screen();
    assert!(screen.contains("This erases the disk"), "{screen}");
    assert!(screen.contains("replacing everything on the disk"));

    // The word people type without reading does not start anything.
    session.type_keys(b"yes\r");
    assert!(
        session.wait_for("Type vda exactly"),
        "a wrong phrase must be refused:\n{}",
        session.screen()
    );
    assert!(
        !session.screen().contains("Partition the disk"),
        "the installation must not have started"
    );
}

#[test]
fn typing_the_disk_name_runs_the_dry_run_and_writes_nothing() {
    let machine = FakeMachine::new("dryrun").disk("vda", 64);
    let mut session = Session::new(&machine, &["--dry-run"]);
    session.reach_confirmation("vda");

    session.type_keys(b"vda\r");
    assert!(
        session.wait_for("Install the bootloader"),
        "{}",
        session.screen()
    );
    let screen = session.screen();
    assert!(screen.contains("What would happen"), "{screen}");
    assert!(screen.contains("Partition the disk"));

    // Leaving puts the terminal back and reports success.
    session.type_keys(b"\r");
    assert_eq!(session.wait_for_exit(), Some(0));
}

#[test]
fn escape_leaves_without_touching_anything() {
    let machine = FakeMachine::new("escape").disk("vda", 64);
    let mut session = Session::new(&machine, &["--dry-run"]);
    assert!(session.wait_for("Enter to begin"));

    // A bare escape only becomes a keypress once nothing follows it, which
    // the installer has to wait for rather than block on a read.
    session.type_keys(b"\x1b");
    assert_eq!(
        session.wait_for_exit(),
        Some(0),
        "escape should leave cleanly"
    );
}

#[test]
fn escape_steps_back_through_the_screens() {
    let machine = FakeMachine::new("back").disk("vda", 64);
    let mut session = Session::new(&machine, &["--dry-run"]);
    session.reach_confirmation("vda");

    session.type_keys(b"\x1b");
    assert!(
        session.wait_for("Tab switches fields"),
        "{}",
        session.screen()
    );
    session.type_keys(b"\x1b");
    assert!(session.wait_for("GiB"), "{}", session.screen());
}

#[test]
fn the_machine_can_be_named_before_installing() {
    let machine = FakeMachine::new("naming").disk("vda", 64);
    let mut session = Session::new(&machine, &["--dry-run"]);
    assert!(session.wait_for("Enter to begin"));
    session.type_keys(b"\r");
    assert!(session.wait_for("GiB"));
    session.type_keys(b"\r");
    assert!(session.wait_for("Tab switches fields"));

    // Clear the default host name and type another.
    session.type_keys(b"\x7f\x7f\x7f\x7f\x7f\x7fworkshop");
    assert!(session.wait_for("workshop"), "{}", session.screen());

    session.type_keys(b"\r");
    assert!(
        session.wait_for("Type vda to confirm"),
        "{}",
        session.screen()
    );
    assert!(
        session.screen().contains("workshop"),
        "the name should be in the summary:\n{}",
        session.screen()
    );
}

#[test]
fn a_password_is_masked_and_has_to_be_typed_twice() {
    let machine = FakeMachine::new("password").disk("vda", 64);
    let mut session = Session::new(&machine, &["--dry-run"]);
    assert!(session.wait_for("Enter to begin"));
    session.type_keys(b"\r");
    assert!(session.wait_for("GiB"));
    session.type_keys(b"\r");
    assert!(session.wait_for("Tab switches fields"));
    assert!(
        session.screen().contains("the screen will never lock"),
        "an empty password should say what it costs:\n{}",
        session.screen()
    );

    // Two tabs to the password field, then a password and a typo of it.
    session.type_keys(b"\t\thunter2\thunterZ\r");
    assert!(session.wait_for("do not match"), "{}", session.screen());
    let screen = session.screen();
    assert!(
        !screen.contains("hunter2") && !screen.contains("hunterZ"),
        "the password was echoed:\n{screen}"
    );
    assert!(
        screen.contains("Set up this machine"),
        "a mismatch must not leave the screen:\n{screen}"
    );

    // Both entries were cleared, so agreeing is typing it twice from here.
    session.type_keys(b"hunter2\thunter2\r");
    assert!(
        session.wait_for("Type vda to confirm"),
        "{}",
        session.screen()
    );
    let screen = session.screen();
    assert!(screen.contains("Password   set"), "{screen}");
    assert!(!screen.contains("hunter2"), "{screen}");
}

#[test]
fn a_machine_with_no_usable_disk_says_so() {
    let machine = FakeMachine::new("nodisk");
    let mut session = Session::new(&machine, &["--dry-run"]);
    assert!(
        session.wait_for("No disk on this machine can be installed onto"),
        "{}",
        session.screen()
    );

    // And it refuses to go any further.
    session.type_keys(b"\r");
    std::thread::sleep(Duration::from_millis(300));
    session.wait_for("nothing");
    assert!(
        !session.screen().contains("Where should tOS go?"),
        "{}",
        session.screen()
    );
}

#[test]
fn the_plan_can_be_printed_without_a_terminal() {
    // What a person runs before trusting the installer with a disk.
    let machine = FakeMachine::new("plan").disk("vda", 64);
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_tos-install"))
        .arg("--plan")
        .env("TOS_INSTALL_SYSROOT", machine.path())
        .output()
        .expect("run tos-install --plan");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "stderr: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(text.contains("/dev/vda"));
    assert!(
        text.contains("sfdisk"),
        "the real commands should be listed: {text}"
    );
    assert!(text.contains("mkfs.ext4"));
    assert!(text.contains("grub-install"));
}

#[test]
fn the_plan_of_a_rescue_session_says_it_cannot_finish_the_job() {
    // The first of the three places this has to be said, and the one a
    // careful person reads before they run anything at all.
    let machine = FakeMachine::new("rescueplan").disk("vda", 64).rescue();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_tos-install"))
        .arg("--plan")
        .env("TOS_INSTALL_SYSROOT", machine.path())
        .output()
        .expect("run tos-install --plan");
    let text = String::from_utf8_lossy(&output.stdout);
    let problem = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "it offered to install: {text}");
    assert!(
        problem.contains("cannot install a bootloader"),
        "stderr: {problem}"
    );
    assert!(
        !text.contains("sfdisk"),
        "steps that will not run were listed: {text}"
    );
}

#[test]
fn a_rescue_session_will_not_take_the_disk_name() {
    // And the second place: the screen where the name is typed, which is the
    // last one before the disk is gone.
    let machine = FakeMachine::new("rescuetui").disk("vda", 64).rescue();
    let mut session = Session::new(&machine, &["--dry-run"]);
    assert!(session.wait_for("Enter to begin"), "{}", session.screen());
    session.type_keys(b"\r");
    assert!(session.wait_for("GiB"), "{}", session.screen());
    session.type_keys(b"\r");
    assert!(
        session.wait_for("Tab switches fields"),
        "{}",
        session.screen()
    );
    session.type_keys(b"\r");

    assert!(
        session.wait_for("cannot install a bootloader"),
        "{}",
        session.screen()
    );
    let screen = session.screen();
    assert!(
        !screen.contains("Type vda to confirm"),
        "there is nothing to confirm:\n{screen}"
    );

    // Typing the name anyway starts nothing.
    session.type_keys(b"vda\r");
    std::thread::sleep(Duration::from_millis(300));
    session.wait_for("nothing");
    assert!(
        !session.screen().contains("Partition the disk"),
        "{}",
        session.screen()
    );
}

#[test]
fn the_motd_names_the_install_command() {
    // This is what the live image prints, and the only place a person is told
    // how to install.
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_tos-install"))
        .arg("--motd")
        .output()
        .expect("run tos-install --motd");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("tos-install"));
    assert!(text.contains("the terminal is the desktop"));
    assert!(text.contains("nothing is written to disk"));
}

#[test]
fn listing_disks_marks_the_ones_that_cannot_be_used() {
    let machine = FakeMachine::new("list").disk("vda", 64).disk("vdb", 1);
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_tos-install"))
        .arg("--list")
        .env("TOS_INSTALL_SYSROOT", machine.path())
        .output()
        .expect("run tos-install --list");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("/dev/vda"));
    assert!(
        text.contains("unusable: too small"),
        "a 1 GiB disk cannot hold a system: {text}"
    );
}

#[test]
fn installing_without_root_refuses_rather_than_half_trying() {
    let machine = FakeMachine::new("asroot").disk("vda", 64);
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_tos-install"))
        .env("TOS_INSTALL_SYSROOT", machine.path())
        .output()
        .expect("run tos-install");
    if unsafe { libc_geteuid() } == 0 {
        return; // running as root, which the guard is not about
    }
    assert!(!output.status.success());
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(text.contains("needs root"), "{text}");
    assert!(
        text.contains("--dry-run"),
        "it should say what to try instead"
    );
}

extern "C" {
    #[link_name = "geteuid"]
    fn libc_geteuid() -> u32;
}

#[test]
fn an_unknown_option_is_an_error() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_tos-install"))
        .arg("--format-everything")
        .output()
        .expect("run tos-install");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown option"));
}
