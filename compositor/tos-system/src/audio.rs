//! Sound: which card is playing, how loud, and whether it is muted.
//!
//! There is no libasound here. The kernel's mixer is a character device,
//! `/dev/snd/controlC<N>`, driven entirely by ioctls, and this module is the
//! client side of that interface written out by hand — the same choice
//! `tos-platform`'s DRM backend makes about `drm_mode.h`, for the same
//! reason: the dependency surface of tOS stays at libc.
//!
//! The structure layouts below come from `include/uapi/sound/asound.h`. They
//! are `#[repr(C)]`, their sizes are asserted against the kernel's in the
//! tests, and nothing about them may drift: the kernel encodes the size of
//! each structure into the ioctl number, so a layout that is wrong does not
//! misbehave quietly, it fails with `ENOTTY`.
//!
//! Everything that reaches the device goes through [`Control`], so the part
//! worth testing — finding the right element, converting a raw level into a
//! percentage, deciding what "muted" means on a card with no mute switch —
//! runs against a fake that answers with structures a test built, on a
//! machine with no sound hardware at all.

use std::cell::Cell;
use std::io;
use std::path::Path;

use crate::sysfs::Sysfs;

#[cfg(target_os = "linux")]
use std::os::unix::io::AsRawFd;

// ---------------------------------------------------------------------------
// The structures, from include/uapi/sound/asound.h
// ---------------------------------------------------------------------------

/// `long` is what the control value union is made of, and it is the only
/// thing in these structures whose width follows the architecture. Every
/// array length that depends on it is written in terms of this, so the
/// layouts stay right on a 32 bit kernel even though tOS does not ship one.
// Every layout below was worked out for a 64 bit kernel, and the size
// assertions that guard them only run there. A 32 bit one lays
// `snd_ctl_elem_value` out differently: the union is eight byte aligned either
// way, because of `long long`, but `long value[128]` is half the size. Rather
// than hand the kernel a structure of the wrong size and find out through
// ENOTTY, refuse to build.
#[cfg(not(target_pointer_width = "64"))]
compile_error!("the ALSA structure layouts here are worked out for 64 bit kernels only");

const LONG: usize = std::mem::size_of::<libc::c_long>();

/// `snd_ctl_elem_value`'s union holds this many `long`s, so a control with
/// more channels than this could not be read at all.
const MAX_VALUES: usize = 128;

/// A card claiming more control elements than this is not answering sensibly.
/// Real cards have tens; the largest professional interfaces have hundreds.
const MAX_ELEMENTS: u32 = 4096;

/// `SNDRV_CTL_ELEM_IFACE_MIXER`. Playback volume lives on the mixer
/// interface; the same name can appear on the PCM interface meaning
/// something else entirely.
const IFACE_MIXER: i32 = 2;

/// `SNDRV_CTL_ELEM_ACCESS_READ` and `_WRITE`.
const ACCESS_READ: u32 = 1 << 0;
const ACCESS_WRITE: u32 = 1 << 1;

/// `struct snd_ctl_card_info`. 376 bytes on every architecture: no pointers
/// and no `long`s, so nothing here moves.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct CardInfo {
    card: i32,
    _pad: i32,
    id: [u8; 16],
    driver: [u8; 16],
    name: [u8; 32],
    longname: [u8; 80],
    _reserved: [u8; 16],
    mixername: [u8; 80],
    components: [u8; 128],
}

impl Default for CardInfo {
    fn default() -> CardInfo {
        // Every field is a plain integer or a byte array, so all zeroes is
        // the empty structure the kernel expects to be handed.
        unsafe { std::mem::zeroed() }
    }
}

impl CardInfo {
    /// The structure as the kernel would have filled it in. Public because a
    /// fake [`Control`] has to be able to answer with one.
    pub fn describing(card: i32, id: &str, name: &str) -> io::Result<CardInfo> {
        let mut info = CardInfo {
            card,
            ..CardInfo::default()
        };
        encode_name(id, &mut info.id)?;
        encode_name(name, &mut info.name)?;
        encode_name(name, &mut info.longname)?;
        Ok(info)
    }

    pub fn number(&self) -> i32 {
        self.card
    }

    /// The short name, such as `HDA Intel PCH`.
    pub fn name(&self) -> io::Result<String> {
        decode_name(&self.name)
    }

    /// The user-selectable identifier, such as `PCH`.
    pub fn id(&self) -> io::Result<String> {
        decode_name(&self.id)
    }

    pub fn driver(&self) -> io::Result<String> {
        decode_name(&self.driver)
    }

    /// Name plus whatever the driver wanted to add about the hardware.
    pub fn longname(&self) -> io::Result<String> {
        decode_name(&self.longname)
    }

    pub fn mixer_name(&self) -> io::Result<String> {
        decode_name(&self.mixername)
    }

    /// Space separated list of what the card is made of, such as
    /// `HDA:10ec0257`. Useful only for telling two identical cards apart.
    pub fn components(&self) -> io::Result<String> {
        decode_name(&self.components)
    }
}

/// `struct snd_ctl_elem_id`, 64 bytes. Identifies one control, either by
/// `numid` or by the interface, name and index together.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ElemId {
    numid: u32,
    iface: i32,
    device: u32,
    subdevice: u32,
    name: [u8; 44],
    index: u32,
}

impl Default for ElemId {
    fn default() -> ElemId {
        unsafe { std::mem::zeroed() }
    }
}

impl ElemId {
    /// A control on the mixer interface, which is where playback volume and
    /// its mute switch live.
    pub fn mixer(numid: u32, name: &str, index: u32) -> io::Result<ElemId> {
        let mut id = ElemId {
            numid,
            iface: IFACE_MIXER,
            index,
            ..ElemId::default()
        };
        encode_name(name, &mut id.name)?;
        Ok(id)
    }

    /// The kernel's own handle for this control. Non-zero once the control
    /// has been listed, and enough on its own to read or write it.
    pub fn numid(&self) -> u32 {
        self.numid
    }

    pub fn iface(&self) -> i32 {
        self.iface
    }

    pub fn device(&self) -> u32 {
        self.device
    }

    pub fn subdevice(&self) -> u32 {
        self.subdevice
    }

    pub fn index(&self) -> u32 {
        self.index
    }

    /// The control's name.
    ///
    /// This is a fixed byte array out of a device, not a Rust string, so it
    /// is decoded rather than trusted: a name with no terminator stops at the
    /// end of the array, and one that is not UTF-8 is an error.
    pub fn name(&self) -> io::Result<String> {
        decode_name(&self.name)
    }
}

/// `struct snd_ctl_elem_list`, 80 bytes on a 64 bit kernel.
///
/// `pids` is a userspace pointer the kernel writes an array of [`ElemId`]
/// through, which is why [`Control::elem_list`] takes the destination as a
/// slice instead: a fake needs no pointers, and the real implementation is
/// then the only place that has to be careful with one.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ElemList {
    offset: u32,
    space: u32,
    used: u32,
    count: u32,
    pids: usize,
    _reserved: [u8; 50],
}

impl Default for ElemList {
    fn default() -> ElemList {
        unsafe { std::mem::zeroed() }
    }
}

impl ElemList {
    /// A request that asks only how many elements there are.
    pub fn counting() -> ElemList {
        ElemList::default()
    }

    /// A request for `space` elements starting at the beginning.
    pub fn asking_for(space: u32) -> ElemList {
        ElemList {
            space,
            ..ElemList::default()
        }
    }

    /// How many elements the card has in total.
    pub fn count(&self) -> u32 {
        self.count
    }

    /// How many the kernel actually wrote out.
    pub fn used(&self) -> u32 {
        self.used
    }

    pub fn space(&self) -> u32 {
        self.space
    }

    pub fn offset(&self) -> u32 {
        self.offset
    }

    /// What a device answering this request reports back.
    pub fn answer(&mut self, count: u32, used: u32) {
        self.count = count;
        self.used = used;
    }
}

/// The type of a control's value, `snd_ctl_elem_type_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElemType {
    None,
    Boolean,
    Integer,
    Enumerated,
    Bytes,
    Iec958,
    Integer64,
    /// A type this kernel knows about and this code does not.
    Unknown(i32),
}

impl ElemType {
    fn from_raw(raw: i32) -> ElemType {
        match raw {
            0 => ElemType::None,
            1 => ElemType::Boolean,
            2 => ElemType::Integer,
            3 => ElemType::Enumerated,
            4 => ElemType::Bytes,
            5 => ElemType::Iec958,
            6 => ElemType::Integer64,
            other => ElemType::Unknown(other),
        }
    }

