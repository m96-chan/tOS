//! What tOS will open, and unlink, because a program asked it to.
//!
//! The kitty protocol lets a program transmit a picture by name instead of
//! inline: `t=f` a path to read, `t=t` a path to read and then delete, `t=s`
//! a POSIX shared memory object. That is how a file manager shows a preview
//! without pushing megabytes of base64 through a PTY, and it is also a
//! program handing the compositor a path and asking it to open it.
//!
//! tOS is not a terminal emulator running as somebody's desktop application.
//! It is PID 1's child, it holds the machine, and in a booted session it is
//! root. So the rules below are not about what a picture is; they are about
//! what a process that owns the machine will do on the say-so of a process
//! that does not own anything. They are argued in full in
//! `docs/design/graphics-file-transmission.md`; in short:
//!
//! - The open is non-blocking and the type is checked on the descriptor, so
//!   that a fifo or a device named as a picture costs a refusal rather than
//!   the session's parse loop.
//! - The size is taken from the descriptor and checked before a byte is read,
//!   so a file cannot burst a budget that a payload could not.
//! - The last component is never followed as a symbolic link.
//! - Deleting is confined: `t=t` and `t=s` name one component in a directory
//!   tOS chose, and the name is unlinked only if it still refers to the file
//!   that was read.

use std::ffi::{CString, OsStr};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};

use tos_term::graphics::Medium;
use tos_term::medium::MediumReader;

/// The directories a `t=t` transfer may name a file in.
///
/// Everything a program is entitled to have tOS delete is a temporary file,
/// and these are the three places on this system where temporary files live.
/// `$TMPDIR` is deliberately not consulted: it comes from the environment of
/// the program doing the asking, and a list of directories tOS will unlink
/// inside is exactly the wrong thing to let the asker choose.
const TEMP_ROOTS: [&str; 3] = ["/tmp", "/var/tmp", "/dev/shm"];

/// Where POSIX shared memory objects are. `shm_open(3)` is `open(2)` on a
/// name under this directory — that is not an implementation detail glibc
/// hides, it is what the Linux ABI for POSIX shared memory *is* — so `t=s`
/// needs no code of its own beyond knowing the directory.
const SHM_ROOT: &str = "/dev/shm";

/// Flags every one of these opens uses.
///
/// `O_NONBLOCK` is the one that matters: without it, opening a fifo for
/// reading waits for a writer that a hostile program will simply never
/// provide, and that wait happens inside the compositor's parse loop, with
/// the whole session behind it. With it, the open returns and `fstat` gets to
/// say no. `O_NOFOLLOW` refuses a symbolic link as the final component, and
/// `O_CLOEXEC` keeps the descriptor out of every program started afterwards,
/// the same rule the input and DRM devices follow.
const OPEN_FLAGS: libc::c_int =
    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;

/// Reads the files graphics commands name.
pub struct ImageFiles {
    temp_roots: Vec<PathBuf>,
    shm_root: PathBuf,
}

impl ImageFiles {
    /// The reader for the machine this is running on.
    pub fn system() -> ImageFiles {
        ImageFiles::at(TEMP_ROOTS.iter().map(PathBuf::from).collect(), SHM_ROOT)
    }

    /// The same, against directories of someone else's choosing.
    ///
    /// The roots are injectable for the reason every root in tOS is — a test
    /// needs somewhere it can make a fifo and a dangling symlink without
    /// leaving them in the developer's `/tmp` — and for no other: nothing
    /// reads these from a configuration file, because a graphics command is
    /// not the place to negotiate which directories the compositor deletes
    /// things in.
    pub fn at(temp_roots: Vec<PathBuf>, shm_root: impl Into<PathBuf>) -> ImageFiles {
        ImageFiles {
            temp_roots,
            shm_root: shm_root.into(),
        }
    }

