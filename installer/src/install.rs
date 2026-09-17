//! Carrying out the plan.
//!
//! Each step is one method, run through a [`Backend`], so the whole sequence
//! can be driven against a recorder in a test. The installer stops at the
//! first failure and reports it: a half-installed disk is easier to reason
//! about than one that carried on after `mkfs` failed.

use crate::exec::{Backend, Output};
use crate::motd;
use crate::plan::{Firmware, Plan, Settings, Step};

/// Where tOS keeps its own files on an installed system.
pub const CREDENTIAL_DIRECTORY: &str = "/etc/tos";

/// Where the machine's password lives: the file every program that
/// authenticates anybody already reads.
///
/// It used to be `/etc/tos/shadow`, a file tOS owned and only the screen lock
/// read, with `*` in `/etc/passwd` to say so. That was honest and it was also
/// a machine no `sshd`, no `su` and no `login` could ever let anybody in to
/// (#111). Writing a PAM module so that the rest of the world could be taught
/// about tOS's own file is work spent arriving where the default already was.
/// `docs/design/credentials.md` works through it.
pub const SHADOW_FILE: &str = "/etc/shadow";

/// The mode `sudo` insists on for a file in `/etc/sudoers.d`. Anything
/// group- or world-writable is skipped, and skipped quietly.
const SUDOERS_MODE: u32 = 0o440;

/// The programs that end this machine, which the drop-in below lets the
/// person run without a password (#158).
///
/// All four are `systemd-sysv` symlinks to `systemctl` on an installed
/// machine, and the shim in `/usr/local/sbin` runs them through `sudo -n` by
/// exactly these paths. `halt` is in the list because `shutdown -H` is it, and
/// leaving it out would be a flag of `shutdown` that asks for a password when
/// the other three do not.
const POWER_PROGRAMS: &str = "/sbin/shutdown, /sbin/poweroff, /sbin/reboot, /sbin/halt";

/// Debian's mode for that file, and what this writes when it is the one
/// creating it: root writes it, the `shadow` group reads it, nobody else sees
/// a hash at all. A hash anyone can read is a hash anyone can attack offline,
/// at their leisure, on a machine they took.
///
/// Only used where tOS writes the file. A Debian root arrives with an
/// `/etc/shadow` of its own and the person's line is appended to it, which
/// leaves the mode and the `root:shadow` ownership dpkg gave it alone.
pub const SHADOW_MODE: u32 = 0o640;

/// What happened to one step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepOutcome {
    Done,
    Failed(String),
}

impl StepOutcome {
    pub fn is_failure(&self) -> bool {
        matches!(self, StepOutcome::Failed(_))
    }
}

/// How far along an installation is.
#[derive(Debug, Clone, Default)]
pub struct Progress {
    /// Steps finished so far, with what happened to each.
    pub finished: Vec<(Step, StepOutcome)>,
    /// The step being run, if any.
    pub current: Option<Step>,
    /// Lines for the log pane.
    pub log: Vec<String>,
}

impl Progress {
    pub fn failure(&self) -> Option<&str> {
        self.finished.iter().find_map(|(_, outcome)| match outcome {
            StepOutcome::Failed(message) => Some(message.as_str()),
            StepOutcome::Done => None,
        })
    }

    pub fn is_complete(&self, plan: &Plan) -> bool {
        self.failure().is_none() && self.finished.len() == plan.steps().len()
    }

    fn note(&mut self, line: impl Into<String>) {
        self.log.push(line.into());
        // The log is only ever shown as the tail of itself.
        if self.log.len() > MAX_LOG_LINES {
            self.log.remove(0);
        }
    }
}

const MAX_LOG_LINES: usize = 500;

/// Runs a [`Plan`] against a [`Backend`].
pub struct Installer<'a> {
    plan: Plan,
    backend: &'a mut dyn Backend,
    pub progress: Progress,
}

