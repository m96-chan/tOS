//! Pseudoterminals.
//!
//! The compositor owns PTY creation directly, as the README requires: there is
//! no terminal emulator process in between, so this is where child processes
//! actually get their controlling terminal.
//!
//! Only POSIX interfaces are used (`posix_openpt` rather than `openpty`), so
//! the same code runs on the Linux target and on a developer machine.

use std::ffi::{CStr, CString, OsStr};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Terminal size, in cells and in pixels.
///
/// The pixel fields matter: applications use them to size images sent through
/// the Kitty graphics protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Winsize {
    pub cols: u16,
    pub rows: u16,
    pub width_px: u16,
    pub height_px: u16,
}

impl Winsize {
    pub fn new(cols: u16, rows: u16, width_px: u16, height_px: u16) -> Self {
        Winsize {
            cols,
            rows,
            width_px,
            height_px,
        }
    }

    fn to_raw(self) -> libc::winsize {
        libc::winsize {
            ws_row: self.rows,
            ws_col: self.cols,
            ws_xpixel: self.width_px,
            ws_ypixel: self.height_px,
        }
    }
}

/// How to start a child on a new PTY.
#[derive(Debug, Clone)]
pub struct PtyConfig {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Variables added to the inherited environment.
    pub env: Vec<(String, String)>,
    /// Variables removed from the inherited environment.
    pub unset_env: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub winsize: Winsize,
}

impl PtyConfig {
    /// A login shell with the environment tOS advertises.
    pub fn shell(winsize: Winsize) -> Self {
        let program = std::env::var_os("SHELL")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| PathBuf::from("/bin/sh"));
        PtyConfig {
            program,
            args: Vec::new(),
            env: vec![
                ("TERM".into(), "xterm-256color".into()),
                ("COLORTERM".into(), "truecolor".into()),
                ("TERM_PROGRAM".into(), "tOS".into()),
                ("TERM_PROGRAM_VERSION".into(), env!("CARGO_PKG_VERSION").into()),
            ],
            unset_env: vec!["COLUMNS".into(), "LINES".into()],
            cwd: None,
            winsize,
        }
    }

    pub fn command(program: impl Into<PathBuf>, args: Vec<String>, winsize: Winsize) -> Self {
        PtyConfig {
            program: program.into(),
            args,
            ..PtyConfig::shell(winsize)
        }
    }
}

/// `ptsname` writes into a static buffer, so calls have to be serialized.
static PTSNAME_LOCK: Mutex<()> = Mutex::new(());

/// A pseudoterminal with a child process attached.
#[derive(Debug)]
pub struct Pty {
    master: RawFd,
    pid: libc::pid_t,
    exit_status: Option<i32>,
}