    /// `t=f`: read a path and leave it alone.
    fn read_file(&self, name: &[u8], max_bytes: usize) -> Result<Vec<u8>, &'static str> {
        let path = Path::new(OsStr::from_bytes(name));
        if !path.is_absolute() {
            // A relative path would resolve against the compositor's working
            // directory, which is wherever `/init` happened to start it. That
            // is not a place any program means, so it is better refused than
            // guessed at.
            return Err("EINVAL:the path must be absolute");
        }
        let path = cstring(name)?;
        let fd = open(libc::AT_FDCWD, &path)?;
        let stat = check(fd.0, max_bytes)?;
        read_all(fd.0, stat.st_size as usize)
    }

    /// `t=t`: read a temporary file and delete it.
    fn read_temp_file(&self, name: &[u8], max_bytes: usize) -> Result<Vec<u8>, &'static str> {
        const OUTSIDE: &str = "EINVAL:a temporary file must be one name in a temporary directory";
        let path = Path::new(OsStr::from_bytes(name));
        let (Some(parent), Some(file)) = (path.parent(), path.file_name()) else {
            return Err(OUTSIDE);
        };
        // Exact equality against the roots, which also settles `.` and `..`:
        // a path with either in it has a parent that is not spelled like any
        // root, and a path ending in one has no file name at all.
        let Some(root) = self.temp_roots.iter().find(|root| *root == parent) else {
            return Err(OUTSIDE);
        };
        // A transfer whose file tOS may not delete is refused whole rather
        // than read and left behind. The sender believes the file is gone
        // after a `t=t`; letting it believe that wrongly is how a temporary
        // file that held a photograph stays on the disk for ever.
        consume(root, file, max_bytes)
    }

    /// `t=s`: read a shared memory object and unlink it.
    fn read_shared_memory(&self, name: &[u8], max_bytes: usize) -> Result<Vec<u8>, &'static str> {
        // POSIX allows one leading slash and forbids any other, which leaves
        // exactly the single component that `/dev/shm` holds.
        let name = name.strip_prefix(b"/").unwrap_or(name);
        if name.is_empty() || name.contains(&b'/') || name == b"." || name == b".." {
            return Err("EINVAL:a shared memory name is one component with no slashes in it");
        }
        consume(&self.shm_root, OsStr::from_bytes(name), max_bytes)
    }
}

impl MediumReader for ImageFiles {
    fn read(
        &mut self,
        medium: Medium,
        name: &[u8],
        max_bytes: usize,
    ) -> Result<Vec<u8>, &'static str> {
        match medium {
            // Direct transmission never reaches a reader: the terminal
            // already has those bytes and does not ask anyone for them.
            Medium::Direct => Err("EINVAL:direct transmission carries its own payload"),
            Medium::File => self.read_file(name, max_bytes),
            Medium::TempFile => self.read_temp_file(name, max_bytes),
            Medium::SharedMemory => self.read_shared_memory(name, max_bytes),
        }
    }
}

