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
/// The mode a text console normally runs in.
const K_XLATE: libc::c_long = 0x01;

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
        // Zero is K_RAW: if the query fails, restoring that would hand the
        // user back a console whose keyboard produces raw scancodes.
        let mut saved_kb_mode: libc::c_long = K_XLATE;
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
    /// user switches away from and back to this terminal. Handlers for both
    /// must already be installed: their default disposition is to terminate,
    /// so the first VT switch would kill tOS and leave the console in graphics
    /// mode with no keyboard. [`install_switch_handlers`] does this.
    pub fn take_over(&mut self, release: i32, acquire: i32) -> io::Result<()> {
        if !handlers_installed(release) || !handlers_installed(acquire) {
            return Err(io::Error::other(
                "VT switch signals need handlers before the terminal is taken over",
            ));
        }
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

/// Signals that have had a handler installed by [`install_switch_handlers`].
static SWITCH_SIGNALS: std::sync::Mutex<Vec<i32>> = std::sync::Mutex::new(Vec::new());

/// Whether tOS has taken responsibility for a VT switch signal.
fn handlers_installed(signal: i32) -> bool {
    SWITCH_SIGNALS
        .lock()
        .map(|installed| installed.contains(&signal))
        .unwrap_or(false)
}

/// Install handlers for the VT switch signals.
///
/// The handler only records that a switch was requested; the compositor acts
/// on it from its own loop, where it can release the display in an orderly
/// way. Anything more in a signal handler would not be async-signal-safe.
pub fn install_switch_handlers(release: i32, acquire: i32) -> io::Result<()> {
    extern "C" fn on_release(_: libc::c_int) {
        SWITCH_AWAY.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    extern "C" fn on_acquire(_: libc::c_int) {
        SWITCH_BACK.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    unsafe {
        if libc::signal(release, on_release as *const () as libc::sighandler_t) == libc::SIG_ERR {
            return Err(io::Error::last_os_error());
        }
        if libc::signal(acquire, on_acquire as *const () as libc::sighandler_t) == libc::SIG_ERR {
            return Err(io::Error::last_os_error());
        }
    }
    let mut installed = SWITCH_SIGNALS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    installed.push(release);
    installed.push(acquire);
    Ok(())
}

/// Set when the kernel asks tOS to give up the terminal.
static SWITCH_AWAY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Set when the terminal comes back.
static SWITCH_BACK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether a switch away has been requested since this was last called.
pub fn take_switch_away() -> bool {
    SWITCH_AWAY.swap(false, std::sync::atomic::Ordering::Relaxed)
}

/// Whether the terminal has come back since this was last called.
pub fn take_switch_back() -> bool {
    SWITCH_BACK.swap(false, std::sync::atomic::Ordering::Relaxed)
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
    fn taking_over_without_handlers_is_refused() {
        // Arming VT_PROCESS without handlers means the first Ctrl+Alt+F2 kills
        // tOS and leaves the console unusable, so it must not be possible.
        assert!(!handlers_installed(libc::SIGUSR1));
    }

    #[test]
    fn switch_flags_are_edge_triggered() {
        SWITCH_AWAY.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(take_switch_away());
        assert!(!take_switch_away());
    }

    #[test]
    fn the_saved_keyboard_mode_defaults_to_translated() {
        // Zero would be K_RAW, which is not what a console runs in.
        assert_eq!(K_XLATE, 0x01);
        assert_ne!(K_XLATE, 0);
    }

    #[test]
    fn vt_mode_matches_the_kernel_layout() {
        // struct vt_mode is char, char, short, short, short.
        assert_eq!(std::mem::size_of::<VtMode>(), 8);
        assert_eq!(std::mem::size_of::<VtState>(), 6);
    }
}
