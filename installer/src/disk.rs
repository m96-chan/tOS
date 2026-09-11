//! Finding the disks tOS could be installed onto.
//!
//! Enumeration reads `/sys/block` directly rather than shelling out to
//! `lsblk`, which is not in the live image. Everything here is pure parsing
//! over a [`DiskSource`], so the awkward cases — the disk the live system
//! booted from, read-only devices, loop and RAM devices — can be tested
//! without any hardware.

use std::collections::BTreeMap;

/// Somewhere disk information can be read from.
///
/// The real implementation reads `/sys` and `/proc`; tests supply a map.
pub trait DiskSource {
    /// Names under `/sys/block`, such as `sda` or `nvme0n1`.
    fn block_devices(&self) -> Vec<String>;
    /// Contents of a file under a device's `/sys/block/<name>/` directory.
    fn attribute(&self, device: &str, path: &str) -> Option<String>;
    /// The contents of `/proc/mounts`.
    fn mounts(&self) -> String;
}

/// Reads `/sys` and `/proc`.
///
/// The root is configurable so the installer can be driven against a
/// prepared tree; `TOS_INSTALL_SYSROOT` is what the integration tests point
/// at, and it is the only way to exercise the real binary on a machine that
/// is not the target.
pub struct SysfsSource {
    root: std::path::PathBuf,
}

impl SysfsSource {
    /// Read the running machine's own `/sys` and `/proc`, unless
    /// `TOS_INSTALL_SYSROOT` says otherwise.
    pub fn new() -> SysfsSource {
        let root = std::env::var_os("TOS_INSTALL_SYSROOT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("/"));
        SysfsSource { root }
    }

    /// Read a specific tree.
    pub fn rooted(root: impl Into<std::path::PathBuf>) -> SysfsSource {
        SysfsSource { root: root.into() }
    }
}

impl Default for SysfsSource {
    fn default() -> Self {
        SysfsSource::new()
    }
}

impl DiskSource for SysfsSource {
    fn block_devices(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(self.root.join("sys/block")) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn attribute(&self, device: &str, path: &str) -> Option<String> {
        let full = self.root.join("sys/block").join(device).join(path);
        std::fs::read_to_string(full).ok()
    }

    fn mounts(&self) -> String {
        std::fs::read_to_string(self.root.join("proc/mounts")).unwrap_or_default()
    }
}

/// A candidate installation target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Disk {
    /// Kernel name, such as `sda`.
    pub name: String,
    /// Device node, such as `/dev/sda`.
    pub path: String,
    /// Capacity in bytes.
    pub bytes: u64,
    /// Model string reported by the device, when it has one.
    pub model: String,
    /// Whether the kernel says the device is removable.
    pub removable: bool,
    /// Whether the device is read only.
    pub read_only: bool,
    /// A partition of this disk is mounted, so it is in use right now.
    pub in_use: bool,
    /// The live system booted from this device.
    pub is_boot_medium: bool,
}

impl Disk {
    /// Capacity in a form a person can read.
    pub fn size_label(&self) -> String {
        format_bytes(self.bytes)
    }

    /// A one line description for the picker.
    pub fn summary(&self) -> String {
        let model = if self.model.is_empty() {
            "unknown model".to_string()
        } else {
            self.model.clone()
        };
        format!("{}  {}  {}", self.path, self.size_label(), model)
    }

    /// Why this disk cannot be installed onto, if it cannot.
    pub fn refusal(&self) -> Option<&'static str> {
        if self.read_only {
            return Some("read only");
        }
        if self.is_boot_medium {
            return Some("this is the medium tOS booted from");
        }
        if self.in_use {
            return Some("a partition is mounted");
        }
        if self.bytes < MINIMUM_BYTES {
            return Some("too small");
        }
        None
    }

    pub fn is_installable(&self) -> bool {
        self.refusal().is_none()
    }

    /// The device node of partition `index`, 1 based.
    ///
    /// NVMe and MMC devices put a `p` between the disk and the partition
    /// number; SCSI and IDE devices do not.
    pub fn partition_path(&self, index: u32) -> String {
        let needs_separator = self
            .name
            .chars()
            .last()
            .map(|c| c.is_ascii_digit())
            .unwrap_or(false);
        if needs_separator {
            format!("{}p{index}", self.path)
        } else {
            format!("{}{index}", self.path)
        }
    }
}