/// Read one name inside `dir`, then unlink it.
///
/// `dir` is tOS's own, `name` is the program's, and only the second is opened
/// with `O_NOFOLLOW`: following a symbolic link that tOS chose is following
/// tOS's own configuration, while following one the sender planted is reading
/// or deleting a file it named without naming.
///
/// Holding the directory open and working from that descriptor is what makes
/// "one name inside a temporary directory" true rather than merely spelled:
/// the name the file was read from and the name that gets unlinked are the
/// same name in the same directory, whatever anybody renames in between.
fn consume(dir: &Path, name: &OsStr, max_bytes: usize) -> Result<Vec<u8>, &'static str> {
    let dir_path = cstring(dir.as_os_str().as_bytes())?;
    let name = cstring(name.as_bytes())?;

    let dirfd = unsafe {
        libc::open(
            dir_path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if dirfd < 0 {
        return Err("EBADF:cannot open the temporary directory");
    }
    let dirfd = Fd(dirfd);

    let fd = open(dirfd.0, &name)?;
    let stat = check(fd.0, max_bytes)?;
    let data = read_all(fd.0, stat.st_size as usize)?;

    // Unlink the file that was read, not the name that was given. Between the
    // open and here, anything may have replaced that name — and tOS is root,
    // so the sticky bit on `/tmp` that stops one program deleting another's
    // file does not stop tOS doing it for them. Comparing the descriptor's
    // device and inode against what the name says now closes that: if they
    // differ the name is somebody else's file and it stays where it is. The
    // picture still arrived, so the transfer is still a success; the only
    // thing lost is a cleanup that was no longer ours to do.
    let mut now: libc::stat = unsafe { std::mem::zeroed() };
    let named =
        unsafe { libc::fstatat(dirfd.0, name.as_ptr(), &mut now, libc::AT_SYMLINK_NOFOLLOW) };
    if named == 0 && now.st_dev == stat.st_dev && now.st_ino == stat.st_ino {
        unsafe { libc::unlinkat(dirfd.0, name.as_ptr(), 0) };
    }
    Ok(data)
}

fn open(dirfd: RawFd, name: &CString) -> Result<Fd, &'static str> {
    let fd = unsafe { libc::openat(dirfd, name.as_ptr(), OPEN_FLAGS) };
    if fd >= 0 {
        return Ok(Fd(fd));
    }
    // `O_NOFOLLOW` reports a symbolic link as `ELOOP`, and that is worth
    // saying plainly: a client that sent a link can send the target instead,
    // which it cannot work out from "cannot open".
    match io::Error::last_os_error().raw_os_error() {
        Some(libc::ELOOP) => Err("EBADF:the path is a symbolic link"),
        Some(libc::ENOENT) => Err("EBADF:no such file"),
        _ => Err("EBADF:cannot open the file"),
    }
}

/// Decide from the descriptor whether this is a file worth reading, and how
/// much of it there is.
///
/// On the descriptor and not on the path: a check made against a name, then
/// acted on through a second lookup of the same name, is a race the sender
/// gets to win by renaming between the two.
fn check(fd: RawFd, max_bytes: usize) -> Result<libc::stat, &'static str> {
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut stat) } < 0 {
        return Err("EBADF:cannot stat the file");
    }
    if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
        // A fifo would have been a wait, a terminal would have been the
        // session's own input, a block device would have been the disk.
        return Err("EINVAL:not a regular file");
    }
    if stat.st_size <= 0 {
        // No image is empty, and on this system the regular files that say
        // they hold nothing are the ones under `/proc` that hold whatever is
        // written to them next — `/proc/kmsg` blocks a reader as surely as a
        // fifo does, and reports a length of zero to say so.
        return Err("EINVAL:the file is empty");
    }
    if stat.st_size as u64 > max_bytes as u64 {
        // Before a byte is read, because a file that cannot be kept is not
        // worth the time it would take to copy in. This is the same ceiling
        // the decoders use, for the same reason: nothing that exceeds the
        // whole store's budget could survive being stored.
        return Err("EINVAL:file exceeds the image budget");
    }
    Ok(stat)
}

fn read_all(fd: RawFd, len: usize) -> Result<Vec<u8>, &'static str> {
    let mut buf = vec![0u8; len];
    let mut filled = 0;
    while filled < len {
        let n = unsafe {
            libc::read(
                fd,
                buf[filled..].as_mut_ptr().cast::<libc::c_void>(),
                len - filled,
            )
        };
        if n < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err("EBADF:cannot read the file");
        }
        if n == 0 {
            // The file was truncated while it was being read. What arrived is
            // still what the sender had at the time, so it is handed to the
            // decoder, which is the thing that knows whether it is an image.
            break;
        }
        filled += n as usize;
    }
    buf.truncate(filled);
    if buf.is_empty() {
        return Err("EINVAL:the file is empty");
    }
    Ok(buf)
}

fn cstring(bytes: &[u8]) -> Result<CString, &'static str> {
    CString::new(bytes).map_err(|_| "EINVAL:the name contains a null byte")
}

/// A descriptor that closes itself.
struct Fd(RawFd);

