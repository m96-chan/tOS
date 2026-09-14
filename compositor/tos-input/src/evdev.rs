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
    ioc(
        IOC_WRITE,
        b'E' as u64,
        0x90,
        std::mem::size_of::<libc::c_int>() as u64,
    )
}

/// The kernel's `struct input_absinfo`: value, minimum, maximum, fuzz, flat
/// and resolution, all `__s32`.
///
/// An array rather than six named fields because the ioctl needs a buffer of
/// the size the kernel writes and nothing here will ever look at four of them.
type AbsInfo = [i32; 6];
const ABS_MINIMUM: usize = 1;
const ABS_MAXIMUM: usize = 2;

fn eviocgabs(axis: u64) -> u64 {
    ioc(
        IOC_READ,
        b'E' as u64,
        0x40 + axis,
        std::mem::size_of::<AbsInfo>() as u64,
    )
}

/// The range one absolute axis reports in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AbsAxis {
    minimum: i32,
    maximum: i32,
}

/// What a device says about the two axes a pointer is made of.
///
/// Carried from the device an event was read from to the translation of that
/// event, because an absolute reading is a number in the device's own units
/// and means nothing without this.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct AbsoluteAxes {
    x: Option<AbsAxis>,
    y: Option<AbsAxis>,
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
    /// The range this device's absolute axes report in, for the devices that
    /// have them. Read once at open, because it does not change while the
    /// node is open and asking per event would be an ioctl per mouse move.
    absolute: AbsoluteAxes,
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
        let absolute = probe_absolute_axes(fd);
        let name = read_name(fd).unwrap_or_else(|| "unknown".to_string());
        Ok(Device {
            fd,
            path,
            name,
            capabilities,
            absolute,
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

    fn absolute_axes(&self) -> AbsoluteAxes {
        self.absolute
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
        let n = unsafe { libc::read(self.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
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
            let event =
                unsafe { std::ptr::read_unaligned(buf[offset..].as_ptr() as *const RawEvent) };
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

/// Ask a device what range its absolute axes report in.
///
/// A device with no absolute axes fails the ioctl and a device can answer with
/// a degenerate range; both are `None`, which the scaling reads as "no range
/// to scale by" rather than as an axis that is zero wide.
fn probe_absolute_axes(fd: RawFd) -> AbsoluteAxes {
    let axis = |code: u16| {
        let mut info = AbsInfo::default();
        let asked = unsafe { libc::ioctl(fd, eviocgabs(code as u64) as _, info.as_mut_ptr()) };
        let (minimum, maximum) = (info[ABS_MINIMUM], info[ABS_MAXIMUM]);
        (asked >= 0 && maximum > minimum).then_some(AbsAxis { minimum, maximum })
    };
    AbsoluteAxes {
        x: axis(ABS_X),
        y: axis(ABS_Y),
    }
}

/// Where an absolute reading lands on the panel.
///
/// A tablet reports 0..32767 whatever the display is, so its units are only a
/// position once they are read against the range they came from. Without a
/// range there is nothing to read them against and the raw value is all there
/// is; `clamp_pointer` then keeps it somewhere reachable, which is the weaker
/// thing this used to do for every device.
fn scale_absolute(value: i32, axis: Option<AbsAxis>, bound: f64) -> f64 {
    let Some(axis) = axis else {
        return value as f64;
    };
    let span = f64::from(axis.maximum) - f64::from(axis.minimum);
    if span <= 0.0 {
        return value as f64;
    }
    let along = (f64::from(value) - f64::from(axis.minimum)) / span;
    along * (bound - 1.0).max(0.0)
}

fn read_name(fd: RawFd) -> Option<String> {
    let mut buf = [0u8; 256];
    let n = unsafe { libc::ioctl(fd, eviocgname(buf.len() as u64) as _, buf.as_mut_ptr()) };
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
    /// The position an absolute report is building up, in display pixels,
    /// until the SYN that completes it.
    absolute: Option<(f64, f64)>,
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
            absolute: None,
        })
    }

    /// Take exclusive ownership of every device.
    pub fn grab_all(&mut self) -> io::Result<()> {
        for device in &mut self.devices {
            device.grab()?;
        }
        Ok(())
    }

    /// Give every device back.
    ///
    /// The other half of [`InputBackend::grab_all`], for the one thing that
    /// takes the machine away underneath a running session: a suspend. An
    /// `EVIOCGRAB` is not something the kernel revokes, so this is not about
    /// losing it — it is about what an exclusive grab means while nobody is
    /// reading. A session that is asleep, or one whose suspend failed halfway,
    /// holding every keyboard on the machine and draining none of them is a
    /// machine that looks dead to everything else that could have been asked
    /// for help.
    ///
    /// Every device is tried whatever the ones before it did, which is the
    /// opposite of [`InputBackend::grab_all`]: stopping at the first refusal
    /// on the way in leaves nothing half owned, and stopping on the way out
    /// would leave the rest of the keyboards grabbed by a process that has
    /// already decided to let go of them.
    pub fn ungrab_all(&mut self) -> io::Result<()> {
        let mut failure = None;
        for device in &mut self.devices {
            if let Err(e) = device.ungrab() {
                failure.get_or_insert(e);
            }
        }
        match failure {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Forget which keys and buttons were held.
    ///
    /// A suspend is the one place where a press can be delivered and its
    /// release cannot: the key that asked for it is let go while the machine
    /// is asleep, with no driver awake to report it. A modifier remembered as
    /// held after that turns every later keystroke into a binding, and a
    /// button remembered as down turns the next mouse move into a drag that
    /// selects half the screen.
    pub fn forget_held_keys(&mut self) {
        self.modifiers = Modifiers::NONE;
        self.modifier_keys.clear();
        self.buttons_down = 0;
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

    /// The position the absolute report being assembled describes so far.
    ///
    /// Where the pointer already is until an axis of it arrives, because a
    /// device leaves an unchanged axis out of its report and the axis it left
    /// out has not moved.
    fn pending_absolute(&self) -> (f64, f64) {
        self.absolute.unwrap_or((self.pointer.x, self.pointer.y))
    }

    /// Read and translate all pending input.
    ///
    /// One device at a time, read and then translated, rather than every
    /// device read into one queue that is translated afterwards. An absolute
    /// reading is in the device's own units and `EVIOCGABS` is per device, so
    /// the translation needs to know which device an event came from — and a
    /// merged queue has nothing left on an event that says. That is what made
    /// scaling look impossible from here; it was only ever thrown away.
    pub fn poll(&mut self) -> io::Result<Vec<InputEvent>> {
        let mut raw = std::mem::take(&mut self.pending);
        let mut events = Vec::new();
        for index in 0..self.devices.len() {
            raw.clear();
            self.devices[index].read_raw(&mut raw)?;
            let axes = self.devices[index].absolute_axes();
            for event in &raw {
                self.translate(*event, axes, &mut events);
            }
        }
        raw.clear();
        self.pending = raw;
        Ok(events)
    }

    fn translate(&mut self, raw: RawEvent, axes: AbsoluteAxes, out: &mut Vec<InputEvent>) {
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
                // Held until the sync rather than written through to the
                // pointer as it arrives. The two axes are two events, so
                // moving on the first would take the pointer out through a
                // corner and back on every report, and the position a report
                // describes is not a position until both halves of it are in.
                ABS_X => {
                    let (_, y) = self.pending_absolute();
                    self.absolute = Some((scale_absolute(raw.value, axes.x, self.bounds.0), y));
                }
                ABS_Y => {
                    let (x, _) = self.pending_absolute();
                    self.absolute = Some((x, scale_absolute(raw.value, axes.y, self.bounds.1)));
                }
                _ => {}
            },
            // A report is only complete at the sync, and only worth sending
            // when the pointer actually moved.
            EV_SYN => {
                let before = (self.pointer.x, self.pointer.y);
                if let Some((x, y)) = self.absolute.take() {
                    self.pointer.x = x;
                    self.pointer.y = y;
                }
                let relative = self.motion != (0.0, 0.0);
                if relative {
                    self.pointer.x += self.motion.0;
                    self.pointer.y += self.motion.1;
                    self.motion = (0.0, 0.0);
                }
                self.clamp_pointer();
                // A delta is a movement whether or not the clamp let the
                // pointer follow it: the arrow is put away while somebody is
                // typing and a motion event is the thing that brings it back,
                // so a mouse pushed into the edge of the panel has to keep
                // speaking. An absolute device instead says where it is, over
                // and over and mostly unchanged, and that is worth passing on
                // only when it differs from where the pointer already was.
                if relative || (self.pointer.x, self.pointer.y) != before {
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
            crate::event::KeyCode::Keypad(key) if !self.modifiers.contains(Modifiers::NUM_LOCK) => {
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
    use crate::event::{KeyCode as K, Keypad};
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
        // EVIOCGABS(ABS_X) is _IOR('E', 0x40, struct input_absinfo), and that
        // struct is six __s32, so 24 bytes.
        assert_eq!(eviocgabs(ABS_X as u64), 0x8018_4540);
        assert_eq!(eviocgabs(ABS_Y as u64), 0x8018_4541);
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

    /// A backend with no devices open, for the translation that needs none.
    fn backend(width: u32, height: u32) -> InputBackend {
        InputBackend {
            devices: Vec::new(),
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
            absolute: None,
        }
    }

    fn raw(kind: u16, code: u16, value: i32) -> RawEvent {
        RawEvent {
            tv_sec: 0,
            tv_usec: 0,
            kind,
            code,
            value,
        }
    }

    /// What a VirtualBox USB tablet says about itself: both axes 0..32767,
    /// whatever the panel underneath them is.
    fn tablet() -> AbsoluteAxes {
        let axis = Some(AbsAxis {
            minimum: 0,
            maximum: 32767,
        });
        AbsoluteAxes { x: axis, y: axis }
    }

    /// One move of a device that reports where it is: the two axes, then the
    /// sync that completes them.
    fn report(backend: &mut InputBackend, axes: AbsoluteAxes, x: i32, y: i32) -> Vec<InputEvent> {
        let mut out = Vec::new();
        backend.translate(raw(EV_ABS, ABS_X, x), axes, &mut out);
        backend.translate(raw(EV_ABS, ABS_Y, y), axes, &mut out);
        backend.translate(raw(EV_SYN, 0, 0), axes, &mut out);
        out
    }

    fn pointer_of(event: &InputEvent) -> PointerEvent {
        match event {
            InputEvent::Pointer(pointer) => *pointer,
            other => panic!("expected a pointer event, got {other:?}"),
        }
    }

    #[test]
    fn an_absolute_report_emits_one_motion_event() {
        // The bug this arm had: `EV_ABS` wrote the position down and nothing
        // ever sent it on, because the only arm that emitted motion was
        // guarded on the relative delta, which an absolute device never sets.
        // Every machine whose pointing device is absolute — a VirtualBox guest
        // with its default `usbtablet`, a touchscreen — had a pointer that
        // could not be seen, because `Pointer::seen` is set by an event that
        // arrived and no event ever arrived.
        //
        // Asserting on the events rather than on `pointer_position` is the
        // whole point of this test. The position was always right; the test
        // that only read it passed the entire time this was broken.
        let mut backend = backend(1920, 1080);
        let out = report(&mut backend, tablet(), 16000, 8000);
        assert_eq!(out.len(), 1);
        assert_eq!(pointer_of(&out[0]).action, MouseAction::Motion);
        assert_eq!(pointer_of(&out[0]).button, None);
    }

    #[test]
    fn an_absolute_position_is_scaled_by_the_range_the_device_reports_in() {
        // 0..32767 across a 1920x1080 panel. The ends are the ends, and the
        // middle of the range is the middle of the panel — rather than, as it
        // was, the far corner, which is where every reading above 1919 landed
        // once the clamp had finished with it.
        let mut backend = backend(1920, 1080);
        report(&mut backend, tablet(), 0, 0);
        assert_eq!(backend.pointer_position(), (0.0, 0.0));

        report(&mut backend, tablet(), 32767, 32767);
        assert_eq!(backend.pointer_position(), (1919.0, 1079.0));

        report(&mut backend, tablet(), 16383, 16383);
        let (x, y) = backend.pointer_position();
        assert!((x - 959.5).abs() < 1.0, "x was {x}");
        assert!((y - 539.5).abs() < 1.0, "y was {y}");
    }

    #[test]
    fn an_absolute_report_that_leaves_an_axis_out_leaves_it_where_it_was() {
        // evdev omits an axis that did not change, and an axis left out of a
        // report has not moved. Seeding the missing half from where the
        // pointer already is, rather than from zero, is what keeps a move
        // along one axis a move along one axis.
        let mut backend = backend(1920, 1080);
        report(&mut backend, tablet(), 16383, 16383);
        let (_, y) = backend.pointer_position();

        let mut out = Vec::new();
        backend.translate(raw(EV_ABS, ABS_X, 32767), tablet(), &mut out);
        backend.translate(raw(EV_SYN, 0, 0), tablet(), &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(backend.pointer_position(), (1919.0, y));
    }

    #[test]
    fn a_sync_with_nothing_new_says_nothing() {
        // A device that reports where it is does so over and over and mostly
        // unchanged. A motion event for every one of those would be a frame
        // asked for per report for a pointer that is sitting still.
        let mut backend = backend(1920, 1080);
        report(&mut backend, tablet(), 16000, 8000);
        assert!(report(&mut backend, tablet(), 16000, 8000).is_empty());

        let mut out = Vec::new();
        backend.translate(raw(EV_SYN, 0, 0), tablet(), &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn an_absolute_report_with_a_button_held_is_a_drag() {
        let mut backend = backend(1920, 1080);
        let mut out = Vec::new();
        backend.translate(raw(EV_KEY, BTN_LEFT, 1), tablet(), &mut out);
        let moved = report(&mut backend, tablet(), 16000, 8000);
        assert_eq!(pointer_of(&moved[0]).action, MouseAction::Drag);
    }

    #[test]
    fn a_relative_mouse_pushed_into_the_edge_goes_on_reporting() {
        // A delta is a movement whether or not the clamp let the pointer
        // follow it. The arrow is put away while somebody types and a motion
        // event is what brings it back, so a mouse held against the edge of
        // the panel that fell silent would be an arrow nobody could get back
        // without first moving away from the edge.
        let mut backend = backend(1920, 1080);
        let none = AbsoluteAxes::default();
        let mut out = Vec::new();
        for _ in 0..4 {
            backend.translate(raw(EV_REL, REL_X, 2000), none, &mut out);
            backend.translate(raw(EV_SYN, 0, 0), none, &mut out);
        }
        assert_eq!(backend.pointer_position().0, 1919.0);
        assert_eq!(out.len(), 4);
    }

    #[test]
    fn an_absolute_position_stays_on_the_panel_when_there_is_no_range_to_scale_by() {
        // `EVIOCGABS` can fail, and a device can answer with a degenerate
        // range. With nothing to scale by, the raw value is all there is — and
        // a tablet's 0..32767 taken raw is thousands of pixels past the edge
        // of any panel. Off the panel the pointer is not merely in the wrong
        // place: the arrow clips away to nothing and a press lands on no pane,
        // so the device stops doing anything at all. The edge is not where the
        // finger was, but it is somewhere reachable, and it is no further out
        // than the raw value already was.
        let mut backend = backend(1920, 1080);
        let none = AbsoluteAxes::default();
        report(&mut backend, none, 32767, 32767);
        assert_eq!(backend.pointer_position(), (1919.0, 1079.0));

        // And a device that reports below its own minimum is the same case the
        // other way up.
        report(&mut backend, none, -40, -40);
        assert_eq!(backend.pointer_position(), (0.0, 0.0));
    }

    #[test]
    fn mouse_buttons_are_recognised() {
        assert_eq!(mouse_button(BTN_LEFT), Some(MouseButton::Left));
        assert_eq!(mouse_button(BTN_RIGHT), Some(MouseButton::Right));
        assert_eq!(mouse_button(30), None);
    }
}
