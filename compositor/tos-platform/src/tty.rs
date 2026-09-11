//! Raw mode for a host terminal.
//!
//! Only the nested development backend needs this: when tOS owns the display
//! directly there is no host terminal to put into raw mode.

use std::io;
use std::os::unix::io::RawFd;

/// A terminal returned to its previous state when dropped.
pub struct RawMode {
    fd: RawFd,
    saved: libc::termios,
    restored: bool,
}

impl RawMode {
    /// Put a terminal into raw mode: no echo, no line buffering, no signal
    /// generation, so that every key reaches tOS unchanged.
    pub fn acquire(fd: RawFd) -> io::Result<RawMode> {
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut saved) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut raw = saved;
        unsafe { libc::cfmakeraw(&mut raw) };
        // Return from read as soon as a single byte is available.
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(RawMode {
            fd,
            saved,
            restored: false,
        })
    }

    /// Restore the saved settings early.
    pub fn restore(&mut self) {
        if !self.restored {
            unsafe {
                libc::tcsetattr(self.fd, libc::TCSANOW, &self.saved);
            }
            self.restored = true;
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Size of a terminal, in cells and pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalSize {
    pub cols: u16,
    pub rows: u16,
    pub width_px: u16,
    pub height_px: u16,
}

/// Ask the kernel how big a terminal is.
pub fn terminal_size(fd: RawFd) -> io::Result<TerminalSize> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(TerminalSize {
        cols: ws.ws_col,
        rows: ws.ws_row,
        width_px: ws.ws_xpixel,
        height_px: ws.ws_ypixel,
    })
}

/// Put a descriptor into non blocking mode.
pub fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Read whatever is available without blocking.
pub fn read_available(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    if n < 0 {
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::WouldBlock {
            return Ok(0);
        }
        return Err(err);
    }
    Ok(n as usize)
}

/// Wait until any of `fds` is readable, or until the timeout expires.
pub fn poll_readable(fds: &[RawFd], timeout_ms: i32) -> io::Result<Vec<RawFd>> {
    let mut poll_fds: Vec<libc::pollfd> = fds
        .iter()
        .map(|&fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    if poll_fds.is_empty() {
        return Ok(Vec::new());
    }
    let n = unsafe {
        libc::poll(
            poll_fds.as_mut_ptr(),
            poll_fds.len() as libc::nfds_t,
            timeout_ms,
        )
    };
    if n < 0 {
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
            return Ok(Vec::new());
        }
        return Err(err);
    }
    Ok(poll_fds
        .into_iter()
        .filter(|p| p.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0)
        .map(|p| p.fd)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn polling_nothing_returns_nothing() {
        assert!(poll_readable(&[], 0).unwrap().is_empty());
    }

    #[test]
    fn a_pipe_becomes_readable() {
        let mut fds = [0 as RawFd; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let (read_end, write_end) = (fds[0], fds[1]);
        assert!(poll_readable(&[read_end], 0).unwrap().is_empty());
        unsafe {
            libc::write(write_end, b"x".as_ptr() as *const libc::c_void, 1);
        }
        assert_eq!(poll_readable(&[read_end], 100).unwrap(), vec![read_end]);

        let mut buf = [0u8; 4];
        assert_eq!(read_available(read_end, &mut buf).unwrap(), 1);
        unsafe {
            libc::close(read_end);
            libc::close(write_end);
        }
    }

    #[test]
    fn non_blocking_reads_return_immediately() {
        let mut fds = [0 as RawFd; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        set_nonblocking(fds[0]).unwrap();
        let mut buf = [0u8; 4];
        assert_eq!(read_available(fds[0], &mut buf).unwrap(), 0);
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
    }
}