impl Drop for Fd {
    fn drop(&mut self) {
        unsafe { libc::close(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use tos_term::graphics::encode_base64;
    use tos_term::{Terminal, TerminalConfig};

    /// A directory that is both of a reader's roots, and goes away with the
    /// test. The real roots are `/tmp` and its siblings; pointing them here
    /// is what lets a test make a fifo and a dangling link without leaving
    /// either in the developer's.
    struct Tree {
        root: PathBuf,
    }

    impl Tree {
        fn new(name: &str) -> Tree {
            let root =
                std::env::temp_dir().join(format!("tos-imagefile-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).expect("temporary directory");
            Tree { root }
        }

        fn files(&self) -> ImageFiles {
            ImageFiles::at(vec![self.root.clone()], &self.root)
        }

        fn write(&self, name: &str, data: &[u8]) -> PathBuf {
            let path = self.root.join(name);
            fs::write(&path, data).expect("write");
            path
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    /// A path as it arrives in a command: bytes, not text.
    fn name(path: &Path) -> &[u8] {
        path.as_os_str().as_bytes()
    }

    #[test]
    fn a_file_is_read_and_left_where_it_is() {
        let tree = Tree::new("read");
        let path = tree.write("picture.png", b"pretend that this is a PNG");
        let mut files = tree.files();
        assert_eq!(
            files.read(Medium::File, name(&path), 1 << 20),
            Ok(b"pretend that this is a PNG".to_vec())
        );
        // `t=f` is a read. Only `t=t` and `t=s` say anything about deleting.
        assert!(path.exists());
    }

    #[test]
    fn a_temporary_file_is_read_and_then_deleted() {
        let tree = Tree::new("temp");
        let path = tree.write("temp.png", b"pixels");
        let mut files = tree.files();
        assert_eq!(
            files.read(Medium::TempFile, name(&path), 1 << 20),
            Ok(b"pixels".to_vec())
        );
        assert!(!path.exists());
    }

    #[test]
    fn a_temporary_file_outside_the_permitted_roots_is_refused() {
        let tree = Tree::new("outside");
        let outside = tree.root.join("elsewhere");
        fs::create_dir(&outside).expect("directory");
        let path = outside.join("temp.png");
        fs::write(&path, b"pixels").expect("write");

        // A subdirectory of a root is not a root: the rule is one name in a
        // directory tOS chose, which is what leaves no intermediate component
        // for a symbolic link to sit in.
        let mut files = tree.files();
        let refused = files.read(Medium::TempFile, name(&path), 1 << 20);
        assert_eq!(
            refused,
            Err("EINVAL:a temporary file must be one name in a temporary directory")
        );
        // Refused before anything was opened, so the file is still there.
        assert!(path.exists());
    }

    #[test]
    fn the_system_reader_will_not_delete_outside_a_temporary_directory() {
        // The check is on the spelling of the path and happens before the
        // open, which is why this test can name a file it would be a very bad
        // idea to actually hand over.
        let mut files = ImageFiles::system();
        assert_eq!(
            files.read(Medium::TempFile, b"/etc/passwd", 1 << 20),
            Err("EINVAL:a temporary file must be one name in a temporary directory")
        );
        assert_eq!(
            files.read(Medium::TempFile, b"/tmp/../etc/passwd", 1 << 20),
            Err("EINVAL:a temporary file must be one name in a temporary directory")
        );
    }

    #[test]
    fn a_fifo_is_refused_without_waiting_for_a_writer() {
        let tree = Tree::new("fifo");
        let path = tree.root.join("fifo");
        let c_path = CString::new(name(&path)).expect("path");
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);

        // This is the denial of service the whole policy exists for: an
        // ordinary blocking open of a fifo nobody writes to never returns,
        // and what would be waiting is the compositor's parse loop with the
        // session behind it. The answer is read on another thread so that
        // losing `O_NONBLOCK` costs a failed test rather than a hung one.
        let (tx, rx) = mpsc::channel();
        let asked = path.clone();
        let mut files = tree.files();
        thread::spawn(move || {
            let _ = tx.send(files.read(Medium::File, name(&asked), 1 << 20));
        });
        let answer = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("opening a fifo must not wait for a writer");
        assert_eq!(answer, Err("EINVAL:not a regular file"));
    }

    #[test]
    fn a_device_is_not_a_regular_file() {
        let mut files = ImageFiles::system();
        assert_eq!(
            files.read(Medium::File, b"/dev/null", 1 << 20),
            Err("EINVAL:not a regular file")
        );
    }

    #[test]
    fn a_symbolic_link_is_refused() {
        let tree = Tree::new("symlink");
        let target = tree.write("picture.png", b"pixels");
        let link = tree.root.join("link.png");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");

        let mut files = tree.files();
        assert_eq!(
            files.read(Medium::File, name(&link), 1 << 20),
            Err("EBADF:the path is a symbolic link")
        );
        // And a link in a temporary directory is not a way to have tOS delete
        // what it points at either.
        assert_eq!(
            files.read(Medium::TempFile, name(&link), 1 << 20),
            Err("EBADF:the path is a symbolic link")
        );
        assert!(link.symlink_metadata().is_ok());
        assert!(target.exists());
    }

    #[test]
    fn a_file_larger_than_the_cap_is_refused() {
        let tree = Tree::new("large");
        let path = tree.write("large.png", &vec![0u8; 4096]);
        let mut files = tree.files();
        // The length comes from the descriptor, so this is refused without
        // the four kilobytes ever being copied anywhere.
        assert_eq!(
            files.read(Medium::File, name(&path), 1024),
            Err("EINVAL:file exceeds the image budget")
        );
        assert_eq!(
            files.read(Medium::File, name(&path), 4096).map(|d| d.len()),
            Ok(4096)
        );
    }

    #[test]
    fn an_empty_file_is_refused() {
        let tree = Tree::new("empty");
        let path = tree.write("empty.png", b"");
        let mut files = tree.files();
        assert_eq!(
            files.read(Medium::File, name(&path), 1 << 20),
            Err("EINVAL:the file is empty")
        );
    }

    #[test]
    fn a_relative_path_is_refused() {
        let mut files = ImageFiles::system();
        assert_eq!(
            files.read(Medium::File, b"picture.png", 1 << 20),
            Err("EINVAL:the path must be absolute")
        );
    }

    #[test]
    fn a_shared_memory_object_is_read_and_unlinked() {
        let tree = Tree::new("shm");
        let path = tree.write("tos-image", b"pixels");
        let mut files = tree.files();
        // POSIX spells the name with a leading slash and no other.
        assert_eq!(
            files.read(Medium::SharedMemory, b"/tos-image", 1 << 20),
            Ok(b"pixels".to_vec())
        );
        assert!(!path.exists());

        // A client that leaves the slash off means the same object.
        tree.write("tos-image", b"pixels");
        assert_eq!(
            files.read(Medium::SharedMemory, b"tos-image", 1 << 20),
            Ok(b"pixels".to_vec())
        );
    }

    #[test]
    fn a_shared_memory_name_with_a_path_in_it_is_refused() {
        let tree = Tree::new("shmpath");
        let mut files = tree.files();
        for asked in [&b"/sub/tos-image"[..], b"..", b"/", b""] {
            assert_eq!(
                files.read(Medium::SharedMemory, asked, 1 << 20),
                Err("EINVAL:a shared memory name is one component with no slashes in it")
            );
        }
    }

    #[test]
    fn a_file_that_is_not_an_image_is_answered_with_a_protocol_error() {
        let tree = Tree::new("notanimage");
        let path = tree.write("notes.txt", b"this is not a picture");

        // The whole way through: a command naming a file, the reader that
        // opens it, and the decoder that has the last word on what it is.
        let mut terminal = Terminal::new(10, 4, TerminalConfig::default());
        terminal.set_medium_reader(Box::new(tree.files()));
        let payload = encode_base64(name(&path));
        terminal.advance(format!("\x1b_Ga=T,f=100,t=f,i=4;{payload}\x1b\\").as_bytes());

        assert_eq!(
            String::from_utf8(terminal.take_output()).unwrap(),
            "\x1b_Gi=4;EINVAL:not a PNG\x1b\\"
        );
        assert!(terminal.graphics().image(4).is_none());
    }
}
