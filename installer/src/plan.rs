//! What an installation consists of.
//!
//! The plan is worked out before anything is touched, so the confirmation
//! screen can show exactly what is about to happen and the whole sequence can
//! be checked in a test.

use crate::disk::{format_bytes, Disk};

/// How the machine boots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Firmware {
    /// Booted through UEFI: the ESP is what matters.
    Uefi,
    /// Booted through a legacy BIOS: GRUB goes in a BIOS boot partition.
    Bios,
}

impl Firmware {
    /// Which one this machine booted with.
    ///
    /// The kernel only creates `/sys/firmware/efi` when it was started by UEFI
    /// firmware, which is the one reliable signal available here.
    pub fn detect(backend: &dyn crate::exec::Backend) -> Firmware {
        if backend.exists("/sys/firmware/efi") {
            Firmware::Uefi
        } else {
            Firmware::Bios
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Firmware::Uefi => "UEFI",
            Firmware::Bios => "BIOS",
        }
    }
}

/// Whether this session can make a disk bootable at all.
///
/// GRUB is in the Debian rootfs and nowhere else on the image, so the answer
/// is yes in a live session and no in a rescue one — the session that runs
/// when the squashfs will not mount. That is the session with the least to
/// spare, and the one that would otherwise find out at the eighth step, with
/// the disk already partitioned, formatted and written to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bootloader {
    /// `grub-install` is here, so the last step can run.
    Present,
    /// It is not, and so nothing started from this session can finish.
    Absent,
}

impl Bootloader {
    /// Where `grub-install` lives on a session that has one.
    ///
    /// `/usr/sbin` is Debian's and `/sbin` is the same file on a usr-merged
    /// root. `/sbin` is also where `iso/mkiso.sh` puts the tools it copies
    /// into the initramfs, so if GRUB is ever put back there this finds it
    /// without having to be remembered.
    const PATHS: [&'static str; 2] = ["/usr/sbin/grub-install", "/sbin/grub-install"];

    /// Which one this session is.
    ///
    /// Looked for by path rather than run, for the same reason `can_unpack`
    /// is: the only honest moment to ask is before anything has been written
    /// to the disk, and running grub-install to find out is exactly the thing
    /// being avoided.
    ///
    /// The paths move with `TOS_INSTALL_SYSROOT`, the way [`SysfsSource`]'s
    /// do. Without that the answer on a developer's machine would depend on
    /// whether that machine happens to have GRUB installed, and the session
    /// tests drive the real binary.
    ///
    /// [`SysfsSource`]: crate::disk::SysfsSource
    pub fn detect(backend: &dyn crate::exec::Backend) -> Bootloader {
        let root = std::env::var("TOS_INSTALL_SYSROOT").unwrap_or_default();
        let root = root.trim_end_matches('/');
        if Bootloader::PATHS
            .iter()
            .any(|path| backend.exists(&format!("{root}{path}")))
        {
            Bootloader::Present
        } else {
            Bootloader::Absent
        }
    }
}

/// The password for the account the installer creates.
///
/// It is a type of its own rather than a `String` for one reason: `Settings`
/// and `Plan` both derive `Debug`, and both get printed — by `--plan`, by a
/// failing test, by anything that ever decides to log what it is about to do.
/// Nothing outside the field it is typed into and the hash it becomes has any
/// business with the value, so this cannot be printed, only hashed.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Password(String);

impl std::fmt::Debug for Password {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_empty() {
            "Password(none)"
        } else {
            "Password(set)"
        })
    }
}

impl Password {
    /// The most characters the field accepts. SHA-512 crypt has no limit of
    /// its own; this one only stops a key held down from growing a value the
    /// box can no longer show.
    pub const MAX_CHARS: usize = 128;

    /// Whether no password was given at all. The account's `/etc/shadow`
    /// field is then `*`, nothing authenticates as it, and the lock refuses
    /// to engage for want of a password to check — which is the whole
    /// of what declining one means.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Append a typed character, unless the field is already full.
    pub fn push(&mut self, ch: char) {
        if self.0.chars().count() < Password::MAX_CHARS {
            self.0.push(ch);
        }
    }

