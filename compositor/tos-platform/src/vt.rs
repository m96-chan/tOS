//! Owning a Linux virtual terminal.
//!
//! When tOS runs on bare hardware it must stop the kernel from also drawing
//! on the screen and from interpreting the keyboard, and it must hand both
//! back when the user switches away with a VT switch key.

use std::ffi::CString;
use std::io;
use std::os::unix::io::RawFd;

// From <linux/kd.h> and <linux/vt.h>.
const KDSETMODE: u64 = 0x4b3a;
const KDGETMODE: u64 = 0x4b3b;
const KDSKBMODE: u64 = 0x4b45;
const KDGKBMODE: u64 = 0x4b44;
const VT_GETMODE: u64 = 0x5601;
const VT_SETMODE: u64 = 0x5602;
const VT_GETSTATE: u64 = 0x5603;
const VT_ACTIVATE: u64 = 0x5606;
const VT_WAITACTIVE: u64 = 0x5607;
const VT_RELDISP: u64 = 0x5605;

const KD_TEXT: libc::c_long = 0x00;
const KD_GRAPHICS: libc::c_long = 0x01;
/// Raw scancodes with no translation; tOS reads evdev instead, so the console
/// keyboard is simply switched off.
const K_OFF: libc::c_long = 0x04;

const VT_AUTO: u8 = 0x00;
const VT_PROCESS: u8 = 0x01;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct VtMode {
    mode: u8,
    waitv: u8,
    relsig: i16,
    acqsig: i16,
    frsig: i16,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct VtState {
    v_active: u16,
    v_signal: u16,
    v_state: u16,
}

fn ioctl_ptr<T>(fd: RawFd, request: u64, arg: &mut T) -> io::Result<()> {
    if unsafe { libc::ioctl(fd, request as _, arg as *mut T) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn ioctl_value(fd: RawFd, request: u64, arg: libc::c_long) -> io::Result<()> {
    if unsafe { libc::ioctl(fd, request as _, arg) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// A virtual terminal tOS has taken over.
pub struct VirtualTerminal {
    fd: RawFd,
    number: u16,
    saved_kd_mode: libc::c_long,
    saved_kb_mode: libc::c_long,
    saved_vt_mode: VtMode,
    owned: bool,
}

impl VirtualTerminal {
    /// Open the VT this process is already running on.
    pub fn current() -> io::Result<VirtualTerminal> {
        VirtualTerminal::open("/dev/tty0")
    }

    pub fn open(path: &str) -> io::Result<VirtualTerminal> {
        let c_path = CString::new(path)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bad tty path"))?;
        let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }

        let mut state = VtState::default();
        let number = match ioctl_ptr(fd, VT_GETSTATE, &mut state) {
            Ok(()) => state.v_active,
            Err(_) => 0,
        };

        let mut saved_kd_mode: libc::c_long = KD_TEXT;
        let _ = ioctl_ptr(fd, KDGETMODE, &mut saved_kd_mode);
        let mut saved_kb_mode: libc::c_long = 0;
        let _ = ioctl_ptr(fd, KDGKBMODE, &mut saved_kb_mode);
        let mut saved_vt_mode = VtMode::default();
        let _ = ioctl_ptr(fd, VT_GETMODE, &mut saved_vt_mode);

        Ok(VirtualTerminal {
            fd,
            number,
            saved_kd_mode,
            saved_kb_mode,
            saved_vt_mode,
            owned: false,
        })
    }

    pub fn fd(&self) -> RawFd {
        self.fd
    }

    pub fn number(&self) -> u16 {
        self.number
    }

    /// Stop the kernel drawing text and reading the keyboard on this VT.
    ///
    /// `release` and `acquire` are the signals the kernel will send when the
    /// user switches away from and back to this terminal.
    pub fn take_over(&mut self, release: i32, acquire: i32) -> io::Result<()> {
        ioctl_value(self.fd, KDSETMODE, KD_GRAPHICS)?;
        // Losing this leaves the console unusable, so failure is fatal here.
        if let Err(e) = ioctl_value(self.fd, KDSKBMODE, K_OFF) {
            let _ = ioctl_value(self.fd, KDSETMODE, self.saved_kd_mode);
            return Err(e);
        }

        let mut mode = VtMode {
            mode: VT_PROCESS,
            waitv: 0,
            relsig: release as i16,
            acqsig: acquire as i16,
            frsig: 0,
        };
        if let Err(e) = ioctl_ptr(self.fd, VT_SETMODE, &mut mode) {
            let _ = ioctl_value(self.fd, KDSKBMODE, self.saved_kb_mode);
            let _ = ioctl_value(self.fd, KDSETMODE, self.saved_kd_mode);
            return Err(e);
        }
        self.owned = true;
        Ok(())
    }

    /// Agree to a VT switch away from tOS.
    pub fn allow_switch_away(&self) -> io::Result<()> {
        ioctl_value(self.fd, VT_RELDISP, 1)
    }

    /// Acknowledge coming back.
    pub fn acknowledge_switch_back(&self) -> io::Result<()> {
        ioctl_value(self.fd, VT_RELDISP, 2)
    }

    /// Switch to another virtual terminal.
    pub fn activate(&self, number: u16) -> io::Result<()> {
        ioctl_value(self.fd, VT_ACTIVATE, number as libc::c_long)?;
        ioctl_value(self.fd, VT_WAITACTIVE, number as libc::c_long)
    }

    /// Hand the terminal back to the kernel.
    pub fn restore(&mut self) {
        if !self.owned {
            return;
        }
        let mut mode = VtMode {
            mode: VT_AUTO,
            ..self.saved_vt_mode
        };
        let _ = ioctl_ptr(self.fd, VT_SETMODE, &mut mode);
        let _ = ioctl_value(self.fd, KDSKBMODE, self.saved_kb_mode);
        let _ = ioctl_value(self.fd, KDSETMODE, self.saved_kd_mode);
        self.owned = false;
    }
}

impl Drop for VirtualTerminal {
    fn drop(&mut self) {
        // Leaving a VT in graphics mode with the keyboard off would make the
        // machine look dead, so this must happen however tOS exits.
        self.restore();
        unsafe {
            libc::close(self.fd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_numbers_match_the_kernel_headers() {
        assert_eq!(KDSETMODE, 0x4b3a);
        assert_eq!(KDSKBMODE, 0x4b45);
        assert_eq!(VT_SETMODE, 0x5602);
        assert_eq!(VT_RELDISP, 0x5605);
    }

    #[test]
    fn vt_mode_matches_the_kernel_layout() {
        // struct vt_mode is char, char, short, short, short.
        assert_eq!(std::mem::size_of::<VtMode>(), 8);
        assert_eq!(std::mem::size_of::<VtState>(), 6);
    }
}