/// Anything smaller than this cannot hold a usable system.
pub const MINIMUM_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Sector size assumed when converting `/sys/block/<dev>/size`.
const SECTOR_BYTES: u64 = 512;

/// Device name prefixes that are never installation targets.
const EXCLUDED_PREFIXES: &[&str] = &[
    "loop", "ram", "zram", "sr", "fd", "dm-", "md", "nbd", "zd",
];

/// List the disks on this machine, in the order they should be offered.
pub fn enumerate(source: &dyn DiskSource) -> Vec<Disk> {
    let mounts = parse_mounts(&source.mounts());
    let mut disks: Vec<Disk> = source
        .block_devices()
        .into_iter()
        .filter(|name| !is_excluded(name))
        .filter_map(|name| read_disk(source, &name, &mounts))
        .collect();

    // Installable disks first, then by size: the most likely target is the
    // one the cursor starts on.
    disks.sort_by(|a, b| {
        b.is_installable()
            .cmp(&a.is_installable())
            .then(b.bytes.cmp(&a.bytes))
            .then(a.name.cmp(&b.name))
    });
    disks
}

fn is_excluded(name: &str) -> bool {
    EXCLUDED_PREFIXES
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

fn read_disk(
    source: &dyn DiskSource,
    name: &str,
    mounts: &BTreeMap<String, String>,
) -> Option<Disk> {
    // A device with no size is not a disk tOS can use.
    let sectors: u64 = source.attribute(name, "size")?.trim().parse().ok()?;
    if sectors == 0 {
        return None;
    }

    let flag = |path: &str| {
        source
            .attribute(name, path)
            .map(|value| value.trim() == "1")
            .unwrap_or(false)
    };
    let model = source
        .attribute(name, "device/model")
        .map(|value| value.trim().to_string())
        .unwrap_or_default();

    let path = format!("/dev/{name}");
    // A mount of the disk itself or of any of its partitions counts as busy.
    let in_use = mounts
        .keys()
        .any(|device| device == &path || is_partition_of(device, &path));
    let is_boot_medium = mounts.iter().any(|(device, mount)| {
        (device == &path || is_partition_of(device, &path))
            && (mount == "/" || mount == "/run/live/medium" || mount == "/cdrom")
    });

    Some(Disk {
        name: name.to_string(),
        path,
        bytes: sectors * SECTOR_BYTES,
        model,
        removable: flag("removable"),
        read_only: flag("ro"),
        in_use,
        is_boot_medium,
    })
}

/// Whether `device` is a partition of `disk`, by name.
fn is_partition_of(device: &str, disk: &str) -> bool {
    let Some(suffix) = device.strip_prefix(disk) else {
        return false;
    };
    let suffix = suffix.strip_prefix('p').unwrap_or(suffix);
    !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit())
}

/// Device to mount point, from `/proc/mounts`.
fn parse_mounts(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let (Some(device), Some(mount)) = (fields.next(), fields.next()) else {
            continue;
        };
        if !device.starts_with("/dev/") {
            continue;
        }
        out.insert(device.to_string(), unescape_mount(mount));
    }
    out
}

/// `/proc/mounts` escapes spaces and a few other characters in octal.
fn unescape_mount(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let digits: String = chars.clone().take(3).collect();
        match u32::from_str_radix(&digits, 8).ok().and_then(char::from_u32) {
            Some(decoded) if digits.len() == 3 => {
                out.push(decoded);
                for _ in 0..3 {
                    chars.next();
                }
            }
            _ => out.push('\\'),
        }
    }
    out
}

