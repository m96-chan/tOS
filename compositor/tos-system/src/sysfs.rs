//! Reading the kernel's view of the machine.

use std::path::{Path, PathBuf};
use std::str::FromStr;

/// A view of `/sys` and `/proc`, rooted somewhere.
///
/// The root is injectable because none of what this crate reads exists on a
/// developer's machine. A test points it at a directory laid out like the real
/// thing; on a running machine it is `/`.
#[derive(Debug, Clone)]
pub struct Sysfs {
    root: PathBuf,
}

impl Sysfs {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Sysfs { root: root.into() }
    }

    /// The real machine.
    pub fn system() -> Self {
        Sysfs::new("/")
    }

    /// Where `path` lands under this root. `path` is always absolute-looking
    /// (`/sys/class/...`); the leading slash is what would otherwise throw the
    /// root away.
    pub fn path(&self, path: &str) -> PathBuf {
        self.root.join(path.trim_start_matches('/'))
    }

    pub fn exists(&self, path: &str) -> bool {
        self.path(path).exists()
    }

    /// A file's contents with the trailing newline gone, or `None` when it
    /// cannot be read. Sysfs files are missing far more often than they are
    /// broken, so absence is not an error worth carrying.
    pub fn read(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(self.path(path))
            .ok()
            .map(|text| text.trim_end_matches('\n').to_string())
    }

    /// A file holding one number.
    pub fn read_number<T: FromStr>(&self, path: &str) -> Option<T> {
        self.read(path)?.trim().parse().ok()
    }

    /// The names in a directory, sorted, so a listing does not reorder itself
    /// between frames.
    pub fn list(&self, dir: &str) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.path(dir))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect();
        names.sort();
        names
    }

    /// Replace a file under this root, making the directories above it first.
    ///
    /// The one write in a module whose name says reading, and it is here
    /// rather than anywhere else because of the root. `/etc/resolv.conf` is
    /// the file a DHCP lease has to land in, and a test that could not point
    /// that write at a temporary directory would either have to skip it or
    /// overwrite the resolver of the machine it is running on. Putting it on
    /// [`Sysfs`] means the write is redirected by the same one line that
    /// redirects every read: `Config::system_root`.
    ///
    /// Unlike the reads, this reports its failure. A file that cannot be read
    /// is a machine that does not have that thing, which is ordinary; a file
    /// that cannot be written is something somebody asked for that did not
    /// happen, and they are owed the reason.
    pub fn write(&self, path: &str, contents: &str) -> std::io::Result<()> {
        let full = self.path(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(full, contents)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway directory that cleans up after itself.
    pub(crate) struct Fake {
        pub root: PathBuf,
    }

    impl Fake {
        pub fn new(name: &str) -> Fake {
            let root =
                std::env::temp_dir().join(format!("tos-sysfs-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            Fake { root }
        }

        pub fn file(&self, path: &str, contents: &str) -> &Fake {
            let full = self.root.join(path.trim_start_matches('/'));
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, contents).unwrap();
            self
        }

        pub fn sysfs(&self) -> Sysfs {
            Sysfs::new(&self.root)
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn a_file_is_read_without_its_trailing_newline() {
        let fake = Fake::new("read");
        fake.file("/sys/class/power_supply/BAT0/capacity", "87\n");
        assert_eq!(
            fake.sysfs().read("/sys/class/power_supply/BAT0/capacity"),
            Some("87".to_string())
        );
    }

    #[test]
    fn a_missing_file_is_absence_rather_than_an_error() {
        let fake = Fake::new("missing");
        assert_eq!(
            fake.sysfs().read("/sys/class/power_supply/BAT0/capacity"),
            None
        );
        assert!(!fake.sysfs().exists("/sys/class/net/eth0"));
    }

    #[test]
    fn a_number_is_parsed_and_a_word_is_not() {
        let fake = Fake::new("number");
        fake.file("/n/good", "42\n").file("/n/bad", "charging\n");
        assert_eq!(fake.sysfs().read_number::<u32>("/n/good"), Some(42));
        assert_eq!(fake.sysfs().read_number::<u32>("/n/bad"), None);
    }

    #[test]
    fn a_listing_comes_back_in_a_settled_order() {
        let fake = Fake::new("list");
        fake.file("/sys/class/net/wlan0/x", "")
            .file("/sys/class/net/eth0/x", "");
        assert_eq!(fake.sysfs().list("/sys/class/net"), vec!["eth0", "wlan0"]);
    }

    #[test]
    fn listing_somewhere_that_is_not_there_is_empty() {
        let fake = Fake::new("nolist");
        assert!(fake.sysfs().list("/sys/class/net").is_empty());
    }

    #[test]
    fn a_write_lands_under_the_root_and_not_on_the_real_machine() {
        let fake = Fake::new("write");
        let sysfs = fake.sysfs();
        // `/etc` does not exist in the temporary directory, which is the
        // ordinary case on a fresh root: the write makes it rather than
        // failing.
        sysfs
            .write("/etc/resolv.conf", "nameserver 192.168.1.1\n")
            .expect("write");
        assert_eq!(
            sysfs.read("/etc/resolv.conf"),
            Some("nameserver 192.168.1.1".to_string())
        );
        assert!(
            fake.root.join("etc/resolv.conf").exists(),
            "it went somewhere else entirely"
        );
    }

    #[test]
    fn a_write_that_cannot_happen_says_so_rather_than_going_quiet() {
        // A root that is not a directory, which is what pointing the
        // compositor at a path that does not exist gives.
        let sysfs = Sysfs::new("/proc/self/cmdline/not-a-directory");
        assert!(sysfs
            .write("/etc/resolv.conf", "nameserver 1.1.1.1\n")
            .is_err());
    }
}
