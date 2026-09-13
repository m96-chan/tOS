//! Running the commands an installation is made of.
//!
//! Every destructive action goes through [`Backend`]. That seam is what lets
//! the whole installation be driven in a test without a disk in sight, and it
//! is also what `--dry-run` is: a backend that writes down what it was asked
//! to do instead of doing it.

use std::io;
use std::process::Command;

/// The result of running one command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn is_success(&self) -> bool {
        self.status == 0
    }
}

/// Somewhere commands can be run and files written.
pub trait Backend {
    /// Run a program with arguments, waiting for it to finish.
    fn run(&mut self, program: &str, args: &[&str]) -> io::Result<Output>;

    /// Run a program, feeding it `input` on standard input.
    fn run_with_input(&mut self, program: &str, args: &[&str], input: &str) -> io::Result<Output>;

    /// Write a file, creating parent directories, with whatever permissions
    /// the umask gives it. That is right for everything in `/etc` that is
    /// meant to be read.
    fn write_file(&mut self, path: &str, contents: &str) -> io::Result<()> {
        self.write_file_with_mode(path, contents, None)
    }

    /// Write a file, creating parent directories, and give it `mode` if one
    /// is asked for.
    ///
    /// The mode is part of creating the file rather than something done to it
    /// afterwards: a credential that exists for even a moment as a readable
    /// file has been readable, and a `chmod` that fails leaves it that way.
    fn write_file_with_mode(
        &mut self,
        path: &str,
        contents: &str,
        mode: Option<u32>,
    ) -> io::Result<()>;

    /// Add to the end of a file, creating it if it is not there.
    ///
    /// Wanted because a Debian root already has an `/etc/passwd`, and it is
    /// full of the system accounts Debian's own packages run as — `_apt` is
    /// the one apt drops to before it touches the network, and a machine
    /// without it cannot download anything. Replacing that file with the two
    /// lines tOS cares about, which is what the installer did when the disk
    /// held nothing but busybox, would take apt apart on the way in.
    fn append_file(&mut self, path: &str, contents: &str) -> io::Result<()>;

    /// Copy a directory tree.
    fn copy_tree(&mut self, from: &str, to: &str) -> io::Result<()>;

    /// Create a directory and any missing parents.
    fn create_dir(&mut self, path: &str) -> io::Result<()>;

    /// Whether a path exists. Used to decide between UEFI and BIOS.
    fn exists(&self, path: &str) -> bool;

    /// Restart the machine now, and do not come back.
    ///
    /// There is no success to return, which is why this hands back the error
    /// itself rather than a [`Result`]: a restart that happens never returns
    /// here, so anything a caller receives is a machine that is still
    /// running, and something the caller has to say out loud.
    ///
    /// The signature is as much of the fix as the implementation is. What was
    /// here before ran `reboot` off the `PATH` and dropped the answer with
    /// `let _ =`. On the live image that program was busybox, and busybox
    /// without `-f` restarts nothing itself: it signals PID 1 and leaves the
    /// rest to init. PID 1 in a live session was `/bin/sh /sbin/tos-session`,
    /// a `while :` loop with no traps, so the signal went nowhere, busybox
    /// exited 0, and the installer had a success to discard. There was no
    /// failure to notice, which is why the key had never worked on any
    /// machine. Now there is no success to drop.
    ///
    /// PID 1 does listen now — it is systemd on both images since #110 — and
    /// this still goes to the kernel. Not because nobody would answer, but
    /// because there is nothing for a clean shutdown to do here: the session
    /// this runs in is a tmpfs over a read-only squashfs, and the disk that
    /// matters was unmounted by the installation's last step.
    fn reboot(&mut self) -> io::Error;
}

/// A backend that really does it.
pub struct System;