/// Bytes as a short human readable string.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [(&str, u64); 4] = [
        ("TiB", 1 << 40),
        ("GiB", 1 << 30),
        ("MiB", 1 << 20),
        ("KiB", 1 << 10),
    ];
    for (unit, scale) in UNITS {
        if bytes >= scale {
            let whole = bytes / scale;
            let tenths = (bytes % scale) * 10 / scale;
            return if whole >= 100 || tenths == 0 {
                format!("{whole} {unit}")
            } else {
                format!("{whole}.{tenths} {unit}")
            };
        }
    }
    format!("{bytes} B")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A disk source backed by a table, for tests.
    #[derive(Default)]
    pub struct FakeDisks {
        pub devices: Vec<String>,
        pub attributes: BTreeMap<(String, String), String>,
        pub mounts: String,
    }

    impl FakeDisks {
        pub fn disk(mut self, name: &str, sectors: u64, model: &str) -> Self {
            self.devices.push(name.to_string());
            self.attributes
                .insert((name.into(), "size".into()), sectors.to_string());
            self.attributes
                .insert((name.into(), "device/model".into()), model.into());
            self
        }

        pub fn flag(mut self, name: &str, attribute: &str, value: bool) -> Self {
            self.attributes.insert(
                (name.into(), attribute.into()),
                if value { "1".into() } else { "0".into() },
            );
            self
        }

        pub fn mounted(mut self, device: &str, at: &str) -> Self {
            self.mounts
                .push_str(&format!("{device} {at} ext4 rw,relatime 0 0\n"));
            self
        }
    }

    impl DiskSource for FakeDisks {
        fn block_devices(&self) -> Vec<String> {
            self.devices.clone()
        }
        fn attribute(&self, device: &str, path: &str) -> Option<String> {
            self.attributes
                .get(&(device.to_string(), path.to_string()))
                .cloned()
        }
        fn mounts(&self) -> String {
            self.mounts.clone()
        }
    }

    /// Sectors for a size in gibibytes.
    fn gib(n: u64) -> u64 {
        n * (1 << 30) / SECTOR_BYTES
    }

    #[test]
    fn disks_are_read_from_the_source() {
        let source = FakeDisks::default().disk("sda", gib(64), "QEMU HARDDISK");
        let disks = enumerate(&source);
        assert_eq!(disks.len(), 1);
        assert_eq!(disks[0].path, "/dev/sda");
        assert_eq!(disks[0].bytes, 64 * (1 << 30));
        assert_eq!(disks[0].model, "QEMU HARDDISK");
        assert!(disks[0].is_installable());
    }

    #[test]
    fn virtual_devices_are_never_offered() {
        let source = FakeDisks::default()
            .disk("loop0", gib(4), "")
            .disk("ram0", gib(4), "")
            .disk("sr0", gib(4), "QEMU DVD-ROM")
            .disk("zram0", gib(4), "")
            .disk("dm-0", gib(4), "")
            .disk("sda", gib(8), "disk");
        let disks = enumerate(&source);
        assert_eq!(disks.len(), 1);
        assert_eq!(disks[0].name, "sda");
    }

    #[test]
    fn a_zero_sized_device_is_skipped() {
        // An empty card reader slot reports a size of zero.
        let source = FakeDisks::default().disk("sdb", 0, "Card Reader");
        assert!(enumerate(&source).is_empty());
    }

    #[test]
    fn read_only_disks_are_refused() {
        let source = FakeDisks::default()
            .disk("sda", gib(8), "disk")
            .flag("sda", "ro", true);
        let disks = enumerate(&source);
        assert!(!disks[0].is_installable());
        assert_eq!(disks[0].refusal(), Some("read only"));
    }

    #[test]
    fn tiny_disks_are_refused() {
        let source = FakeDisks::default().disk("sda", gib(1), "tiny");
        assert_eq!(enumerate(&source)[0].refusal(), Some("too small"));
    }

    #[test]
    fn a_mounted_disk_is_refused() {
        let source = FakeDisks::default()
            .disk("sda", gib(8), "disk")
            .mounted("/dev/sda1", "/mnt/data");
        assert_eq!(enumerate(&source)[0].refusal(), Some("a partition is mounted"));
    }

    #[test]
    fn the_boot_medium_is_refused() {
        // Installing over the medium being booted from would be fatal.
        let source = FakeDisks::default()
            .disk("sda", gib(8), "disk")
            .mounted("/dev/sda1", "/run/live/medium");
        assert_eq!(
            enumerate(&source)[0].refusal(),
            Some("this is the medium tOS booted from")
        );
    }

    #[test]
    fn the_root_device_is_refused() {
        let source = FakeDisks::default()
            .disk("sda", gib(8), "disk")
            .mounted("/dev/sda2", "/");
        assert!(!enumerate(&source)[0].is_installable());
    }

    #[test]
    fn nvme_partitions_are_not_mistaken_for_other_disks() {
        // /dev/nvme0n1p1 belongs to nvme0n1, but nvme0n2 is a different disk.
        let source = FakeDisks::default()
            .disk("nvme0n1", gib(8), "ssd")
            .disk("nvme0n2", gib(8), "ssd")
            .mounted("/dev/nvme0n1p1", "/");
        let disks = enumerate(&source);
        let busy = disks.iter().find(|d| d.name == "nvme0n1").unwrap();
        let free = disks.iter().find(|d| d.name == "nvme0n2").unwrap();
        assert!(!busy.is_installable());
        assert!(free.is_installable());
    }

    #[test]
    fn partition_paths_follow_the_device_naming_rules() {
        let scsi = Disk {
            name: "sda".into(),
            path: "/dev/sda".into(),
            bytes: 0,
            model: String::new(),
            removable: false,
            read_only: false,
            in_use: false,
            is_boot_medium: false,
        };
        assert_eq!(scsi.partition_path(1), "/dev/sda1");

        let nvme = Disk {
            name: "nvme0n1".into(),
            path: "/dev/nvme0n1".into(),
            ..scsi.clone()
        };
        assert_eq!(nvme.partition_path(2), "/dev/nvme0n1p2");

        let mmc = Disk {
            name: "mmcblk0".into(),
            path: "/dev/mmcblk0".into(),
            ..scsi.clone()
        };
        assert_eq!(mmc.partition_path(1), "/dev/mmcblk0p1");
    }

    #[test]
    fn installable_disks_are_offered_first_and_largest_first() {
        let source = FakeDisks::default()
            .disk("sda", gib(8), "small")
            .disk("sdb", gib(64), "large")
            .disk("sdc", gib(32), "busy")
            .mounted("/dev/sdc1", "/");
        let disks = enumerate(&source);
        assert_eq!(disks[0].name, "sdb");
        assert_eq!(disks[1].name, "sda");
        assert_eq!(disks[2].name, "sdc", "the unusable disk is offered last");
    }

    #[test]
    fn mount_points_with_spaces_are_decoded() {
        let mounts = parse_mounts("/dev/sda1 /mnt/my\\040disk ext4 rw 0 0\n");
        assert_eq!(mounts.get("/dev/sda1").unwrap(), "/mnt/my disk");
    }

    #[test]
    fn non_device_mounts_are_ignored() {
        let mounts = parse_mounts("proc /proc proc rw 0 0\nsysfs /sys sysfs rw 0 0\n");
        assert!(mounts.is_empty());
    }

    #[test]
    fn sizes_read_the_way_people_write_them() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1 << 20), "1 MiB");
        assert_eq!(format_bytes(3 * (1 << 30)), "3 GiB");
        assert_eq!(format_bytes(3 * (1 << 30) + (1 << 29)), "3.5 GiB");
        assert_eq!(format_bytes(500 * (1 << 30)), "500 GiB");
        assert_eq!(format_bytes(2 * (1 << 40)), "2 TiB");
    }

    #[test]
    fn a_disk_summarises_itself_for_the_picker() {
        let source = FakeDisks::default().disk("sda", gib(64), "QEMU HARDDISK");
        let summary = enumerate(&source)[0].summary();
        assert!(summary.contains("/dev/sda"));
        assert!(summary.contains("64 GiB"));
        assert!(summary.contains("QEMU HARDDISK"));
    }

    #[test]
    fn a_disk_with_no_model_still_summarises() {
        let source = FakeDisks::default().disk("vda", gib(20), "");
        assert!(enumerate(&source)[0].summary().contains("unknown model"));
    }

    #[test]
    fn reading_a_machine_with_no_sys_block_yields_nothing() {
        // The installer must not panic where /sys is absent, which is every
        // developer machine that is not Linux.
        let disks = enumerate(&SysfsSource::rooted("/nonexistent-root"));
        assert!(disks.is_empty());
    }

    #[test]
    fn a_prepared_tree_can_be_read_from_disk() {
        let root = std::env::temp_dir().join("tos-install-sysfs-test");
        let _ = std::fs::remove_dir_all(&root);
        let device = root.join("sys/block/vda");
        std::fs::create_dir_all(device.join("device")).unwrap();
        std::fs::write(device.join("size"), "41943040\n").unwrap();
        std::fs::write(device.join("device/model"), "QEMU HARDDISK\n").unwrap();
        std::fs::create_dir_all(root.join("proc")).unwrap();
        std::fs::write(root.join("proc/mounts"), "").unwrap();

        let disks = enumerate(&SysfsSource::rooted(&root));
        assert_eq!(disks.len(), 1);
        assert_eq!(disks[0].path, "/dev/vda");
        assert_eq!(disks[0].size_label(), "20 GiB");
        assert!(disks[0].is_installable());
        let _ = std::fs::remove_dir_all(&root);
    }
}