    pub fn pop(&mut self) {
        self.0.pop();
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }

    /// What gets hashed.
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }

    /// What the masked field counts. The drawing code replaces every
    /// character of it with an asterisk before a cell is touched.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for Password {
    fn from(value: &str) -> Password {
        Password(value.to_string())
    }
}

/// What the user chose on the configuration screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub hostname: String,
    pub username: String,
    /// The password for that user, empty when they declined one.
    pub password: Password,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            hostname: "tos".to_string(),
            username: "tos".to_string(),
            password: Password::default(),
        }
    }
}

impl Settings {
    /// Why these settings cannot be used, if they cannot.
    pub fn problem(&self) -> Option<String> {
        if let Some(problem) = name_problem(&self.hostname, "Host name") {
            return Some(problem);
        }
        if let Some(problem) = name_problem(&self.username, "User name") {
            return Some(problem);
        }
        if let Some(taken) = DEBIAN_NAMES.iter().find(|name| **name == self.username) {
            return Some(format!("User name {taken} is Debian's already"));
        }
        None
    }

    pub fn is_valid(&self) -> bool {
        self.problem().is_none()
    }

    /// How the password reads on the last screen before the disk is erased.
    ///
    /// An empty one is allowed, so it has to be said out loud somewhere the
    /// user cannot walk past it: a machine nothing can log in to is a choice,
    /// and this is where it stops being a silent one.
    pub fn password_summary(&self) -> &'static str {
        if self.password.is_empty() {
            "none, so nothing will log in"
        } else {
            "set"
        }
    }
}

/// The names a Debian base system has already taken.
///
/// The installer adds its user by appending one line to the `/etc/passwd` and
/// one to the `/etc/group` the rootfs brought with it, which is right — `_apt`
/// and the rest are Debian's to keep — but it means a name can now collide,
/// where writing both files whole made that impossible. `getpwnam` and
/// `getgrnam` answer with the first line that matches, so a person who called
/// themselves `games` would be handed uid 5, a home of `/usr/games` and a
/// shell of `nologin`, with their password hash and their home directory
/// sitting beside it belonging to nobody.
///
/// The group names are here for the same reason and are the longer half of the
/// list: `disk`, `video` and `sudo` are nobody's user name but all three are
/// group names Debian ships, and a duplicate there gives the person a primary
/// gid of 1000 while `chgrp disk` moves a file to gid 6.
///
/// Refused at the screen where the name is typed, which is before the disk has
/// been touched. `root` is in the list rather than beside it, because it was
/// only ever the first name of this kind.
const DEBIAN_NAMES: [&str; 37] = [
    // /etc/passwd
    "root",
    "daemon",
    "bin",
    "sys",
    "sync",
    "games",
    "man",
    "lp",
    "mail",
    "news",
    "uucp",
    "proxy",
    "www-data",
    "backup",
    "list",
    "irc",
    "nobody",
    "systemd-network",
    "sshd",
    "messagebus",
    // /etc/group, which the same append writes to
    "disk",
    "tty",
    "dialout",
    "fax",
    "voice",
    "cdrom",
    "floppy",
    "tape",
    "audio",
    "video",
    "plugdev",
    "staff",
    "users",
    "nogroup",
    "sudo",
    "src",
    "shadow",
];

/// What a session with no GRUB in it has to say for itself.
///
/// This is the one thing the installer refuses over rather than degrades
/// through, and the difference is what is left behind. A rescue session with
/// no `unsquashfs` installs the busybox world instead of Debian and says so;
/// that disk is worse than Debian and it boots. A disk with no bootloader is
/// not a worse tOS, it is a disk somebody's files used to be on.
///
/// Kept to 64 columns so the confirmation screen can print it unwrapped.
const NO_BOOTLOADER: &[&str] = &[
    "This session cannot install a bootloader.",
    "GRUB is in the Debian rootfs, which is what this session could",
    "not mount. It could still erase this disk and copy a system onto",
    "it, and the machine would have nothing to start from afterwards.",
    "Boot the medium again, or write a new one, and install there.",
];