impl<'a> Installer<'a> {
    pub fn new(plan: Plan, backend: &'a mut dyn Backend) -> Installer<'a> {
        Installer {
            plan,
            backend,
            progress: Progress::default(),
        }
    }

    pub fn plan(&self) -> &Plan {
        &self.plan
    }

    /// Run everything, stopping at the first failure.
    pub fn run(&mut self) -> &Progress {
        for step in self.plan.steps() {
            if self.progress.failure().is_some() {
                break;
            }
            self.run_step(step);
        }
        &self.progress
    }

    /// Run one step, recording what happened.
    pub fn run_step(&mut self, step: Step) -> StepOutcome {
        self.progress.current = Some(step);
        self.progress.note(format!("── {}", step.label()));

        // Asked of every step rather than once at the top of `run`, because
        // `run_step` is a door of its own: a session that cannot finish the
        // job must not be able to start it through either one. The message is
        // the first line only — the rest of the reason belongs on the screen
        // that could still have stopped this, and this is the backstop behind
        // it rather than the explanation.
        let result = match self.plan.refusal() {
            Some(refusal) => Err(refusal[0].to_string()),
            None => self.dispatch(step),
        };

        let outcome = match result {
            Ok(()) => StepOutcome::Done,
            Err(message) => {
                self.progress.note(format!("!! {message}"));
                StepOutcome::Failed(message)
            }
        };
        self.progress.current = None;
        self.progress.finished.push((step, outcome.clone()));
        outcome
    }

    fn dispatch(&mut self, step: Step) -> Result<(), String> {
        match step {
            Step::Partition => self.partition(),
            Step::FormatEsp => self.format_esp(),
            Step::FormatRoot => self.format_root(),
            Step::Mount => self.mount(),
            Step::CopySystem => self.copy_system(),
            Step::Configure => self.configure(),
            Step::Bootloader => self.bootloader(),
            Step::Finish => self.finish(),
        }
    }

    // ---- the steps ------------------------------------------------------

    fn partition(&mut self) -> Result<(), String> {
        // Anything still mounted from an earlier attempt would keep the kernel
        // from re-reading the table.
        self.unmount_all();

        let script = self.plan.partition_script();
        let disk = self.plan.disk.path.clone();
        self.command_with_input("sfdisk", &["--wipe", "always", "--quiet", &disk], &script)?;
        // The kernel needs telling before the partition nodes appear. Recent
        // sfdisk does this itself; these are for the versions that do not, and
        // a live image carries whichever of them it has.
        let _ = self.backend.run("partprobe", &[&disk]);
        let _ = self.backend.run("partx", &["-u", &disk]);
        let _ = self.backend.run("udevadm", &["settle"]);
        self.wait_for_partition(&self.plan.root_partition())?;
        Ok(())
    }

    fn format_esp(&mut self) -> Result<(), String> {
        let partition = self.plan.boot_partition();
        self.command("mkfs.vfat", &["-F", "32", "-n", "TOS-ESP", &partition])
    }

    fn format_root(&mut self) -> Result<(), String> {
        let partition = self.plan.root_partition();
        self.command("mkfs.ext4", &["-F", "-L", "tos-root", &partition])
    }

    fn mount(&mut self) -> Result<(), String> {
        let mount_point = self.plan.mount_point.clone();
        self.backend
            .create_dir(&mount_point)
            .map_err(|e| format!("cannot create {mount_point}: {e}"))?;
        let root = self.plan.root_partition();
        self.command("mount", &[&root, &mount_point])?;

        if self.plan.firmware == Firmware::Uefi {
            let esp_mount = self.plan.esp_mount();
            self.backend
                .create_dir(&esp_mount)
                .map_err(|e| format!("cannot create {esp_mount}: {e}"))?;
            let esp = self.plan.boot_partition();
            self.command("mount", &[&esp, &esp_mount])?;
        }
        Ok(())
    }

    fn copy_system(&mut self) -> Result<(), String> {
        // Everything the kernel provides is left out of both paths below:
        // those are mount points in the installed system, not files.
        for directory in ["dev", "proc", "sys", "run", "tmp", "mnt", "var/log"] {
            let path = format!("{}/{directory}", self.plan.mount_point);
            self.backend
                .create_dir(&path)
                .map_err(|e| format!("cannot create {path}: {e}"))?;
        }

        // Both halves of the question, because the image being on the medium
        // does not mean this session can open it. The rescue session — the one
        // that runs when the squashfs would not mount and `/init` fell back to
        // the initramfs — still has the medium and so still has the image,
        // while its busybox world has no `unsquashfs` anywhere. Asking only
        // about the file sent exactly that session down the path it cannot
        // finish, and it found out here: after Partition, FormatEsp and
        // FormatRoot had already been run over the disk it was installing to.
        if self.backend.exists(&self.plan.rootfs_image) && self.can_unpack() {
            self.unpack_rootfs()?;
        } else {
            self.copy_live_system()?;
        }

        self.copy_boot_files()
    }

    /// Whether this session can open a squashfs at all.
    ///
    /// Looked for by path rather than run, because the only honest moment to
    /// ask is before anything has been written to the disk, and running it to
    /// find out is a thing that can go wrong on its own.
    fn can_unpack(&self) -> bool {
        UNSQUASHFS
            .iter()
            .any(|program| self.backend.exists(program))
    }

    /// Unpack the Debian rootfs from the medium onto the new root.
    ///
    /// This is what makes an installed tOS machine a machine rather than a
    /// photograph of one. What lands here has a glibc, a dpkg and an apt, so
    /// the first thing its owner wants to add to it is something they can
    /// actually add; before this the disk got a copy of the busybox initramfs
    /// and whatever `iso/mkiso.sh` had not packed was unreachable forever.
    fn unpack_rootfs(&mut self) -> Result<(), String> {
        let image = self.plan.rootfs_image.clone();
        let target = self.plan.mount_point.clone();
        self.progress.note("   unpacking the Debian rootfs");
        // `-f` because the root is already mounted and so already has a
        // lost+found, and under UEFI an empty /boot/efi for the ESP as well;
        // unsquashfs refuses a destination that exists without it. The
        // progress bar is a carriage-return animation, and the installer's
        // log pane is a list of lines.
        self.command("unsquashfs", &["-f", "-no-progress", "-d", &target, &image])
    }

    /// Copy the running system onto the disk, which is what there is to
    /// install when the medium carries no rootfs image.
    ///
    /// An image built before the rootfs step existed, or one whose squashfs
    /// would not mount, leaves the session running out of the initramfs. The
    /// disk then gets that same small world — no dpkg and no apt — which is
    /// worse than Debian and much better than refusing to install at all.
    fn copy_live_system(&mut self) -> Result<(), String> {
        self.progress
            .note("   no rootfs image on the medium: copying the live system");
        for directory in COPIED_DIRECTORIES {
            let from = format!("{}{directory}", self.plan.source_root.trim_end_matches('/'));
            if !self.backend.exists(&from) {
                continue;
            }
            let to = format!("{}{directory}", self.plan.mount_point);
            self.progress.note(format!("   copying {directory}"));
            self.backend
                .copy_tree(&from, &to)
                .map_err(|e| format!("cannot copy {from}: {e}"))?;
        }
        Ok(())
    }

    /// Put the kernel and initramfs where GRUB expects them.
    ///
    /// They cannot come from the running filesystem: the live system *is* an
    /// initramfs, and it does not contain the kernel that unpacked it. They
    /// come from the medium, which `/init` mounts for exactly this reason.
    fn copy_boot_files(&mut self) -> Result<(), String> {
        let boot = format!("{}/boot", self.plan.mount_point);
        self.backend
            .create_dir(&boot)
            .map_err(|e| format!("cannot create {boot}: {e}"))?;

        let source = self.plan.boot_source.trim_end_matches('/').to_string();
        let mut copied = 0;
        for file in BOOT_FILES {
            let from = format!("{source}/{file}");
            if !self.backend.exists(&from) {
                continue;
            }
            self.progress.note(format!("   copying {file}"));
            self.backend
                .copy_tree(&from, &format!("{boot}/{file}"))
                .map_err(|e| format!("cannot copy {from}: {e}"))?;
            copied += 1;
        }

        if copied == 0 {
            // Without these the installed disk would not boot, and finding
            // that out at the GRUB prompt is no way to learn it.
            return Err(format!(
                "no kernel found in {source}: the live medium is not mounted"
            ));
        }
        Ok(())
    }

    fn configure(&mut self) -> Result<(), String> {
        let root = self.plan.mount_point.clone();
        let settings = self.plan.settings.clone();

        self.write(
            &format!("{root}/etc/hostname"),
            &format!("{}\n", settings.hostname),
        )?;
        self.write(
            &format!("{root}/etc/hosts"),
            &format!(
                "127.0.0.1\tlocalhost\n127.0.1.1\t{}\n::1\tlocalhost ip6-localhost\n",
                settings.hostname
            ),
        )?;

        self.accounts(&root, &settings)?;
        let home = format!("{root}/home/{}", settings.username);
        self.backend
            .create_dir(&home)
            .map_err(|e| format!("cannot create {home}: {e}"))?;
        // Furnished, where the shell that reads it is there. An interactive
        // shell that is not a login shell reads ~/.bashrc and nothing else —
        // not /etc/profile, not $ENV — so without this the account lands on a
        // bare `bash-5.2$` with no history, no colour and no message of the
        // day. ~/.profile is the other half of the same account: a login
        // shell is the one kind that does not read ~/.bashrc, so `su - tos`
        // would get none of what the pane it was typed in has. The rootfs
        // ships both files as /root/... and /etc/skel/...; these are the
        // copies for the account that did not exist when the image was built.
        if self.login_shell(&root) == "/bin/bash" {
            self.write(&format!("{home}/.bashrc"), BASHRC)?;
            self.write(&format!("{home}/.profile"), PROFILE)?;
        }
        // And the home has to belong to the person, which nothing so far has
        // made it: the installer runs as root on the live system, so the
        // directory and everything just written into it is root's. A home the
        // account cannot write is a shell that cannot save a line of history,
        // which is most of what the rc file above is there for.
        //
        // Checked, unlike the chmods below it. A chmod that fails leaves a
        // script that still runs; this failing leaves an account that cannot
        // write its own home, and an install that reported success is the
        // only place that would ever have said so.
        self.command(
            "chown",
            &["-R", &format!("{ACCOUNT_ID}:{ACCOUNT_ID}"), &home],
        )?;

        let fstab = self.fstab();
        self.write(&format!("{root}/etc/fstab"), &fstab)?;

        self.session_user(&root, &settings)?;

        // Say that this disk was installed. /etc is copied from the live
        // system, message of the day and all, so without a mark left here
        // every shell on the finished machine would go on announcing a live
        // session and offering to install the disk it is already on.
        let directory = format!("{root}{CREDENTIAL_DIRECTORY}");
        self.backend
            .create_dir(&directory)
            .map_err(|e| format!("cannot create {directory}: {e}"))?;
        self.write(
            &format!("{root}{}", motd::INSTALLED_PATH),
            &format!("tOS {}\n", env!("CARGO_PKG_VERSION")),
        )?;
        Ok(())
    }

    /// Give the installed system the account the installer was told about.
    ///
    /// A Debian root arrives with an `/etc/passwd` of its own, holding the
    /// system accounts Debian's packages run as. `_apt` is one of them, and
    /// apt drops to it before it opens a socket; a machine missing that line
    /// cannot download a package at all. So the person's account is added to
    /// that file rather than written over it. When there is no such file the
    /// disk got the busybox world instead, and the two lines below are the
    /// whole of its user database.
    ///
    /// The password field is `x`, and there is an `/etc/shadow` line behind
    /// it. `x` means "the hash is in /etc/shadow", which used to be a lie —
    /// the credential was `/etc/tos/shadow` and the field said `*` to say so —
    /// and #111 is the decision that made it true instead: the machine's
    /// password lives where every program that authenticates anybody already
    /// looks for it.
    fn accounts(&mut self, root: &str, settings: &Settings) -> Result<(), String> {
        let user = &settings.username;
        let passwd = format!("{root}/etc/passwd");
        let group = format!("{root}/etc/group");
        let shadow = format!("{root}{SHADOW_FILE}");
        let shell = self.login_shell(root);
        let account = format!("{user}:x:{ACCOUNT_ID}:{ACCOUNT_ID}:{user}:/home/{user}:{shell}\n");
        let membership = format!("{user}:x:{ACCOUNT_ID}:\n");
        let secret = self.password_field(settings)?;

        // Each file answers for itself. Deciding both from whether `passwd`
        // exists made the group file's fate depend on the other file's
        // evidence, and `append_file` creates what it cannot open — so a root
        // with a passwd and no group got an `/etc/group` whose only line was
        // the person's, with no `root`, no `tty`, no `disk` and no `_apt`
        // group on the machine at all. Written silently, and reported as a
        // successful install.
        self.add_account(&passwd, &account, "root:x:0:0:root:/root:/bin/sh\n")?;
        self.add_account(&group, &membership, "root:x:0:\n")?;
        self.add_secret(&shadow, &secret, settings)?;
        self.add_sudo(root, settings)
    }

    /// Give the person a way to be root, because after #112 they are not one.
    ///
    /// A session's panes run as the account now, not as the compositor. That
    /// is the point of having logged in, and it takes the machine's only route
    /// to root with it: Debian's root carries `*` and tOS never changes it, so
    /// `su` has nothing to accept, and an installed machine would have no way
    /// to install a package on itself. Unadministrable is not more secure; it
    /// is just finished in the wrong place.
    ///
    /// `sudo` is the route, and it is the one #111 was already aiming at. The
    /// whole reason the password went into `/etc/shadow` rather than a private
    /// file was so that the programs which authenticate people could use it,
    /// and `sudo` reads it through PAM exactly as `sshd` does. Nothing extra
    /// has to be taught anything.
    ///
    /// A drop-in rather than a line appended to the `sudo` group, because the
    /// group needs an existing line edited in place — Debian ships `sudo:x:27:`
    /// — and every other account this installer writes is appended whole.
    /// `/etc/sudoers.d` is the supported way in and needs no surgery.
    ///
    /// **`NOPASSWD` exactly when there is no password**, which is the same rule
    /// the login screen obeys: a machine whose owner declined one is not being
    /// guarded, and `docs/design/login.md` says so. Requiring a password there
    /// would ask for one that cannot exist — `*` authenticates nobody — and
    /// leave the machine locked out of itself while protecting nothing that
    /// anyone at its keyboard does not already have.
    ///
    /// **And `NOPASSWD` for the four programs that turn the machine off**
    /// (#158). `/sbin/shutdown` on this image is `systemd-sysv`'s symlink to
    /// `systemctl`, which has to reach PID 1 — and since #119 a pane runs as
    /// the person, who can reach it by neither route this image leaves open:
    /// there is no D-Bus, so no `logind` and no polkit, and
    /// `/run/systemd/private` is `srwx------ root root`. So `shutdown -h now`
    /// said `Failed to connect to bus`, exited 1, and the machine stayed up.
    /// `/usr/local/sbin/shutdown` — `iso/shutdown`, first on the session's
    /// PATH — is what hands the real program to `sudo -n`, and this is the
    /// line that lets it through.
    ///
    /// It gives away nothing that was being kept. The person standing at this
    /// machine can already power it off without a password from the
    /// compositor's power menu, and by holding the power button in; a
    /// password on `shutdown` alone guarded a door with no wall beside it,
    /// while making the one command everybody knows the only way that did not
    /// work.
    fn add_sudo(&mut self, root: &str, settings: &Settings) -> Result<(), String> {
        let user = &settings.username;
        let rule = if settings.password.is_empty() {
            format!("{user} ALL=(ALL:ALL) NOPASSWD: ALL\n")
        } else {
            // Named by the path `sudo` is handed, which is the path the shim
            // hands it: /sbin, not /usr/sbin, though usr-merge makes them the
            // same file. A second line rather than one with two tags on it,
            // because a `NOPASSWD:` and a `PASSWD:` in one rule is a sudoers
            // line nobody reads correctly twice; the later line wins for the
            // commands it names and the earlier one still covers everything
            // else, with a password.
            format!(
                "{user} ALL=(ALL:ALL) ALL\n\
                 # Turning the machine off, without a password: the power menu\n\
                 # and the power button already do that much. See #158.\n\
                 {user} ALL=(ALL:ALL) NOPASSWD: {POWER_PROGRAMS}\n"
            )
        };
        let path = format!("{root}/etc/sudoers.d/{user}");
        self.progress
            .note(format!("   write {path} (mode {SUDOERS_MODE:04o})"));
        // sudo refuses to read a drop-in that anybody but root can write, and
        // says so by ignoring it: a 0644 file here would be a machine that
        // silently still has no way to root.
        self.backend
            .write_file_with_mode(&path, &rule, Some(SUDOERS_MODE))
            .map_err(|e| format!("cannot write {path}: {e}"))
    }

    /// The password field of the person's `/etc/shadow` line: their hash, or
    /// `*` where they declined a password.
    ///
    /// `*` is not a hash of anything and no password produces it, so an
    /// account that carries it is one nothing can authenticate — which is
    /// exactly what "no password" means, and what every other Debian account
    /// in that file already says. Hashing an empty password instead would
    /// give the machine a lock that opens for a bare Enter, and a login screen
    /// that does the same.
    ///
    /// This is the installer's last chance to hold the password; after it,
    /// only the hash exists anywhere on the disk.
    fn password_field(&mut self, settings: &Settings) -> Result<String, String> {
        if settings.password.is_empty() {
            self.progress
                .note("   no password given: nothing will log in, and the screen lock stays off");
            return Ok("*".into());
        }
        tos_crypt::hash_password(settings.password.as_bytes())
            .map_err(|e| format!("cannot hash the password: {e}"))
    }

    /// Add the person's line to `/etc/shadow`, or write the file if the root
    /// on the disk has none.
    ///
    /// The aging fields are left empty rather than filled in. tOS runs no time
    /// synchronisation of any kind, so the day number this would write is
    /// whatever the RTC claims — and a machine that came up in 1970 would
    /// write `0`, which every login program reads as "this password must be
    /// changed before you may come in", on a machine with no `passwd` command
    /// to change it with. Empty means the aging features are off, which is the
    /// truth about a machine that has no clock to age anything against.
    fn add_secret(&mut self, path: &str, secret: &str, settings: &Settings) -> Result<(), String> {
        let line = format!("{}:{secret}:::::::\n", settings.username);
        if self.backend.exists(path) {
            return self.append(path, &line);
        }
        // Nothing but tOS has written a root here, so root's own line comes
        // with it: `*`, because the installer sets no root password and an
        // account with no line at all is one some login programs let in
        // without asking anything.
        self.progress
            .note(format!("   write {path} (mode {SHADOW_MODE:04o})"));
        self.backend
            .write_file_with_mode(path, &format!("root:*:::::::\n{line}"), Some(SHADOW_MODE))
            .map_err(|e| format!("cannot write {path}: {e}"))
    }

    /// The shell to name in `/etc/passwd`, which is whichever one is there.
    ///
    /// The Debian rootfs has bash and that is the shell a person expects — ash
    /// has no programmable completion, a weaker line editor, no arrays and no
    /// `[[`, and somebody who brings their dotfiles with them brings bash
    /// ones. But the fallback path puts the busybox world on the disk, which
    /// has no bash at all, and a passwd naming one there is a login that fails
    /// rather than a shell that is merely spartan. So it is asked, not assumed
    /// — the same question `iso/live-session` asks on the live side.
    fn login_shell(&self, root: &str) -> &'static str {
        if self.backend.exists(&format!("{root}/bin/bash")) {
            "/bin/bash"
        } else {
            "/bin/sh"
        }
    }

