//! Reading input directly from the kernel.
//!
//! tOS talks to evdev rather than to libinput so that the first milestone has
//! no userspace input stack under it at all. Devices are opened from
//! `/dev/input`, grabbed so the virtual terminal does not also react to them,
//! and read as raw `input_event` structs.

use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};

use crate::event::{
    InputEvent, KeyEvent, KeyState, Modifiers, MouseAction, MouseButton, PointerEvent,
};
use crate::keymap;

// Event types from <linux/input-event-codes.h>.
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;

const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;

const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;

const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;
const BTN_SIDE: u16 = 0x113;
const BTN_EXTRA: u16 = 0x114;
const BTN_TOUCH: u16 = 0x14a;

/// The kernel's `struct input_event`.
///
/// The timestamp is a kernel `struct timeval`, whose fields are kernel longs.
/// `libc::time_t` and `libc::suseconds_t` are deprecated on musl because
/// musl's own definitions changed width; the kernel's did not, and it is the
/// kernel that writes these bytes.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct RawEvent {
    tv_sec: libc::c_long,
    tv_usec: libc::c_long,
    kind: u16,
    code: u16,
    value: i32,
}

const EVENT_SIZE: usize = std::mem::size_of::<RawEvent>();

/// Build an ioctl request number the way `<asm/ioctl.h>` does.
const fn ioc(dir: u64, kind: u64, nr: u64, size: u64) -> u64 {
    (dir << 30) | (size << 16) | (kind << 8) | nr
}

const IOC_READ: u64 = 2;
const IOC_WRITE: u64 = 1;

fn eviocgbit(ev: u64, len: u64) -> u64 {
    ioc(IOC_READ, b'E' as u64, 0x20 + ev, len)
}

fn eviocgname(len: u64) -> u64 {
    ioc(IOC_READ, b'E' as u64, 0x06, len)
}

fn eviocgrab() -> u64 {
    ioc(IOC_WRITE, b'E' as u64, 0x90, std::mem::size_of::<libc::c_int>() as u64)
}

/// What a device can produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Capabilities {
    pub keyboard: bool,
    pub pointer: bool,
    pub touch: bool,
}

/// One opened input device.
#[derive(Debug)]
pub struct Device {
    fd: RawFd,
    path: PathBuf,
    name: String,
    capabilities: Capabilities,
    grabbed: bool,
}