    fn as_raw(self) -> i32 {
        match self {
            ElemType::None => 0,
            ElemType::Boolean => 1,
            ElemType::Integer => 2,
            ElemType::Enumerated => 3,
            ElemType::Bytes => 4,
            ElemType::Iec958 => 5,
            ElemType::Integer64 => 6,
            ElemType::Unknown(other) => other,
        }
    }
}

/// `struct snd_ctl_elem_info`, 272 bytes.
///
/// The `value` union is kept as raw bytes and decoded by hand. That is not
/// laziness: the union's first arm is three `long`s, so modelling it as a
/// Rust struct would make the whole type's alignment follow the
/// architecture, where a byte array has exactly the layout the kernel does on
/// both. The union starts at offset 80, which is already `long` aligned, so
/// there is no padding in front of it to reproduce.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ElemInfo {
    id: ElemId,
    kind: i32,
    access: u32,
    count: u32,
    owner: i32,
    value: [u8; 128],
    _reserved: [u8; 64],
}

impl Default for ElemInfo {
    fn default() -> ElemInfo {
        unsafe { std::mem::zeroed() }
    }
}

impl ElemInfo {
    /// A request for what the kernel knows about `id`.
    pub fn about(id: ElemId) -> ElemInfo {
        ElemInfo {
            id,
            ..ElemInfo::default()
        }
    }

    /// An answer describing an integer control, such as a volume.
    pub fn integer(id: ElemId, count: u32, range: Range) -> ElemInfo {
        let mut info = ElemInfo::about(id);
        info.kind = ElemType::Integer.as_raw();
        info.access = ACCESS_READ | ACCESS_WRITE;
        info.count = count;
        // Three longs is 24 bytes at most, well inside the 128 byte union, so
        // none of these can fail.
        let _ = write_long(&mut info.value, 0, range.min);
        let _ = write_long(&mut info.value, 1, range.max);
        let _ = write_long(&mut info.value, 2, range.step);
        info
    }

    /// An answer describing a boolean control, such as a mute switch.
    pub fn boolean(id: ElemId, count: u32) -> ElemInfo {
        let mut info = ElemInfo::about(id);
        info.kind = ElemType::Boolean.as_raw();
        info.access = ACCESS_READ | ACCESS_WRITE;
        info.count = count;
        info
    }

    /// The same element with its read access taken away. Rarer than read only,
    /// but drivers do publish write only controls, and picking one as the mute
    /// switch would break every reading of the volume.
    pub fn write_only(mut self) -> ElemInfo {
        self.access &= !ACCESS_READ;
        self
    }

    /// The same element with its write access taken away. Drivers do publish
    /// controls a program may read and not change, and a fake has to be able
    /// to be one.
    pub fn read_only(mut self) -> ElemInfo {
        self.access &= !ACCESS_WRITE;
        self
    }

    pub fn id(&self) -> &ElemId {
        &self.id
    }

    pub fn kind(&self) -> ElemType {
        ElemType::from_raw(self.kind)
    }

    /// How many values the control has, one per channel.
    pub fn count(&self) -> u32 {
        self.count
    }

    /// The process holding this control locked, or zero. Reported by the
    /// kernel; nothing here takes a lock.
    pub fn owner(&self) -> i32 {
        self.owner
    }

    pub fn is_readable(&self) -> bool {
        self.access & ACCESS_READ != 0
    }

    pub fn is_writable(&self) -> bool {
        self.access & ACCESS_WRITE != 0
    }

    /// The channel count, refused when it could not be true.
    ///
    /// Zero values means the control cannot be read at all, and more than the
    /// value union holds means the card is not answering sensibly. This is
    /// the only place that number becomes an index bound.
    pub fn channels(&self) -> io::Result<usize> {
        let count = self.count as usize;
        if count == 0 || count > MAX_VALUES {
            return Err(nonsense(&format!("a control with {count} channels")));
        }
        Ok(count)
    }

    /// The range an integer control's value lives in.
    pub fn range(&self) -> io::Result<Range> {
        if self.kind() != ElemType::Integer {
            return Err(nonsense("a range for a control that is not an integer"));
        }
        Range {
            min: read_long(&self.value, 0)?,
            max: read_long(&self.value, 1)?,
            step: read_long(&self.value, 2)?,
        }
        .validated()
    }
}

/// `struct snd_ctl_elem_value`, 1224 bytes on a 64 bit kernel.
///
/// Nearly all of it is the value union, `long value[128]`, decoded by hand
/// for the same reason [`ElemInfo`]'s is.
#[repr(C)]
#[derive(Clone)]
pub struct ElemValue {
    id: ElemId,
    indirect: u32,
    /// The union that follows holds `long long`, so it is eight byte aligned
    /// on every target this could run on, including a 32 bit one, and C pads
    /// to it here. The size assertions below only run on 64 bit, so this line
    /// is the only thing keeping a 32 bit build honest.
    _pad: [u8; LONG - 4],
    value: [u8; MAX_VALUES * LONG],
    _reserved: [u8; 128],
}

impl ElemValue {
    /// An empty value structure addressed at one control.
    pub fn for_element(id: ElemId) -> ElemValue {
        // Every field is a plain integer or a byte array, so zeroes are a
        // valid instance; it is also a kilobyte, which is not worth writing
        // out a field at a time.
        let mut value: ElemValue = unsafe { std::mem::zeroed() };
        value.id = id;
        value.indirect = 0;
        value
    }

    pub fn id(&self) -> &ElemId {
        &self.id
    }

    /// One channel's value.
    pub fn get(&self, channel: usize) -> io::Result<i64> {
        read_long(&self.value, channel)
    }

    /// Set one channel's value.
    pub fn set(&mut self, channel: usize, level: i64) -> io::Result<()> {
        write_long(&mut self.value, channel, level)
    }
}

// ---------------------------------------------------------------------------
// Decoding the bytes that come back
// ---------------------------------------------------------------------------

fn nonsense(what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("the sound card reported {what}"),
    )
}

/// A NUL terminated name out of a fixed array in a kernel structure.
///
/// The terminator is not trusted to be there: a name filling the whole array
/// stops at its end rather than running past it.
fn decode_name(bytes: &[u8]) -> io::Result<String> {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    match std::str::from_utf8(&bytes[..end]) {
        Ok(name) => Ok(name.to_string()),
        Err(_) => Err(nonsense("a name that is not UTF-8")),
    }
}

/// Put a name into a fixed array, leaving room for the terminator.
fn encode_name(name: &str, into: &mut [u8]) -> io::Result<()> {
    let bytes = name.as_bytes();
    if bytes.len() >= into.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("'{name}' is too long for a {} byte field", into.len()),
        ));
    }
    into[..bytes.len()].copy_from_slice(bytes);
    Ok(())
}

/// One `long` out of a value union, by index.
///
/// The bounds check is the whole point: `index` comes from a channel count
/// the device chose, so it is never allowed to address past the array.
fn read_long(area: &[u8], index: usize) -> io::Result<i64> {
    let start = index
        .checked_mul(LONG)
        .ok_or_else(|| nonsense("an impossible channel number"))?;
    let end = start
        .checked_add(LONG)
        .ok_or_else(|| nonsense("an impossible channel number"))?;
    let bytes = area
        .get(start..end)
        .ok_or_else(|| nonsense("a channel past the end of its value structure"))?;
    let mut raw = [0u8; LONG];
    raw.copy_from_slice(bytes);
    // `c_long` is already `i64` on the 64 bit kernels tOS targets, so this
    // cast widens on a 32 bit one and does nothing here; clippy only ever
    // sees whichever it is compiling for.
    #[allow(clippy::unnecessary_cast)]
    Ok(libc::c_long::from_ne_bytes(raw) as i64)
}

/// Put one `long` into a value union, by index.
fn write_long(area: &mut [u8], index: usize, level: i64) -> io::Result<()> {
    let start = index
        .checked_mul(LONG)
        .ok_or_else(|| nonsense("an impossible channel number"))?;
    let end = start
        .checked_add(LONG)
        .ok_or_else(|| nonsense("an impossible channel number"))?;
    let slot = area
        .get_mut(start..end)
        .ok_or_else(|| nonsense("a channel past the end of its value structure"))?;
    // Volume levels are small, but the clamp means a 32 bit `long` cannot
    // silently wrap one into a different number.
    #[allow(clippy::unnecessary_cast)]
    let narrowed = level.clamp(libc::c_long::MIN as i64, libc::c_long::MAX as i64) as libc::c_long;
    slot.copy_from_slice(&narrowed.to_ne_bytes());
    Ok(())
}