/// Names have to survive being written into `/etc/passwd` and a host file.
fn name_problem(name: &str, what: &str) -> Option<String> {
    if name.is_empty() {
        return Some(format!("{what} cannot be empty"));
    }
    if name.len() > 32 {
        return Some(format!("{what} is too long"));
    }
    if !name.chars().next().unwrap().is_ascii_lowercase() {
        return Some(format!("{what} must start with a lowercase letter"));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Some(format!(
            "{what} may only contain lowercase letters, digits and dashes"
        ));
    }
    None
}

/// Where `/init` mounts the medium the live session booted from.
pub const LIVE_MEDIUM_BOOT: &str = "/run/live/medium/boot";

/// The Debian rootfs on the medium, which is what an installed machine is
/// made of. `iso/mkiso.sh` puts it here and `iso/init` mounts it from here.
///
/// Unpacked from the medium rather than copied out of the running session:
/// the live root is this same image with a tmpfs overlay in front of it, so
/// copying it would carry across every file the session happened to write —
/// the resolver a DHCP lease left, a half-finished `apt install`, a password
/// somebody typed into a file. The image is the same bytes every time.
pub const LIVE_ROOTFS_IMAGE: &str = "/run/live/medium/live/filesystem.squashfs";

/// Where the target root is mounted while it is being installed to.
///
/// Named because more than the plan needs it: what `--plan` prints has to be
/// able to talk about files under the new root before there is a `Plan` to ask.
pub const MOUNT_POINT: &str = "/mnt/target";

/// Sizes of the partitions the installer creates.
pub const ESP_MIB: u64 = 512;
/// The BIOS boot partition GRUB embeds its core image into.
pub const BIOS_BOOT_MIB: u64 = 1;

/// One thing the installer does, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Write a fresh GPT with the partitions the plan calls for.
    Partition,
    /// Make the EFI system partition.
    FormatEsp,
    /// Make the root filesystem.
    FormatRoot,
    /// Mount root, and the ESP beneath it.
    Mount,
    /// Put the system onto the root filesystem: unpack the Debian rootfs, or
    /// copy the running one when the medium carries no rootfs image.
    CopySystem,
    /// Write `/etc` for the installed system.
    Configure,
    /// Install the bootloader.
    Bootloader,
    /// Flush and unmount, so the disk is consistent when the machine reboots.
    Finish,
}

impl Step {
    /// A short line for the progress list.
    pub fn label(self) -> &'static str {
        match self {
            Step::Partition => "Partition the disk",
            Step::FormatEsp => "Create the EFI system partition",
            Step::FormatRoot => "Create the root filesystem",
            Step::Mount => "Mount the new system",
            Step::CopySystem => "Unpack the system onto the disk",
            Step::Configure => "Write the system configuration",
            Step::Bootloader => "Install the bootloader",
            Step::Finish => "Flush and unmount",
        }
    }
}

/// Everything the installer needs to know before it starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub disk: Disk,
    pub firmware: Firmware,
    /// Whether the last step can be carried out, which decides whether any of
    /// the others are allowed to start. See [`Plan::refusal`].
    pub bootloader: Bootloader,
    pub settings: Settings,
    /// Where the new system is assembled.
    pub mount_point: String,
    /// What is copied onto the disk.
    pub source_root: String,
    /// Where the kernel and initramfs are, which on a live ISO is the medium
    /// rather than the running filesystem: an initramfs does not contain the
    /// kernel that loaded it.
    pub boot_source: String,
    /// The Debian rootfs image to unpack onto the disk. When it is not on the
    /// medium the installer falls back to copying the running system, which
    /// is what an image built before the rootfs existed leaves it with.
    pub rootfs_image: String,
}

impl Plan {
    pub fn new(disk: Disk, firmware: Firmware, bootloader: Bootloader, settings: Settings) -> Plan {
        Plan {
            disk,
            firmware,
            bootloader,
            settings,
            mount_point: MOUNT_POINT.to_string(),
            source_root: "/".to_string(),
            boot_source: LIVE_MEDIUM_BOOT.to_string(),
            rootfs_image: LIVE_ROOTFS_IMAGE.to_string(),
        }
    }