impl Device {
    /// Open a device node and work out what it can do.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Device> {
        let path = path.as_ref().to_path_buf();
        let c_path = CString::new(path.as_os_str().to_string_lossy().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bad device path"))?;
        // Without O_CLOEXEC every process started in a pane would inherit a
        // readable descriptor on every keyboard in the machine.
        let fd = unsafe {
            libc::open(
                c_path.as_ptr(),
                libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }

        let capabilities = probe_capabilities(fd);
        let name = read_name(fd).unwrap_or_else(|| "unknown".to_string());
        Ok(Device {
            fd,
            path,
            name,
            capabilities,
            grabbed: false,
        })
    }

    pub fn fd(&self) -> RawFd {
        self.fd
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    /// Take exclusive ownership, so the kernel's own terminal does not also
    /// receive these events.
    pub fn grab(&mut self) -> io::Result<()> {
        let one: libc::c_int = 1;
        if unsafe { libc::ioctl(self.fd, eviocgrab() as _, &one) } < 0 {
            return Err(io::Error::last_os_error());
        }
        self.grabbed = true;
        Ok(())
    }

    pub fn ungrab(&mut self) -> io::Result<()> {
        let zero: libc::c_int = 0;
        if unsafe { libc::ioctl(self.fd, eviocgrab() as _, &zero) } < 0 {
            return Err(io::Error::last_os_error());
        }
        self.grabbed = false;
        Ok(())
    }

    /// Read whatever events are pending.
    fn read_raw(&mut self, out: &mut Vec<RawEvent>) -> io::Result<usize> {
        let mut buf = [0u8; EVENT_SIZE * 64];
        let n = unsafe {
            libc::read(
                self.fd,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
            )
        };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::WouldBlock {
                return Ok(0);
            }
            return Err(err);
        }
        let count = n as usize / EVENT_SIZE;
        for i in 0..count {
            let offset = i * EVENT_SIZE;
            // The kernel guarantees alignment and layout of these structs.
            let event = unsafe {
                std::ptr::read_unaligned(buf[offset..].as_ptr() as *const RawEvent)
            };
            out.push(event);
        }
        Ok(count)
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        if self.grabbed {
            let _ = self.ungrab();
        }
        unsafe {
            libc::close(self.fd);
        }
    }
}

fn probe_capabilities(fd: RawFd) -> Capabilities {
    let mut caps = Capabilities::default();
    let mut keys = [0u8; 96]; // enough to cover BTN_ codes up to 0x2ff
    let len = keys.len() as u64;
    if unsafe { libc::ioctl(fd, eviocgbit(EV_KEY as u64, len) as _, keys.as_mut_ptr()) } >= 0 {
        let has = |code: u16| {
            let index = (code / 8) as usize;
            index < keys.len() && keys[index] & (1 << (code % 8)) != 0
        };
        // A keyboard is anything that reports ordinary letter keys.
        caps.keyboard = has(30) && has(31); // KEY_A and KEY_S
        caps.pointer = has(BTN_LEFT);
        caps.touch = has(BTN_TOUCH);
    }
    let mut rel = [0u8; 4];
    if unsafe { libc::ioctl(fd, eviocgbit(EV_REL as u64, 4) as _, rel.as_mut_ptr()) } >= 0
        && rel[0] & 0b11 == 0b11
    {
        caps.pointer = true;
    }
    caps
}

fn read_name(fd: RawFd) -> Option<String> {
    let mut buf = [0u8; 256];
    let n = unsafe {
        libc::ioctl(
            fd,
            eviocgname(buf.len() as u64) as _,
            buf.as_mut_ptr(),
        )
    };
    if n <= 0 {
        return None;
    }
    let bytes = &buf[..(n as usize).saturating_sub(1)];
    Some(String::from_utf8_lossy(bytes).into_owned())
}

/// Absolute pointer position, in pixels on the display.
#[derive(Debug, Clone, Copy, Default)]
struct Pointer {
    x: f64,
    y: f64,
}

/// All input devices, read as one stream.
pub struct InputBackend {
    devices: Vec<Device>,
    modifiers: Modifiers,
    /// Which physical modifier keys are down, so releasing one of a pair does
    /// not clear the modifier while the other is still held.
    modifier_keys: Vec<crate::event::ModifierKey>,
    pointer: Pointer,
    bounds: (f64, f64),
    /// Buttons currently held, so motion can be reported as a drag.
    buttons_down: u8,
    pending: Vec<RawEvent>,
    /// Pointer movement accumulated until the next SYN.
    motion: (f64, f64),
}

impl InputBackend {
    /// Open every usable device under `/dev/input`.
    pub fn open_all(width: u32, height: u32) -> io::Result<InputBackend> {
        let mut devices = Vec::new();
        for entry in fs::read_dir("/dev/input")? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with("event") {
                continue;
            }
            match Device::open(entry.path()) {
                Ok(device) => {
                    let caps = device.capabilities();
                    if caps.keyboard || caps.pointer || caps.touch {
                        devices.push(device);
                    }
                }
                // A device tOS cannot open is not fatal; others may still work.
                Err(_) => continue,
            }
        }
        if devices.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no usable input devices in /dev/input",
            ));
        }
        Ok(InputBackend {
            devices,
            modifiers: Modifiers::NONE,
            modifier_keys: Vec::new(),
            pointer: Pointer {
                x: width as f64 / 2.0,
                y: height as f64 / 2.0,
            },
            bounds: (width as f64, height as f64),
            buttons_down: 0,
            pending: Vec::new(),
            motion: (0.0, 0.0),
        })
    }

    /// Take exclusive ownership of every device.
    pub fn grab_all(&mut self) -> io::Result<()> {
        for device in &mut self.devices {
            device.grab()?;
        }
        Ok(())
    }

    pub fn devices(&self) -> &[Device] {
        &self.devices
    }

    /// File descriptors to poll for readiness.
    pub fn fds(&self) -> Vec<RawFd> {
        self.devices.iter().map(|d| d.fd()).collect()
    }

    pub fn modifiers(&self) -> Modifiers {
        self.modifiers
    }

    pub fn pointer_position(&self) -> (f64, f64) {
        (self.pointer.x, self.pointer.y)
    }

    /// The display size the pointer is confined to.
    pub fn set_bounds(&mut self, width: u32, height: u32) {
        self.bounds = (width as f64, height as f64);
        self.clamp_pointer();
    }

    fn clamp_pointer(&mut self) {
        self.pointer.x = self.pointer.x.clamp(0.0, (self.bounds.0 - 1.0).max(0.0));
        self.pointer.y = self.pointer.y.clamp(0.0, (self.bounds.1 - 1.0).max(0.0));
    }

    /// Read and translate all pending input.
    pub fn poll(&mut self) -> io::Result<Vec<InputEvent>> {
        self.pending.clear();
        for device in &mut self.devices {
            let mut raw = Vec::new();
            device.read_raw(&mut raw)?;
            self.pending.append(&mut raw);
        }
        let pending = std::mem::take(&mut self.pending);
        let mut events = Vec::new();
        for raw in &pending {
            self.translate(*raw, &mut events);
        }
        self.pending = pending;
        self.pending.clear();
        Ok(events)
    }

    fn translate(&mut self, raw: RawEvent, out: &mut Vec<InputEvent>) {
        match raw.kind {
            EV_KEY => self.translate_key(raw, out),
            EV_REL => match raw.code {
                REL_X => self.motion.0 += raw.value as f64,
                REL_Y => self.motion.1 += raw.value as f64,
                REL_WHEEL => {
                    let button = if raw.value > 0 {
                        MouseButton::WheelUp
                    } else {
                        MouseButton::WheelDown
                    };
                    for _ in 0..raw.value.unsigned_abs() {
                        out.push(self.mouse_event(Some(button), MouseAction::Press));
                    }
                }
                REL_HWHEEL => {
                    let button = if raw.value > 0 {
                        MouseButton::WheelRight
                    } else {
                        MouseButton::WheelLeft
                    };
                    for _ in 0..raw.value.unsigned_abs() {
                        out.push(self.mouse_event(Some(button), MouseAction::Press));
                    }
                }
                _ => {}
            },
            EV_ABS => match raw.code {
                // Touch panels report absolute positions in their own units;
                // without the device's axis range this is a direct mapping.
                ABS_X => self.pointer.x = raw.value as f64,
                ABS_Y => self.pointer.y = raw.value as f64,
                _ => {}
            },
            EV_SYN => {
                if self.motion != (0.0, 0.0) {
                    self.pointer.x += self.motion.0;
                    self.pointer.y += self.motion.1;
                    self.clamp_pointer();
                    self.motion = (0.0, 0.0);
                    let action = if self.buttons_down > 0 {
                        MouseAction::Drag
                    } else {
                        MouseAction::Motion
                    };
                    out.push(self.mouse_event(None, action));
                }
            }
            _ => {}
        }
    }

    fn translate_key(&mut self, raw: RawEvent, out: &mut Vec<InputEvent>) {
        if let Some(button) = mouse_button(raw.code) {
            let action = if raw.value == 0 {
                self.buttons_down = self.buttons_down.saturating_sub(1);
                MouseAction::Release
            } else {
                self.buttons_down += 1;
                MouseAction::Press
            };
            out.push(self.mouse_event(Some(button), action));
            return;
        }

        let Some(mapping) = keymap::lookup(raw.code) else {
            return;
        };
        let state = match raw.value {
            0 => KeyState::Release,
            2 => KeyState::Repeat,
            _ => KeyState::Press,
        };

        // Modifier keys update the shared state and are still reported, so the
        // Kitty protocol can see them.
        if let crate::event::KeyCode::ModifierKey(key) = mapping.code {
            if state == KeyState::Release {
                self.modifier_keys.retain(|held| *held != key);
            } else if !self.modifier_keys.contains(&key) {
                self.modifier_keys.push(key);
            }
            // Rebuild from the keys actually held: left and right shift share
            // one modifier bit, and releasing one must not clear it.
            let mut modifiers = Modifiers::NONE;
            for held in &self.modifier_keys {
                modifiers.insert(held.modifier());
            }
            for lock in [Modifiers::CAPS_LOCK, Modifiers::NUM_LOCK] {
                modifiers.set(lock, self.modifiers.contains(lock));
            }
            self.modifiers = modifiers;
        }
        if mapping.code == crate::event::KeyCode::CapsLock && state == KeyState::Press {
            let on = self.modifiers.contains(Modifiers::CAPS_LOCK);
            self.modifiers.set(Modifiers::CAPS_LOCK, !on);
        }
        if mapping.code == crate::event::KeyCode::NumLock && state == KeyState::Press {
            let on = self.modifiers.contains(Modifiers::NUM_LOCK);
            self.modifiers.set(Modifiers::NUM_LOCK, !on);
        }

        // With num lock off the keypad is a navigation cluster, which is what
        // the labels on the keys say and what applications expect.
        let code = match mapping.code {
            crate::event::KeyCode::Keypad(key)
                if !self.modifiers.contains(Modifiers::NUM_LOCK) =>
            {
                keypad_navigation(key).unwrap_or(mapping.code)
            }
            other => other,
        };
        let text = if code == mapping.code {
            mapping.character(
                self.modifiers.contains(Modifiers::SHIFT),
                self.modifiers.contains(Modifiers::CAPS_LOCK),
            )
        } else {
            // A navigation key produces no text.
            None
        };

        let mut event = KeyEvent::new(code, self.modifiers).with_state(state);
        event.text = text;
        event.base = mapping.plain;
        out.push(InputEvent::Key(event));
    }

    fn mouse_event(&self, button: Option<MouseButton>, action: MouseAction) -> InputEvent {
        // A device knows pixels and nothing about cells, so this is a pointer
        // event; the compositor converts it once it knows the font metrics.
        InputEvent::Pointer(PointerEvent {
            button,
            action,
            x: self.pointer.x,
            y: self.pointer.y,
            modifiers: self.modifiers,
        })
    }
}