// ---------------------------------------------------------------------------
// The seam
// ---------------------------------------------------------------------------

/// Somewhere the `SNDRV_CTL_*` ioctls can be issued.
///
/// This is the whole of what this module asks of the kernel. [`Card`] is the
/// real thing; the tests supply a device that answers out of tables, which is
/// the only way any of this runs on a machine with no sound hardware.
///
/// Everything takes `&self` because an ioctl on an open file descriptor
/// changes nothing on this side of it, and a status bar should not need a
/// mutable mixer to draw a speaker icon.
pub trait Control {
    /// `SNDRV_CTL_IOCTL_CARD_INFO`: which card this is.
    fn card_info(&self) -> io::Result<CardInfo>;

    /// `SNDRV_CTL_IOCTL_ELEM_LIST`: the identifiers of the card's controls.
    ///
    /// `into` is the array the identifiers are written into, and it is what
    /// bounds the request: an implementation must never write more than
    /// `into.len()` of them, whatever `list.space()` says.
    fn elem_list(&self, list: &mut ElemList, into: &mut [ElemId]) -> io::Result<()>;

    /// `SNDRV_CTL_IOCTL_ELEM_INFO`: a control's type and value range.
    fn elem_info(&self, info: &mut ElemInfo) -> io::Result<()>;

    /// `SNDRV_CTL_IOCTL_ELEM_READ`: a control's current value.
    fn elem_read(&self, value: &mut ElemValue) -> io::Result<()>;

    /// `SNDRV_CTL_IOCTL_ELEM_WRITE`: change a control's value.
    fn elem_write(&self, value: &ElemValue) -> io::Result<()>;
}

/// The ioctl numbers, from `include/uapi/sound/asound.h`.
///
/// Only the Linux device below issues them, but they are worked out on every
/// platform so the test that checks them against the kernel's macros runs
/// wherever the workspace is built.
#[allow(dead_code)]
mod number {
    use super::{CardInfo, ElemInfo, ElemList, ElemValue};

    const IOC_WRITE: u64 = 1;
    const IOC_READ: u64 = 2;
    /// `'U'`, the type byte every `SNDRV_CTL_IOCTL_*` shares.
    const BASE: u64 = b'U' as u64;

    /// `_IOC(dir, type, nr, size)` from `asm-generic/ioctl.h`.
    const fn ioc(dir: u64, nr: u64, size: usize) -> u64 {
        (dir << 30) | ((size as u64) << 16) | (BASE << 8) | nr
    }

    const fn ior<T>(nr: u64) -> u64 {
        ioc(IOC_READ, nr, std::mem::size_of::<T>())
    }

    const fn iowr<T>(nr: u64) -> u64 {
        ioc(IOC_READ | IOC_WRITE, nr, std::mem::size_of::<T>())
    }

    pub const CARD_INFO: u64 = ior::<CardInfo>(0x01);
    pub const ELEM_LIST: u64 = iowr::<ElemList>(0x10);
    pub const ELEM_INFO: u64 = iowr::<ElemInfo>(0x11);
    pub const ELEM_READ: u64 = iowr::<ElemValue>(0x12);
    pub const ELEM_WRITE: u64 = iowr::<ElemValue>(0x13);
}

/// An open `/dev/snd/controlC<N>`.
#[cfg(target_os = "linux")]
pub struct Card {
    device: std::fs::File,
}

#[cfg(target_os = "linux")]
impl Card {
    /// Open the control device for card `number`.
    pub fn open(number: u32) -> io::Result<Card> {
        Card::open_at(Path::new(&format!("/dev/snd/controlC{number}")))
    }

    /// Open a control device by path, which is what lets the whole lookup be
    /// pointed at a prepared tree rather than the running machine's `/dev`.
    pub fn open_at(path: &Path) -> io::Result<Card> {
        // Read-write even to only read a level: the kernel refuses
        // `ELEM_WRITE` on a descriptor opened read-only, and a mixer that
        // cannot change anything is not worth opening.
        let device = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)?;
        Ok(Card { device })
    }

    fn ioctl<T>(&self, request: u64, argument: &mut T) -> io::Result<()> {
        // `request as _` because musl declares the second argument of
        // `ioctl` as `int` where glibc declares it `unsigned long`.
        let result =
            unsafe { libc::ioctl(self.device.as_raw_fd(), request as _, argument as *mut T) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
impl Control for Card {
    fn card_info(&self) -> io::Result<CardInfo> {
        let mut info = CardInfo::default();
        self.ioctl(number::CARD_INFO, &mut info)?;
        Ok(info)
    }

    fn elem_list(&self, list: &mut ElemList, into: &mut [ElemId]) -> io::Result<()> {
        // The kernel writes `space` identifiers through `pids`, so `space` is
        // clamped to what was actually allocated. Passing the caller's number
        // through would be handing the kernel a length it could overrun.
        list.space = list.space.min(into.len() as u32);
        list.pids = if list.space == 0 {
            0
        } else {
            into.as_mut_ptr() as usize
        };
        self.ioctl(number::ELEM_LIST, list)
    }

    fn elem_info(&self, info: &mut ElemInfo) -> io::Result<()> {
        self.ioctl(number::ELEM_INFO, info)
    }

    fn elem_read(&self, value: &mut ElemValue) -> io::Result<()> {
        self.ioctl(number::ELEM_READ, value)
    }

    fn elem_write(&self, value: &ElemValue) -> io::Result<()> {
        // The kernel writes back through the same structure, so it gets a
        // copy rather than the caller's.
        let mut copy = value.clone();
        self.ioctl(number::ELEM_WRITE, &mut copy)
    }
}

/// Stands in for the Linux control device on a machine that has none, so the
/// crate still builds and its tests still run away from the target.
#[cfg(not(target_os = "linux"))]
pub struct Card;

#[cfg(not(target_os = "linux"))]
impl Card {
    pub fn open(_number: u32) -> io::Result<Card> {
        Err(unsupported())
    }

    pub fn open_at(_path: &Path) -> io::Result<Card> {
        Err(unsupported())
    }
}

#[cfg(not(target_os = "linux"))]
fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "the ALSA control interface is a Linux kernel interface",
    )
}

#[cfg(not(target_os = "linux"))]
impl Control for Card {
    fn card_info(&self) -> io::Result<CardInfo> {
        Err(unsupported())
    }

    fn elem_list(&self, _list: &mut ElemList, _into: &mut [ElemId]) -> io::Result<()> {
        Err(unsupported())
    }

    fn elem_info(&self, _info: &mut ElemInfo) -> io::Result<()> {
        Err(unsupported())
    }

    fn elem_read(&self, _value: &mut ElemValue) -> io::Result<()> {
        Err(unsupported())
    }

    fn elem_write(&self, _value: &ElemValue) -> io::Result<()> {
        Err(unsupported())
    }
}

// ---------------------------------------------------------------------------
// Finding the cards
// ---------------------------------------------------------------------------

/// The card numbers the kernel knows about, lowest first.
///
/// `/sys/class/sound` is the real answer; `/dev/snd` is there for an
/// initramfs mounted without sysfs, which is a shape tOS boots in.
pub fn cards(sysfs: &Sysfs) -> Vec<u32> {
    let mut numbers: Vec<u32> = sysfs
        .list("/sys/class/sound")
        .iter()
        .filter_map(|name| name.strip_prefix("card")?.parse().ok())
        .collect();
    if numbers.is_empty() {
        numbers = sysfs
            .list("/dev/snd")
            .iter()
            .filter_map(|name| name.strip_prefix("controlC")?.parse().ok())
            .collect();
    }
    numbers.sort_unstable();
    numbers.dedup();
    numbers
}

/// Whether a card has anything that can play sound.
///
/// A card with only capture devices is a microphone, and a machine whose
/// webcam enumerates first should still put its volume control on the
/// speakers.
fn can_play(sysfs: &Sysfs, card: u32) -> bool {
    // A playback PCM is `pcmC<card>D<device>p`; the trailing `p` is what
    // distinguishes it from the `c` of a capture device.
    if sysfs
        .list(&format!("/sys/class/sound/card{card}"))
        .iter()
        .any(|name| name.starts_with("pcm") && name.ends_with('p'))
    {
        return true;
    }
    let prefix = format!("pcmC{card}D");
    sysfs
        .list("/dev/snd")
        .iter()
        .any(|name| name.starts_with(&prefix) && name.ends_with('p'))
}