    /// Add one line to an account file, or write the file if it is not there.
    ///
    /// `root_line` is what the file needs before the person's line when tOS is
    /// the one creating it — the busybox world, where these two lines are the
    /// whole user database. Where Debian unpacked its own, `root` is Debian's
    /// to write and this only appends.
    fn add_account(&mut self, path: &str, line: &str, root_line: &str) -> Result<(), String> {
        if self.backend.exists(path) {
            return self.append(path, line);
        }
        self.write(path, &format!("{root_line}{line}"))
    }

    /// Say whose session this machine's is.
    ///
    /// Everything else about the session is already on the disk: the rootfs
    /// carries `/sbin/tos-session` and the `tos-session.service` that runs it,
    /// so an installed machine starts the compositor because the image it came
    /// from does (#110). The one thing the image cannot know is who is at the
    /// keyboard, and `TOS_USER` is what the screen lock asks `/etc/shadow`
    /// about (#111).
    ///
    /// A drop-in rather than an edit of the unit: the unit is the image's and
    /// this line is the machine's, and a unit rewritten here is one that stops
    /// improving the day a new image is installed over it. The three files
    /// this replaced — `/etc/inittab`, `/etc/rc` and `/etc/tos-session` — were
    /// the whole of what an installed machine ran, and they are gone rather
    /// than carried alongside an init that does all three jobs.
    fn session_user(&mut self, root: &str, settings: &Settings) -> Result<(), String> {
        let directory = format!("{root}{SESSION_DROPIN_DIRECTORY}");
        self.backend
            .create_dir(&directory)
            .map_err(|e| format!("cannot create {directory}: {e}"))?;
        self.write(
            &format!("{root}{SESSION_DROPIN}"),
            &format!(
                "# Written by the tOS installer: whose session this machine's is.\n\
                 [Service]\n\
                 Environment=TOS_USER={}\n",
                settings.username
            ),
        )
    }

    /// The installed system's `/etc/fstab`, by label so that the disk can move.
    fn fstab(&self) -> String {
        let mut fstab = String::from(
            "# Written by the tOS installer.\n\
             LABEL=tos-root  /          ext4  defaults,relatime  0 1\n",
        );
        if self.plan.firmware == Firmware::Uefi {
            fstab.push_str("LABEL=TOS-ESP   /boot/efi  vfat  umask=0077         0 2\n");
        }
        // `proc`, `sysfs` and `devpts` used to be here, because /etc/rc
        // mounted them by hand and a line in this file was the only place
        // that said so. systemd mounts all three before anything else runs
        // (#110), so what they are now is three mount units generated to
        // cover mountpoints that are already covered — which systemd carries
        // out anyway, stacking a second mount over each and saying so on the
        // console. A filesystem mounted twice is not a worse machine; a file
        // that looks like it is arranging the boot and is not is a worse file.
        fstab
    }

    fn bootloader(&mut self) -> Result<(), String> {
        let root = self.plan.mount_point.clone();
        let boot_dir = format!("{root}/boot");
        self.backend
            .create_dir(&format!("{boot_dir}/grub"))
            .map_err(|e| format!("cannot create {boot_dir}/grub: {e}"))?;

        let disk = self.plan.disk.path.clone();
        match self.plan.firmware {
            Firmware::Uefi => {
                let esp = self.plan.esp_mount();
                self.command(
                    "grub-install",
                    &[
                        "--target=x86_64-efi",
                        &format!("--efi-directory={esp}"),
                        &format!("--boot-directory={boot_dir}"),
                        "--bootloader-id=tOS",
                        // The live image has no NVRAM access worth relying on,
                        // and the removable path boots on every firmware.
                        "--removable",
                        "--recheck",
                    ],
                )?;
            }
            Firmware::Bios => {
                self.command(
                    "grub-install",
                    &[
                        "--target=i386-pc",
                        &format!("--boot-directory={boot_dir}"),
                        "--recheck",
                        &disk,
                    ],
                )?;
            }
        }

        let config = self.grub_config();
        self.write(&format!("{boot_dir}/grub/grub.cfg"), &config)?;
        Ok(())
    }

    /// GRUB's configuration for the installed system.
    ///
    /// Written directly rather than through `grub-mkconfig`, which needs a
    /// Debian userspace the live image does not have yet.
    ///
    /// The quiet entry asks for `loglevel=3` as well, which `quiet` on its own
    /// does not give: `quiet` leaves the console at 4, and `KERN_ERR` is 3, so
    /// every driver that logs an error on the way in prints over a boot that
    /// is meant to be silent. On a VirtualBox guest with the VMSVGA adapter —
    /// the way most people meet tOS — that is the first line on the screen,
    /// from `vmwgfx` writing a version string to a host log port VirtualBox
    /// does not implement (#129). Nothing is lost by not painting it:
    /// journald reads `/dev/kmsg` whatever the console is set to, so
    /// `journalctl -b -p err` still has it, and the verbose entry beside this
    /// one still prints everything.
    fn grub_config(&self) -> String {
        format!(
            "set timeout=2\n\
             set default=0\n\
             \n\
             menuentry \"tOS\" {{\n\
             \tsearch --no-floppy --label --set=root tos-root\n\
             \tlinux /boot/vmlinuz {CMDLINE} quiet loglevel=3\n\
             \tinitrd /boot/initramfs.gz\n\
             }}\n\
             \n\
             menuentry \"tOS (verbose)\" {{\n\
             \tsearch --no-floppy --label --set=root tos-root\n\
             \tlinux /boot/vmlinuz {CMDLINE}\n\
             \tinitrd /boot/initramfs.gz\n\
             }}\n"
        )
    }

    fn finish(&mut self) -> Result<(), String> {
        self.command("sync", &[])?;
        self.unmount_all();
        Ok(())
    }

    // ---- helpers --------------------------------------------------------

    /// Unmount whatever the installer mounted, deepest first.
    fn unmount_all(&mut self) {
        if self.plan.firmware == Firmware::Uefi {
            let esp = self.plan.esp_mount();
            let _ = self.backend.run("umount", &[&esp]);
        }
        let mount_point = self.plan.mount_point.clone();
        let _ = self.backend.run("umount", &[&mount_point]);
    }

    /// A partition node does not appear the instant the table is written.
    fn wait_for_partition(&mut self, path: &str) -> Result<(), String> {
        for _ in 0..PARTITION_WAIT_ATTEMPTS {
            if self.backend.exists(path) {
                return Ok(());
            }
            let _ = self.backend.run("udevadm", &["settle"]);
        }
        // A recorder never reports anything as existing, so a missing node is
        // only fatal when the backend can actually see the filesystem.
        if self.backend.exists("/dev") {
            return Err(format!("{path} never appeared after partitioning"));
        }
        Ok(())
    }

