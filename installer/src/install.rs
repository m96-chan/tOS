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

/// The credential the screen lock reads: one `$6$` line, and deliberately not
/// `/etc/shadow`. Writing a tOS password into Debian's file would silently
/// make it the machine's login password too, and reading Debian's file back
/// would mean verifying the yescrypt hashes it holds. The reasoning is in
/// `docs/design/screen-lock.md`.
pub const CREDENTIAL_FILE: &str = "/etc/tos/shadow";

/// Root reads it, nobody else. A hash anyone can read is a hash anyone can
/// attack offline, at their leisure, on a machine they took.
pub const CREDENTIAL_MODE: u32 = 0o600;

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

        let outcome = match self.dispatch(step) {
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

        if self.backend.exists(&self.plan.rootfs_image) {
            self.unpack_rootfs()?;
        } else {
            self.copy_live_system()?;
        }

        self.copy_boot_files()
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
        self.write_credential(&root, &settings)?;
        let home = format!("{root}/home/{}", settings.username);
        self.backend
            .create_dir(&home)
            .map_err(|e| format!("cannot create {home}: {e}"))?;

        let fstab = self.fstab();
        self.write(&format!("{root}/etc/fstab"), &fstab)?;

        // The console starts tOS, which is the whole point of the machine.
        self.write(
            &format!("{root}/etc/inittab"),
            &format!(
                "::sysinit:/etc/rc\n::respawn:{SESSION_SCRIPT_PATH}\n::ctrlaltdel:/sbin/reboot\n"
            ),
        )?;
        self.write(&format!("{root}/etc/rc"), RC_SCRIPT)?;
        let rc = format!("{root}/etc/rc");
        let _ = self.backend.run("chmod", &["755", &rc]);
        let session = format!("{root}{SESSION_SCRIPT_PATH}");
        self.write(&session, SESSION_SCRIPT)?;
        let _ = self.backend.run("chmod", &["755", &session]);

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
    /// The password field is `*` either way, not `x`. `x` means "the hash is
    /// in /etc/shadow", and tOS sets no shadow entry for this account — its
    /// credential is /etc/tos/shadow, which no login program reads. Promising
    /// a hash that is not there is how this line came to be a lie about an
    /// account with no credential at all. `*` says what is true, that nothing
    /// logs in through this file, and it stays true the day a PAM arrives on
    /// the disk. Nothing gates the console in either case: it starts the
    /// compositor directly, exactly as the live image does.
    fn accounts(&mut self, root: &str, settings: &Settings) -> Result<(), String> {
        let user = &settings.username;
        let passwd = format!("{root}/etc/passwd");
        let group = format!("{root}/etc/group");
        let account = format!("{user}:*:1000:1000:{user}:/home/{user}:/bin/sh\n");
        let membership = format!("{user}:x:1000:\n");

        if self.backend.exists(&passwd) {
            self.append(&passwd, &account)?;
            return self.append(&group, &membership);
        }

        self.write(
            &passwd,
            &format!("root:*:0:0:root:/root:/bin/sh\n{account}"),
        )?;
        self.write(&group, &format!("root:x:0:\n{membership}"))
    }

    /// Write the credential the tOS lock unlocks with, if there is one.
    ///
    /// An empty password is allowed, and it produces no file rather than a
    /// hash of nothing. The empty string hashes perfectly well, and a lock
    /// that engaged and then opened for a bare Enter would be worse than no
    /// lock at all; `docs/design/screen-lock.md` makes the missing file mean
    /// "there is nothing to unlock with", which is exactly what is true of a
    /// machine whose owner declined a password.
    fn write_credential(&mut self, root: &str, settings: &Settings) -> Result<(), String> {
        if settings.password.is_empty() {
            self.progress
                .note("   no password given: the screen lock will stay off");
            return Ok(());
        }

        let directory = format!("{root}{CREDENTIAL_DIRECTORY}");
        self.backend
            .create_dir(&directory)
            .map_err(|e| format!("cannot create {directory}: {e}"))?;

        // Hashing is the installer's last chance to hold the password; after
        // this only the hash exists anywhere on the disk.
        let hash = tos_crypt::hash_password(settings.password.as_bytes())
            .map_err(|e| format!("cannot hash the password: {e}"))?;

        // One `$6$` line and a terminator, which is what a reader trims
        // before it parses. Nothing names the user: this file is the
        // session's credential, not a user database, and the machine has
        // exactly one person at the keyboard.
        let path = format!("{root}{CREDENTIAL_FILE}");
        self.progress
            .note(format!("   write {path} (mode {CREDENTIAL_MODE:04o})"));
        self.backend
            .write_file_with_mode(&path, &format!("{hash}\n"), Some(CREDENTIAL_MODE))
            .map_err(|e| format!("cannot write {path}: {e}"))
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
        fstab.push_str(
            "proc            /proc      proc  defaults           0 0\n\
             sysfs           /sys       sysfs defaults           0 0\n\
             devpts          /dev/pts   devpts gid=5,mode=620    0 0\n",
        );
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
    fn grub_config(&self) -> String {
        format!(
            "set timeout=2\n\
             set default=0\n\
             \n\
             menuentry \"tOS\" {{\n\
             \tsearch --no-floppy --label --set=root tos-root\n\
             \tlinux /boot/vmlinuz {CMDLINE} quiet\n\
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
/// `/usr` is here for what little is under it, and `/usr/lib/grub` with it,
/// which is what an installed machine would need to put its bootloader back.
/// The font and the dictionary are not in the initramfs any more — they are
/// in the rootfs, which is the path this one is not — so a machine installed
/// this way draws Latin from the compositor's built-in ASCII face and cannot
/// type Japanese.
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

/// A recorder primed to look like a live session with its medium mounted.
///
/// This is what a dry run and `--plan` walk, so that what they print is the
/// whole sequence rather than the prefix that runs before the first missing
/// path stops it.
pub fn planning_backend() -> crate::exec::Recorder {
    let mut backend = crate::exec::Recorder::new();
    for directory in COPIED_DIRECTORIES {
        backend.existing.push(directory.to_string());
    }
    for file in BOOT_FILES {
        backend
            .existing
            .push(format!("{}/{file}", crate::plan::LIVE_MEDIUM_BOOT));
    }
    // The rootfs image too, so that what `--plan` prints is the installation
    // that is going to happen rather than the fallback nobody will take.
    backend
        .existing
        .push(crate::plan::LIVE_ROOTFS_IMAGE.to_string());
    backend
}

/// Where the session's environment is written, and what /etc/inittab respawns.
pub const SESSION_SCRIPT_PATH: &str = "/etc/tos-session";

/// The environment the session runs in, and then the compositor.
///
/// busybox init hands a program it respawns almost nothing, and none of what a
/// tOS session needs: `ENV`, which is how each pane's shell comes to read
/// `/etc/profile` and print the message of the day, nor `HOME`, nor `SHELL`.
/// The live image exports these in `/init` and nothing carries an environment
/// across `switch_root`, so an installed machine writes them down here
/// instead. `iso/init` is the live counterpart and the two have to agree.
const SESSION_SCRIPT: &str = "#!/bin/sh\n\
                              # Written by the tOS installer.\n\
                              export HOME=/root\n\
                              export SHELL=/bin/sh\n\
                              export TOS=1\n\
                              export ENV=/etc/profile\n\
                              exec /sbin/tos\n";

/// The installed system's startup script.
const RC_SCRIPT: &str = "#!/bin/sh\n\
                         # Written by the tOS installer.\n\
                         mount -t proc none /proc 2>/dev/null\n\
                         mount -t sysfs none /sys 2>/dev/null\n\
                         mount -t devtmpfs none /dev 2>/dev/null\n\
                         mkdir -p /dev/pts /dev/shm\n\
                         mount -t devpts none /dev/pts 2>/dev/null\n\
                         mount -t tmpfs none /dev/shm 2>/dev/null\n\
                         mount -t tmpfs none /tmp 2>/dev/null\n\
                         mount -t tmpfs none /run 2>/dev/null\n\
                         mount -o remount,rw / 2>/dev/null\n\
                         hostname -F /etc/hostname 2>/dev/null\n";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::Disk;
    use crate::exec::Recorder;
    use crate::plan::Settings;

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
        Plan::new(disk(), firmware, Settings::default())
    }

    /// Run a whole installation against a recorder.
    fn install(firmware: Firmware) -> Recorder {
        let mut backend = planning_backend();
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
    fn live_backend() -> Recorder {
        planning_backend()
    }

    /// The same, on a medium that carries no Debian rootfs: an image built
    /// before the rootfs step existed, which the installer still has to be
    /// able to install from.
    fn rootfsless_backend() -> Recorder {
        let mut backend = planning_backend();
        backend
            .existing
            .retain(|path| path != crate::plan::LIVE_ROOTFS_IMAGE);
        backend
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
    fn the_configuration_names_the_machine_and_the_user() {
        let mut backend = live_backend();
        let settings = Settings {
            hostname: "workshop".into(),
            username: "yusuke".into(),
            ..Settings::default()
        };
        let mut installer =
            Installer::new(Plan::new(disk(), Firmware::Uefi, settings), &mut backend);
        installer.run();

        assert_eq!(written(&backend, "/mnt/target/etc/hostname"), "workshop\n");
        assert!(written(&backend, "/mnt/target/etc/hosts").contains("workshop"));
        let passwd = written(&backend, "/mnt/target/etc/passwd");
        assert!(passwd.contains("yusuke:*:1000:1000"));
        assert!(passwd.starts_with("root:*:0:0"));
        assert!(written(&backend, "/mnt/target/etc/group").contains("yusuke:x:1000:"));
        assert!(backend.did("mkdir -p /mnt/target/home/yusuke"));
    }

    #[test]
    fn the_passwd_file_does_not_promise_a_shadow_file() {
        // `x` means "the hash is in /etc/shadow", and there is no /etc/shadow:
        // the tOS credential is /etc/tos/shadow, which nothing that reads
        // passwd knows about. `*` is the true statement.
        let backend = install(Firmware::Uefi);
        let passwd = written(&backend, "/mnt/target/etc/passwd");
        assert!(
            !passwd.contains(":x:"),
            "still promising a shadow: {passwd}"
        );
        assert!(passwd.contains("root:*:0:0"), "{passwd}");
        assert!(passwd.contains("tos:*:1000:1000"), "{passwd}");
        assert!(!wrote(&backend, "/mnt/target/etc/shadow"));
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
        assert!(added.contains("tos:*:1000:1000"), "{added}");
        assert!(
            !added.contains("root:"),
            "root is Debian's line to write, not ours: {added}"
        );
    }

    #[test]
    fn a_password_is_hashed_into_the_file_the_lock_reads() {
        let mut backend = live_backend();
        let settings = Settings {
            username: "yusuke".into(),
            password: "correct horse battery staple".into(),
            ..Settings::default()
        };
        let mut installer =
            Installer::new(Plan::new(disk(), Firmware::Uefi, settings), &mut backend);
        let progress = installer.run();
        assert!(progress.failure().is_none());

        let (contents, mode) = file(&backend, "/mnt/target/etc/tos/shadow");
        assert_eq!(mode, Some(0o600), "the hash must not be readable: {mode:?}");
        assert!(backend.did("mkdir -p /mnt/target/etc/tos"));

        // One line, and a terminator a reader trims before it parses.
        assert_eq!(contents.lines().count(), 1);
        assert!(contents.ends_with('\n'));
        let hash = contents.trim_end();
        assert!(hash.starts_with("$6$"), "not a SHA-512 crypt line: {hash}");

        // The point of the whole issue: what was typed opens this.
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
        let mut installer =
            Installer::new(Plan::new(disk(), Firmware::Uefi, settings), &mut backend);
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
                Installer::new(Plan::new(disk(), Firmware::Uefi, settings), &mut backend).run();
                written(&backend, "/mnt/target/etc/tos/shadow")
            })
            .collect();
        assert_ne!(hashes[0], hashes[1]);
    }

    #[test]
    fn no_password_means_no_credential_rather_than_a_hash_of_nothing() {
        // An empty password hashes perfectly well, and a lock that opened for
        // a bare Enter would be worse than one that refuses to engage.
        let backend = install(Firmware::Uefi);
        assert!(
            !wrote(&backend, "/mnt/target/etc/tos/shadow"),
            "an empty password must leave no credential: {:?}",
            backend.transcript()
        );
    }

    #[test]
    fn the_installed_system_starts_the_compositor() {
        let backend = install(Firmware::Uefi);
        let inittab = backend
            .actions
            .iter()
            .find_map(|action| match action {
                crate::exec::Action::WriteFile { path, contents, .. }
                    if path.ends_with("/etc/inittab") =>
                {
                    Some(contents.clone())
                }
                _ => None,
            })
            .expect("no inittab");
        assert!(
            inittab.contains(SESSION_SCRIPT_PATH),
            "the machine has to boot into tOS: {inittab}"
        );
        let session = written(&backend, &format!("/mnt/target{SESSION_SCRIPT_PATH}"));
        assert!(
            session.contains("exec /sbin/tos"),
            "the session script has to end at the compositor: {session}"
        );
    }

    #[test]
    fn the_session_carries_the_environment_init_does_not() {
        // busybox init respawns with almost nothing set. Without ENV no pane's
        // shell reads /etc/profile, which is where the message of the day
        // comes from; /init exports the same set on the live image.
        let backend = install(Firmware::Bios);
        let session = written(&backend, &format!("/mnt/target{SESSION_SCRIPT_PATH}"));
        for variable in ["HOME=/root", "SHELL=/bin/sh", "TOS=1", "ENV=/etc/profile"] {
            assert!(
                session.contains(variable),
                "the session should export {variable}: {session}"
            );
        }
    }

    #[test]
    fn the_font_comes_along_so_an_installed_machine_can_draw_japanese() {
        // Every path tos-font searches is under /usr/share/fonts, and the
        // face is now a package inside the rootfs rather than a file copied
        // past dpkg. Without one the machine falls back to the ASCII face and
        // every kana is a box. The fallback still copies /usr for its own
        // sake; what it copies has no font in it, which iso/mkiso.sh says.
        let backend = install(Firmware::Bios);
        assert!(backend.did("unsquashfs"));
        let mut backend = rootfsless_backend();
        install_with(Firmware::Bios, &mut backend);
        assert!(backend.did("copy /usr -> /mnt/target/usr"));
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
