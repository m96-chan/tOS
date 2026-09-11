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

/// What the user chose on the configuration screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub hostname: String,
    pub username: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            hostname: "tos".to_string(),
            username: "tos".to_string(),
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
        if self.username == "root" {
            return Some("User name cannot be root".to_string());
        }
        None
    }

    pub fn is_valid(&self) -> bool {
        self.problem().is_none()
    }
}

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
    /// Put the system onto the root filesystem.
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
            Step::CopySystem => "Copy tOS onto the disk",
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
    pub settings: Settings,
    /// Where the new system is assembled.
    pub mount_point: String,
    /// What is copied onto the disk.
    pub source_root: String,
    /// Where the kernel and initramfs are, which on a live ISO is the medium
    /// rather than the running filesystem: an initramfs does not contain the
    /// kernel that loaded it.
    pub boot_source: String,
}

impl Plan {
    pub fn new(disk: Disk, firmware: Firmware, settings: Settings) -> Plan {
        Plan {
            disk,
            firmware,
            settings,
            mount_point: "/mnt/target".to_string(),
            source_root: "/".to_string(),
            boot_source: LIVE_MEDIUM_BOOT.to_string(),
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

    fn plan(firmware: Firmware) -> Plan {
        Plan::new(disk(), firmware, Settings::default())
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
        };
        assert!(settings.problem().unwrap().starts_with("User name"));
    }
}
