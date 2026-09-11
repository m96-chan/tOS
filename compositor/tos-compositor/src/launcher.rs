//! The launcher's list: every program on `$PATH`.
//!
//! This is the launcher half of the overlay — the part that knows the items
//! are programs. The overlay itself does the filtering and the drawing, and
//! the compositor decides that choosing one opens a pane.
//!
//! `$PATH` is scanned once, when the overlay opens, and never again while it
//! is up. Scanning on every keystroke would put a few thousand `stat` calls
//! between a key and the frame that answers it.

use std::collections::HashSet;
use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::overlay::OverlayItem;

/// The most programs one scan will collect.
///
/// A directory on `$PATH` can be arbitrarily large — a stale build tree, a
/// mount of something enormous — and the compositor is single threaded, so the
/// scan has to have an end. Stopping short leaves a usable launcher; blocking
/// the frame loop for a second does not.
const MAX_PROGRAMS: usize = 8192;

/// Every executable on `$PATH`, deduplicated and sorted by name.
///
/// Each item's detail is the directory it was found in, which is also the one
/// it will run from: the first match in `$PATH` order wins, the way a shell
/// resolves it.
pub fn programs_on_path() -> Vec<OverlayItem> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
    programs_in(&dirs)
}

/// The same scan over an explicit list of directories.
pub fn programs_in(dirs: &[PathBuf]) -> Vec<OverlayItem> {
    collect(dirs, MAX_PROGRAMS)
}

fn collect(dirs: &[PathBuf], limit: usize) -> Vec<OverlayItem> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut found: Vec<OverlayItem> = Vec::new();

    'dirs: for dir in dirs {
        // An empty element of `$PATH` means the working directory. Running
        // whatever happens to be in it is a surprise at best, so tOS does not
        // offer it.
        if dir.as_os_str().is_empty() {
            continue;
        }
        // A directory that is missing, is not a directory, or cannot be read
        // is simply not contributing anything. None of those is an error worth
        // interrupting the launcher for.
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if found.len() >= limit {
                break 'dirs;
            }
            // A name that is not UTF-8 cannot be typed into the query, so it
            // could never be picked out of the list anyway.
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name.starts_with('.') || seen.contains(name) {
                continue;
            }
            if !is_program(&entry.path()) {
                continue;
            }
            seen.insert(name.to_string());
            found.push(OverlayItem::with_detail(name, dir.to_string_lossy()));
        }
    }

    found.sort_by(|a, b| a.label.cmp(&b.label));
    found
}

/// Whether this path is something this user can run.
///
/// The metadata is followed through symlinks on purpose: most of `/usr/bin` on
/// a packaged system is links, and the link itself carries no useful mode. A
/// broken link fails the `stat` and is quietly not a program.
///
/// The permission question is asked with `access` rather than by reading the
/// mode bits, because the two do not always agree — a file can carry an
/// execute bit for a user this is not. Answering it the same way
/// [`tos_pty::which`] does is what keeps the launcher from offering a name the
/// pane spawner would then fail to resolve. `access` alone says yes to a
/// directory, hence the file check as well.
fn is_program(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    let Ok(c_path) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // Safe: the pointer is a valid NUL terminated string for the call.
    unsafe { libc::access(c_path.as_ptr(), libc::X_OK) == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A directory of this test's own, in the system temporary directory.
    /// Tests run in parallel in one process, so the counter keeps them apart.
    fn scratch(name: &str) -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "tos-launcher-{}-{name}-{unique}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("scratch directory");
        dir
    }

    fn executable(dir: &Path, name: &str) {
        let path = dir.join(name);
        fs::write(&path, "#!/bin/sh\n").expect("write");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    fn names(items: &[OverlayItem]) -> Vec<&str> {
        items.iter().map(|item| item.label.as_str()).collect()
    }

    #[test]
    fn executables_are_listed_in_order() {
        let dir = scratch("order");
        executable(&dir, "zebra");
        executable(&dir, "alpha");
        let programs = programs_in(std::slice::from_ref(&dir));
        assert_eq!(names(&programs), ["alpha", "zebra"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn plain_files_and_directories_are_not_programs() {
        let dir = scratch("kinds");
        executable(&dir, "runnable");
        fs::write(dir.join("readme"), "not executable").expect("write");
        fs::create_dir(dir.join("subdir")).expect("mkdir");
        fs::set_permissions(dir.join("subdir"), fs::Permissions::from_mode(0o755))
            .expect("chmod");
        assert_eq!(names(&programs_in(std::slice::from_ref(&dir))), ["runnable"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn hidden_files_are_left_out() {
        let dir = scratch("hidden");
        executable(&dir, ".hidden");
        executable(&dir, "shown");
        assert_eq!(names(&programs_in(std::slice::from_ref(&dir))), ["shown"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_name_in_two_directories_appears_once_from_the_first() {
        let first = scratch("first");
        let second = scratch("second");
        executable(&first, "tool");
        executable(&second, "tool");
        executable(&second, "other");
        let programs = programs_in(&[first.clone(), second.clone()]);
        assert_eq!(names(&programs), ["other", "tool"]);
        let tool = programs.iter().find(|i| i.label == "tool").unwrap();
        assert_eq!(tool.detail, first.to_string_lossy());
        let _ = fs::remove_dir_all(&first);
        let _ = fs::remove_dir_all(&second);
    }

    #[test]
    fn a_missing_or_unreadable_directory_is_skipped() {
        let dir = scratch("present");
        executable(&dir, "here");
        let missing = dir.join("does-not-exist");
        let not_a_directory = dir.join("here");
        let programs = programs_in(&[missing, not_a_directory, dir.clone()]);
        assert_eq!(names(&programs), ["here"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_path_element_is_not_the_working_directory() {
        // Otherwise the launcher would offer whatever is in the directory the
        // compositor happens to have been started from.
        let programs = programs_in(&[PathBuf::new()]);
        assert!(programs.is_empty());
    }

    #[test]
    fn the_scan_stops_at_its_limit() {
        let dir = scratch("limit");
        for i in 0..10 {
            executable(&dir, &format!("program-{i}"));
        }
        assert_eq!(collect(std::slice::from_ref(&dir), 4).len(), 4);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_real_path_has_programs_on_it() {
        // The point of the launcher is the machine it runs on, so this one is
        // not mocked: the scan has to survive whatever is actually on $PATH.
        let programs = programs_on_path();
        assert!(!programs.is_empty(), "no programs found on $PATH");
        let names = names(&programs);
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "the list should be sorted");
        let mut unique = names.clone();
        unique.dedup();
        assert_eq!(names.len(), unique.len(), "the list should be deduplicated");
        // Every name it offers is one the pane spawner can resolve.
        for item in programs.iter().take(20) {
            assert!(
                tos_pty::which(&item.label).is_some(),
                "{} is not runnable",
                item.label
            );
        }
    }
}