/// The cards in the order they should be tried: the ones that can play
/// first, then by number.
pub fn card_order(sysfs: &Sysfs) -> Vec<u32> {
    let mut cards = cards(sysfs);
    cards.sort_by_key(|&card| (!can_play(sysfs, card), card));
    cards
}

/// The card to reach for when nobody has said which, or `None` on a machine
/// with no sound card.
pub fn default_card(sysfs: &Sysfs) -> Option<u32> {
    card_order(sysfs).into_iter().next()
}

// ---------------------------------------------------------------------------
// Levels
// ---------------------------------------------------------------------------

/// The range an integer control's value lives in, as the card reports it.
///
/// This is almost never `0..=100`. Intel HDA cards commonly say `0..=87`,
/// USB headsets say things like `0..=37`, and a control carrying decibels
/// straight through can say `-10239..=400`. A percentage is therefore a
/// conversion, and this is the only place it happens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    pub min: i64,
    pub max: i64,
    /// The granularity the driver asks for, or zero when it does not care.
    pub step: i64,
}

impl Range {
    pub fn new(min: i64, max: i64, step: i64) -> Range {
        Range { min, max, step }
    }

    /// The same range, refused if it could not be one.
    ///
    /// An empty or backwards range would make every conversion below either
    /// divide by zero or produce a number outside `0..=100`, so it is stopped
    /// here rather than defended against everywhere after.
    fn validated(self) -> io::Result<Range> {
        if self.max <= self.min {
            return Err(nonsense(&format!(
                "a volume range of {}..={}",
                self.min, self.max
            )));
        }
        if self.step < 0 {
            return Err(nonsense(&format!("a volume step of {}", self.step)));
        }
        Ok(self)
    }

    fn span(&self) -> i128 {
        self.max as i128 - self.min as i128
    }

    /// Where a raw value sits in this range, as a percentage.
    ///
    /// A value outside the range is clamped rather than disbelieved: drivers
    /// do report a level slightly past their own maximum, and that is not
    /// worth refusing to draw a volume for.
    pub fn percent_of(&self, raw: i64) -> u8 {
        let span = self.span();
        if span <= 0 {
            return 0;
        }
        let offset = (raw.clamp(self.min, self.max) as i128) - (self.min as i128);
        // Rounded to nearest, so the ends land exactly on 0 and 100 and no
        // amount of stepping up and down drifts.
        ((offset * 100 + span / 2) / span) as u8
    }

    /// The raw value a percentage means in this range.
    pub fn raw_for(&self, percent: u8) -> i64 {
        let span = self.span();
        if span <= 0 {
            return self.min;
        }
        let percent = percent.min(100) as i128;
        let mut offset = (percent * span + 50) / 100;
        // A driver that insists on a granularity gets it, because a value it
        // will not take is rounded by the hardware and then reads back as a
        // different percentage than the one that was asked for.
        if self.step > 1 {
            let step = self.step as i128;
            offset = ((offset + step / 2) / step) * step;
        }
        let raw = self.min as i128 + offset;
        // Snapping to a step can overshoot; the range is what the card will
        // actually accept.
        raw.clamp(self.min as i128, self.max as i128) as i64
    }
}

/// What a volume indicator needs to know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Volume {
    /// `0..=100`.
    pub percent: u8,
    pub muted: bool,
}

/// How much one press of a volume key moves the level.
pub const STEP_PERCENT: u8 = 5;

/// Where the level goes when a card with no mute switch is unmuted and
/// nothing is remembered about where it was before.
const DEFAULT_LEVEL: u8 = 25;

// ---------------------------------------------------------------------------
// Choosing which control to drive
// ---------------------------------------------------------------------------

/// Control name prefixes worth driving, best first.
///
/// `Master` is what a card with a real output mixer calls it. Plenty of
/// laptops and most USB devices have no such thing and expose `PCM` or
/// `Speaker` instead, and on a few machines `Headphone` is all there is.
/// Which of these exists is a property of the hardware, so the list is walked
/// rather than assumed.
const PREFERRED: &[&str] = &["Master", "PCM", "Speaker", "Headphone"];

/// Every control element on a card.
///
/// Two calls: one to learn the count, one to fetch that many. Both numbers
/// that come back bound either an allocation or an index, so both are checked
/// before they are used as either.
pub fn elements(control: &dyn Control) -> io::Result<Vec<ElemId>> {
    let mut counting = ElemList::counting();
    control.elem_list(&mut counting, &mut [])?;

    let count = counting.count();
    if count > MAX_ELEMENTS {
        return Err(nonsense(&format!("{count} control elements")));
    }
    if count == 0 {
        return Ok(Vec::new());
    }

    let mut ids = vec![ElemId::default(); count as usize];
    let mut list = ElemList::asking_for(count);
    control.elem_list(&mut list, &mut ids)?;

    let used = list.used() as usize;
    if used > ids.len() {
        return Err(nonsense(&format!(
            "{used} control elements after being given room for {}",
            ids.len()
        )));
    }
    ids.truncate(used);
    Ok(ids)
}

/// The controls this module drives on one card.
struct Chosen {
    /// The name of the volume control, for the interface to show.
    name: String,
    volume: ElemId,
    range: Range,
    channels: usize,
    /// The mute switch and its channel count, when the card has one.
    switch: Option<(ElemId, usize)>,
}

/// Find the playback volume, and the switch that mutes it.
///
/// `Ok(None)` means the card has controls but none of them is a playback
/// volume this code can drive — an HDMI-only card, say. That is a fact about
/// the hardware, not a failure.
fn choose(control: &dyn Control) -> io::Result<Option<Chosen>> {
    let mut named: Vec<(ElemId, String)> = Vec::new();
    for id in elements(control)? {
        if id.iface() != IFACE_MIXER {
            continue;
        }
        // Decoded once, here, so a name that is not UTF-8 is caught before
        // anything is matched against it.
        let name = id.name()?;
        named.push((id, name));
    }

    for prefix in PREFERRED {
        let wanted = format!("{prefix} Playback Volume");
        let Some((id, _)) = named.iter().find(|(_, name)| *name == wanted) else {
            continue;
        };

        let mut info = ElemInfo::about(*id);
        control.elem_info(&mut info)?;
        // A control of the wrong type, or one that cannot be changed, is not
        // this card's volume however it is named. Fall through to the next
        // candidate rather than failing the whole card.
        if info.kind() != ElemType::Integer || !info.is_readable() || !info.is_writable() {
            continue;
        }

        let channels = info.channels()?;
        let range = info.range()?;
        let switch = find_switch(control, &named, prefix)?;
        return Ok(Some(Chosen {
            name: wanted,
            volume: *id,
            range,
            channels,
            switch,
        }));
    }

    Ok(None)
}

/// The mute switch that goes with a volume control.
///
/// The matching switch is preferred; failing that `Master Playback Switch`,
/// which on a card that has one mutes everything downstream of whatever is
/// being driven. Plenty of cards have no switch at all, which is why this is
/// an `Option` rather than a requirement.
fn find_switch(
    control: &dyn Control,
    named: &[(ElemId, String)],
    prefix: &str,
) -> io::Result<Option<(ElemId, usize)>> {
    let candidates = [
        format!("{prefix} Playback Switch"),
        "Master Playback Switch".to_string(),
    ];
    for wanted in candidates {
        let Some((id, _)) = named.iter().find(|(_, name)| *name == wanted) else {
            continue;
        };
        let mut info = ElemInfo::about(*id);
        control.elem_info(&mut info)?;
        // Readable as well as writable: `volume()` reads the switch on every
        // call, so a write-only one would not be a mute button, it would be
        // the whole mixer failing with EACCES from then on.
        if info.kind() != ElemType::Boolean || !info.is_writable() || !info.is_readable() {
            continue;
        }
        return Ok(Some((*id, info.channels()?)));
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// The mixer
// ---------------------------------------------------------------------------

/// One card's playback volume.
///
/// Constructed once: the element identifiers and the range are settled at
/// that point, so reading the level afterwards is two ioctls and no
/// searching, which is what a status bar redrawing itself needs.
pub struct Mixer<C: Control> {
    control: C,
    card: String,
    element: String,
    volume: ElemId,
    range: Range,
    channels: usize,
    switch: Option<(ElemId, usize)>,
    /// Where the level was before a mute that had to be done by turning it
    /// all the way down, because this card has no switch to flip.
    before_mute: Cell<Option<u8>>,
}

/// Written out by hand because a control device is not itself worth printing
/// and need not be `Debug` to be put in a mixer.
impl<C: Control> std::fmt::Debug for Mixer<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mixer")
            .field("card", &self.card)
            .field("element", &self.element)
            .field("range", &self.range)
            .field("channels", &self.channels)
            .field("has_mute_switch", &self.switch.is_some())
            .finish()
    }
}