impl Backend for System {
    fn run(&mut self, program: &str, args: &[&str]) -> io::Result<Output> {
        let output = Command::new(program).args(args).output()?;
        Ok(Output {
            status: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    fn run_with_input(&mut self, program: &str, args: &[&str], input: &str) -> io::Result<Output> {
        use std::io::Write;
        use std::process::Stdio;

        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        // The pipe has to be closed before waiting, or a program that reads to
        // end of file never finishes.
        {
            let stdin = child.stdin.as_mut().ok_or_else(|| {
                io::Error::new(io::ErrorKind::BrokenPipe, "no stdin for the child")
            })?;
            stdin.write_all(input.as_bytes())?;
        }
        drop(child.stdin.take());
        let output = child.wait_with_output()?;
        Ok(Output {
            status: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    fn write_file_with_mode(
        &mut self,
        path: &str,
        contents: &str,
        mode: Option<u32>,
    ) -> io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        if let Some(parent) = std::path::Path::new(path).parent() {
            std::fs::create_dir_all(parent)?;
        }
        let Some(mode) = mode else {
            return std::fs::write(path, contents);
        };
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(path)?;
        // `mode` on the open only applies to a file that did not exist, and
        // an install that is being repeated onto the same mount would find
        // one that does. Say it again so the mode is the file's either way.
        std::fs::set_permissions(path, PermissionsExt::from_mode(mode))?;
        file.write_all(contents.as_bytes())
    }

    fn append_file(&mut self, path: &str, contents: &str) -> io::Result<()> {
        use std::io::Write;

        if let Some(parent) = std::path::Path::new(path).parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        file.write_all(contents.as_bytes())
    }

    fn copy_tree(&mut self, from: &str, to: &str) -> io::Result<()> {
        copy_tree(std::path::Path::new(from), std::path::Path::new(to))
    }

    fn create_dir(&mut self, path: &str) -> io::Result<()> {
        std::fs::create_dir_all(path)
    }

    fn exists(&self, path: &str) -> bool {
        std::path::Path::new(path).exists()
    }

    fn reboot(&mut self) -> io::Error {
        // SAFETY: `reboot` is handed one immediate by value — no pointer, no
        // buffer, nothing of ours that has to outlive the call — and it is
        // unsafe only because it is an `extern "C"` call. `RB_AUTOBOOT` is the
        // restart the kernel performs itself, so it cannot be swallowed by a
        // PID 1 that is not listening for signals, and it does not depend on
        // which `reboot` happens to come first on the `PATH`.
        //
        // The abruptness costs nothing here: the caller syncs first, the
        // installation's last step has already flushed and unmounted the
        // target, and the root this runs from is a tmpfs overlay over a
        // read-only squashfs with nothing to write back.
        let result = unsafe { libc::reboot(libc::RB_AUTOBOOT) };

        // `reboot(2)` returns only when it failed, and then -1. Anything else
        // is the kernel coming back from a restart it did not perform, and
        // `errno` would hold whatever the last unrelated call left there —
        // rendered, most likely, as "Success". That is the exact shape of the
        // bug this replaces, so say what happened rather than ask `errno`.
        if result != -1 {
            return io::Error::other(format!(
                "reboot(2) returned {result} and the machine is still running"
            ));
        }
        io::Error::last_os_error()
    }
}

/// Recursive copy that preserves the executable bit, which matters for every
/// binary the live system is made of.
fn copy_tree(from: &std::path::Path, to: &std::path::Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    if from.is_symlink() {
        let target = std::fs::read_link(from)?;
        if to.exists() {
            std::fs::remove_file(to)?;
        }
        return std::os::unix::fs::symlink(target, to);
    }
    if from.is_file() {
        std::fs::copy(from, to)?;
        let permissions = std::fs::metadata(from)?.permissions();
        std::fs::set_permissions(to, PermissionsExt::from_mode(permissions.mode()))?;
        return Ok(());
    }
    if !from.is_dir() {
        // Device nodes, sockets and fifos are recreated by the running system,
        // not copied out of the live one.
        return Ok(());
    }

    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        copy_tree(&entry.path(), &to.join(entry.file_name()))?;
    }
    Ok(())
}

/// Something a backend was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Run {
        program: String,
        args: Vec<String>,
        input: Option<String>,
    },
    WriteFile {
        path: String,
        contents: String,
        /// The permissions the file was asked for, if any were.
        mode: Option<u32>,
    },
    AppendFile {
        path: String,
        contents: String,
    },
    CopyTree {
        from: String,
        to: String,
    },
    CreateDir {
        path: String,
    },
    /// The machine was asked to restart. Only a fake backend ever records
    /// this, because the real one does not come back to be recorded.
    Reboot,
}

impl Action {
    /// A shell-like rendering, for the dry run listing and the log.
    pub fn describe(&self) -> String {
        match self {
            Action::Run {
                program,
                args,
                input,
            } => {
                let mut text = program.clone();
                for arg in args {
                    text.push(' ');
                    if arg.contains(' ') {
                        text.push_str(&format!("'{arg}'"));
                    } else {
                        text.push_str(arg);
                    }
                }
                if input.is_some() {
                    text.push_str(" < (script)");
                }
                text
            }
            // The mode is in the description because a dry run is how someone
            // checks that the credential is not about to be world readable.
            Action::WriteFile {
                path,
                mode: Some(mode),
                ..
            } => format!("write {path} (mode {mode:04o})"),
            Action::WriteFile { path, .. } => format!("write {path}"),
            Action::AppendFile { path, .. } => format!("append to {path}"),
            Action::CopyTree { from, to } => format!("copy {from} -> {to}"),
            Action::CreateDir { path } => format!("mkdir -p {path}"),
            // Named for the syscall and not for the program, so that a
            // transcript reading plain `reboot` is visibly the old thing:
            // spawning whatever is on the `PATH` and hoping PID 1 agrees.
            Action::Reboot => "reboot(2)".to_string(),
        }
    }
}

/// Records what it was asked to do and reports success.
///
/// This is `--dry-run`, and it is also what the tests assert against.
#[derive(Default)]
pub struct Recorder {
    pub actions: Vec<Action>,
    /// Paths [`Backend::exists`] should answer yes for.
    pub existing: Vec<String>,
    /// Canned output for programs, matched on the program name.
    pub responses: Vec<(String, Output)>,
}

impl Recorder {
    pub fn new() -> Recorder {
        Recorder::default()
    }

    /// Pretend this path exists, for example `/sys/firmware/efi`.
    pub fn with_existing(mut self, path: &str) -> Recorder {
        self.existing.push(path.to_string());
        self
    }

    /// Make a program fail, so error handling can be exercised.
    pub fn failing(mut self, program: &str, stderr: &str) -> Recorder {
        self.responses.push((
            program.to_string(),
            Output {
                status: 1,
                stdout: String::new(),
                stderr: stderr.to_string(),
            },
        ));
        self
    }

    /// Everything it was asked to do, as text.
    pub fn transcript(&self) -> Vec<String> {
        self.actions.iter().map(Action::describe).collect()
    }

    /// Whether any recorded action's description contains `needle`.
    pub fn did(&self, needle: &str) -> bool {
        self.transcript().iter().any(|line| line.contains(needle))
    }

    /// The position of the first action whose description contains `needle`.
    pub fn position_of(&self, needle: &str) -> Option<usize> {
        self.transcript()
            .iter()
            .position(|line| line.contains(needle))
    }

    fn response(&self, program: &str) -> Output {
        self.responses
            .iter()
            .find(|(name, _)| name == program)
            .map(|(_, output)| output.clone())
            .unwrap_or(Output {
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
    }
}

impl Backend for Recorder {
    fn run(&mut self, program: &str, args: &[&str]) -> io::Result<Output> {
        self.actions.push(Action::Run {
            program: program.to_string(),
            args: args.iter().map(|a| a.to_string()).collect(),
            input: None,
        });
        Ok(self.response(program))
    }

    fn run_with_input(&mut self, program: &str, args: &[&str], input: &str) -> io::Result<Output> {
        self.actions.push(Action::Run {
            program: program.to_string(),
            args: args.iter().map(|a| a.to_string()).collect(),
            input: Some(input.to_string()),
        });
        Ok(self.response(program))
    }

    fn write_file_with_mode(
        &mut self,
        path: &str,
        contents: &str,
        mode: Option<u32>,
    ) -> io::Result<()> {
        self.actions.push(Action::WriteFile {
            path: path.to_string(),
            contents: contents.to_string(),
            mode,
        });
        Ok(())
    }

    fn append_file(&mut self, path: &str, contents: &str) -> io::Result<()> {
        self.actions.push(Action::AppendFile {
            path: path.to_string(),
            contents: contents.to_string(),
        });
        Ok(())
    }

    fn copy_tree(&mut self, from: &str, to: &str) -> io::Result<()> {
        self.actions.push(Action::CopyTree {
            from: from.to_string(),
            to: to.to_string(),
        });
        Ok(())
    }

    fn create_dir(&mut self, path: &str) -> io::Result<()> {
        self.actions.push(Action::CreateDir {
            path: path.to_string(),
        });
        Ok(())
    }

    fn exists(&self, path: &str) -> bool {
        self.existing.iter().any(|p| p == path)
    }

    fn reboot(&mut self) -> io::Error {
        self.actions.push(Action::Reboot);
        // A recorder has no machine to take down, and the trait gives it no
        // way to pretend otherwise — which is the point of the signature. The
        // tests read the action; this error is what a caller would print if a
        // recorder ever reached a real user, and it would be true.
        io::Error::other("recorded the reboot rather than performing it")
    }
}

/// `--dry-run`: a recorder that also reports what it would have done.
pub type DryRun = Recorder;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recorder_writes_down_what_it_was_asked() {
        let mut backend = Recorder::new();
        backend.run("sfdisk", &["/dev/sda"]).unwrap();
        backend.write_file("/mnt/etc/hostname", "tos\n").unwrap();
        assert_eq!(
            backend.transcript(),
            vec!["sfdisk /dev/sda", "write /mnt/etc/hostname"]
        );
    }

    #[test]
    fn a_recorder_reports_success_by_default() {
        let mut backend = Recorder::new();
        assert!(backend.run("true", &[]).unwrap().is_success());
    }

    #[test]
    fn a_recorder_can_be_told_to_fail() {
        let mut backend = Recorder::new().failing("mkfs.ext4", "no such device");
        let output = backend.run("mkfs.ext4", &["/dev/sda2"]).unwrap();
        assert!(!output.is_success());
        assert_eq!(output.stderr, "no such device");
        // Other programs still succeed.
        assert!(backend.run("sync", &[]).unwrap().is_success());
    }

    #[test]
    fn input_is_recorded_with_the_command() {
        let mut backend = Recorder::new();
        backend
            .run_with_input("sfdisk", &["/dev/sda"], "label: gpt\n")
            .unwrap();
        match &backend.actions[0] {
            Action::Run { input, .. } => assert_eq!(input.as_deref(), Some("label: gpt\n")),
            other => panic!("expected a run, got {other:?}"),
        }
        assert!(backend.did("< (script)"));
    }

    #[test]
    fn descriptions_quote_arguments_with_spaces() {
        let action = Action::Run {
            program: "grub-install".into(),
            args: vec!["--bootloader-id".into(), "tOS on disk".into()],
            input: None,
        };
        assert_eq!(
            action.describe(),
            "grub-install --bootloader-id 'tOS on disk'"
        );
    }

    #[test]
    fn existence_is_answered_from_the_table() {
        let backend = Recorder::new().with_existing("/sys/firmware/efi");
        assert!(backend.exists("/sys/firmware/efi"));
        assert!(!backend.exists("/sys/firmware/nothing"));
    }

    #[test]
    fn a_recorder_records_the_reboot_it_cannot_perform() {
        let mut backend = Recorder::new();
        let failure = backend.reboot();
        assert_eq!(backend.transcript(), vec!["reboot(2)"]);
        // The description says which reboot, because the bug being fixed was
        // the other one: a child process that answers 0 whether or not the
        // machine is going anywhere.
        assert!(!backend.did("reboot "), "{:?}", backend.transcript());
        assert!(failure.to_string().contains("recorded"), "{failure}");
    }

    // There is deliberately no test of `System::reboot`. It is the one method
    // on the trait whose success cannot be observed and whose failure depends
    // on who is running the suite: as an ordinary user it returns EPERM, and
    // as root — which is how the ISO image is built — it would take the
    // machine down in the middle of `cargo test`. The seam exists so that
    // everything above it can be tested without ever reaching this call.

    #[test]
    fn the_real_backend_runs_a_program() {
        let mut backend = System;
        let output = backend.run("/bin/echo", &["hello"]).unwrap();
        assert!(output.is_success());
        assert_eq!(output.stdout.trim(), "hello");
    }

    #[test]
    fn the_real_backend_reports_failure_rather_than_erroring() {
        let mut backend = System;
        let output = backend.run("/bin/sh", &["-c", "exit 3"]).unwrap();
        assert_eq!(output.status, 3);
    }

    #[test]
    fn the_real_backend_feeds_standard_input() {
        let mut backend = System;
        let output = backend
            .run_with_input("/bin/cat", &[], "partition table\n")
            .unwrap();
        assert_eq!(output.stdout, "partition table\n");
    }

    #[test]
    fn the_real_backend_writes_files_and_makes_parents() {
        let mut backend = System;
        let dir = std::env::temp_dir().join("tos-install-exec-test");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("etc").join("hostname");
        backend.write_file(path.to_str().unwrap(), "tos\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "tos\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_recorder_keeps_the_mode_that_was_asked_for() {
        let mut backend = Recorder::new();
        backend
            .write_file("/mnt/etc/passwd", "root:*:0:0\n")
            .unwrap();
        backend
            .write_file_with_mode("/mnt/etc/shadow", "root:*:::::::\n", Some(0o640))
            .unwrap();
        assert_eq!(
            backend.transcript(),
            vec!["write /mnt/etc/passwd", "write /mnt/etc/shadow (mode 0640)"]
        );
    }

    #[test]
    fn the_real_backend_writes_a_file_only_root_can_read() {
        use std::os::unix::fs::PermissionsExt;
        let mut backend = System;
        let dir = std::env::temp_dir().join("tos-install-mode-test");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("etc").join("shadow");
        let name = path.to_str().unwrap().to_string();

        backend
            .write_file_with_mode(&name, "$6$salt$hash\n", Some(0o600))
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "$6$salt$hash\n");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        // A second install onto the same mount finds the file already there,
        // and it must not keep whatever mode it had.
        std::fs::set_permissions(&path, PermissionsExt::from_mode(0o644)).unwrap();
        backend
            .write_file_with_mode(&name, "$6$other$hash\n", Some(0o600))
            .unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "$6$other$hash\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_real_backend_copies_a_tree_keeping_the_executable_bit() {
        use std::os::unix::fs::PermissionsExt;
        let mut backend = System;
        let root = std::env::temp_dir().join("tos-install-copy-test");
        let _ = std::fs::remove_dir_all(&root);
        let from = root.join("from");
        std::fs::create_dir_all(from.join("bin")).unwrap();
        std::fs::write(from.join("bin").join("tos"), "binary").unwrap();
        std::fs::set_permissions(
            from.join("bin").join("tos"),
            PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        std::os::unix::fs::symlink("tos", from.join("bin").join("link")).unwrap();

        let to = root.join("to");
        backend
            .copy_tree(from.to_str().unwrap(), to.to_str().unwrap())
            .unwrap();

        let copied = to.join("bin").join("tos");
        assert_eq!(std::fs::read_to_string(&copied).unwrap(), "binary");
        assert_eq!(
            std::fs::metadata(&copied).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert!(to.join("bin").join("link").is_symlink());
        let _ = std::fs::remove_dir_all(&root);
    }
}