impl Pty {
    /// Open a PTY and fork a child onto its slave side.
    pub fn spawn(config: &PtyConfig) -> io::Result<Pty> {
        // Everything the child needs must be allocated before the fork: after
        // it, only async-signal-safe calls are allowed.
        let program = cstring(config.program.as_os_str())?;
        let mut argv_owned = vec![program.clone()];
        for arg in &config.args {
            argv_owned.push(cstring(OsStr::new(arg))?);
        }
        let mut argv: Vec<*const libc::c_char> =
            argv_owned.iter().map(|s| s.as_ptr()).collect();
        argv.push(std::ptr::null());

        let env_pairs = build_env(config)?;
        let mut envp: Vec<*const libc::c_char> = env_pairs.iter().map(|s| s.as_ptr()).collect();
        envp.push(std::ptr::null());

        let cwd = match &config.cwd {
            Some(dir) => Some(cstring(dir.as_os_str())?),
            None => None,
        };

        let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
        if master < 0 {
            return Err(io::Error::last_os_error());
        }
        let master = Fd(master);

        if unsafe { libc::grantpt(master.0) } < 0 || unsafe { libc::unlockpt(master.0) } < 0 {
            return Err(io::Error::last_os_error());
        }

        let slave_name = {
            let _guard = PTSNAME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let ptr = unsafe { libc::ptsname(master.0) };
            if ptr.is_null() {
                return Err(io::Error::last_os_error());
            }
            unsafe { CStr::from_ptr(ptr) }.to_owned()
        };

        let slave = unsafe { libc::open(slave_name.as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
        if slave < 0 {
            return Err(io::Error::last_os_error());
        }
        let slave = Fd(slave);

        let winsize = config.winsize.to_raw();
        if unsafe { libc::ioctl(master.0, libc::TIOCSWINSZ, &winsize) } < 0 {
            return Err(io::Error::last_os_error());
        }

        let pid = unsafe { libc::fork() };
        match pid {
            -1 => Err(io::Error::last_os_error()),
            0 => {
                // Child. Any failure here must exit rather than return, or two
                // copies of the compositor would keep running.
                unsafe {
                    child_setup(master.0, slave.0, cwd.as_deref());
                    libc::execve(program.as_ptr(), argv.as_ptr(), envp.as_ptr());
                    // execve only returns on failure.
                    libc::_exit(127);
                }
            }
            pid => {
                drop(slave);
                let master_fd = master.into_raw();
                set_nonblocking(master_fd)?;
                set_cloexec(master_fd)?;
                Ok(Pty {
                    master: master_fd,
                    pid,
                    exit_status: None,
                })
            }
        }
    }

    /// The master file descriptor, for polling.
    pub fn fd(&self) -> RawFd {
        self.master
    }

    pub fn pid(&self) -> i32 {
        self.pid
    }

    /// Read available output. `Ok(0)` means the child closed the terminal.
    pub fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = unsafe {
            libc::read(
                self.master,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
            )
        };
        if n < 0 {
            let err = io::Error::last_os_error();
            // Linux reports a hung up PTY as EIO rather than end of file.
            if err.raw_os_error() == Some(libc::EIO) {
                return Ok(0);
            }
            return Err(err);
        }
        Ok(n as usize)
    }

    /// Write input to the child. May write less than the whole buffer.
    pub fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = unsafe {
            libc::write(
                self.master,
                buf.as_ptr() as *const libc::c_void,
                buf.len(),
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(n as usize)
    }

    /// Tell the child its terminal changed size.
    pub fn resize(&self, winsize: Winsize) -> io::Result<()> {
        let raw = winsize.to_raw();
        if unsafe { libc::ioctl(self.master, libc::TIOCSWINSZ, &raw) } < 0 {
            return Err(io::Error::last_os_error());
        }
        // The kernel sends SIGWINCH to the foreground process group itself.
        Ok(())
    }

    /// Reap the child without blocking. Returns its exit status once it ends.
    pub fn try_wait(&mut self) -> io::Result<Option<i32>> {
        if let Some(status) = self.exit_status {
            return Ok(Some(status));
        }
        let mut status = 0;
        let result = unsafe { libc::waitpid(self.pid, &mut status, libc::WNOHANG) };
        match result {
            -1 => {
                let err = io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::ECHILD) {
                    // Already reaped elsewhere; treat as a normal exit.
                    self.exit_status = Some(0);
                    return Ok(Some(0));
                }
                Err(err)
            }
            0 => Ok(None),
            _ => {
                let code = decode_status(status);
                self.exit_status = Some(code);
                Ok(Some(code))
            }
        }
    }

    pub fn is_alive(&mut self) -> bool {
        matches!(self.try_wait(), Ok(None))
    }

    /// Send a signal to the child's process group.
    ///
    /// The child makes itself a group leader with `setsid`, but that happens
    /// after the fork returns here, so a signal sent immediately after
    /// spawning can arrive before the group exists. In that window the signal
    /// goes to the process itself.
    pub fn signal(&self, signal: i32) -> io::Result<()> {
        if unsafe { libc::kill(-self.pid, signal) } == 0 {
            return Ok(());
        }
        let group_error = io::Error::last_os_error();
        if group_error.raw_os_error() != Some(libc::ESRCH) {
            return Err(group_error);
        }
        if unsafe { libc::kill(self.pid, signal) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Wait until the descriptor is readable, or until `timeout_ms` elapses.
    pub fn poll_readable(&self, timeout_ms: i32) -> io::Result<bool> {
        let mut fds = libc::pollfd {
            fd: self.master,
            events: libc::POLLIN,
            revents: 0,
        };
        let n = unsafe { libc::poll(&mut fds, 1, timeout_ms) };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                return Ok(false);
            }
            return Err(err);
        }
        Ok(n > 0)
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        // Hang up the terminal so the child notices, then let it go.
        let _ = self.signal(libc::SIGHUP);
        unsafe {
            libc::close(self.master);
        }
        let mut status = 0;
        unsafe {
            libc::waitpid(self.pid, &mut status, libc::WNOHANG);
        }
    }
}

/// Everything the child does between `fork` and `execve`.
///
/// # Safety
/// Must be called only in the child of a fork, and must use only
/// async-signal-safe functions.
unsafe fn child_setup(master: RawFd, slave: RawFd, cwd: Option<&CStr>) {
    // A new session, so this process can take a controlling terminal.
    if libc::setsid() < 0 {
        libc::_exit(126);
    }
    // The request type differs between platforms, hence the cast.
    if libc::ioctl(slave, libc::TIOCSCTTY as _, 0) < 0 {
        libc::_exit(126);
    }

    if libc::dup2(slave, 0) < 0 || libc::dup2(slave, 1) < 0 || libc::dup2(slave, 2) < 0 {
        libc::_exit(126);
    }
    if slave > 2 {
        libc::close(slave);
    }
    libc::close(master);

    if let Some(dir) = cwd {
        if libc::chdir(dir.as_ptr()) < 0 {
            libc::_exit(126);
        }
    }

    // Signal dispositions are inherited across exec; reset the ones the
    // compositor may have ignored so the child behaves like a normal process.
    for signal in [libc::SIGPIPE, libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM] {
        libc::signal(signal, libc::SIG_DFL);
    }
    let mut empty: libc::sigset_t = std::mem::zeroed();
    libc::sigemptyset(&mut empty);
    libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());
}

fn build_env(config: &PtyConfig) -> io::Result<Vec<CString>> {
    let mut vars: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| !config.unset_env.contains(k))
        .collect();
    for (key, value) in &config.env {
        vars.retain(|(k, _)| k != key);
        vars.push((key.clone(), value.clone()));
    }
    vars.iter()
        .map(|(k, v)| cstring(OsStr::new(&format!("{k}={v}"))))
        .collect()
}

fn cstring(value: &OsStr) -> io::Result<CString> {
    CString::new(value.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "value contains a nul byte"))
}

fn decode_status(status: i32) -> i32 {
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status)
    } else {
        -1
    }
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn set_cloexec(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// A descriptor that closes itself, used while a PTY is being set up.
struct Fd(RawFd);

impl Fd {
    fn into_raw(self) -> RawFd {
        let fd = self.0;
        std::mem::forget(self);
        fd
    }
}

impl Drop for Fd {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.0);
        }
    }
}

/// Resolve a program name against `PATH`, as a shell would.
pub fn which(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        let path = PathBuf::from(program);
        return path.is_file().then_some(path);
    }
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    let Ok(c) = cstring(path.as_os_str()) else {
        return false;
    };
    unsafe { libc::access(c.as_ptr(), libc::X_OK) == 0 }
}