    fn command(&mut self, program: &str, args: &[&str]) -> Result<(), String> {
        self.progress
            .note(format!("   $ {}", describe(program, args)));
        let output = self
            .backend
            .run(program, args)
            .map_err(|e| format!("{program}: {e}"))?;
        self.check(program, output)
    }

    fn command_with_input(
        &mut self,
        program: &str,
        args: &[&str],
        input: &str,
    ) -> Result<(), String> {
        self.progress
            .note(format!("   $ {}", describe(program, args)));
        let output = self
            .backend
            .run_with_input(program, args, input)
            .map_err(|e| format!("{program}: {e}"))?;
        self.check(program, output)
    }

    fn check(&mut self, program: &str, output: Output) -> Result<(), String> {
        for line in output.stdout.lines().chain(output.stderr.lines()) {
            self.progress.note(format!("   {line}"));
        }
        if output.is_success() {
            return Ok(());
        }
        let detail = output
            .stderr
            .lines()
            .next()
            .or_else(|| output.stdout.lines().next())
            .unwrap_or("no output")
            .to_string();
        Err(format!("{program} failed ({}): {detail}", output.status))
    }

    fn write(&mut self, path: &str, contents: &str) -> Result<(), String> {
        self.progress.note(format!("   write {path}"));
        self.backend
            .write_file(path, contents)
            .map_err(|e| format!("cannot write {path}: {e}"))
    }

    fn append(&mut self, path: &str, contents: &str) -> Result<(), String> {
        self.progress.note(format!("   append to {path}"));
        self.backend
            .append_file(path, contents)
            .map_err(|e| format!("cannot append to {path}: {e}"))
    }
}

fn describe(program: &str, args: &[&str]) -> String {
    let mut text = program.to_string();
    for arg in args {
        text.push(' ');
        text.push_str(arg);
    }
    text
}

/// How many times to wait for a partition node before giving up.
const PARTITION_WAIT_ATTEMPTS: usize = 20;

/// What is copied out of the live system onto the disk when there is no
/// rootfs image to unpack instead.
///
/// That happens on an image built before the rootfs step existed, and on one
/// whose squashfs would not mount. The live image is then an initramfs, so
/// this is the whole of it: the compositor, busybox and the kernel modules.
/// `/boot` is not among them, because it is on the medium rather than in the
/// initramfs.
///
/// `/usr` is here for what little is under it, and that is less than it was:
/// `/usr/lib/grub` went to the rootfs with the rest of GRUB, so this path can
/// no longer put a bootloader onto a disk either. The font and the dictionary
/// left the initramfs the same way — they are in the rootfs, which is the
/// path this one is not — so a machine installed this way draws Latin from
/// the compositor's built-in ASCII face and cannot type Japanese.
///
/// Which leaves no session that reaches here: the only one without an
/// `unsquashfs` is the rescue session, and [`Plan::refusal`] stops that one
/// before it starts. It stays because the two questions are separate — what
/// goes onto the disk, and whether the disk can then be booted — and it is
/// only today's image that makes the answers coincide.
pub const COPIED_DIRECTORIES: &[&str] = &["/bin", "/sbin", "/lib", "/usr", "/etc", "/root"];

/// The files GRUB loads, taken from the live medium.
pub const BOOT_FILES: &[&str] = &["vmlinuz", "initramfs.gz"];

/// The kernel command line an installed tOS machine boots with.
///
/// Two of these four words are a security decision rather than a convenience,
/// and a third decision is a word that is deliberately not here.
/// `docs/design/lock-other-doors.md` argues all three.
///
/// `console=tty0` and no `console=ttyS0`: `/init` execs a shell on
/// `/dev/console` when the compositor exits, so a serial console on an
/// installed machine is an unauthenticated root shell on a wire. The live
/// image wants one and says so in `iso/mkiso.sh`; a machine somebody leaves
/// alone does not.
///
/// No `tos.rescue`, which is the word `/init` wants before it execs that
/// shell at all. An installed machine that will not start its compositor is
/// rescued by adding it at the GRUB prompt.
///
/// `sysctl.kernel.sysrq=434` is `0x1b2`: the Debian kernel's own default mask
/// of `0x1b6` with `SYSRQ_ENABLE_KEYBOARD` (`0x4`) taken out. That bit carries
/// `Alt+SysRq+k` and `Alt+SysRq+r`, the two SysRq functions that take a locked
/// session away from the compositor; the sync, remount-read-only and reboot
/// bits stay, so S-U-B still gets a wedged machine down without losing the
/// filesystem. There is no `sysrq=` boot parameter — the mask is a sysctl, and
/// `sysctl.*=` is the generic form the kernel applies just before `/init`.
const CMDLINE: &str = "root=LABEL=tos-root rw console=tty0 sysctl.kernel.sysrq=434";

/// Where `unsquashfs` lives on a session that has one.
///
/// `/usr/bin` is where the rootfs build asserts it landed and `/bin` is the
/// same file on a usr-merged Debian. `/sbin` is where `iso/mkiso.sh` puts the
/// tools it copies into the initramfs, so if the rescue session is ever given
/// one this finds it there rather than having to be remembered.
const UNSQUASHFS: [&str; 3] = ["/usr/bin/unsquashfs", "/bin/unsquashfs", "/sbin/unsquashfs"];

/// A recorder primed to look like the session it is running in.
///
/// This is what a dry run and `--plan` walk, so that what they print is the
/// whole sequence rather than the prefix that runs before the first missing
/// path stops it.
///
/// The directories and the boot files are the shape of a live session and are
/// taken as read. Which *installation* this is, though, is asked of the
/// machine: whether there is a rootfs image on the medium and something able
/// to open it is the one question that decides between unpacking Debian and
/// copying the session, and hard-coding the answer made the dry run describe
/// the unpack on a rescue session that cannot do it — the session where
/// somebody is most likely to read the plan before they let it run.
pub fn planning_backend() -> crate::exec::Recorder {
    planning_backend_for(&crate::exec::System)
}

/// The same, against a stated world rather than this machine.
///
/// Public to the crate's tests, which have to describe a live medium without
/// being run on one.
fn planning_backend_for(world: &dyn crate::exec::Backend) -> crate::exec::Recorder {
    let mut backend = crate::exec::Recorder::new();
    for directory in COPIED_DIRECTORIES {
        backend.existing.push(directory.to_string());
    }
    for file in BOOT_FILES {
        backend
            .existing
            .push(format!("{}/{file}", crate::plan::LIVE_MEDIUM_BOOT));
    }

    let image = crate::plan::LIVE_ROOTFS_IMAGE;
    let tool = UNSQUASHFS.iter().find(|tool| world.exists(tool));
    if let (true, Some(tool)) = (world.exists(image), tool) {
        backend.existing.push(image.to_string());
        backend.existing.push(tool.to_string());
        // The account files the unpacked rootfs will bring with it, so the
        // plan shows the append a real install makes rather than the overwrite
        // it would only do onto a disk that came from the busybox world — and
        // the bash it brings, which is what decides the login shell it writes.
        // /etc/shadow is Debian's too: base-passwd writes a line for every
        // system account it creates, all of them `*`.
        for file in ["etc/passwd", "etc/group", "etc/shadow", "bin/bash"] {
            backend
                .existing
                .push(format!("{}/{file}", crate::plan::MOUNT_POINT));
        }
    }
    backend
}

/// A recorder for a session that has a medium with a rootfs on it.
///
/// `pub(crate)` because `app.rs`'s tests drive the dry run and have to describe
/// a live medium without being run on one — and since [`planning_backend`]
/// started asking the machine, a helper that called it was describing whatever
/// machine `cargo test` happened to be on.
#[cfg(test)]
pub(crate) fn live_planning_backend() -> crate::exec::Recorder {
    let mut world = crate::exec::Recorder::new();
    world
        .existing
        .push(crate::plan::LIVE_ROOTFS_IMAGE.to_string());
    world.existing.push(UNSQUASHFS[0].to_string());
    planning_backend_for(&world)
}

/// The uid and gid of the account the installer creates, as `/etc/passwd`
/// names it and as the account's home is chowned to. One place, because a
/// home owned by a different number than the account is a shell that cannot
/// write its own history and says nothing about why.
const ACCOUNT_ID: &str = "1000";

/// The `~/.bashrc` a tOS machine gives an account it creates.
///
/// `include_str!` rather than a second copy: `iso/mkiso.sh` installs this same
/// file as `/root/.bashrc` and `/etc/skel/.bashrc` inside the image, and a
/// machine installed from an image is meant to be that image.
const BASHRC: &str = include_str!("../../iso/bashrc");

/// The `~/.profile` that goes with it, for the login shells that do not read
/// `~/.bashrc` at all.
///
/// Same reason for `include_str!`: `iso/mkiso.sh` installs this file as
/// `/root/.profile` and `/etc/skel/.profile`.
const PROFILE: &str = include_str!("../../iso/dot-profile");

/// Where a drop-in for `tos-session.service` goes: systemd reads every `.conf`
/// in this directory on top of the unit itself.
///
/// The unit is the image's — `iso/mkiso.sh` writes it into the rootfs — and
/// this is the installer's half: the one thing about the session that is this
/// machine's rather than every machine's.
pub const SESSION_DROPIN_DIRECTORY: &str = "/etc/systemd/system/tos-session.service.d";