/// What a keypad key means when num lock is off.
fn keypad_navigation(key: crate::event::Keypad) -> Option<crate::event::KeyCode> {
    use crate::event::{Keypad, KeyCode as K};
    Some(match key {
        Keypad::Digit(0) => K::Insert,
        Keypad::Digit(1) => K::End,
        Keypad::Digit(2) => K::Down,
        Keypad::Digit(3) => K::PageDown,
        Keypad::Digit(4) => K::Left,
        Keypad::Digit(6) => K::Right,
        Keypad::Digit(7) => K::Home,
        Keypad::Digit(8) => K::Up,
        Keypad::Digit(9) => K::PageUp,
        Keypad::Digit(5) => K::Keypad(Keypad::Begin),
        Keypad::Decimal => K::Delete,
        // The arithmetic keys and enter mean the same either way.
        _ => return None,
    })
}

fn mouse_button(code: u16) -> Option<MouseButton> {
    Some(match code {
        BTN_LEFT | BTN_TOUCH => MouseButton::Left,
        BTN_RIGHT => MouseButton::Right,
        BTN_MIDDLE => MouseButton::Middle,
        BTN_SIDE => MouseButton::Other(8),
        BTN_EXTRA => MouseButton::Other(9),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_numbers_match_the_kernel_macros() {
        // EVIOCGRAB is _IOW('E', 0x90, int), which is 0x40044590 on Linux.
        assert_eq!(eviocgrab(), 0x4004_4590);
        // EVIOCGNAME(256) is _IOC(_IOC_READ, 'E', 0x06, 256).
        assert_eq!(eviocgname(256), 0x8100_4506);
    }

    #[test]
    fn event_struct_matches_the_kernel_size() {
        // Two kernel longs of timeval plus two u16 and an i32.
        assert_eq!(EVENT_SIZE, std::mem::size_of::<libc::c_long>() * 2 + 8);
        // On the 64-bit targets tOS runs on, that is 24 bytes.
        #[cfg(target_pointer_width = "64")]
        assert_eq!(EVENT_SIZE, 24);
    }

    #[test]
    fn devices_are_opened_close_on_exec() {
        // Not a runtime check, but the flag must stay in the open call: a
        // child inheriting these descriptors could read every keystroke.
        let source = include_str!("evdev.rs");
        assert!(source.contains("libc::O_CLOEXEC"));
    }

    #[test]
    fn the_keypad_is_a_navigation_cluster_without_num_lock() {
        use crate::event::{KeyCode, Keypad};
        assert_eq!(keypad_navigation(Keypad::Digit(8)), Some(KeyCode::Up));
        assert_eq!(keypad_navigation(Keypad::Digit(1)), Some(KeyCode::End));
        assert_eq!(keypad_navigation(Keypad::Decimal), Some(KeyCode::Delete));
        // Arithmetic keys are the same either way.
        assert_eq!(keypad_navigation(Keypad::Add), None);
        assert_eq!(keypad_navigation(Keypad::Enter), None);
    }

    #[test]
    fn mouse_buttons_are_recognised() {
        assert_eq!(mouse_button(BTN_LEFT), Some(MouseButton::Left));
        assert_eq!(mouse_button(BTN_RIGHT), Some(MouseButton::Right));
        assert_eq!(mouse_button(30), None);
    }
}