    /// The partition holding the bootloader. Partition 1 either way: an ESP
    /// under UEFI, a BIOS boot partition otherwise.
    pub fn boot_partition(&self) -> String {
        self.disk.partition_path(1)
    }

    pub fn root_partition(&self) -> String {
        self.disk.partition_path(2)
    }

    /// Where the ESP is mounted inside the new system.
    pub fn esp_mount(&self) -> String {
        format!("{}/boot/efi", self.mount_point)
    }

    /// The steps, in order. Formatting an ESP only happens under UEFI.
    pub fn steps(&self) -> Vec<Step> {
        let mut steps = vec![Step::Partition];
        if self.firmware == Firmware::Uefi {
            steps.push(Step::FormatEsp);
        }
        steps.extend([
            Step::FormatRoot,
            Step::Mount,
            Step::CopySystem,
            Step::Configure,
            Step::Bootloader,
            Step::Finish,
        ]);
        steps
    }

    /// The `sfdisk` script that lays the disk out.
    ///
    /// Sizes are left to sfdisk beyond the first partition, so the root
    /// filesystem takes whatever is left however large the disk is.
    pub fn partition_script(&self) -> String {
        let mut script = String::from("label: gpt\n");
        match self.firmware {
            Firmware::Uefi => {
                script.push_str(&format!("size={ESP_MIB}MiB, type=uefi, name=\"tOS ESP\"\n"));
            }
            Firmware::Bios => {
                script.push_str(&format!(
                    "size={BIOS_BOOT_MIB}MiB, type=21686148-6449-6E6F-744E-656564454649, \
                     name=\"BIOS boot\"\n"
                ));
            }
        }
        script.push_str("type=linux, name=\"tOS root\"\n");
        script
    }

    /// What the confirmation screen shows, one line per row.
    pub fn summary(&self) -> Vec<String> {
        let boot = match self.firmware {
            Firmware::Uefi => format!(
                "{}   {} MiB   EFI system partition",
                self.boot_partition(),
                ESP_MIB
            ),
            Firmware::Bios => format!(
                "{}   {} MiB   BIOS boot partition",
                self.boot_partition(),
                BIOS_BOOT_MIB
            ),
        };
        let overhead = match self.firmware {
            Firmware::Uefi => ESP_MIB,
            Firmware::Bios => BIOS_BOOT_MIB,
        } * 1024
            * 1024;
        vec![
            format!(
                "Disk       {}  ({})",
                self.disk.path,
                self.disk.size_label()
            ),
            format!("Firmware   {}", self.firmware.label()),
            format!("Host name  {}", self.settings.hostname),
            format!("User       {}", self.settings.username),
            format!("Password   {}", self.settings.password_summary()),
            String::new(),
            "New partition table (GPT), replacing everything on the disk:".to_string(),
            format!("  {boot}"),
            format!(
                "  {}   {}   root filesystem (ext4)",
                self.root_partition(),
                format_bytes(self.disk.bytes.saturating_sub(overhead))
            ),
        ]
    }