impl<C: Control> Mixer<C> {
    /// Take over a control device, finding the volume control on it.
    ///
    /// Fails with `NotFound` when the card has no playback volume. That is a
    /// real answer about a real card, so it is the caller's to handle, and
    /// [`open_default_in`] handles it by moving on to the next card.
    pub fn attach(control: C) -> io::Result<Mixer<C>> {
        let info = control.card_info()?;
        let card = info.name()?;
        let chosen = choose(&control)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("'{card}' has no playback volume control"),
            )
        })?;
        Ok(Mixer {
            control,
            card,
            element: chosen.name,
            volume: chosen.volume,
            range: chosen.range,
            channels: chosen.channels,
            switch: chosen.switch,
            before_mute: Cell::new(None),
        })
    }

    /// The card's name, such as `HDA Intel PCH`.
    pub fn card_name(&self) -> &str {
        &self.card
    }

    /// Which control was picked, such as `PCM Playback Volume`. Worth showing
    /// when it is not the Master, because then it does not control
    /// everything.
    pub fn element_name(&self) -> &str {
        &self.element
    }

    /// The raw range the card reports, which a percentage is a conversion of.
    pub fn range(&self) -> Range {
        self.range
    }

    /// How many channels the volume control has.
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Whether the card has a real mute switch. When it does not, muting
    /// turns the level down instead.
    pub fn has_mute_switch(&self) -> bool {
        self.switch.is_some()
    }

    /// The device underneath, for anything this module does not cover.
    pub fn control(&self) -> &C {
        &self.control
    }

    /// The current level, and whether sound is muted.
    pub fn volume(&self) -> io::Result<Volume> {
        let percent = self.range.percent_of(self.loudest()?);
        let muted = match &self.switch {
            Some((id, channels)) => {
                // Off on every channel, so a card with one side switched off
                // is not called muted.
                self.read_all(id, *channels)?.iter().all(|&on| on == 0)
            }
            None => percent == 0,
        };
        Ok(Volume { percent, muted })
    }

    /// Set the level, clamped to `0..=100` and then to whatever the card
    /// actually accepts. Returns the level as it reads back.
    pub fn set_volume(&self, percent: u8) -> io::Result<Volume> {
        let raw = self.range.raw_for(percent);
        self.write_all(&self.volume, self.channels, raw)?;
        // An explicit level replaces whatever was being held for an unmute.
        self.before_mute.set(None);
        self.volume()
    }

    /// Raise the level by one step.
    pub fn volume_up(&self) -> io::Result<Volume> {
        self.raise(STEP_PERCENT)
    }

    /// Lower the level by one step.
    pub fn volume_down(&self) -> io::Result<Volume> {
        self.lower(STEP_PERCENT)
    }

    /// Raise the level by `step` percentage points, stopping at the top.
    pub fn raise(&self, step: u8) -> io::Result<Volume> {
        self.nudge(step as i16)
    }

    /// Lower the level by `step` percentage points, stopping at the bottom.
    pub fn lower(&self, step: u8) -> io::Result<Volume> {
        self.nudge(-(step as i16))
    }

    /// Move the level by `step` percentage points, but never by nothing.
    ///
    /// A percentage is a lossy way to name a level: a control whose whole
    /// range is `0..=1` has one point between silence and full, and five
    /// percent of it rounds back to where it started. Asking for that in
    /// percent alone leaves the volume key doing nothing at all, for ever, so
    /// a move that would not move goes one raw value instead — which on such a
    /// control is the only move there is.
    fn nudge(&self, step: i16) -> io::Result<Volume> {
        let from = self.loudest()?;
        let wanted = (self.range.percent_of(from) as i16 + step).clamp(0, 100) as u8;
        let mut raw = self.range.raw_for(wanted);
        if raw == from && step != 0 {
            let by = self.range.step.max(1);
            raw = if step > 0 {
                from.saturating_add(by).min(self.range.max)
            } else {
                from.saturating_sub(by).max(self.range.min)
            };
        }
        self.write_all(&self.volume, self.channels, raw)?;
        self.before_mute.set(None);
        self.volume()
    }

    /// Mute or unmute.
    ///
    /// With a switch this leaves the level alone, so unmuting comes back to
    /// where it was. Without one there is nothing to flip, so the level goes
    /// all the way down and the old one is held here to be put back — which
    /// is what every mixer does on such a card, and the only reason this type
    /// has any state at all.
    pub fn set_muted(&self, muted: bool) -> io::Result<Volume> {
        match &self.switch {
            Some((id, channels)) => {
                self.write_all(id, *channels, i64::from(!muted))?;
            }
            None if muted => {
                let now = self.volume()?.percent;
                if now > 0 {
                    self.before_mute.set(Some(now));
                }
                self.write_all(&self.volume, self.channels, self.range.min)?;
            }
            None => {
                // Never back to nothing: an unmute that leaves the machine
                // silent reads as a broken unmute.
                let restored = self.before_mute.take().unwrap_or(DEFAULT_LEVEL).max(1);
                let raw = self.range.raw_for(restored);
                self.write_all(&self.volume, self.channels, raw)?;
            }
        }
        self.volume()
    }

    /// Flip between muted and not.
    pub fn toggle_mute(&self) -> io::Result<Volume> {
        let muted = self.volume()?.muted;
        self.set_muted(!muted)
    }

    /// The loudest channel's raw level.
    ///
    /// The loudest rather than the first, so a card whose channels have
    /// drifted apart never has its indicator claim silence while sound is
    /// still coming out of one side.
    fn loudest(&self) -> io::Result<i64> {
        let values = self.read_all(&self.volume, self.channels)?;
        values
            .into_iter()
            .max()
            .ok_or_else(|| nonsense("a volume control with no channels"))
    }

    fn read_all(&self, id: &ElemId, channels: usize) -> io::Result<Vec<i64>> {
        let mut value = ElemValue::for_element(*id);
        self.control.elem_read(&mut value)?;
        (0..channels).map(|channel| value.get(channel)).collect()
    }

    fn write_all(&self, id: &ElemId, channels: usize, level: i64) -> io::Result<()> {
        let mut value = ElemValue::for_element(*id);
        for channel in 0..channels {
            value.set(channel, level)?;
        }
        self.control.elem_write(&value)
    }
}

/// The mixer of this machine's default card.
///
/// `Ok(None)` is the ordinary answer on a machine with no sound card, which
/// is a normal machine and not worth an error.
pub fn open_default() -> io::Result<Option<Mixer<Card>>> {
    open_default_in(&Sysfs::system())
}

