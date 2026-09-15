//! `tos-install`: put tOS on a disk, from a pane in the live session.

use std::io::{self, Read, Write};
use std::os::unix::io::AsRawFd;
use std::process::ExitCode;

use tos_input::host::HostInput;
use tos_input::InputEvent;
use tos_install::app::{App, Command, Stage};
use tos_install::disk::{self, SysfsSource};
use tos_install::exec::{Backend, System};
use tos_install::motd;
use tos_install::plan::{Bootloader, Firmware};
use tos_install::ui::{Screen, Terminal};

/// How long to wait for the rest of an escape sequence before deciding the
/// user pressed the escape key.
const ESCAPE_WAIT_MS: i32 = 60;

const USAGE: &str = "\
tos-install, the tOS installer

usage: tos-install [options]

options:
  --dry-run      show what would be done, write nothing
  --plan         print the plan for the first usable disk and exit
  --motd         print the live session message and exit
  --list         list the disks tOS can see and exit
  -h, --help     show this message
  -V, --version  show the version

The installer erases the disk it is pointed at. It asks for the disk's own
name before it does, and it refuses the medium the live session booted from.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| args.iter().any(|a| a == flag);

    if has("-h") || has("--help") {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    if has("-V") || has("--version") {
        println!("tos-install {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    if has("--motd") {
        // Used by the live image's shell profile. The terminal is asked what
        // it is before anything is written to it: a pane gets the picture and
        // everything else gets the banner drawn in cells (#132).
        let screen = motd::Screen::probe(std::io::stdout().as_raw_fd());
        print!("{}", motd::greeting(screen));
        return ExitCode::SUCCESS;
    }

    let unknown: Vec<&String> = args
        .iter()
        .filter(|a| {
            !matches!(
                a.as_str(),
                "--dry-run" | "--plan" | "--motd" | "--list" | "-h" | "--help" | "-V" | "--version"
            )
        })
        .collect();
    if let Some(argument) = unknown.first() {
        eprintln!("tos-install: unknown option: {argument}");
        eprintln!("try 'tos-install --help'");
        return ExitCode::from(2);
    }

    let disks = disk::enumerate(&SysfsSource::new());
    if has("--list") {
        return list_disks(&disks);
    }
    if has("--plan") {
        return print_plan(&disks);
    }

    let dry_run = has("--dry-run");
    if !dry_run && !is_root() {
        eprintln!("tos-install: installing needs root; try --dry-run to see the plan");
        return ExitCode::FAILURE;
    }

    match run(disks, dry_run) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("tos-install: {e}");
            ExitCode::FAILURE
        }
    }
}

fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

fn list_disks(disks: &[tos_install::Disk]) -> ExitCode {
    if disks.is_empty() {
        println!("No disks found.");
        return ExitCode::SUCCESS;
    }
    for disk in disks {
        match disk.refusal() {
            Some(reason) => println!("{}   unusable: {reason}", disk.summary()),
            None => println!("{}", disk.summary()),
        }
    }
    ExitCode::SUCCESS
}

fn print_plan(disks: &[tos_install::Disk]) -> ExitCode {
    let Some(disk) = disks.iter().find(|d| d.is_installable()) else {
        eprintln!("tos-install: no disk on this machine can be installed onto");
        return ExitCode::FAILURE;
    };
    let firmware = Firmware::detect(&System);
    let plan = tos_install::Plan::new(
        disk.clone(),
        firmware,
        Bootloader::detect(&System),
        tos_install::plan::Settings::default(),
    );
    for line in plan.summary() {
        println!("{line}");
    }
    println!();

    // A session that cannot finish the job prints why instead of the steps.
    // Printing them as well would be the lie this is here to stop: eight
    // things that are not going to happen, the first of which erases a disk.
    if let Some(refusal) = plan.refusal() {
        for line in refusal {
            eprintln!("{line}");
        }
        return ExitCode::FAILURE;
    }

    println!("Steps:");

    // Walk the plan against a recorder so the listing is what would really
    // run, rather than a description of it.
    let mut recorder = tos_install::install::planning_backend();
    let mut installer = tos_install::Installer::new(plan, &mut recorder);
    installer.run();
    for action in &recorder.actions {
        println!("  {}", action.describe());
    }
    ExitCode::SUCCESS
}

fn run(disks: Vec<tos_install::Disk>, dry_run: bool) -> io::Result<ExitCode> {
    let firmware = Firmware::detect(&System);
    let mut app = App::new(disks, firmware, Bootloader::detect(&System), dry_run);

    let mut terminal = Terminal::acquire()?;
    let (cols, rows) = terminal.size();
    let mut screen = Screen::new(cols, rows);
    let mut decoder = HostInput::new();
    let mut buf = [0u8; 4096];
    let mut reboot = false;
    let stdin_fd = std::os::unix::io::AsRawFd::as_raw_fd(&io::stdin());

    app.draw(&mut screen);
    terminal.write(&screen.render())?;

    while !app.should_quit() {
        // Re-reading the size every frame is what makes the installer follow
        // its pane being resized.
        let (cols, rows) = terminal.size();
        screen.resize(cols, rows);

        // Waiting rather than blocking is what makes the escape key work: a
        // bare escape only becomes a keypress once nothing follows it.
        let ready = tos_platform::tty::poll_readable(&[stdin_fd], ESCAPE_WAIT_MS)?;
        let events = if ready.is_empty() {
            decoder.flush()
        } else {
            let read = io::stdin().read(&mut buf)?;
            if read == 0 {
                break;
            }
            decoder.feed(&buf[..read])
        };
        if events.is_empty() {
            continue;
        }

        let mut install_now = false;
        for event in events {
            let InputEvent::Key(key) = event else {
                continue;
            };
            match app.key(&key) {
                Command::Install => install_now = true,
                Command::Reboot => {
                    reboot = true;
                    break;
                }
                Command::Quit => break,
                Command::None => {}
            }
        }

        if install_now {
            // Show the progress screen before the first step starts, so the
            // user is not looking at the confirmation while a disk is wiped.
            app.draw(&mut screen);
            terminal.write(&screen.render())?;

            let mut system = System;
            let backend: &mut dyn Backend = &mut system;
            app.install(backend);
        }

        app.draw(&mut screen);
        terminal.write(&screen.render())?;

        if reboot {
            break;
        }
    }

    terminal.release();
    let message = if app.stage == Stage::Finished {
        Some(app.outcome_message())
    } else {
        None
    };
    if let Some(message) = message {
        println!("{message}");
    }
    io::stdout().flush()?;

    if reboot {
        let mut system = System;
        // Everything below this line is unreachable on a machine that
        // restarts: `App::reboot` only returns the reason it did not. That is
        // the whole of #104 — the offer used to be answered by a `reboot`
        // off the PATH whose exit status meant nothing, so the one outcome
        // the user needed to hear about was the one that could not be seen.
        let failure = app.reboot(&mut system);
        eprintln!("tos-install: the machine did not reboot: {failure}");
        eprintln!("tos-install: tOS is on the disk; restart when you are ready.");
        return Ok(ExitCode::FAILURE);
    }

    Ok(if app.installed || dry_run {
        ExitCode::SUCCESS
    } else if app.stage == Stage::Finished {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}
