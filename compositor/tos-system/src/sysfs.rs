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
            let root = std::env::temp_dir().join(format!("tos-sysfs-{name}-{}", std::process::id()));
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
        assert_eq!(fake.sysfs().read("/sys/class/power_supply/BAT0/capacity"), None);
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
        fake.file("/sys/class/net/wlan0/x", "").file("/sys/class/net/eth0/x", "");
        assert_eq!(fake.sysfs().list("/sys/class/net"), vec!["eth0", "wlan0"]);
    }

    #[test]
    fn listing_somewhere_that_is_not_there_is_empty() {
        let fake = Fake::new("nolist");
        assert!(fake.sysfs().list("/sys/class/net").is_empty());
    }
}