/// The same, against a given root, which is what makes it testable and what
/// lets it be pointed at a tree that is not this machine's.
pub fn open_default_in(sysfs: &Sysfs) -> io::Result<Option<Mixer<Card>>> {
    let mut first_failure: Option<io::Error> = None;
    for card in card_order(sysfs) {
        let path = sysfs.path(&format!("/dev/snd/controlC{card}"));
        let attempt: io::Result<Mixer<Card>> = Card::open_at(&path).and_then(Mixer::attach);
        match attempt {
            Ok(mixer) => return Ok(Some(mixer)),
            Err(error) => {
                // The first card's complaint is the interesting one; a later
                // card failing after an earlier one worked is never reached.
                if first_failure.is_none() {
                    first_failure = Some(error);
                }
            }
        }
    }
    match first_failure {
        // Cards exist and none of them could be driven: worth saying.
        Some(error) => Err(error),
        // No cards at all: silence.
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::path::PathBuf;

    // -----------------------------------------------------------------------
    // A card that answers out of tables
    // -----------------------------------------------------------------------

    struct FakeElement {
        id: ElemId,
        info: ElemInfo,
        values: RefCell<Vec<i64>>,
    }

    /// A control device that answers the way a driver would, and writes down
    /// what it was told to change.
    ///
    /// This is the whole reason [`Control`] exists: none of the logic above
    /// it has ever seen a sound card on the machine these tests run on.
    struct FakeCard {
        info: CardInfo,
        elements: Vec<FakeElement>,
        /// An element count to report instead of the truth.
        claimed_count: Option<u32>,
        /// Claim to have written out one more identifier than there was room
        /// for, which is the shape of a kernel structure that cannot be
        /// trusted.
        overreport_used: bool,
        writes: RefCell<Vec<(String, Vec<i64>)>>,
    }

    impl FakeCard {
        fn new(name: &str) -> FakeCard {
            FakeCard {
                info: CardInfo::describing(0, "PCH", name).unwrap(),
                elements: Vec::new(),
                claimed_count: None,
                overreport_used: false,
                writes: RefCell::new(Vec::new()),
            }
        }

        fn element(
            mut self,
            name: &str,
            describe: impl FnOnce(ElemId) -> ElemInfo,
            values: &[i64],
        ) -> FakeCard {
            let numid = self.elements.len() as u32 + 1;
            let id = ElemId::mixer(numid, name, 0).unwrap();
            let info = describe(id);
            self.elements.push(FakeElement {
                id,
                info,
                values: RefCell::new(values.to_vec()),
            });
            self
        }

        fn volume(self, name: &str, range: Range, values: &[i64]) -> FakeCard {
            let count = values.len() as u32;
            self.element(name, |id| ElemInfo::integer(id, count, range), values)
        }

        fn switch(self, name: &str, values: &[i64]) -> FakeCard {
            let count = values.len() as u32;
            self.element(name, |id| ElemInfo::boolean(id, count), values)
        }

        fn claiming(mut self, count: u32) -> FakeCard {
            self.claimed_count = Some(count);
            self
        }

        fn overreporting(mut self) -> FakeCard {
            self.overreport_used = true;
            self
        }

        fn find(&self, numid: u32) -> io::Result<&FakeElement> {
            self.elements
                .iter()
                .find(|element| element.id.numid() == numid)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such control"))
        }

        /// The last thing written to a named control.
        fn last_write(&self, name: &str) -> Option<Vec<i64>> {
            self.writes
                .borrow()
                .iter()
                .rev()
                .find(|(written, _)| written == name)
                .map(|(_, values)| values.clone())
        }
    }

    impl Control for FakeCard {
        fn card_info(&self) -> io::Result<CardInfo> {
            Ok(self.info)
        }

        fn elem_list(&self, list: &mut ElemList, into: &mut [ElemId]) -> io::Result<()> {
            let count = self.claimed_count.unwrap_or(self.elements.len() as u32);
            let room = into.len().min(list.space() as usize);
            let mut used = 0;
            for (slot, element) in into.iter_mut().zip(self.elements.iter()).take(room) {
                *slot = element.id;
                used += 1;
            }
            if self.overreport_used {
                used += 1;
            }
            list.answer(count, used);
            Ok(())
        }

        fn elem_info(&self, info: &mut ElemInfo) -> io::Result<()> {
            *info = self.find(info.id().numid())?.info;
            Ok(())
        }

        fn elem_read(&self, value: &mut ElemValue) -> io::Result<()> {
            let element = self.find(value.id().numid())?;
            for (channel, level) in element.values.borrow().iter().enumerate() {
                value.set(channel, *level)?;
            }
            Ok(())
        }

        fn elem_write(&self, value: &ElemValue) -> io::Result<()> {
            let element = self.find(value.id().numid())?;
            let mut values = element.values.borrow_mut();
            for (channel, slot) in values.iter_mut().enumerate() {
                *slot = value.get(channel)?;
            }
            self.writes
                .borrow_mut()
                .push((element.id.name()?, values.clone()));
            Ok(())
        }
    }

    /// A throwaway tree shaped like `/sys` and `/dev`, which cleans up after
    /// itself.
    struct FakeTree {
        root: PathBuf,
    }

    impl FakeTree {
        fn new(name: &str) -> FakeTree {
            let root =
                std::env::temp_dir().join(format!("tos-audio-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            FakeTree { root }
        }

        fn dir(&self, path: &str) -> &FakeTree {
            std::fs::create_dir_all(self.root.join(path.trim_start_matches('/'))).unwrap();
            self
        }

        /// A device node, which for the purpose of listing a directory is an
        /// empty file.
        fn node(&self, path: &str) -> &FakeTree {
            let full = self.root.join(path.trim_start_matches('/'));
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, "").unwrap();
            self
        }

        fn sysfs(&self) -> Sysfs {
            Sysfs::new(&self.root)
        }
    }

    impl Drop for FakeTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// The card most laptops actually are.
    fn hda_card() -> FakeCard {
        FakeCard::new("HDA Intel PCH")
            .volume("Master Playback Volume", Range::new(0, 87, 0), &[87, 87])
            .switch("Master Playback Switch", &[1, 1])
    }

    // -----------------------------------------------------------------------
    // The structures themselves
    // -----------------------------------------------------------------------

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn the_structures_are_the_size_the_kernel_expects() {
        // `sizeof` on x86_64 and arm64, from include/uapi/sound/asound.h. The
        // kernel puts these numbers inside the ioctl request, so a layout
        // that has drifted fails with ENOTTY rather than misbehaving.
        assert_eq!(std::mem::size_of::<CardInfo>(), 376);
        assert_eq!(std::mem::size_of::<ElemId>(), 64);
        assert_eq!(std::mem::size_of::<ElemList>(), 80);
        assert_eq!(std::mem::size_of::<ElemInfo>(), 272);
        assert_eq!(std::mem::size_of::<ElemValue>(), 1224);
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn the_value_unions_start_where_c_would_have_put_them() {
        // Both unions are `long` aligned in C and byte arrays here, so their
        // offsets are the one thing this file has to reproduce by hand.
        let info = ElemInfo::default();
        let base = &info as *const ElemInfo as usize;
        assert_eq!(info.value.as_ptr() as usize - base, 80);

        let value = ElemValue::for_element(ElemId::default());
        let base = &value as *const ElemValue as usize;
        assert_eq!(value.value.as_ptr() as usize - base, 72);
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn the_ioctl_numbers_match_the_kernel_macros() {
        // SNDRV_CTL_IOCTL_CARD_INFO is _IOR('U', 0x01, snd_ctl_card_info),
        // and the rest are _IOWR('U', nr, ...) of their own structure.
        assert_eq!(number::CARD_INFO, 0x8178_5501);
        assert_eq!(number::ELEM_LIST, 0xc050_5510);
        assert_eq!(number::ELEM_INFO, 0xc110_5511);
        assert_eq!(number::ELEM_READ, 0xc4c8_5512);
        assert_eq!(number::ELEM_WRITE, 0xc4c8_5513);
    }

    #[test]
    fn a_name_too_long_for_its_field_is_refused_rather_than_truncated() {
        assert!(ElemId::mixer(1, &"x".repeat(43), 0).is_ok());
        // 44 bytes leaves no room for the terminator.
        assert!(ElemId::mixer(1, &"x".repeat(44), 0).is_err());
    }

    #[test]
    fn a_name_with_no_terminator_stops_at_the_end_of_its_field() {
        let id = ElemId {
            name: [b'a'; 44],
            ..ElemId::default()
        };
        assert_eq!(id.name().unwrap(), "a".repeat(44));
    }

    #[test]
    fn a_channel_past_the_end_of_a_value_structure_is_refused() {
        let mut value = ElemValue::for_element(ElemId::default());
        assert!(value.get(MAX_VALUES - 1).is_ok());
        assert!(value.get(MAX_VALUES).is_err());
        assert!(value.get(usize::MAX).is_err());
        assert!(value.set(MAX_VALUES, 1).is_err());
    }

    // -----------------------------------------------------------------------
    // Percentages
    // -----------------------------------------------------------------------

    #[test]
    fn a_percentage_survives_the_round_trip_through_a_cards_own_range() {
        // 0..=87 is what an Intel HDA codec reports, and it is exactly the
        // case where the raw value and the percentage look nothing alike.
        let range = Range::new(0, 87, 0);
        for percent in 0..=100u8 {
            let raw = range.raw_for(percent);
            assert!((0..=87).contains(&raw), "{percent}% became {raw}");
            let back = range.percent_of(raw);
            // A range with fewer than 100 steps cannot represent every
            // percentage, so one point of rounding is the best there is.
            assert!(
                back.abs_diff(percent) <= 1,
                "{percent}% came back as {back}%"
            );
        }
    }

    #[test]
    fn the_ends_of_a_range_round_to_exactly_nothing_and_everything() {
        // A coarse card, a USB headset, a control carrying decibels straight
        // through, and a switch pretending to be a volume.
        for range in [
            Range::new(0, 87, 0),
            Range::new(0, 37, 0),
            Range::new(-10239, 400, 0),
            Range::new(0, 1, 0),
        ] {
            assert_eq!(range.percent_of(range.min), 0, "{range:?}");
            assert_eq!(range.percent_of(range.max), 100, "{range:?}");
            assert_eq!(range.raw_for(0), range.min, "{range:?}");
            assert_eq!(range.raw_for(100), range.max, "{range:?}");
        }
    }

    #[test]
    fn a_level_outside_the_range_the_card_reports_is_clamped_not_believed() {
        let range = Range::new(0, 87, 0);
        assert_eq!(range.percent_of(-40), 0);
        assert_eq!(range.percent_of(9000), 100);
        // Above a hundred percent is still a hundred percent.
        assert_eq!(range.raw_for(255), 87);
    }

    #[test]
    fn a_step_the_driver_insists_on_is_honoured() {
        let range = Range::new(0, 100, 8);
        for percent in 0..=100u8 {
            let raw = range.raw_for(percent);
            assert!(
                raw % 8 == 0 || raw == range.max,
                "{percent}% became {raw}, which the card would round itself"
            );
        }
        assert_eq!(range.raw_for(0), 0);
        assert_eq!(range.raw_for(100), 100);
    }

    // -----------------------------------------------------------------------
    // Picking a control
    // -----------------------------------------------------------------------

    #[test]
    fn a_master_control_is_preferred_when_the_card_has_one() {
        let card = FakeCard::new("HDA Intel PCH")
            .volume("PCM Playback Volume", Range::new(0, 255, 0), &[255, 255])
            .volume("Master Playback Volume", Range::new(0, 87, 0), &[43, 43])
            .switch("Master Playback Switch", &[1, 1]);
        let mixer = Mixer::attach(card).unwrap();
        assert_eq!(mixer.element_name(), "Master Playback Volume");
        assert_eq!(mixer.card_name(), "HDA Intel PCH");
        assert_eq!(mixer.range(), Range::new(0, 87, 0));
        assert_eq!(mixer.channels(), 2);
    }

    #[test]
    fn pcm_is_taken_when_the_card_has_no_master() {
        let card = FakeCard::new("USB Audio")
            .volume("Capture Volume", Range::new(0, 30, 0), &[10])
            .volume("PCM Playback Volume", Range::new(0, 37, 0), &[18])
            .switch("PCM Playback Switch", &[1]);
        let mixer = Mixer::attach(card).unwrap();
        assert_eq!(mixer.element_name(), "PCM Playback Volume");
        assert!(mixer.has_mute_switch());
    }

    #[test]
    fn speaker_is_taken_when_neither_master_nor_pcm_is_there() {
        let card = FakeCard::new("bytcr-rt5640").volume(
            "Speaker Playback Volume",
            Range::new(0, 31, 0),
            &[20, 20],
        );
        let mixer = Mixer::attach(card).unwrap();
        assert_eq!(mixer.element_name(), "Speaker Playback Volume");
        // No switch on this card, which is the common shape on such hardware.
        assert!(!mixer.has_mute_switch());
    }

    #[test]
    fn a_master_switch_is_used_by_a_volume_that_has_none_of_its_own() {
        let card = FakeCard::new("Mixed")
            .volume("PCM Playback Volume", Range::new(0, 31, 0), &[20])
            .switch("Master Playback Switch", &[1]);
        let mixer = Mixer::attach(card).unwrap();
        assert_eq!(mixer.element_name(), "PCM Playback Volume");
        assert!(mixer.has_mute_switch());
        mixer.set_muted(true).unwrap();
        assert_eq!(
            mixer.control().last_write("Master Playback Switch"),
            Some(vec![0])
        );
    }

    #[test]
    fn a_master_that_cannot_be_written_is_passed_over_for_one_that_can() {
        let card = FakeCard::new("Odd")
            .element(
                "Master Playback Volume",
                |id| ElemInfo::integer(id, 2, Range::new(0, 87, 0)).read_only(),
                &[40, 40],
            )
            .volume("PCM Playback Volume", Range::new(0, 31, 0), &[15, 15]);
        assert_eq!(
            Mixer::attach(card).unwrap().element_name(),
            "PCM Playback Volume"
        );
    }

    #[test]
    fn a_volume_that_is_not_on_the_mixer_interface_is_not_the_volume() {
        let mut card =
            FakeCard::new("Odd").volume("Master Playback Volume", Range::new(0, 87, 0), &[40]);
        // SNDRV_CTL_ELEM_IFACE_PCM, where the same name means something else.
        card.elements[0].id.iface = 3;
        assert_eq!(
            Mixer::attach(card).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn a_card_with_no_playback_volume_at_all_is_not_a_mixer() {
        let card = FakeCard::new("HDA ATI HDMI")
            .switch("IEC958 Playback Switch", &[1])
            .volume("IEC958 Playback Volume", Range::new(0, 127, 0), &[127]);
        assert_eq!(
            Mixer::attach(card).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn a_card_with_no_controls_at_all_lists_nothing_and_is_not_a_mixer() {
        let card = FakeCard::new("Empty");
        assert!(elements(&card).unwrap().is_empty());
        assert_eq!(
            Mixer::attach(card).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    // -----------------------------------------------------------------------
    // Reading and changing the level
    // -----------------------------------------------------------------------

    #[test]
    fn the_level_is_read_as_a_percentage_of_the_cards_own_range() {
        let mixer = Mixer::attach(hda_card()).unwrap();
        assert_eq!(
            mixer.volume().unwrap(),
            Volume {
                percent: 100,
                muted: false
            }
        );
    }

    #[test]
    fn setting_the_level_writes_every_channel_in_the_cards_own_units() {
        let mixer = Mixer::attach(hda_card()).unwrap();
        let set = mixer.set_volume(50).unwrap();
        // Half of 0..=87 is 43.5, and the card only takes whole numbers, so
        // what it then reads back is 44 of 87 rather than exactly half.
        assert_eq!(
            mixer.control().last_write("Master Playback Volume"),
            Some(vec![44, 44])
        );
        assert_eq!(set.percent, 51);
        assert!(!set.muted);
    }

    #[test]
    fn an_unbalanced_card_reports_its_louder_channel() {
        let card = FakeCard::new("Drifted").volume(
            "Master Playback Volume",
            Range::new(0, 87, 0),
            &[0, 87],
        );
        let mixer = Mixer::attach(card).unwrap();
        assert_eq!(mixer.volume().unwrap().percent, 100);
    }

    #[test]
    fn volume_up_and_down_stop_at_the_ends_of_the_range() {
        let mixer = Mixer::attach(hda_card()).unwrap();
        // Already at the top.
        assert_eq!(mixer.volume_up().unwrap().percent, 100);
        for _ in 0..40 {
            mixer.volume_down().unwrap();
        }
        assert_eq!(mixer.volume().unwrap().percent, 0);
        assert_eq!(
            mixer.control().last_write("Master Playback Volume"),
            Some(vec![0, 0])
        );
        for _ in 0..40 {
            mixer.volume_up().unwrap();
        }
        assert_eq!(mixer.volume().unwrap().percent, 100);
        assert_eq!(
            mixer.control().last_write("Master Playback Volume"),
            Some(vec![87, 87])
        );
    }

    #[test]
    fn one_step_up_and_back_down_lands_where_it_started() {
        let mixer = Mixer::attach(hda_card()).unwrap();
        let start = mixer.set_volume(40).unwrap().percent;
        mixer.volume_up().unwrap();
        assert_eq!(mixer.volume_down().unwrap().percent, start);
    }

    #[test]
    fn a_write_only_switch_is_not_taken_as_the_mute_button() {
        // `volume()` reads the switch every time it is called, so adopting one
        // that cannot be read would not cost a mute button, it would cost
        // every reading of the volume from then on.
        let card = FakeCard::new("Odd")
            .volume("Master Playback Volume", Range::new(0, 87, 0), &[43])
            .element(
                "Master Playback Switch",
                |id| ElemInfo::boolean(id, 1).write_only(),
                &[1],
            );
        let mixer = Mixer::attach(card).unwrap();
        assert!(!mixer.has_mute_switch(), "a write only switch was adopted");
        // The point of refusing it: the level still reads.
        assert_eq!(mixer.volume().unwrap().percent, 49);
    }

    #[test]
    fn the_volume_key_moves_a_control_too_coarse_for_percentages() {
        // A switch-like control has one raw value between silence and full, so
        // five percent of it rounds back to where it started. Stepping in
        // percent alone would leave the volume key doing nothing for ever.
        for range in [
            Range::new(0, 1, 0),
            Range::new(0, 3, 0),
            Range::new(0, 7, 0),
        ] {
            let card = FakeCard::new("Coarse").volume("Master Playback Volume", range, &[0]);
            let mixer = Mixer::attach(card).unwrap();
            let before = mixer.volume().unwrap().percent;
            let after = mixer.volume_up().unwrap().percent;
            assert!(
                after > before,
                "{range:?} stuck at {before}% after a step up"
            );
            assert_eq!(
                mixer.volume_down().unwrap().percent,
                before,
                "{range:?} did not come back down"
            );
        }
    }

    #[test]
    fn a_step_coarser_than_the_key_still_moves_by_one_step() {
        // The driver insists on multiples of 32, which is more than the five
        // percent the key asks for, so the move snaps back to where it was.
        let card =
            FakeCard::new("Chunky").volume("Master Playback Volume", Range::new(0, 100, 32), &[0]);
        let mixer = Mixer::attach(card).unwrap();
        let after = mixer.volume_up().unwrap().percent;
        assert!(after > 0, "stuck at {after}% with a step of 32");
    }

    // -----------------------------------------------------------------------
    // Muting
    // -----------------------------------------------------------------------

    #[test]
    fn muting_flips_the_switch_and_leaves_the_level_where_it_was() {
        let mixer = Mixer::attach(hda_card()).unwrap();
        let muted = mixer.set_muted(true).unwrap();
        assert!(muted.muted);
        assert_eq!(muted.percent, 100);
        assert_eq!(
            mixer.control().last_write("Master Playback Switch"),
            Some(vec![0, 0])
        );

        let back = mixer.toggle_mute().unwrap();
        assert!(!back.muted);
        assert_eq!(back.percent, 100);
    }

    #[test]
    fn a_card_with_one_side_switched_off_is_not_called_muted() {
        let card = FakeCard::new("Half")
            .volume("Master Playback Volume", Range::new(0, 87, 0), &[87, 87])
            .switch("Master Playback Switch", &[0, 1]);
        let mixer = Mixer::attach(card).unwrap();
        assert!(!mixer.volume().unwrap().muted);
    }

    #[test]
    fn a_card_with_no_switch_mutes_by_turning_the_level_down_and_remembers_it() {
        let card = FakeCard::new("bytcr-rt5640").volume(
            "Speaker Playback Volume",
            Range::new(0, 31, 0),
            &[20],
        );
        let mixer = Mixer::attach(card).unwrap();
        assert!(!mixer.has_mute_switch());

        let before = mixer.volume().unwrap().percent;
        let muted = mixer.set_muted(true).unwrap();
        assert!(muted.muted);
        assert_eq!(muted.percent, 0);
        assert_eq!(
            mixer.control().last_write("Speaker Playback Volume"),
            Some(vec![0])
        );

        let back = mixer.set_muted(false).unwrap();
        assert!(!back.muted);
        assert_eq!(back.percent, before);
    }

    #[test]
    fn unmuting_a_card_that_was_already_silent_does_not_leave_it_silent() {
        let card = FakeCard::new("bytcr-rt5640").volume(
            "Speaker Playback Volume",
            Range::new(0, 31, 0),
            &[0],
        );
        let mixer = Mixer::attach(card).unwrap();
        assert!(mixer.volume().unwrap().muted);
        let back = mixer.set_muted(false).unwrap();
        assert!(!back.muted);
        assert!(back.percent > 0);
    }

    // -----------------------------------------------------------------------
    // Cards that answer with nonsense
    // -----------------------------------------------------------------------

    #[test]
    fn a_card_claiming_more_elements_than_could_exist_is_an_error() {
        let card = hda_card().claiming(100_000);
        assert_eq!(
            Mixer::attach(card).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn a_card_writing_out_more_elements_than_it_was_given_room_for_is_an_error() {
        let card = hda_card().overreporting();
        assert_eq!(
            Mixer::attach(card).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn a_control_name_that_is_not_utf8_is_an_error_rather_than_a_panic() {
        let mut card = hda_card();
        card.elements[0].id.name[0] = 0xff;
        assert_eq!(
            Mixer::attach(card).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn a_card_name_that_is_not_utf8_is_an_error_rather_than_a_panic() {
        let mut card = hda_card();
        card.info.name[0] = 0x80;
        assert_eq!(
            Mixer::attach(card).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn a_volume_range_that_runs_backwards_is_an_error() {
        let card =
            FakeCard::new("Broken").volume("Master Playback Volume", Range::new(100, 0, 0), &[40]);
        assert_eq!(
            Mixer::attach(card).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn a_channel_count_bigger_than_the_value_structure_holds_is_an_error() {
        for count in [0, 9999] {
            let card = FakeCard::new("Broken").element(
                "Master Playback Volume",
                |id| ElemInfo::integer(id, count, Range::new(0, 87, 0)),
                &[40],
            );
            assert_eq!(
                Mixer::attach(card).unwrap_err().kind(),
                io::ErrorKind::InvalidData,
                "a control claiming {count} channels"
            );
        }
    }

    #[test]
    fn a_device_that_refuses_every_ioctl_is_an_error_and_not_a_panic() {
        struct Deaf;

        impl Deaf {
            fn no() -> io::Error {
                io::Error::from_raw_os_error(libc::ENOTTY)
            }
        }

        impl Control for Deaf {
            fn card_info(&self) -> io::Result<CardInfo> {
                Err(Deaf::no())
            }
            fn elem_list(&self, _list: &mut ElemList, _into: &mut [ElemId]) -> io::Result<()> {
                Err(Deaf::no())
            }
            fn elem_info(&self, _info: &mut ElemInfo) -> io::Result<()> {
                Err(Deaf::no())
            }
            fn elem_read(&self, _value: &mut ElemValue) -> io::Result<()> {
                Err(Deaf::no())
            }
            fn elem_write(&self, _value: &ElemValue) -> io::Result<()> {
                Err(Deaf::no())
            }
        }

        assert!(Mixer::attach(Deaf).is_err());
    }

    // -----------------------------------------------------------------------
    // Finding the cards
    // -----------------------------------------------------------------------

    #[test]
    fn a_machine_with_no_sound_card_is_quiet_about_it() {
        let tree = FakeTree::new("silent");
        assert!(cards(&tree.sysfs()).is_empty());
        assert_eq!(default_card(&tree.sysfs()), None);
        // Not an error: a machine with no sound card is a normal machine.
        assert!(open_default_in(&tree.sysfs()).unwrap().is_none());
    }

    #[test]
    fn cards_come_out_of_sysfs_in_numeric_order() {
        let tree = FakeTree::new("order");
        tree.dir("/sys/class/sound/card10")
            .dir("/sys/class/sound/card2")
            .dir("/sys/class/sound/card0")
            // Everything else in that directory is not a card.
            .dir("/sys/class/sound/controlC0")
            .dir("/sys/class/sound/pcmC0D0p");
        assert_eq!(cards(&tree.sysfs()), vec![0, 2, 10]);
    }

    #[test]
    fn cards_are_found_in_dev_snd_when_there_is_no_sysfs() {
        let tree = FakeTree::new("devsnd");
        tree.node("/dev/snd/controlC1")
            .node("/dev/snd/pcmC1D0p")
            .node("/dev/snd/seq")
            .node("/dev/snd/timer");
        assert_eq!(cards(&tree.sysfs()), vec![1]);
        assert_eq!(default_card(&tree.sysfs()), Some(1));
    }

    #[test]
    fn the_default_card_is_one_that_can_actually_play() {
        let tree = FakeTree::new("playback");
        // card0 is a webcam's microphone; the speakers are card1.
        tree.dir("/sys/class/sound/card0/pcmC0D0c")
            .dir("/sys/class/sound/card1/pcmC1D0p");
        assert_eq!(default_card(&tree.sysfs()), Some(1));
        assert_eq!(card_order(&tree.sysfs()), vec![1, 0]);
    }

    #[test]
    fn the_lowest_numbered_card_wins_when_none_of_them_admit_to_playing() {
        let tree = FakeTree::new("nopcm");
        tree.dir("/sys/class/sound/card1")
            .dir("/sys/class/sound/card0");
        assert_eq!(card_order(&tree.sysfs()), vec![0, 1]);
    }

    #[test]
    fn a_card_that_exists_but_cannot_be_opened_is_worth_saying_so() {
        let tree = FakeTree::new("missing-node");
        // Sysfs says there is a card, but there is no device node behind it,
        // which is what a machine missing its snd modules looks like.
        tree.dir("/sys/class/sound/card0/pcmC0D0p");
        assert!(open_default_in(&tree.sysfs()).is_err());
    }
}