/// The one this installer writes.
///
/// Named for what is in it rather than numbered. Debian's convention of a
/// numeric prefix is for ordering several drop-ins against each other, and
/// there is one.
pub const SESSION_DROPIN: &str = "/etc/systemd/system/tos-session.service.d/user.conf";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::Disk;
    use crate::exec::Recorder;
    use crate::plan::{Bootloader, Settings};

    fn disk() -> Disk {
        Disk {
            name: "sda".into(),
            path: "/dev/sda".into(),
            bytes: 64 * (1 << 30),
            model: "QEMU HARDDISK".into(),
            removable: false,
            read_only: false,
            in_use: false,
            is_boot_medium: false,
        }
    }

    fn plan(firmware: Firmware) -> Plan {
        Plan::new(disk(), firmware, Bootloader::Present, Settings::default())
    }

    /// Run a whole installation against a recorder.
    fn install(firmware: Firmware) -> Recorder {
        let mut backend = live_backend();
        let mut installer = Installer::new(plan(firmware), &mut backend);
        let progress = installer.run();
        assert!(
            progress.failure().is_none(),
            "installation failed: {:?}",
            progress.failure()
        );
        backend
    }

    /// A recorder that looks like a live session with its medium mounted.
    ///
    /// Stated as a world rather than subtracted from a fixture: the plan is
    /// derived from what the session has, so a test says what is on the
    /// machine and reads back the installation that follows from it.
    fn live_backend() -> Recorder {
        live_planning_backend()
    }

    /// The same, on a medium that carries no Debian rootfs: an image built
    /// before the rootfs step existed, which the installer still has to be
    /// able to install from — and, with the image present but nothing able to
    /// open it, the rescue session.
    fn rootfsless_backend() -> Recorder {
        planning_backend_for(&Recorder::new())
    }

    /// Run a whole installation against a recorder that has been set up.
    fn install_with(firmware: Firmware, backend: &mut Recorder) {
        let mut installer = Installer::new(plan(firmware), backend);
        let progress = installer.run();
        assert!(
            progress.failure().is_none(),
            "installation failed: {:?}",
            progress.failure()
        );
    }

    /// What was written to a path, and the mode it was asked for.
    fn file(backend: &Recorder, path: &str) -> (String, Option<u32>) {
        backend
            .actions
            .iter()
            .find_map(|action| match action {
                crate::exec::Action::WriteFile {
                    path: written,
                    contents,
                    mode,
                } if written == path => Some((contents.clone(), *mode)),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{path} was never written"))
    }

    /// What was written to a path, when only the contents matter.
    fn written(backend: &Recorder, path: &str) -> String {
        file(backend, path).0
    }

    /// What was added to the end of a path.
    fn appended(backend: &Recorder, path: &str) -> String {
        backend
            .actions
            .iter()
            .find_map(|action| match action {
                crate::exec::Action::AppendFile {
                    path: touched,
                    contents,
                } if touched == path => Some(contents.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("nothing was appended to {path}"))
    }

    /// Whether a path was written at all.
    fn wrote(backend: &Recorder, path: &str) -> bool {
        backend.actions.iter().any(|action| {
            matches!(action, crate::exec::Action::WriteFile { path: written, .. } if written == path)
        })
    }

    #[test]
    fn a_uefi_installation_runs_every_step() {
        let mut backend = live_backend();
        let mut installer = Installer::new(plan(Firmware::Uefi), &mut backend);
        let progress = installer.run();
        assert!(progress.is_complete(&plan(Firmware::Uefi)));
        assert_eq!(progress.finished.len(), 8);
    }

    #[test]
    fn the_kernel_is_taken_from_the_live_medium() {
        // An initramfs does not contain the kernel that loaded it.
        let backend = install(Firmware::Uefi);
        assert!(backend.did(&format!(
            "copy {}/vmlinuz -> /mnt/target/boot/vmlinuz",
            crate::plan::LIVE_MEDIUM_BOOT
        )));
        assert!(backend.did("initramfs.gz -> /mnt/target/boot/initramfs.gz"));
    }

    #[test]
    fn a_session_that_cannot_install_grub_never_touches_the_disk() {
        // The whole of this change in one assertion. A rescue session runs
        // Partition, FormatRoot and CopySystem perfectly well and then finds
        // at the eighth step that it has no grub-install; by then the disk it
        // was pointed at is gone and what replaced it does not boot. So the
        // question is asked before the first step instead, and the answer
        // stops every one of them.
        let mut backend = rootfsless_backend();
        let refused = Plan::new(
            disk(),
            Firmware::Uefi,
            Bootloader::Absent,
            Settings::default(),
        );
        let mut installer = Installer::new(refused, &mut backend);
        let progress = installer.run();

        let failure = progress.failure().expect("it went ahead");
        assert!(failure.contains("cannot install a bootloader"), "{failure}");
        for command in [
            "sfdisk",
            "mkfs.ext4",
            "mkfs.vfat",
            "unsquashfs",
            "grub-install",
        ] {
            assert!(!backend.did(command), "{command} ran anyway");
        }
        assert!(
            backend.actions.is_empty(),
            "the disk was touched: {:?}",
            backend.actions
        );
    }

    #[test]
    fn one_step_of_a_refused_installation_is_refused_too() {
        // `run_step` is a way in of its own, and a guard at the top of `run`
        // would leave it open.
        let mut backend = live_backend();
        let refused = Plan::new(
            disk(),
            Firmware::Bios,
            Bootloader::Absent,
            Settings::default(),
        );
        let outcome = Installer::new(refused, &mut backend).run_step(Step::Partition);
        assert!(matches!(outcome, StepOutcome::Failed(m) if m.contains("bootloader")));
        assert!(!backend.did("sfdisk"));
    }

    #[test]
    fn a_session_with_grub_installs_as_it_always_did() {
        // The other half of the assertion above: the refusal is about the one
        // session that has no GRUB, and not a new way for a live one to fail.
        let backend = install(Firmware::Uefi);
        assert!(backend.did("grub-install --target=x86_64-efi"));
    }

    #[test]
    fn a_bootloader_is_looked_for_where_debian_and_the_initramfs_keep_one() {
        assert_eq!(Bootloader::detect(&Recorder::new()), Bootloader::Absent);
        for path in ["/usr/sbin/grub-install", "/sbin/grub-install"] {
            let world = Recorder::new().with_existing(path);
            assert_eq!(Bootloader::detect(&world), Bootloader::Present, "{path}");
        }
    }

    #[test]
    fn an_unmounted_medium_stops_the_installation() {
        // Otherwise the disk is written, GRUB is installed, and the machine
        // drops to a GRUB prompt on first boot with no kernel to load.
        let mut backend = Recorder::new();
        for directory in COPIED_DIRECTORIES {
            backend.existing.push(directory.to_string());
        }
        let mut installer = Installer::new(plan(Firmware::Uefi), &mut backend);
        let progress = installer.run();
        let failure = progress.failure().expect("should have failed");
        assert!(failure.contains("no kernel found"), "{failure}");
        assert!(!backend.did("grub-install"), "it carried on regardless");
    }

    #[test]
    fn the_disk_is_partitioned_with_a_gpt_script() {
        let backend = install(Firmware::Uefi);
        assert!(backend.did("sfdisk"));
        match backend
            .actions
            .iter()
            .find(|a| a.describe().starts_with("sfdisk"))
            .unwrap()
        {
            crate::exec::Action::Run { input, args, .. } => {
                assert!(args.contains(&"/dev/sda".to_string()));
                assert!(input.as_ref().unwrap().contains("label: gpt"));
            }
            other => panic!("expected a run, got {other:?}"),
        }
    }

    #[test]
    fn the_kernel_is_told_to_reread_the_table() {
        // Without this the partition nodes may not exist when mkfs runs.
        let backend = install(Firmware::Uefi);
        assert!(backend.did("partx -u /dev/sda") || backend.did("partprobe /dev/sda"));
        let reread = backend.position_of("partx -u").unwrap();
        let format = backend.position_of("mkfs.ext4").unwrap();
        assert!(reread < format);
    }

    #[test]
    fn filesystems_are_made_on_the_right_partitions() {
        let backend = install(Firmware::Uefi);
        assert!(backend.did("mkfs.vfat -F 32 -n TOS-ESP /dev/sda1"));
        assert!(backend.did("mkfs.ext4 -F -L tos-root /dev/sda2"));
    }

    #[test]
    fn a_bios_install_does_not_format_the_boot_partition() {
        // grub-install writes raw sectors there; a filesystem would be lost.
        let backend = install(Firmware::Bios);
        assert!(!backend.did("mkfs.vfat"));
        assert!(backend.did("mkfs.ext4 -F -L tos-root /dev/sda2"));
    }

    #[test]
    fn partitioning_happens_before_formatting() {
        let backend = install(Firmware::Uefi);
        let partition = backend.position_of("sfdisk").unwrap();
        let format = backend.position_of("mkfs.ext4").unwrap();
        let mount = backend.position_of("mount /dev/sda2").unwrap();
        assert!(partition < format, "formatted before the table was written");
        assert!(format < mount, "mounted before the filesystem existed");
    }

    #[test]
    fn the_esp_is_mounted_under_the_root_it_belongs_to() {
        let backend = install(Firmware::Uefi);
        let root = backend.position_of("mount /dev/sda2 /mnt/target").unwrap();
        let esp = backend
            .position_of("mount /dev/sda1 /mnt/target/boot/efi")
            .unwrap();
        assert!(
            root < esp,
            "the ESP mount would be hidden by the root mount"
        );
    }

    #[test]
    fn the_debian_rootfs_is_unpacked_onto_the_disk() {
        let backend = install(Firmware::Uefi);
        assert!(
            backend.did(&format!(
                "unsquashfs -f -no-progress -d /mnt/target {}",
                crate::plan::LIVE_ROOTFS_IMAGE
            )),
            "the rootfs was not unpacked: {:?}",
            backend.transcript()
        );
    }

    #[test]
    fn the_rootfs_is_unpacked_rather_than_the_live_session_being_copied() {
        // The live root is this same image with a tmpfs over it, so copying
        // it would put whatever the session scribbled onto the disk.
        let backend = install(Firmware::Uefi);
        for directory in COPIED_DIRECTORIES {
            assert!(
                !backend.did(&format!("copy {directory} -> /mnt/target{directory}")),
                "{directory} was copied out of the live session"
            );
        }
    }

    #[test]
    fn a_medium_without_a_rootfs_falls_back_to_copying_the_live_system() {
        let mut backend = rootfsless_backend();
        install_with(Firmware::Uefi, &mut backend);
        for directory in COPIED_DIRECTORIES {
            assert!(
                backend.did(&format!("copy {directory} -> /mnt/target{directory}")),
                "{directory} was not copied"
            );
        }
        assert!(!backend.did("unsquashfs"), "there was nothing to unpack");
    }

    #[test]
    fn kernel_directories_are_created_but_not_copied() {
        let backend = install(Firmware::Uefi);
        assert!(backend.did("mkdir -p /mnt/target/proc"));
        assert!(backend.did("mkdir -p /mnt/target/dev"));
        assert!(
            !backend.did("copy /proc"),
            "copying /proc would never finish"
        );
        assert!(!backend.did("copy /sys"));
    }

    #[test]
    fn a_missing_source_directory_is_skipped_rather_than_failing() {
        // A live image without /root should still install. Only the fallback
        // copies anything, so that is where this can go wrong.
        let mut backend = rootfsless_backend();
        backend.existing.retain(|path| path != "/root");
        install_with(Firmware::Uefi, &mut backend);
        assert!(backend.did("copy /bin -> /mnt/target/bin"));
        assert!(!backend.did("copy /root"));
    }

    #[test]
    fn a_session_that_cannot_open_a_squashfs_copies_instead_of_failing() {
        // The rescue session: the squashfs would not mount, `/init` fell back
        // to the initramfs, and the medium — and so the image on it — is still
        // there while nothing in that busybox world can open it. Deciding on
        // the image alone sent exactly that session down the unpack path, and
        // it found out at the unpack: after Partition, FormatEsp and FormatRoot
        // had already been run over the disk.
        let mut backend = planning_backend();
        backend
            .existing
            .retain(|path| !path.ends_with("unsquashfs"));
        install_with(Firmware::Uefi, &mut backend);

        assert!(
            !backend.did("unsquashfs"),
            "unpacked with a tool this session has not got"
        );
        assert!(backend.did("copy /bin -> /mnt/target/bin"));
    }

    #[test]
    fn the_configuration_names_the_machine_and_the_user() {
        let mut backend = live_backend();
        let settings = Settings {
            hostname: "workshop".into(),
            username: "yusuke".into(),
            ..Settings::default()
        };
        let mut installer = Installer::new(
            Plan::new(disk(), Firmware::Uefi, Bootloader::Present, settings),
            &mut backend,
        );
        installer.run();

        assert_eq!(written(&backend, "/mnt/target/etc/hostname"), "workshop\n");
        assert!(written(&backend, "/mnt/target/etc/hosts").contains("workshop"));
        // Appended, not written: this install unpacks a Debian rootfs, which
        // arrives with its own passwd and its own root line. The medium that
        // carries no rootfs is where tOS writes the whole file, and that is
        // where root's line is asserted.
        let passwd = appended(&backend, "/mnt/target/etc/passwd");
        assert!(passwd.contains("yusuke:x:1000:1000"));
        assert!(appended(&backend, "/mnt/target/etc/group").contains("yusuke:x:1000:"));
        assert!(backend.did("mkdir -p /mnt/target/home/yusuke"));
    }

    #[test]
    fn the_person_gets_a_way_to_be_root_they_can_actually_use() {
        // A session's panes run as the person now, and Debian's root carries
        // `*`, so without this an installed machine has no route to root at
        // all: `su` accepts nothing and there is nobody else to become.
        let mut backend = live_backend();
        let settings = Settings {
            username: "yusuke".into(),
            password: "hunter2".into(),
            ..Settings::default()
        };
        let mut installer = Installer::new(
            Plan::new(disk(), Firmware::Uefi, Bootloader::Present, settings),
            &mut backend,
        );
        installer.run();

        let (rule, mode) = file(&backend, "/mnt/target/etc/sudoers.d/yusuke");
        assert_eq!(
            rule,
            "yusuke ALL=(ALL:ALL) ALL\n\
             # Turning the machine off, without a password: the power menu\n\
             # and the power button already do that much. See #158.\n\
             yusuke ALL=(ALL:ALL) NOPASSWD: \
             /sbin/shutdown, /sbin/poweroff, /sbin/reboot, /sbin/halt\n"
        );
        // sudo skips a drop-in anybody but root can write, and skips it
        // silently — a 0644 here would be a machine that still cannot.
        assert_eq!(mode, Some(0o440), "sudo ignores a writable drop-in");
    }

    #[test]
    fn turning_the_machine_off_costs_no_password() {
        // #158: `shutdown -h now` in a pane could not turn an installed
        // machine off at all — the pane is the person since #119, and
        // `systemctl` reaches PID 1 through a bus this image does not ship
        // and a socket only root can open. The shim on PATH hands the real
        // program to `sudo -n`, and this is the line that lets it through.
        //
        // Asserted by the path `sudo` matches against, one program at a time,
        // because a rule that named three of the four would be a machine
        // where one flag of `shutdown` asks for a password and the rest do
        // not — and nothing but this would say so.
        let mut backend = live_backend();
        let settings = Settings {
            username: "yusuke".into(),
            password: "hunter2".into(),
            ..Settings::default()
        };
        let mut installer = Installer::new(
            Plan::new(disk(), Firmware::Uefi, Bootloader::Present, settings),
            &mut backend,
        );
        installer.run();

        let (rule, _) = file(&backend, "/mnt/target/etc/sudoers.d/yusuke");
        let nopasswd = rule
            .lines()
            .find(|line| line.contains("NOPASSWD:"))
            .expect("the drop-in grants the power programs without a password");
        for program in [
            "/sbin/shutdown",
            "/sbin/poweroff",
            "/sbin/reboot",
            "/sbin/halt",
        ] {
            assert!(nopasswd.contains(program), "{nopasswd} omits {program}");
        }
        // And everything else still costs one. A machine that asked for no
        // password anywhere is a different decision from this one, and the
        // login screen's rule — no password set, no boundary — is the only
        // thing that makes it.
        assert!(
            rule.contains("yusuke ALL=(ALL:ALL) ALL\n"),
            "the person is still root for everything else, with a password: {rule}"
        );
    }

    #[test]
    fn a_machine_with_no_password_is_not_locked_out_of_itself() {
        // The same rule the login screen obeys: no password, no boundary.
        // Asking for one that cannot exist — the field is `*`, which
        // authenticates nobody — would leave the machine unable to administer
        // itself while guarding nothing that somebody at its keyboard has not
        // already got.
        let mut backend = live_backend();
        install_with(Firmware::Uefi, &mut backend);
        let (rule, _) = file(&backend, "/mnt/target/etc/sudoers.d/tos");
        assert_eq!(rule, "tos ALL=(ALL:ALL) NOPASSWD: ALL\n");
    }

    #[test]
    fn the_passwd_file_promises_a_shadow_line_that_is_there() {
        // `x` means "the hash is in /etc/shadow". It said `*` until #111,
        // because the credential was /etc/tos/shadow and nothing that reads
        // passwd knew about it; now the machine's password is in the file
        // every login program already reads, and the `x` is true.
        // Both files are tOS's to write only where no Debian rootfs was
        // unpacked first; where one was, root belongs to Debian and the
        // installer appends its one line.
        let mut backend = rootfsless_backend();
        install_with(Firmware::Uefi, &mut backend);
        let passwd = written(&backend, "/mnt/target/etc/passwd");
        assert!(passwd.contains("root:x:0:0"), "{passwd}");
        assert!(passwd.contains("tos:x:1000:1000"), "{passwd}");

        let (shadow, mode) = file(&backend, "/mnt/target/etc/shadow");
        assert_eq!(mode, Some(0o640), "the hash must not be world readable");
        // No password was asked for in this plan, so the person's field is
        // `*` — an account nothing authenticates as, which is what root's
        // says too.
        assert!(shadow.contains("root:*:"), "{shadow}");
        assert!(shadow.contains("tos:*:"), "{shadow}");
    }

    #[test]
    fn a_debian_roots_own_accounts_are_added_to_rather_than_replaced() {
        // The unpacked rootfs brings its own /etc/passwd, and `_apt` is in it.
        // apt drops to that account before it opens a socket, so a passwd file
        // written over the top of Debian's is a machine that cannot download
        // the package it was installed to be able to download.
        let mut backend = live_backend();
        backend.existing.push("/mnt/target/etc/passwd".to_string());
        install_with(Firmware::Uefi, &mut backend);

        assert!(
            !wrote(&backend, "/mnt/target/etc/passwd"),
            "Debian's own passwd was overwritten"
        );
        assert!(
            !wrote(&backend, "/mnt/target/etc/group"),
            "Debian's own group file was overwritten"
        );
        assert!(backend.did("append to /mnt/target/etc/passwd"));
        assert!(backend.did("append to /mnt/target/etc/group"));

        let added = appended(&backend, "/mnt/target/etc/passwd");
        assert!(added.contains("tos:x:1000:1000"), "{added}");
        assert!(
            !added.contains("root:"),
            "root is Debian's line to write, not ours: {added}"
        );

        // And the same for /etc/shadow, which Debian writes a line into for
        // every system account it makes. Written over, it would be a machine
        // whose accounts all claimed a hash that was no longer in the file.
        assert!(
            !wrote(&backend, "/mnt/target/etc/shadow"),
            "Debian's own shadow file was overwritten"
        );
        let secret = appended(&backend, "/mnt/target/etc/shadow");
        assert!(secret.starts_with("tos:"), "{secret}");
        assert!(!secret.contains("root:"), "{secret}");
    }

    #[test]
    fn a_password_is_hashed_into_the_line_that_logs_the_person_in() {
        let mut backend = live_backend();
        let settings = Settings {
            username: "yusuke".into(),
            password: "correct horse battery staple".into(),
            ..Settings::default()
        };
        let mut installer = Installer::new(
            Plan::new(disk(), Firmware::Uefi, Bootloader::Present, settings),
            &mut backend,
        );
        let progress = installer.run();
        assert!(progress.failure().is_none());

        // One line, added to the file Debian brought, and in that file's
        // format: the account first, the hash second, and the aging fields
        // after it empty.
        let line = appended(&backend, "/mnt/target/etc/shadow");
        assert_eq!(line.lines().count(), 1);
        assert!(line.ends_with('\n'));
        let mut fields = line.trim_end().split(':');
        assert_eq!(fields.next(), Some("yusuke"));
        let hash = fields.next().expect("a password field");
        assert!(hash.starts_with("$6$"), "not a SHA-512 crypt line: {hash}");
        assert_eq!(fields.clone().count(), 7, "not a shadow line: {line}");
        assert!(fields.all(str::is_empty), "aging was written: {line}");

        // The point of the whole issue: what was typed opens this, and every
        // program on the machine that authenticates anybody reads it.
        assert_eq!(
            tos_crypt::verify_password(b"correct horse battery staple", hash),
            Ok(true)
        );
        assert_eq!(tos_crypt::verify_password(b"", hash), Ok(false));
        assert_eq!(
            tos_crypt::verify_password(b"correct horse battery stapl", hash),
            Ok(false)
        );
    }

    #[test]
    fn the_password_itself_is_never_recorded_anywhere() {
        let mut backend = live_backend();
        let settings = Settings {
            password: "hunter2".into(),
            ..Settings::default()
        };
        let mut installer = Installer::new(
            Plan::new(disk(), Firmware::Uefi, Bootloader::Present, settings),
            &mut backend,
        );
        let log = installer.run().log.join("\n");

        let transcript = format!("{:?}", backend.actions);
        assert!(!transcript.contains("hunter2"), "{transcript}");
        assert!(!log.contains("hunter2"), "{log}");
        // Nor does the log carry the hash, which is a thing to attack.
        assert!(!log.contains("$6$"), "{log}");
    }

    #[test]
    fn two_installations_of_the_same_password_get_different_hashes() {
        // A fresh salt each time, so two machines with one password do not
        // give each other away.
        let hashes: Vec<String> = (0..2)
            .map(|_| {
                let mut backend = live_backend();
                let settings = Settings {
                    password: "hunter2".into(),
                    ..Settings::default()
                };
                Installer::new(
                    Plan::new(disk(), Firmware::Uefi, Bootloader::Present, settings),
                    &mut backend,
                )
                .run();
                appended(&backend, "/mnt/target/etc/shadow")
            })
            .collect();
        assert_ne!(hashes[0], hashes[1]);
    }

    #[test]
    fn no_password_means_an_account_nothing_can_log_in_to() {
        // An empty password hashes perfectly well, and a lock that opened for
        // a bare Enter — or an sshd that did — would be worse than an account
        // nobody can authenticate as. `*` is how that file says so, and it is
        // what every Debian system account in it already carries.
        let backend = install(Firmware::Uefi);
        let line = appended(&backend, "/mnt/target/etc/shadow");
        assert!(line.starts_with("tos:*:"), "{line}");
        assert!(!line.contains("$6$"), "a password was invented: {line}");
    }

    #[test]
    fn the_installed_system_starts_the_compositor() {
        // Nothing here writes an init script any more, and that is the point:
        // the rootfs on the disk carries tos-session.service and the systemd
        // that reads it, so an installed machine boots into tOS because the
        // image it came from does (#110). What the installer adds is the one
        // thing the image cannot know.
        let backend = install(Firmware::Uefi);
        let dropin = written(&backend, &format!("/mnt/target{SESSION_DROPIN}"));
        assert!(backend.did(&format!("mkdir -p /mnt/target{SESSION_DROPIN_DIRECTORY}")));
        assert!(dropin.contains("[Service]"), "not a unit file: {dropin}");
        assert!(
            dropin.contains("Environment=TOS_USER=tos"),
            "the session has to know whose it is: {dropin}"
        );
    }

    #[test]
    fn nothing_is_written_for_an_init_that_is_no_longer_there() {
        // /etc/inittab, /etc/rc and /etc/tos-session were the whole of what
        // an installed machine ran, and every one of them is somebody else's
        // job now. Left behind, they would be three files that look like they
        // are running the machine and are read by nothing.
        let backend = install(Firmware::Uefi);
        for path in [
            "/mnt/target/etc/inittab",
            "/mnt/target/etc/rc",
            "/mnt/target/etc/tos-session",
        ] {
            assert!(
                !wrote(&backend, path),
                "{path} was written: {:?}",
                backend.transcript()
            );
        }
    }

    #[test]
    fn an_account_on_a_debian_disk_gets_bash_and_a_bashrc_to_go_with_it() {
        // #82. ash has no programmable completion, a much weaker line editor,
        // no arrays and no `[[`, and every dotfile somebody arrives with is a
        // bash dotfile — so a machine that hands them `/bin/sh` reads as
        // broken rather than as deliberately small. The rc file is half of it:
        // a pane runs an interactive shell that is not a login shell, which is
        // the one case bash reads ~/.bashrc and neither /etc/profile nor $ENV.
        let backend = install(Firmware::Uefi);

        let account = appended(&backend, "/mnt/target/etc/passwd");
        assert!(
            account.contains(":/home/tos:/bin/bash\n"),
            "the account should log in to bash: {account}"
        );
        let bashrc = written(&backend, "/mnt/target/home/tos/.bashrc");
        assert!(bashrc.contains("HISTCONTROL"), "{bashrc}");
        assert_eq!(
            bashrc, BASHRC,
            "the installed rc file is not the one in the tree"
        );
        // And the other half: bash reads ~/.bashrc for an interactive shell
        // that is not a login shell, which is what a pane runs — `su - tos`
        // is a login shell and reads this file instead.
        let profile = written(&backend, "/mnt/target/home/tos/.profile");
        assert!(profile.contains(".bashrc"), "{profile}");
        assert_eq!(
            profile, PROFILE,
            "the installed profile is not the one in the tree"
        );
    }

    #[test]
    fn the_home_belongs_to_the_account_that_lives_in_it() {
        // The installer runs as root on the live system, so the home and the
        // dotfiles in it are root's until something says otherwise. A home
        // the account cannot write is a shell that cannot append a line to
        // ~/.bash_history — the rc file's history settings would be furniture
        // in a room with no floor.
        let backend = install(Firmware::Uefi);

        assert!(
            backend.did("chown -R 1000:1000 /mnt/target/home/tos"),
            "nothing gave the account its own home: {:?}",
            backend.transcript()
        );
        // After the files, or it would be chowning a directory it is about to
        // put root-owned files into.
        let chown = backend
            .position_of("chown -R 1000:1000 /mnt/target/home/tos")
            .expect("no chown");
        let bashrc = backend
            .position_of("write /mnt/target/home/tos/.bashrc")
            .expect("no bashrc");
        assert!(
            bashrc < chown,
            "the home was chowned before the files were written into it: {:?}",
            backend.transcript()
        );
    }

    #[test]
    fn a_disk_that_got_the_busybox_world_is_not_promised_a_bash() {
        // The fallback copies the initramfs, which has no bash anywhere. A
        // passwd naming one there is a login that fails rather than a shell
        // that is merely spartan, and an rc file for it is furniture for a
        // room nobody can enter.
        let mut backend = rootfsless_backend();
        install_with(Firmware::Uefi, &mut backend);

        let passwd = written(&backend, "/mnt/target/etc/passwd");
        assert!(passwd.contains(":/bin/sh\n"), "{passwd}");
        assert!(!passwd.contains("bash"), "{passwd}");
        assert!(
            !wrote(&backend, "/mnt/target/home/tos/.bashrc"),
            "a bashrc was written for a disk with no bash"
        );
        // And no profile either: all it does is hand a login shell the rc
        // file that is not there.
        assert!(
            !wrote(&backend, "/mnt/target/home/tos/.profile"),
            "a profile was written for a disk with no bash"
        );
        // The home is still the account's, which has nothing to do with bash.
        assert!(backend.did("chown -R 1000:1000 /mnt/target/home/tos"));
    }

    #[test]
    fn a_group_file_that_is_missing_on_its_own_is_written_whole() {
        // The two files used to be decided together, from whether passwd
        // existed, and `append_file` creates what it cannot open — so a root
        // with a passwd and no group got a group file whose only line was the
        // person's: no root group, no tty, no disk, and no _apt for apt to
        // drop to.
        let mut backend = live_backend();
        let path = format!("{}/etc/group", crate::plan::MOUNT_POINT);
        backend.existing.retain(|existing| *existing != path);
        install_with(Firmware::Uefi, &mut backend);

        let group = written(&backend, &path);
        assert!(
            group.starts_with("root:x:0:"),
            "a group file tOS wrote itself has to have a root group: {group}"
        );
        assert!(group.contains("tos:x:1000:"), "{group}");
        // And passwd, which was there, is still only appended to.
        assert!(backend.did(&format!(
            "append to {}/etc/passwd",
            crate::plan::MOUNT_POINT
        )));
    }

    #[test]
    fn the_plan_describes_the_installation_this_session_can_actually_do() {
        // `--plan` and the dry run walk a recorder, and it used to be told
        // that the medium and unsquashfs were there whatever the machine had.
        // On a rescue session — the one that exists because the squashfs
        // would not mount, and the one where somebody is most likely to read
        // the plan first — it described an unpack that could not happen.
        let mut backend = rootfsless_backend();
        install_with(Firmware::Uefi, &mut backend);

        assert!(
            !backend.did("unsquashfs"),
            "the plan promised an unpack this session cannot do: {:?}",
            backend.transcript()
        );
        assert!(backend.did("copy /bin -> /mnt/target/bin"));
    }

    #[test]
    fn the_session_the_machine_runs_is_the_one_in_the_tree() {
        // The environment used to be written out a second time here, because
        // busybox init respawned a script the installer wrote and could not
        // run iso/live-session. The two copies drifted by a TERM and a PATH
        // before a test compared them. Under systemd both images run the same
        // file, so the drift is gone and what is left to pin is that the file
        // still sets up a session: ENV, which is how a pane running dash
        // reads /etc/profile and prints the message of the day, and the rest
        // of what a compositor started by an init system is handed nothing of.
        let session = include_str!("../../iso/live-session");
        for variable in [
            "export HOME=/root",
            "export TOS_USER",
            "export SHELL",
            "export TERM=xterm-256color",
            "export TOS=1",
            "export ENV=/etc/profile",
            "export PATH=/usr/local/sbin:",
        ] {
            assert!(
                session.contains(variable),
                "iso/live-session should export {variable}"
            );
        }
        // TOS_USER is defaulted rather than assigned, so that the drop-in the
        // installer writes is not overwritten by the script it configures.
        assert!(
            session.contains(": \"${TOS_USER:=root}\""),
            "an installed machine's TOS_USER would be overwritten"
        );
        // And bash is asked for rather than assumed, because the rescue
        // session out of the initramfs has none.
        assert!(session.contains("/bin/bash"), "the session found no bash");
    }

    #[test]
    fn an_installed_disk_says_that_it_was_installed() {
        let backend = install(Firmware::Bios);
        let marker = written(&backend, "/mnt/target/etc/tos/installed");
        assert!(
            marker.starts_with("tOS "),
            "the marker should name what put it there: {marker}"
        );
    }

    #[test]
    fn fstab_mounts_by_label_so_the_disk_can_move() {
        let mut backend = live_backend();
        let mut installer = Installer::new(plan(Firmware::Uefi), &mut backend);
        installer.run();
        let fstab = backend
            .actions
            .iter()
            .find_map(|action| match action {
                crate::exec::Action::WriteFile { path, contents, .. }
                    if path.ends_with("/etc/fstab") =>
                {
                    Some(contents.clone())
                }
                _ => None,
            })
            .expect("no fstab");
        assert!(fstab.contains("LABEL=tos-root  /"));
        assert!(fstab.contains("LABEL=TOS-ESP   /boot/efi"));
        // Device names change between boots; labels do not.
        assert!(!fstab.contains("/dev/sda"));
        // And the kernel's own filesystems are not in here: systemd mounts
        // them, and a line here would stack a second mount on each.
        for api in ["/proc", "/sys", "/dev/pts"] {
            assert!(!fstab.contains(api), "{api} is systemd's to mount: {fstab}");
        }
    }

    #[test]
    fn a_bios_fstab_has_no_esp() {
        let mut backend = live_backend();
        let mut installer = Installer::new(plan(Firmware::Bios), &mut backend);
        installer.run();
        let fstab = backend
            .actions
            .iter()
            .find_map(|action| match action {
                crate::exec::Action::WriteFile { path, contents, .. }
                    if path.ends_with("/etc/fstab") =>
                {
                    Some(contents.clone())
                }
                _ => None,
            })
            .unwrap();
        assert!(!fstab.contains("TOS-ESP"));
    }

    #[test]
    fn uefi_installs_grub_to_the_esp() {
        let backend = install(Firmware::Uefi);
        assert!(backend.did("grub-install --target=x86_64-efi"));
        assert!(backend.did("--efi-directory=/mnt/target/boot/efi"));
        // Removable path, because the live image cannot rely on NVRAM.
        assert!(backend.did("--removable"));
    }

    #[test]
    fn bios_installs_grub_to_the_disk_itself() {
        let backend = install(Firmware::Bios);
        assert!(backend.did("grub-install --target=i386-pc"));
        assert!(backend.did("/dev/sda"));
        assert!(!backend.did("--efi-directory"));
    }

    #[test]
    fn grub_finds_the_root_by_label() {
        let backend = install(Firmware::Uefi);
        let config = backend
            .actions
            .iter()
            .find_map(|action| match action {
                crate::exec::Action::WriteFile { path, contents, .. }
                    if path.ends_with("grub.cfg") =>
                {
                    Some(contents.clone())
                }
                _ => None,
            })
            .expect("no grub.cfg");
        assert!(config.contains("--label --set=root tos-root"));
        assert!(config.contains("root=LABEL=tos-root"));
        assert!(config.contains("menuentry \"tOS\""));
    }

    #[test]
    fn the_prompt_wears_the_session_s_own_green() {
        // #141. The prompt kept a blue for a release after everything else tOS
        // draws moved to the picture's green, and it kept it for a plain
        // reason: the colour lives in a shell file and the theme lives in
        // Rust, and nothing held the two together. This is the thing that
        // holds them — not a copy of the number, a comparison against it.
        let accent = tos_compositor::chrome::ACCENT;
        let wanted = format!("38;2;{};{};{}", accent.r, accent.g, accent.b);
        assert!(
            BASHRC.contains(&wanted),
            "the prompt should name the accent as {wanted}"
        );
        // On the prompt itself, and as a true colour rather than an index:
        // the 256 are what applications ask for by name and are not tOS's to
        // theme. Read from the line and not from the file, or the note above
        // it naming the colour it used to be would answer for it.
        let prompt = BASHRC
            .lines()
            .find(|line| line.starts_with("PS1="))
            .expect("a prompt");
        assert!(prompt.contains(&wanted), "{prompt}");
        assert!(
            !prompt.contains("38;5;110"),
            "the prompt still asks for the old blue by palette index: {prompt}"
        );
    }

    #[test]
    fn only_the_quiet_entry_stops_painting_the_kernel_s_errors() {
        // `quiet` leaves the console loglevel at 4, and `KERN_ERR` is 3, so a
        // driver logging an error on the way in prints over a boot that is
        // meant to be silent (#129). Asking for 3 stops that and keeps
        // `KERN_CRIT` and worse.
        //
        // Exactly one of the two entries, and it is the quiet one. The verbose
        // entry exists to be watched; an entry that asked for both `quiet` and
        // everything would be neither.
        let backend = install(Firmware::Uefi);
        let config = backend
            .actions
            .iter()
            .find_map(|action| match action {
                crate::exec::Action::WriteFile { path, contents, .. }
                    if path.ends_with("grub.cfg") =>
                {
                    Some(contents.clone())
                }
                _ => None,
            })
            .expect("no grub.cfg");
        assert_eq!(config.matches("loglevel=3").count(), 1);
        let quiet = config
            .lines()
            .find(|line| line.contains("quiet"))
            .expect("a quiet entry");
        assert!(quiet.contains("loglevel=3"), "{quiet}");
    }

    /// The doors a screen lock cannot close on its own, closed on the command
    /// line instead. See `docs/design/lock-other-doors.md`.
    #[test]
    fn the_installed_command_line_shuts_the_doors_the_lock_cannot() {
        let backend = install(Firmware::Uefi);
        let config = backend
            .actions
            .iter()
            .find_map(|action| match action {
                crate::exec::Action::WriteFile { path, contents, .. }
                    if path.ends_with("grub.cfg") =>
                {
                    Some(contents.clone())
                }
                _ => None,
            })
            .expect("no grub.cfg");

        // 0x1b2: Debian's own 0x1b6 without SYSRQ_ENABLE_KEYBOARD (0x4).
        // Alt+SysRq+k and Alt+SysRq+r are what that bit carries, and both take
        // a locked session away from the compositor.
        assert!(
            config.contains("sysctl.kernel.sysrq=434"),
            "SysRq policy must be stated, not inherited: {config}"
        );
        // /init execs a shell on /dev/console when the compositor exits, so a
        // serial console here would be an unauthenticated root shell.
        assert!(
            !config.contains("ttyS0"),
            "an installed machine gets no serial console: {config}"
        );
        // And the shell itself is not asked for. Only the live image asks.
        assert!(
            !config.contains("tos.rescue"),
            "the emergency shell is not a boot menu entry: {config}"
        );
        // Both entries, not just the quiet one.
        assert_eq!(config.matches("sysctl.kernel.sysrq=434").count(), 2);
    }

    #[test]
    fn everything_is_unmounted_at_the_end() {
        let backend = install(Firmware::Uefi);
        let sync = backend.position_of("sync").unwrap();
        let umount_root = backend
            .transcript()
            .iter()
            .rposition(|line| line == "umount /mnt/target")
            .unwrap();
        assert!(sync < umount_root, "unmounted before flushing");
        // Deepest first, or the root unmount would be refused.
        let umount_esp = backend
            .transcript()
            .iter()
            .rposition(|line| line == "umount /mnt/target/boot/efi")
            .unwrap();
        assert!(umount_esp < umount_root);
    }

    #[test]
    fn a_failing_step_stops_the_installation() {
        let mut backend = Recorder::new().failing("mkfs.ext4", "device is busy");
        let mut installer = Installer::new(plan(Firmware::Uefi), &mut backend);
        let progress = installer.run();

        let failure = progress.failure().expect("should have failed");
        assert!(failure.contains("mkfs.ext4"));
        assert!(failure.contains("device is busy"));
        // Nothing after the failure was attempted.
        assert!(!backend.did("grub-install"));
        assert!(!backend.did("copy /bin"));
    }

    #[test]
    fn a_failing_step_is_reported_against_that_step() {
        let mut backend = live_backend();
        backend.responses.push((
            "grub-install".to_string(),
            crate::exec::Output {
                status: 1,
                stdout: String::new(),
                stderr: "cannot find EFI directory".to_string(),
            },
        ));
        let mut installer = Installer::new(plan(Firmware::Uefi), &mut backend);
        installer.run();
        let (step, outcome) = installer
            .progress
            .finished
            .iter()
            .find(|(_, outcome)| outcome.is_failure())
            .unwrap();
        assert_eq!(*step, Step::Bootloader);
        assert!(matches!(outcome, StepOutcome::Failed(m) if m.contains("grub-install")));
    }

    #[test]
    fn the_log_records_what_was_run() {
        let mut backend = live_backend();
        let mut installer = Installer::new(plan(Firmware::Uefi), &mut backend);
        let progress = installer.run();
        let log = progress.log.join("\n");
        assert!(log.contains("Partition the disk"));
        assert!(log.contains("sfdisk"));
        assert!(log.contains("Install the bootloader"));
    }

    #[test]
    fn the_log_does_not_grow_without_bound() {
        let mut progress = Progress::default();
        for i in 0..MAX_LOG_LINES * 3 {
            progress.note(format!("line {i}"));
        }
        assert_eq!(progress.log.len(), MAX_LOG_LINES);
        // It is the tail that is kept.
        assert!(progress
            .log
            .last()
            .unwrap()
            .contains(&format!("line {}", MAX_LOG_LINES * 3 - 1)));
    }

    #[test]
    fn a_stale_mount_is_cleared_before_partitioning() {
        // A second attempt after a failure must not trip over its own mounts.
        let backend = install(Firmware::Uefi);
        let first_umount = backend.position_of("umount").unwrap();
        let partition = backend.position_of("sfdisk").unwrap();
        assert!(first_umount < partition);
    }
}