    /// Why this session must not erase a disk, if it must not.
    ///
    /// [`Disk::refusal`] asks this of the disk; this asks it of the session
    /// the disk is about to be erased from. It is asked in three places —
    /// `--plan`, the screen where the disk's name is typed, and the installer
    /// itself — so that the answer arrives while it still costs nothing, and
    /// so that nothing can reach `sfdisk` without having been past it.
    ///
    /// Several lines because one is not enough. "Cannot install a bootloader"
    /// sounds like a step that would be skipped; what it means is that the
    /// disk would be gone and the machine would not start.
    pub fn refusal(&self) -> Option<&'static [&'static str]> {
        match self.bootloader {
            Bootloader::Present => None,
            Bootloader::Absent => Some(NO_BOOTLOADER),
        }
    }

    /// The sentence a user has to type to confirm. Using the device name means
    /// a mistyped disk cannot be confirmed by muscle memory.
    pub fn confirmation_phrase(&self) -> String {
        self.disk.name.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::Recorder;

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

    #[test]
    fn a_name_debian_already_uses_is_refused_before_the_disk_is_touched() {
        // The installer appends its line to the passwd the rootfs brought,
        // so a name that is already in it produces a second entry that
        // getpwnam never returns: the person's home, their credential and
        // their uid all belong to an account nothing can reach. Writing the
        // whole file used to make that impossible.
        // Both halves: a user name Debian's passwd has, and a name that is
        // only ever a group — the same append writes to /etc/group too.
        for taken in [
            "root", "games", "www-data", "nobody", "disk", "sudo", "video",
        ] {
            let settings = Settings {
                username: taken.to_string(),
                ..Settings::default()
            };
            let problem = settings.problem();
            assert!(
                problem.is_some_and(|p| p.contains(taken)),
                "{taken} was accepted as a user name"
            );
        }
        // And an ordinary name still is one.
        let settings = Settings {
            username: "yusuke".into(),
            ..Settings::default()
        };
        assert_eq!(settings.problem(), None);
    }

    fn plan(firmware: Firmware) -> Plan {
        Plan::new(disk(), firmware, Bootloader::Present, Settings::default())
    }

    #[test]
    fn firmware_is_detected_from_the_kernel() {
        let uefi = Recorder::new().with_existing("/sys/firmware/efi");
        assert_eq!(Firmware::detect(&uefi), Firmware::Uefi);
        assert_eq!(Firmware::detect(&Recorder::new()), Firmware::Bios);
    }

    #[test]
    fn uefi_installs_format_an_esp() {
        let steps = plan(Firmware::Uefi).steps();
        assert!(steps.contains(&Step::FormatEsp));
        assert_eq!(steps[0], Step::Partition);
        assert_eq!(steps.last(), Some(&Step::Finish));
    }

    #[test]
    fn bios_installs_do_not_format_an_esp() {
        // A BIOS boot partition holds raw GRUB, and formatting it would
        // destroy what grub-install puts there.
        let steps = plan(Firmware::Bios).steps();
        assert!(!steps.contains(&Step::FormatEsp));
    }

    #[test]
    fn the_system_is_copied_before_it_is_configured() {
        let steps = plan(Firmware::Uefi).steps();
        let copy = steps.iter().position(|s| *s == Step::CopySystem).unwrap();
        let configure = steps.iter().position(|s| *s == Step::Configure).unwrap();
        let bootloader = steps.iter().position(|s| *s == Step::Bootloader).unwrap();
        assert!(copy < configure, "configuration would be overwritten");
        assert!(configure < bootloader);
    }

    #[test]
    fn the_partition_script_asks_for_gpt() {
        let script = plan(Firmware::Uefi).partition_script();
        assert!(script.starts_with("label: gpt\n"));
        assert!(script.contains("type=uefi"));
        assert!(script.contains("type=linux"));
        // Root has no size, so it takes the rest of the disk.
        let root = script.lines().last().unwrap();
        assert!(!root.contains("size="), "root should take the remainder");
    }

    #[test]
    fn the_bios_script_asks_for_a_bios_boot_partition() {
        let script = plan(Firmware::Bios).partition_script();
        // The GUID GRUB looks for when embedding its core image.
        assert!(script.contains("21686148-6449-6E6F-744E-656564454649"));
        assert!(!script.contains("type=uefi"));
    }

    #[test]
    fn partitions_are_named_after_the_disk() {
        let plan = plan(Firmware::Uefi);
        assert_eq!(plan.boot_partition(), "/dev/sda1");
        assert_eq!(plan.root_partition(), "/dev/sda2");

        let nvme = Plan::new(
            Disk {
                name: "nvme0n1".into(),
                path: "/dev/nvme0n1".into(),
                ..disk()
            },
            Firmware::Uefi,
            Bootloader::Present,
            Settings::default(),
        );
        assert_eq!(nvme.root_partition(), "/dev/nvme0n1p2");
    }

    #[test]
    fn the_summary_says_what_will_be_destroyed() {
        let summary = plan(Firmware::Uefi).summary().join("\n");
        assert!(summary.contains("/dev/sda"));
        assert!(summary.contains("64 GiB"));
        assert!(summary.contains("UEFI"));
        assert!(
            summary.contains("replacing everything on the disk"),
            "the user has to be told: {summary}"
        );
    }

    #[test]
    fn a_session_with_no_grub_refuses_the_whole_installation() {
        assert_eq!(plan(Firmware::Uefi).refusal(), None);

        let refused = Plan::new(
            disk(),
            Firmware::Uefi,
            Bootloader::Absent,
            Settings::default(),
        );
        let reason = refused.refusal().expect("it offered to install");
        let text = reason.join(" ");
        assert!(text.contains("cannot install a bootloader"), "{text}");
        // And says what that means for the disk. On its own, "no bootloader"
        // reads like a step that would be skipped.
        assert!(text.contains("erase this disk"), "{text}");
        // The confirmation screen prints these unwrapped, inside a frame 70
        // columns wide with two columns of inset on each side.
        for line in reason {
            assert!(line.chars().count() <= 64, "too wide to print: {line}");
        }
    }

    #[test]
    fn the_confirmation_phrase_is_the_disk_name() {
        // Typing "yes" is something a person does without reading.
        assert_eq!(plan(Firmware::Uefi).confirmation_phrase(), "sda");
    }

    #[test]
    fn default_settings_are_valid() {
        assert!(Settings::default().is_valid());
    }

    #[test]
    fn names_are_checked_before_they_reach_etc() {
        let bad = |hostname: &str, username: &str| {
            Settings {
                hostname: hostname.into(),
                username: username.into(),
                ..Settings::default()
            }
            .problem()
        };
        assert!(bad("", "tos").is_some());
        assert!(bad("tos", "").is_some());
        assert!(bad("TOS", "tos").is_some(), "uppercase host name");
        assert!(bad("1tos", "tos").is_some(), "leading digit");
        assert!(bad("tos machine", "tos").is_some(), "space");
        assert!(bad("tos", "root").is_some(), "root is taken");
        assert!(bad("tos", "tos:x:0").is_some(), "passwd field separator");
        assert!(bad(&"a".repeat(40), "tos").is_some(), "too long");

        assert!(bad("tos-laptop", "yusuke").is_none());
    }

    #[test]
    fn the_error_says_which_field_is_wrong() {
        let settings = Settings {
            hostname: "tos".into(),
            username: "Root".into(),
            ..Settings::default()
        };
        assert!(settings.problem().unwrap().starts_with("User name"));
    }

    #[test]
    fn a_password_never_prints_itself() {
        // Everything that prints a plan prints this, so the one thing it must
        // not say is what was typed.
        let settings = Settings {
            password: "hunter2".into(),
            ..Settings::default()
        };
        let printed = format!(
            "{:?}",
            Plan::new(disk(), Firmware::Uefi, Bootloader::Present, settings)
        );
        assert!(!printed.contains("hunter2"), "{printed}");
        assert!(printed.contains("Password(set)"), "{printed}");
        assert!(format!("{:?}", Settings::default()).contains("Password(none)"));
    }

    #[test]
    fn the_summary_says_whether_there_is_a_password() {
        let without = plan(Firmware::Uefi).summary().join("\n");
        assert!(
            without.contains("Password   none, so nothing will log in"),
            "declining a password has to be visible: {without}"
        );

        let settings = Settings {
            password: "hunter2".into(),
            ..Settings::default()
        };
        let with = Plan::new(disk(), Firmware::Uefi, Bootloader::Present, settings)
            .summary()
            .join("\n");
        assert!(with.contains("Password   set"));
        assert!(!with.contains("hunter2"), "the summary echoed it: {with}");
    }

    #[test]
    fn a_password_field_fills_up_rather_than_growing() {
        let mut password = Password::default();
        assert!(password.is_empty());
        for _ in 0..Password::MAX_CHARS + 10 {
            password.push('a');
        }
        assert_eq!(password.as_bytes().len(), Password::MAX_CHARS);
        password.pop();
        assert_eq!(password.as_bytes().len(), Password::MAX_CHARS - 1);
        password.clear();
        assert!(password.is_empty());
    }
}
