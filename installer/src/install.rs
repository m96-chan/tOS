//! Carrying out the plan.
//!
//! Each step is one method, run through a [`Backend`], so the whole sequence
//! can be driven against a recorder in a test. The installer stops at the
//! first failure and reports it: a half-installed disk is easier to reason
//! about than one that carried on after `mkfs` failed.

use crate::exec::{Backend, Output};
use crate::plan::{Firmware, Plan, Step};

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
        // The live system is an initramfs, so what is on disk is what runs.
        // Everything the kernel provides is left out: those are mount points
        // in the installed system, not files to copy.
        for directory in ["dev", "proc", "sys", "run", "tmp", "mnt", "var/log"] {
            let path = format!("{}/{directory}", self.plan.mount_point);
            self.backend
                .create_dir(&path)
                .map_err(|e| format!("cannot create {path}: {e}"))?;
        }

        for directory in COPIED_DIRECTORIES {
            let from = format!(
                "{}{directory}",
                self.plan.source_root.trim_end_matches('/')
            );
            if !self.backend.exists(&from) {
                continue;
            }
            let to = format!("{}{directory}", self.plan.mount_point);
            self.progress.note(format!("   copying {directory}"));
            self.backend
                .copy_tree(&from, &to)
                .map_err(|e| format!("cannot copy {from}: {e}"))?;
        }

        self.copy_boot_files()
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

        // A minimal passwd and group, so the installed system has the user the
        // installer was told about. Login is not gated yet: the console starts
        // the compositor directly, exactly as the live image does.
        self.write(
            &format!("{root}/etc/passwd"),
            &format!(
                "root:x:0:0:root:/root:/bin/sh\n{user}:x:1000:1000:{user}:/home/{user}:/bin/sh\n",
                user = settings.username
            ),
        )?;
        self.write(
            &format!("{root}/etc/group"),
            &format!(
                "root:x:0:\n{user}:x:1000:\n",
                user = settings.username
            ),
        )?;
        let home = format!("{root}/home/{}", settings.username);
        self.backend
            .create_dir(&home)
            .map_err(|e| format!("cannot create {home}: {e}"))?;

        let fstab = self.fstab();
        self.write(&format!("{root}/etc/fstab"), &fstab)?;

        // The console starts tOS, which is the whole point of the machine.
        self.write(
            &format!("{root}/etc/inittab"),
            "::sysinit:/etc/rc\n::respawn:/sbin/tos\n::ctrlaltdel:/sbin/reboot\n",
        )?;
        self.write(
            &format!("{root}/etc/rc"),
            RC_SCRIPT,
        )?;
        let rc = format!("{root}/etc/rc");
        let _ = self.backend.run("chmod", &["755", &rc]);
        Ok(())
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
             \tlinux /boot/vmlinuz root=LABEL=tos-root rw console=tty0 quiet\n\
             \tinitrd /boot/initramfs.gz\n\
             }}\n\
             \n\
             menuentry \"tOS (verbose)\" {{\n\
             \tsearch --no-floppy --label --set=root tos-root\n\
             \tlinux /boot/vmlinuz root=LABEL=tos-root rw console=tty0\n\
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

/// What is copied out of the live system onto the disk.
///
/// The live image is an initramfs, so this is the whole of it: the compositor,
/// busybox and the kernel modules. `/boot` is not among them, because it is
/// on the medium rather than in the initramfs.
pub const COPIED_DIRECTORIES: &[&str] = &["/bin", "/sbin", "/lib", "/etc", "/root"];

/// The files GRUB loads, taken from the live medium.
pub const BOOT_FILES: &[&str] = &["vmlinuz", "initramfs.gz"];

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
    backend
}

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
        assert!(root < esp, "the ESP mount would be hidden by the root mount");
    }

    #[test]
    fn the_system_is_copied_out_of_the_live_image() {
        let backend = install(Firmware::Uefi);
        for directory in COPIED_DIRECTORIES {
            assert!(
                backend.did(&format!("copy {directory} -> /mnt/target{directory}")),
                "{directory} was not copied"
            );
        }
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
        // A live image without /root should still install.
        let mut backend = live_backend();
        backend.existing.retain(|path| path != "/root");
        let mut installer = Installer::new(plan(Firmware::Uefi), &mut backend);
        let progress = installer.run();
        assert!(progress.failure().is_none());
        assert!(backend.did("copy /bin -> /mnt/target/bin"));
        assert!(!backend.did("copy /root"));
    }

    #[test]
    fn the_configuration_names_the_machine_and_the_user() {
        let mut backend = live_backend();
        let settings = Settings {
            hostname: "workshop".into(),
            username: "yusuke".into(),
        };
        let mut installer =
            Installer::new(Plan::new(disk(), Firmware::Uefi, settings), &mut backend);
        installer.run();

        let written = |path: &str| {
            backend
                .actions
                .iter()
                .find_map(|action| match action {
                    crate::exec::Action::WriteFile { path: p, contents } if p == path => {
                        Some(contents.clone())
                    }
                    _ => None,
                })
                .unwrap_or_else(|| panic!("{path} was never written"))
        };

        assert_eq!(written("/mnt/target/etc/hostname"), "workshop\n");
        assert!(written("/mnt/target/etc/hosts").contains("workshop"));
        let passwd = written("/mnt/target/etc/passwd");
        assert!(passwd.contains("yusuke:x:1000:1000"));
        assert!(passwd.starts_with("root:x:0:0"));
        assert!(written("/mnt/target/etc/group").contains("yusuke:x:1000:"));
        assert!(backend.did("mkdir -p /mnt/target/home/yusuke"));
    }

    #[test]
    fn the_installed_system_starts_the_compositor() {
        let backend = install(Firmware::Uefi);
        let inittab = backend
            .actions
            .iter()
            .find_map(|action| match action {
                crate::exec::Action::WriteFile { path, contents }
                    if path.ends_with("/etc/inittab") =>
                {
                    Some(contents.clone())
                }
                _ => None,
            })
            .expect("no inittab");
        assert!(
            inittab.contains("/sbin/tos"),
            "the machine has to boot into tOS: {inittab}"
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
                crate::exec::Action::WriteFile { path, contents }
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
                crate::exec::Action::WriteFile { path, contents }
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
                crate::exec::Action::WriteFile { path, contents }
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
        assert!(progress.log.last().unwrap().contains(&format!(
            "line {}",
            MAX_LOG_LINES * 3 - 1
        )));
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
