//! Bluetooth adapters, without BlueZ.
//!
//! Everything a desktop calls "Bluetooth" — scanning, pairing, trusting,
//! connecting a headset — is BlueZ, and BlueZ is reached over D-Bus. tOS has no
//! D-Bus, so none of that is available here and none of it is pretended at.
//!
//! What the kernel offers on its own is an `AF_BLUETOOTH` socket and a handful
//! of ioctls: which adapters exist, what their address is, whether they are up,
//! and taking them up and down. That is what this module is. Pairing is out of
//! scope, and deliberately so — see the note on [`Bluetooth::scan`].
//!
//! Enumeration reads `/sys/class/bluetooth` rather than calling
//! `HCIGETDEVLIST`, for three reasons: it needs no privilege and no socket, so
//! a machine whose kernel has no Bluetooth in it stays quiet instead of
//! erroring; it is the only place that says which rfkill switch belongs to
//! which adapter; and it is a directory tree, which a test can lay down.
//! Acting on an adapter — reading its flags, taking it up or down, blocking it,
//! running an inquiry — goes over the socket, because sysfs cannot say any of
//! that.

use std::io;

use crate::sysfs::Sysfs;

// ---------------------------------------------------------------------------
// kernel constants
// ---------------------------------------------------------------------------

/// `HCI_MAX_DEV` from `include/net/bluetooth/hci.h`. Nothing above this can
/// exist, so a directory claiming otherwise was not made by the kernel.
const HCI_MAX_DEV: u16 = 16;

/// `AF_BLUETOOTH` and `BTPROTO_HCI`. Written out rather than taken from libc so
/// the numbers sit next to the comment saying where they came from.
#[cfg(target_os = "linux")]
const AF_BLUETOOTH: libc::c_int = 31;
#[cfg(target_os = "linux")]
const BTPROTO_HCI: libc::c_int = 1;

/// Bits of `hci_dev_info.flags`, from `include/net/bluetooth/hci.h`.
const HCI_UP: u32 = 1 << 0;
const HCI_PSCAN: u32 = 1 << 3;
const HCI_ISCAN: u32 = 1 << 4;
const HCI_INQUIRY: u32 = 1 << 7;

// ---------------------------------------------------------------------------
// fixed layout kernel structures
// ---------------------------------------------------------------------------

/// `struct hci_dev_info` from `include/net/bluetooth/hci_sock.h`.
///
/// Declared on every target, not only Linux, so the layout assertions in the
/// tests below run on a developer's machine as well as in CI. Most of the
/// fields are never read — they are here because the kernel's copy is this
/// shape, and a short one would be filled in wrong.
#[repr(C)]
#[derive(Clone, Copy)]
#[allow(dead_code)]
struct HciDevInfo {
    dev_id: u16,
    name: [u8; 8],
    bdaddr: [u8; 6],
    flags: u32,
    dev_type: u8,
    features: [u8; 8],
    pkt_type: u32,
    link_policy: u32,
    link_mode: u32,
    acl_mtu: u16,
    acl_pkts: u16,
    sco_mtu: u16,
    sco_pkts: u16,
    stat: HciDevStats,
}

/// `struct hci_dev_stats`, the tail of `hci_dev_info`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
#[allow(dead_code)]
struct HciDevStats {
    err_rx: u32,
    err_tx: u32,
    cmd_tx: u32,
    evt_rx: u32,
    acl_tx: u32,
    acl_rx: u32,
    sco_tx: u32,
    sco_rx: u32,
    byte_rx: u32,
    byte_tx: u32,
}

impl Default for HciDevInfo {
    fn default() -> Self {
        // All zeroes is what the kernel expects to be handed: it fills in
        // everything except `dev_id`.
        unsafe { std::mem::zeroed() }
    }
}

/// `struct hci_inquiry_req`, the head of the buffer `HCIINQUIRY` is given.
///
/// The bytes are assembled by hand in [`inquiry_request_bytes`] rather than by
/// writing this structure through a pointer; it is here for its size, which is
/// where the results start, and to say what those bytes are.
#[repr(C)]
#[derive(Clone, Copy)]
#[allow(dead_code)]
struct HciInquiryReq {
    dev_id: u16,
    flags: u16,
    lap: [u8; 3],
    /// Inquiry length in units of 1.28 seconds, as the controller counts it.
    length: u8,
    num_rsp: u8,
}

/// `struct inquiry_info`, one result. Packed in the kernel, so packed here.
#[repr(C, packed)]
#[derive(Clone, Copy)]
#[allow(dead_code)]
struct InquiryInfo {
    bdaddr: [u8; 6],
    pscan_rep_mode: u8,
    pscan_period_mode: u8,
    pscan_mode: u8,
    dev_class: [u8; 3],
    clock_offset: u16,
}

/// `struct rfkill_event` from `include/uapi/linux/rfkill.h`.
///
/// Newer kernels have grown this structure, but they still accept a write of
/// the original eight bytes, and the original is all a soft block needs.
/// Assembled by hand in [`rfkill_event_bytes`], for the same reason as above.
#[repr(C, packed)]
#[derive(Clone, Copy)]
#[allow(dead_code)]
struct RfkillEvent {
    idx: u32,
    kind: u8,
    op: u8,
    soft: u8,
    hard: u8,
}

// ---------------------------------------------------------------------------
// what a caller sees
// ---------------------------------------------------------------------------

/// One controller, as the kernel has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Adapter {
    /// Kernel name, such as `hci0`.
    pub name: String,
    /// The number in the name, which is what every ioctl here is addressed by.
    pub index: u16,
    /// `AA:BB:CC:DD:EE:FF`, or empty when the adapter has never been up and so
    /// has never told the kernel its address.
    pub address: String,
    /// The adapter is up and the kernel is talking to it.
    pub powered: bool,
    /// Answers inquiries: what a phone would call "visible".
    pub discoverable: bool,
    /// Answers connection attempts from a device that already knows it.
    pub connectable: bool,
    /// An inquiry is running on this adapter right now.
    pub inquiring: bool,
    /// Whether [`powered`](Adapter::powered) and the flags beside it were read
    /// from the kernel at all. False means the HCI socket could not be opened —
    /// no privilege, or no Bluetooth in the kernel — and those flags are the
    /// safe assumption rather than an answer.
    pub state_known: bool,
    /// The kill switch belonging to this adapter, when it has one. Every real
    /// adapter registers one; a fake tree need not.
    pub rfkill: Option<RfKill>,
}

impl Adapter {
    /// Blocked by software or by a physical switch, either of which means the
    /// adapter cannot be brought up as things stand.
    pub fn is_blocked(&self) -> bool {
        self.rfkill.as_ref().is_some_and(|r| r.soft || r.hard)
    }

    /// Blocked by a physical switch, which no amount of writing to
    /// `/dev/rfkill` will undo.
    pub fn is_hard_blocked(&self) -> bool {
        self.rfkill.as_ref().is_some_and(|r| r.hard)
    }

    /// A line for the status area.
    pub fn summary(&self) -> String {
        let address = if self.address.is_empty() {
            "no address".to_string()
        } else {
            self.address.clone()
        };
        format!("{}  {}  {}", self.name, address, self.state())
    }

    /// The one word a person wants: what is this adapter doing.
    pub fn state(&self) -> &'static str {
        if self.is_hard_blocked() {
            "hard blocked"
        } else if self.is_blocked() {
            "blocked"
        } else if !self.state_known {
            "unknown"
        } else if self.inquiring {
            "scanning"
        } else if self.powered {
            "on"
        } else {
            "off"
        }
    }
}

/// The kill switch registered for an adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RfKill {
    /// The index a `/dev/rfkill` event is addressed by.
    pub index: u32,
    /// Blocked by software, which is the one that can be undone from here.
    pub soft: bool,
    /// Blocked by a physical switch.
    pub hard: bool,
}

/// A link the kernel currently holds to another device.
///
/// This is emphatically not a list of paired devices. The kernel does not keep
/// one: pairing state belongs to whatever did the pairing, and on an ordinary
/// Linux machine that is BlueZ's own store under `/var/lib/bluetooth`. What
/// sysfs has is the links that exist right now, which is a real listing and is
/// all that can be listed honestly from here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connection {
    /// `AA:BB:CC:DD:EE:FF` of the far end.
    pub address: String,
    /// The connection handle, which is the number in the directory name.
    pub handle: u16,
    /// What kind of link it is.
    pub kind: LinkKind,
}

/// The `type` attribute of a link directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    /// Data, which is most things.
    Acl,
    /// Voice.
    Sco,
    /// Better voice.
    ESco,
    /// Bluetooth Low Energy.
    Le,
    /// Something this code has not been taught.
    Other,
}

impl LinkKind {
    fn parse(text: &str) -> LinkKind {
        match text.trim().to_ascii_uppercase().as_str() {
            "ACL" => LinkKind::Acl,
            "SCO" => LinkKind::Sco,
            "ESCO" => LinkKind::ESco,
            "LE" => LinkKind::Le,
            _ => LinkKind::Other,
        }
    }

    /// A word for the listing.
    pub fn label(&self) -> &'static str {
        match self {
            LinkKind::Acl => "data",
            LinkKind::Sco | LinkKind::ESco => "voice",
            LinkKind::Le => "low energy",
            LinkKind::Other => "link",
        }
    }
}

/// A device that answered an inquiry.
///
/// There is no name here. A name means a remote name request, which means a
/// connection, which is machinery this module does not have. What an inquiry
/// gives is an address and a class, and the class is enough to say what sort of
/// thing answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovered {
    pub address: String,
    /// The 24 bit class of device.
    pub class: u32,
}

impl Discovered {
    /// What the class of device says this is.
    pub fn kind(&self) -> DeviceKind {
        DeviceKind::from_class(self.class)
    }
}

/// The major device class, which is as much as an inquiry result can tell you.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    Computer,
    Phone,
    Network,
    AudioVideo,
    Peripheral,
    Imaging,
    Wearable,
    Toy,
    Health,
    Unknown,
}

impl DeviceKind {
    /// Bits 8 to 12 of the class of device are the major class, from the
    /// Bluetooth assigned numbers.
    pub fn from_class(class: u32) -> DeviceKind {
        match (class >> 8) & 0x1f {
            1 => DeviceKind::Computer,
            2 => DeviceKind::Phone,
            3 => DeviceKind::Network,
            4 => DeviceKind::AudioVideo,
            5 => DeviceKind::Peripheral,
            6 => DeviceKind::Imaging,
            7 => DeviceKind::Wearable,
            8 => DeviceKind::Toy,
            9 => DeviceKind::Health,
            _ => DeviceKind::Unknown,
        }
    }

    /// A word for the listing.
    pub fn label(&self) -> &'static str {
        match self {
            DeviceKind::Computer => "computer",
            DeviceKind::Phone => "phone",
            DeviceKind::Network => "network",
            DeviceKind::AudioVideo => "audio",
            DeviceKind::Peripheral => "peripheral",
            DeviceKind::Imaging => "imaging",
            DeviceKind::Wearable => "wearable",
            DeviceKind::Toy => "toy",
            DeviceKind::Health => "health",
            DeviceKind::Unknown => "device",
        }
    }
}

/// Why an adapter could not be changed.
#[derive(Debug)]
pub enum Error {
    /// No adapter by that name, which on most machines means no Bluetooth.
    NoAdapter(String),
    /// A kill switch is in the way. `hardware` means a physical switch, which
    /// software cannot undo: the user has to flip it.
    Blocked { hardware: bool },
    /// The adapter is down, and what was asked needs it up.
    NotPowered(String),
    /// The kernel refused for want of privilege. Taking an adapter up needs
    /// `CAP_NET_ADMIN`, which in tOS means running as root.
    NotPermitted,
    /// This build has no Bluetooth in it, which is every build that is not
    /// Linux.
    Unsupported,
    Io(io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NoAdapter(name) => write!(f, "no Bluetooth adapter called {name}"),
            Error::Blocked { hardware: true } => {
                write!(f, "blocked by a hardware switch, which has to be flipped")
            }
            Error::Blocked { hardware: false } => write!(f, "blocked by rfkill"),
            Error::NotPowered(name) => write!(f, "{name} is not powered on"),
            Error::NotPermitted => write!(f, "not permitted; this needs to run as root"),
            Error::Unsupported => write!(f, "this build has no Bluetooth support"),
            Error::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        // `ERFKILL` has no `ErrorKind`, so it is matched by number. It is what
        // the kernel answers when a soft block beat us to the adapter.
        const ERFKILL: i32 = 132;
        match e.raw_os_error() {
            Some(libc::EPERM) | Some(libc::EACCES) => Error::NotPermitted,
            Some(ERFKILL) => Error::Blocked { hardware: false },
            _ if e.kind() == io::ErrorKind::Unsupported => Error::Unsupported,
            _ => Error::Io(e),
        }
    }
}

// ---------------------------------------------------------------------------
// the seam
// ---------------------------------------------------------------------------

/// Everything that is not a file read.
///
/// Every ioctl, socket and device write in this module goes through here, for
/// the same reason `installer/src/exec.rs` has its `Backend`: the logic above
/// it is then testable on a machine with no adapter, no `AF_BLUETOOTH` and no
/// `/dev/rfkill`, which is every machine this is written on.
pub trait Control {
    /// `HCIGETDEVINFO`, returning `hci_dev_info.flags`.
    ///
    /// An error here is ordinary rather than alarming: no Bluetooth in the
    /// kernel, or no privilege to open the socket. The caller turns it into
    /// "state not known" instead of a failure.
    fn device_flags(&mut self, index: u16) -> io::Result<u32>;

    /// `HCIDEVUP`.
    fn power_up(&mut self, index: u16) -> io::Result<()>;

    /// `HCIDEVDOWN`.
    fn power_down(&mut self, index: u16) -> io::Result<()>;

    /// Write a change event to `/dev/rfkill`, addressed by rfkill index.
    fn set_soft_blocked(&mut self, rfkill_index: u32, blocked: bool) -> io::Result<()>;

    /// `HCIINQUIRY`, blocking for roughly `seconds` while the controller looks.
    fn inquiry(&mut self, index: u16, seconds: u8) -> io::Result<Vec<Discovered>>;
}

/// The real one.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemControl;

// ---------------------------------------------------------------------------
// pure parsing and building, which is where the bugs would be
// ---------------------------------------------------------------------------

/// The bytes that go to and come back from the kernel.
///
/// They live together because only the Linux implementation calls them — the
/// ioctls and devices they belong to exist nowhere else — while the tests
/// exercise them on every target, which is the whole reason they are plain
/// functions over bytes rather than pointers written through in place.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod wire {
    use super::{Discovered, InquiryInfo};

    /// `_IOC` from `include/uapi/asm-generic/ioctl.h`, which is the encoding
    /// every architecture tOS targets uses. `tos-platform`'s `drm.rs` builds
    /// its numbers the same way.
    const IOC_WRITE: u64 = 1;
    const IOC_READ: u64 = 2;
    const HCI_IOC_MAGIC: u64 = b'H' as u64;

    const fn ioc(dir: u64, nr: u64, size: u64) -> u64 {
        (dir << 30) | (size << 16) | (HCI_IOC_MAGIC << 8) | nr
    }

    /// From `include/net/bluetooth/hci_sock.h`. All four are declared over
    /// `int`, so the size in the number is four whatever is really passed:
    /// `HCIDEVUP` and `HCIDEVDOWN` take the device number by value rather than
    /// through a pointer, and the other two are given a buffer.
    pub(super) const HCIDEVUP: u64 = ioc(IOC_WRITE, 201, 4);
    pub(super) const HCIDEVDOWN: u64 = ioc(IOC_WRITE, 202, 4);
    pub(super) const HCIGETDEVINFO: u64 = ioc(IOC_READ, 211, 4);
    pub(super) const HCIINQUIRY: u64 = ioc(IOC_READ, 240, 4);

    /// `RFKILL_OP_CHANGE` and `RFKILL_TYPE_BLUETOOTH` from
    /// `include/uapi/linux/rfkill.h`.
    pub(super) const RFKILL_OP_CHANGE: u8 = 2;
    pub(super) const RFKILL_TYPE_BLUETOOTH: u8 = 2;

    /// The General Inquiry Access Code, least significant byte first, from the
    /// Bluetooth core specification. Anything discoverable answers on this one.
    pub(super) const GIAC: [u8; 3] = [0x33, 0x8b, 0x9e];

    /// `IREQ_CACHE_FLUSH`: ask the controller rather than repeating whatever
    /// the kernel's inquiry cache still remembers.
    pub(super) const IREQ_CACHE_FLUSH: u16 = 1;

    /// Asked for an unlimited number of results the kernel will write up to
    /// 255, so that is how much room the inquiry buffer has to have.
    pub(super) const MAX_INQUIRY_RESULTS: usize = 255;

    /// The kernel keeps an address least significant byte first and prints it
    /// the other way round, so anything read as bytes has to be reversed to
    /// match what sysfs says and what is printed on the label of the device.
    pub(super) fn format_address(bytes: &[u8; 6]) -> String {
        let mut text = String::with_capacity(17);
        for (i, byte) in bytes.iter().rev().enumerate() {
            if i > 0 {
                text.push(':');
            }
            text.push_str(&format!("{byte:02X}"));
        }
        text
    }

    /// The eight bytes of a `struct rfkill_event` asking for one device to be
    /// blocked or unblocked.
    ///
    /// The offsets are the structure's: index, type, operation, soft, hard. The
    /// numbers are native endian, because this is a local structure handed to
    /// the kernel on the same machine rather than anything on a wire.
    pub(super) fn rfkill_event_bytes(index: u32, blocked: bool) -> [u8; 8] {
        let mut bytes = [0u8; 8];
        bytes[0..4].copy_from_slice(&index.to_ne_bytes());
        bytes[4] = RFKILL_TYPE_BLUETOOTH;
        bytes[5] = RFKILL_OP_CHANGE;
        bytes[6] = u8::from(blocked);
        // The hardware switch is the kernel's to report, never ours to set.
        bytes[7] = 0;
        bytes
    }

    /// The ten bytes of a `struct hci_inquiry_req`. Byte nine is the padding
    /// the compiler puts on the end of the structure, and the kernel ignores
    /// it.
    pub(super) fn inquiry_request_bytes(index: u16, seconds: u8) -> [u8; 10] {
        let mut bytes = [0u8; 10];
        bytes[0..2].copy_from_slice(&index.to_ne_bytes());
        bytes[2..4].copy_from_slice(&IREQ_CACHE_FLUSH.to_ne_bytes());
        bytes[4..7].copy_from_slice(&GIAC);
        bytes[7] = inquiry_length(seconds);
        // Zero means "however many answer", which the kernel caps at 255 — the
        // number of entries the buffer is allocated for.
        bytes[8] = 0;
        bytes
    }

    /// An inquiry length in the units the controller counts in, 1.28 seconds
    /// each, clamped to the range the specification allows.
    pub(super) fn inquiry_length(seconds: u8) -> u8 {
        let units = (seconds as u16 * 100) / 128;
        units.clamp(1, 0x30) as u8
    }

    /// Results out of the buffer `HCIINQUIRY` wrote into.
    ///
    /// `claimed` is the count the kernel put back in the request. It comes out
    /// of a buffer, so it is clamped to what that buffer can actually hold
    /// before anything is indexed by it.
    pub(super) fn parse_inquiry_results(buffer: &[u8], claimed: usize) -> Vec<Discovered> {
        let entry = std::mem::size_of::<InquiryInfo>();
        let count = claimed.min(buffer.len() / entry);
        let mut found = Vec::with_capacity(count);
        for i in 0..count {
            let Some(bytes) = buffer.get(i * entry..i * entry + entry) else {
                break;
            };
            let mut address = [0u8; 6];
            address.copy_from_slice(&bytes[0..6]);
            // The class of device is three bytes at offset nine, least
            // significant first.
            let class = bytes[9] as u32 | (bytes[10] as u32) << 8 | (bytes[11] as u32) << 16;
            found.push(Discovered {
                address: format_address(&address),
                class,
            });
        }
        found
    }
}

/// Whether a string is `AA:BB:CC:DD:EE:FF`, which keeps anything that is not an
/// address out of a listing of addresses.
fn looks_like_address(text: &str) -> bool {
    let mut parts = 0;
    for part in text.split(':') {
        if part.len() != 2 || !part.bytes().all(|b| b.is_ascii_hexdigit()) {
            return false;
        }
        parts += 1;
    }
    parts == 6
}

/// The number in `hci7`, when the name is an adapter's name at all.
fn adapter_index(name: &str) -> Option<u16> {
    let digits = name.strip_prefix("hci")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let index: u16 = digits.parse().ok()?;
    (index < HCI_MAX_DEV).then_some(index)
}

/// The handle in `hci0:256`, when the name is a link directory.
fn link_handle(adapter: &str, name: &str) -> Option<u16> {
    let rest = name.strip_prefix(adapter)?.strip_prefix(':')?;
    if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    rest.parse().ok()
}

/// The index in `rfkill3`.
fn rfkill_index(name: &str) -> Option<u32> {
    let digits = name.strip_prefix("rfkill")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

// ---------------------------------------------------------------------------
// the module's face
// ---------------------------------------------------------------------------

/// Bluetooth as tOS can see it.
pub struct Bluetooth<C: Control = SystemControl> {
    sysfs: Sysfs,
    control: C,
}

impl Bluetooth<SystemControl> {
    /// The running machine.
    pub fn system() -> Bluetooth<SystemControl> {
        Bluetooth {
            sysfs: Sysfs::system(),
            control: SystemControl,
        }
    }
}

impl<C: Control> Bluetooth<C> {
    pub fn new(sysfs: Sysfs, control: C) -> Bluetooth<C> {
        Bluetooth { sysfs, control }
    }

    /// Whether this machine has Bluetooth at all.
    ///
    /// Most machines tOS runs on do not, and that is not a failure or a thing
    /// to report. A caller drawing a status area asks this and draws nothing.
    pub fn is_present(&self) -> bool {
        !self.adapter_names().is_empty()
    }

    /// Every adapter, in name order.
    pub fn adapters(&mut self) -> Vec<Adapter> {
        self.adapter_names()
            .into_iter()
            .map(|(name, index)| self.read_adapter(&name, index))
            .collect()
    }

    /// One adapter by name, such as `hci0`.
    pub fn adapter(&mut self, name: &str) -> Option<Adapter> {
        let index = adapter_index(name)?;
        self.sysfs
            .exists(&format!("/sys/class/bluetooth/{name}"))
            .then(|| self.read_adapter(name, index))
    }

    /// The first adapter, which is the one a machine with a single adapter
    /// means whenever anybody says "Bluetooth".
    pub fn default_adapter(&mut self) -> Option<Adapter> {
        let (name, index) = self.adapter_names().into_iter().next()?;
        Some(self.read_adapter(&name, index))
    }

    /// The names sysfs has, with their index, in order. Anything that is not
    /// `hci<n>` is not an adapter, and is skipped rather than guessed at.
    fn adapter_names(&self) -> Vec<(String, u16)> {
        self.sysfs
            .list("/sys/class/bluetooth")
            .into_iter()
            .filter_map(|name| adapter_index(&name).map(|index| (name, index)))
            .collect()
    }

    fn read_adapter(&mut self, name: &str, index: u16) -> Adapter {
        let base = format!("/sys/class/bluetooth/{name}");
        let address = self
            .sysfs
            .read(&format!("{base}/address"))
            .map(|text| text.trim().to_ascii_uppercase())
            .filter(|text| looks_like_address(text))
            .unwrap_or_default();

        // Flags are the only way to know whether an adapter is up; sysfs does
        // not say. Not being able to ask is ordinary, so it becomes "not
        // known" rather than an error every caller would have to handle.
        let flags = self.control.device_flags(index);
        let state_known = flags.is_ok();
        let flags = flags.unwrap_or(0);

        Adapter {
            name: name.to_string(),
            index,
            address,
            // `HCI_RUNNING` without `HCI_UP` is an adapter part way through
            // initialising, which is not usable and so is not powered.
            powered: flags & HCI_UP != 0,
            discoverable: flags & HCI_ISCAN != 0,
            connectable: flags & HCI_PSCAN != 0,
            inquiring: flags & HCI_INQUIRY != 0,
            state_known,
            rfkill: self.read_rfkill(name),
        }
    }

    /// The adapter's kill switch.
    ///
    /// The switch is registered as a child of the adapter, so that child
    /// directory is the authoritative link between the two. A tree that does
    /// not have it there gets a second pass matching on the name rfkill
    /// records, which is the adapter's own name.
    fn read_rfkill(&self, adapter: &str) -> Option<RfKill> {
        let base = format!("/sys/class/bluetooth/{adapter}");
        for child in self.sysfs.list(&base) {
            if let Some(index) = rfkill_index(&child) {
                return Some(self.read_rfkill_at(&format!("{base}/{child}"), index));
            }
        }

        for child in self.sysfs.list("/sys/class/rfkill") {
            let Some(index) = rfkill_index(&child) else {
                continue;
            };
            let path = format!("/sys/class/rfkill/{child}");
            let is_bluetooth = self
                .sysfs
                .read(&format!("{path}/type"))
                .is_some_and(|kind| kind.trim() == "bluetooth");
            let is_ours = self
                .sysfs
                .read(&format!("{path}/name"))
                .is_some_and(|name| name.trim() == adapter);
            if is_bluetooth && is_ours {
                return Some(self.read_rfkill_at(&path, index));
            }
        }
        None
    }

    fn read_rfkill_at(&self, path: &str, fallback_index: u32) -> RfKill {
        RfKill {
            // The `index` file is what an event is addressed by; the number in
            // the directory name is the same thing, and is the fallback.
            index: self
                .sysfs
                .read_number(&format!("{path}/index"))
                .unwrap_or(fallback_index),
            soft: self.sysfs.read_number::<u32>(&format!("{path}/soft")) == Some(1),
            hard: self.sysfs.read_number::<u32>(&format!("{path}/hard")) == Some(1),
        }
    }

    /// The links this adapter currently holds.
    ///
    /// See [`Connection`]: these are live links, not remembered devices.
    pub fn connections(&self, adapter: &str) -> Vec<Connection> {
        let base = format!("/sys/class/bluetooth/{adapter}");
        let mut links = Vec::new();
        for child in self.sysfs.list(&base) {
            // A link directory is named for its adapter and handle. The other
            // children — `rfkill3`, `power`, `subsystem` — are not links, and
            // have no address to read.
            let Some(handle) = link_handle(adapter, &child) else {
                continue;
            };
            let address = self
                .sysfs
                .read(&format!("{base}/{child}/address"))
                .map(|text| text.trim().to_ascii_uppercase())
                .filter(|text| looks_like_address(text));
            let Some(address) = address else {
                continue;
            };
            links.push(Connection {
                address,
                handle,
                kind: self
                    .sysfs
                    .read(&format!("{base}/{child}/type"))
                    .map(|text| LinkKind::parse(&text))
                    .unwrap_or(LinkKind::Other),
            });
        }
        links
    }

    /// Bring an adapter up, unblocking it first if a soft block is in the way.
    ///
    /// The order matters: `HCIDEVUP` on a soft blocked adapter fails with
    /// `ERFKILL`, so the block has to go first. A hardware switch is refused
    /// outright rather than attempted, because nothing here can move it.
    pub fn power_on(&mut self, adapter: &Adapter) -> Result<(), Error> {
        if adapter.is_hard_blocked() {
            return Err(Error::Blocked { hardware: true });
        }
        if let Some(rfkill) = adapter.rfkill.as_ref().filter(|r| r.soft) {
            self.control.set_soft_blocked(rfkill.index, false)?;
        }
        match self.control.power_up(adapter.index) {
            Ok(()) => Ok(()),
            // Already being up is the state that was asked for.
            Err(e) if e.raw_os_error() == Some(libc::EALREADY) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Take an adapter down.
    ///
    /// This leaves the kill switch alone. Taking an adapter down and blocking
    /// it are different requests: a block outlives the adapter coming back, and
    /// is not what someone turning Bluetooth off usually means.
    pub fn power_off(&mut self, adapter: &Adapter) -> Result<(), Error> {
        match self.control.power_down(adapter.index) {
            Ok(()) => Ok(()),
            Err(e) if e.raw_os_error() == Some(libc::EALREADY) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Set or clear the software kill switch.
    ///
    /// Blocking also takes the adapter down, because the kernel does that
    /// itself. Unblocking does not bring it back up; [`power_on`](Self::power_on)
    /// is what does that.
    pub fn set_blocked(&mut self, adapter: &Adapter, blocked: bool) -> Result<(), Error> {
        let Some(rfkill) = adapter.rfkill.as_ref() else {
            return Err(Error::NoAdapter(adapter.name.clone()));
        };
        if rfkill.hard && !blocked {
            return Err(Error::Blocked { hardware: true });
        }
        self.control.set_soft_blocked(rfkill.index, blocked)?;
        Ok(())
    }

    /// Look for devices in range.
    ///
    /// This is a BR/EDR inquiry and nothing more. It finds devices that are
    /// discoverable at the moment and says what their address is and roughly
    /// what they are. It does not find Low Energy devices, which advertise
    /// rather than answering an inquiry, and it does not learn names, which
    /// take a connection.
    ///
    /// Pairing with anything found here is not possible from this module, and
    /// is not a small addition: it would need SMP or legacy pairing over an L2CAP
    /// channel and an agent to answer for the user, which is a piece of work in
    /// its own right rather than a missing function.
    ///
    /// The call blocks for about `seconds` while the controller listens, so it
    /// does not belong on a thread that is drawing.
    pub fn scan(&mut self, adapter: &Adapter, seconds: u8) -> Result<Vec<Discovered>, Error> {
        if adapter.is_blocked() {
            return Err(Error::Blocked {
                hardware: adapter.is_hard_blocked(),
            });
        }
        if !adapter.powered {
            return Err(Error::NotPowered(adapter.name.clone()));
        }
        Ok(self.control.inquiry(adapter.index, seconds)?)
    }

    /// The seam, for a caller with its own reason to reach past this.
    pub fn control_mut(&mut self) -> &mut C {
        &mut self.control
    }
}

// ---------------------------------------------------------------------------
// the Linux implementation
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod imp {
    use super::wire::*;
    use super::*;
    use std::os::unix::io::RawFd;

    /// An HCI control socket, closed when it goes out of scope.
    ///
    /// One is opened per call rather than kept: the socket is cheap, and a long
    /// lived descriptor on a device that can be unplugged is not worth the
    /// trouble for a status readout.
    struct Socket(RawFd);

    impl Socket {
        fn open() -> io::Result<Socket> {
            let fd = unsafe {
                libc::socket(
                    AF_BLUETOOTH,
                    libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                    BTPROTO_HCI,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Socket(fd))
        }
    }

    impl Drop for Socket {
        fn drop(&mut self) {
            unsafe { libc::close(self.0) };
        }
    }

    fn ioctl_value(fd: RawFd, request: u64, value: libc::c_int) -> io::Result<()> {
        let result = unsafe { libc::ioctl(fd, request as _, value) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn ioctl_ptr<T>(fd: RawFd, request: u64, arg: *mut T) -> io::Result<()> {
        let result = unsafe { libc::ioctl(fd, request as _, arg) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    impl Control for SystemControl {
        fn device_flags(&mut self, index: u16) -> io::Result<u32> {
            let socket = Socket::open()?;
            let mut info = HciDevInfo {
                dev_id: index,
                ..HciDevInfo::default()
            };
            ioctl_ptr(socket.0, HCIGETDEVINFO, &mut info)?;
            Ok(info.flags)
        }

        fn power_up(&mut self, index: u16) -> io::Result<()> {
            let socket = Socket::open()?;
            ioctl_value(socket.0, HCIDEVUP, index as libc::c_int)
        }

        fn power_down(&mut self, index: u16) -> io::Result<()> {
            let socket = Socket::open()?;
            ioctl_value(socket.0, HCIDEVDOWN, index as libc::c_int)
        }

        fn set_soft_blocked(&mut self, rfkill_index: u32, blocked: bool) -> io::Result<()> {
            use std::io::Write;

            let mut device = std::fs::OpenOptions::new().write(true).open("/dev/rfkill")?;
            device.write_all(&rfkill_event_bytes(rfkill_index, blocked))
        }

        fn inquiry(&mut self, index: u16, seconds: u8) -> io::Result<Vec<Discovered>> {
            let socket = Socket::open()?;
            let request = inquiry_request_bytes(index, seconds);
            let head = request.len();
            let entry = std::mem::size_of::<InquiryInfo>();

            // The kernel copies the request back with the number of answers in
            // it, then writes that many entries immediately afterwards, so the
            // two live in one buffer.
            let mut buffer = vec![0u8; head + MAX_INQUIRY_RESULTS * entry];
            buffer[..head].copy_from_slice(&request);
            ioctl_ptr(socket.0, HCIINQUIRY, buffer.as_mut_ptr())?;

            // Byte eight of the request is `num_rsp`. It has been written by
            // the kernel into a buffer, so it is treated as a claim and
            // checked against the buffer rather than trusted.
            let answered = buffer[8] as usize;
            Ok(parse_inquiry_results(
                buffer.get(head..).unwrap_or(&[]),
                answered,
            ))
        }
    }
}

/// Everywhere that is not Linux.
///
/// `AF_BLUETOOTH` and `/sys` are Linux, and this is developed on machines that
/// have neither. Refusing at run time rather than failing to compile is what
/// keeps the tests above runnable there.
#[cfg(not(target_os = "linux"))]
mod imp {
    use super::*;

    fn unsupported() -> io::Error {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "Bluetooth needs a Linux HCI socket",
        )
    }

    impl Control for SystemControl {
        fn device_flags(&mut self, _index: u16) -> io::Result<u32> {
            Err(unsupported())
        }

        fn power_up(&mut self, _index: u16) -> io::Result<()> {
            Err(unsupported())
        }

        fn power_down(&mut self, _index: u16) -> io::Result<()> {
            Err(unsupported())
        }

        fn set_soft_blocked(&mut self, _rfkill_index: u32, _blocked: bool) -> io::Result<()> {
            Err(unsupported())
        }

        fn inquiry(&mut self, _index: u16, _seconds: u8) -> io::Result<Vec<Discovered>> {
            Err(unsupported())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::wire::*;
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    /// A sysfs tree laid out by hand, cleaned up when it goes out of scope.
    struct Tree {
        root: PathBuf,
    }

    impl Tree {
        fn new(name: &str) -> Tree {
            let root =
                std::env::temp_dir().join(format!("tos-bluetooth-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            Tree { root }
        }

        fn dir(&self, path: &str) -> &Tree {
            std::fs::create_dir_all(self.root.join(path.trim_start_matches('/'))).unwrap();
            self
        }

        fn file(&self, path: &str, contents: &str) -> &Tree {
            let full = self.root.join(path.trim_start_matches('/'));
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, contents).unwrap();
            self
        }

        /// An adapter as the kernel lays one out: a directory with an address
        /// in it and the usual uninteresting neighbours.
        fn adapter(&self, name: &str, address: &str) -> &Tree {
            let base = format!("/sys/class/bluetooth/{name}");
            self.file(&format!("{base}/address"), &format!("{address}\n"));
            self.dir(&format!("{base}/power"));
            self.file(&format!("{base}/uevent"), "DEVTYPE=host\n");
            self
        }

        /// The kill switch the adapter registers as its own child.
        fn switch(&self, adapter: &str, index: u32, soft: u32, hard: u32) -> &Tree {
            let base = format!("/sys/class/bluetooth/{adapter}/rfkill{index}");
            self.file(&format!("{base}/index"), &format!("{index}\n"));
            self.file(&format!("{base}/soft"), &format!("{soft}\n"));
            self.file(&format!("{base}/hard"), &format!("{hard}\n"));
            self.file(&format!("{base}/type"), "bluetooth\n");
            self.file(&format!("{base}/name"), &format!("{adapter}\n"));
            self
        }

        fn sysfs(&self) -> Sysfs {
            Sysfs::new(&self.root)
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// A control that writes down what it was asked instead of doing it, the
    /// way `exec::Recorder` does for the installer.
    #[derive(Default)]
    struct Recorder {
        flags: BTreeMap<u16, u32>,
        actions: Vec<String>,
        /// The HCI socket cannot be opened at all.
        no_socket: bool,
        /// An errno `HCIDEVUP` should fail with.
        up_fails_with: Option<i32>,
        found: Vec<Discovered>,
    }

    impl Recorder {
        fn new() -> Recorder {
            Recorder::default()
        }

        fn with_flags(mut self, index: u16, flags: u32) -> Recorder {
            self.flags.insert(index, flags);
            self
        }

        fn without_a_socket(mut self) -> Recorder {
            self.no_socket = true;
            self
        }

        fn failing_to_power_up(mut self, errno: i32) -> Recorder {
            self.up_fails_with = Some(errno);
            self
        }

        fn finding(mut self, address: &str, class: u32) -> Recorder {
            self.found.push(Discovered {
                address: address.to_string(),
                class,
            });
            self
        }
    }

    impl Control for Recorder {
        fn device_flags(&mut self, index: u16) -> io::Result<u32> {
            if self.no_socket {
                return Err(io::Error::from_raw_os_error(libc::EACCES));
            }
            Ok(self.flags.get(&index).copied().unwrap_or(0))
        }

        fn power_up(&mut self, index: u16) -> io::Result<()> {
            self.actions.push(format!("up hci{index}"));
            match self.up_fails_with {
                Some(errno) => Err(io::Error::from_raw_os_error(errno)),
                None => Ok(()),
            }
        }

        fn power_down(&mut self, index: u16) -> io::Result<()> {
            self.actions.push(format!("down hci{index}"));
            Ok(())
        }

        fn set_soft_blocked(&mut self, rfkill_index: u32, blocked: bool) -> io::Result<()> {
            self.actions
                .push(format!("rfkill {rfkill_index} soft={}", u8::from(blocked)));
            Ok(())
        }

        fn inquiry(&mut self, index: u16, seconds: u8) -> io::Result<Vec<Discovered>> {
            self.actions.push(format!("inquiry hci{index} {seconds}s"));
            Ok(self.found.clone())
        }
    }

    fn actions(bluetooth: &mut Bluetooth<Recorder>) -> Vec<String> {
        bluetooth.control_mut().actions.clone()
    }

    // -- enumeration --------------------------------------------------------

    #[test]
    fn a_machine_with_no_bluetooth_says_so_quietly() {
        let tree = Tree::new("absent");
        let mut bluetooth = Bluetooth::new(tree.sysfs(), Recorder::new());
        assert!(!bluetooth.is_present());
        assert!(bluetooth.adapters().is_empty());
        assert!(bluetooth.default_adapter().is_none());
        assert!(bluetooth.connections("hci0").is_empty());
    }

    #[test]
    fn a_bluetooth_directory_with_nothing_in_it_is_still_no_adapter() {
        let tree = Tree::new("empty-class");
        tree.dir("/sys/class/bluetooth");
        let mut bluetooth = Bluetooth::new(tree.sysfs(), Recorder::new());
        assert!(!bluetooth.is_present());
        assert!(bluetooth.adapters().is_empty());
    }

    #[test]
    fn an_adapter_is_read_with_its_address_and_index() {
        let tree = Tree::new("one");
        tree.adapter("hci0", "00:1A:7D:DA:71:13");
        let mut bluetooth = Bluetooth::new(tree.sysfs(), Recorder::new());
        let adapters = bluetooth.adapters();
        assert_eq!(adapters.len(), 1);
        assert_eq!(adapters[0].name, "hci0");
        assert_eq!(adapters[0].index, 0);
        assert_eq!(adapters[0].address, "00:1A:7D:DA:71:13");
        assert!(!adapters[0].powered);
        assert!(adapters[0].state_known);
        assert!(adapters[0].rfkill.is_none());
    }

    #[test]
    fn adapters_come_back_in_a_settled_order() {
        let tree = Tree::new("order");
        tree.adapter("hci1", "00:00:00:00:00:01");
        tree.adapter("hci0", "00:00:00:00:00:00");
        let mut bluetooth = Bluetooth::new(tree.sysfs(), Recorder::new());
        let names: Vec<String> = bluetooth.adapters().into_iter().map(|a| a.name).collect();
        assert_eq!(names, vec!["hci0", "hci1"]);
        assert_eq!(bluetooth.default_adapter().unwrap().name, "hci0");
    }

    #[test]
    fn a_directory_that_is_not_an_adapter_is_ignored() {
        let tree = Tree::new("junk");
        tree.dir("/sys/class/bluetooth/hci");
        tree.dir("/sys/class/bluetooth/hcifoo");
        // Above `HCI_MAX_DEV`, so not something the kernel made.
        tree.dir("/sys/class/bluetooth/hci99");
        tree.dir("/sys/class/bluetooth/rfkill0");
        let mut bluetooth = Bluetooth::new(tree.sysfs(), Recorder::new());
        assert!(bluetooth.adapters().is_empty());
        assert!(bluetooth.adapter("hci99").is_none());
    }

    #[test]
    fn an_address_the_kernel_did_not_fill_in_is_left_empty() {
        let tree = Tree::new("no-address");
        tree.dir("/sys/class/bluetooth/hci0");
        tree.file("/sys/class/bluetooth/hci1/address", "not an address\n");
        let mut bluetooth = Bluetooth::new(tree.sysfs(), Recorder::new());
        let adapters = bluetooth.adapters();
        assert_eq!(adapters[0].address, "");
        assert_eq!(adapters[1].address, "");
    }

    #[test]
    fn asking_for_an_adapter_that_is_not_there_finds_nothing() {
        let tree = Tree::new("by-name");
        tree.adapter("hci0", "00:1A:7D:DA:71:13");
        let mut bluetooth = Bluetooth::new(tree.sysfs(), Recorder::new());
        assert_eq!(bluetooth.adapter("hci0").unwrap().index, 0);
        assert!(bluetooth.adapter("hci1").is_none());
        assert!(bluetooth.adapter("wlan0").is_none());
    }

    // -- state --------------------------------------------------------------

    #[test]
    fn an_adapter_the_kernel_has_up_reads_as_powered() {
        let tree = Tree::new("powered");
        tree.adapter("hci0", "00:1A:7D:DA:71:13");
        let control = Recorder::new().with_flags(0, HCI_UP | HCI_PSCAN | HCI_ISCAN);
        let mut bluetooth = Bluetooth::new(tree.sysfs(), control);
        let adapter = bluetooth.adapter("hci0").unwrap();
        assert!(adapter.powered);
        assert!(adapter.connectable);
        assert!(adapter.discoverable);
        assert!(!adapter.inquiring);
        assert_eq!(adapter.state(), "on");
    }

    #[test]
    fn an_adapter_part_way_through_starting_is_not_yet_powered() {
        let tree = Tree::new("starting");
        tree.adapter("hci0", "00:1A:7D:DA:71:13");
        // `HCI_RUNNING` with no `HCI_UP`.
        let control = Recorder::new().with_flags(0, 1 << 2);
        let mut bluetooth = Bluetooth::new(tree.sysfs(), control);
        assert!(!bluetooth.adapter("hci0").unwrap().powered);
    }

    #[test]
    fn an_adapter_in_the_middle_of_an_inquiry_says_it_is_scanning() {
        let tree = Tree::new("scanning");
        tree.adapter("hci0", "00:1A:7D:DA:71:13");
        let control = Recorder::new().with_flags(0, HCI_UP | HCI_INQUIRY);
        let mut bluetooth = Bluetooth::new(tree.sysfs(), control);
        let adapter = bluetooth.adapter("hci0").unwrap();
        assert!(adapter.inquiring);
        assert_eq!(adapter.state(), "scanning");
    }

    #[test]
    fn an_adapter_whose_state_cannot_be_read_says_unknown_rather_than_off() {
        let tree = Tree::new("no-socket");
        tree.adapter("hci0", "00:1A:7D:DA:71:13");
        let mut bluetooth = Bluetooth::new(tree.sysfs(), Recorder::new().without_a_socket());
        let adapter = bluetooth.adapter("hci0").unwrap();
        assert!(!adapter.state_known);
        assert!(!adapter.powered);
        assert_eq!(adapter.state(), "unknown");
        // The adapter is still listed: not being able to ask is not absence.
        assert!(bluetooth.is_present());
    }

    #[test]
    fn a_summary_reads_as_a_line_for_a_person() {
        let tree = Tree::new("summary");
        tree.adapter("hci0", "00:1A:7D:DA:71:13");
        let control = Recorder::new().with_flags(0, HCI_UP);
        let mut bluetooth = Bluetooth::new(tree.sysfs(), control);
        assert_eq!(
            bluetooth.adapter("hci0").unwrap().summary(),
            "hci0  00:1A:7D:DA:71:13  on"
        );

        let bare = Tree::new("summary-bare");
        bare.dir("/sys/class/bluetooth/hci0");
        let mut bluetooth = Bluetooth::new(bare.sysfs(), Recorder::new());
        assert_eq!(
            bluetooth.adapter("hci0").unwrap().summary(),
            "hci0  no address  off"
        );
    }

    // -- rfkill -------------------------------------------------------------

    #[test]
    fn a_soft_block_is_read_from_the_switch_under_the_adapter() {
        let tree = Tree::new("soft");
        tree.adapter("hci0", "00:1A:7D:DA:71:13").switch("hci0", 3, 1, 0);
        let mut bluetooth = Bluetooth::new(tree.sysfs(), Recorder::new());
        let adapter = bluetooth.adapter("hci0").unwrap();
        assert_eq!(
            adapter.rfkill,
            Some(RfKill {
                index: 3,
                soft: true,
                hard: false
            })
        );
        assert!(adapter.is_blocked());
        assert!(!adapter.is_hard_blocked());
        assert_eq!(adapter.state(), "blocked");
    }

    #[test]
    fn a_hardware_switch_is_told_apart_from_a_software_one() {
        let tree = Tree::new("hard");
        tree.adapter("hci0", "00:1A:7D:DA:71:13").switch("hci0", 0, 1, 1);
        let mut bluetooth = Bluetooth::new(tree.sysfs(), Recorder::new());
        let adapter = bluetooth.adapter("hci0").unwrap();
        assert!(adapter.is_hard_blocked());
        assert_eq!(adapter.state(), "hard blocked");
    }

    #[test]
    fn a_switch_is_matched_by_name_when_it_is_not_under_the_adapter() {
        let tree = Tree::new("by-rfkill-name");
        tree.adapter("hci0", "00:1A:7D:DA:71:13");
        tree.file("/sys/class/rfkill/rfkill2/type", "bluetooth\n")
            .file("/sys/class/rfkill/rfkill2/name", "hci0\n")
            .file("/sys/class/rfkill/rfkill2/soft", "1\n")
            .file("/sys/class/rfkill/rfkill2/hard", "0\n");
        let mut bluetooth = Bluetooth::new(tree.sysfs(), Recorder::new());
        let rfkill = bluetooth.adapter("hci0").unwrap().rfkill.unwrap();
        assert_eq!(rfkill.index, 2);
        assert!(rfkill.soft);
    }

    #[test]
    fn the_wireless_cards_switch_is_not_the_bluetooth_one() {
        let tree = Tree::new("wrong-rfkill");
        tree.adapter("hci0", "00:1A:7D:DA:71:13");
        tree.file("/sys/class/rfkill/rfkill0/type", "wlan\n")
            .file("/sys/class/rfkill/rfkill0/name", "phy0\n")
            .file("/sys/class/rfkill/rfkill0/soft", "1\n")
            .file("/sys/class/rfkill/rfkill0/hard", "0\n");
        let mut bluetooth = Bluetooth::new(tree.sysfs(), Recorder::new());
        assert!(bluetooth.adapter("hci0").unwrap().rfkill.is_none());
    }

    // -- acting -------------------------------------------------------------

    #[test]
    fn powering_on_an_adapter_brings_it_up() {
        let tree = Tree::new("up");
        tree.adapter("hci0", "00:1A:7D:DA:71:13");
        let mut bluetooth = Bluetooth::new(tree.sysfs(), Recorder::new());
        let adapter = bluetooth.adapter("hci0").unwrap();
        bluetooth.power_on(&adapter).unwrap();
        assert_eq!(actions(&mut bluetooth), vec!["up hci0"]);
    }

    #[test]
    fn powering_on_a_blocked_adapter_unblocks_it_before_bringing_it_up() {
        let tree = Tree::new("unblock-then-up");
        tree.adapter("hci0", "00:1A:7D:DA:71:13").switch("hci0", 3, 1, 0);
        let mut bluetooth = Bluetooth::new(tree.sysfs(), Recorder::new());
        let adapter = bluetooth.adapter("hci0").unwrap();
        bluetooth.power_on(&adapter).unwrap();
        assert_eq!(
            actions(&mut bluetooth),
            vec!["rfkill 3 soft=0", "up hci0"],
            "the block has to go first or HCIDEVUP fails with ERFKILL"
        );
    }

    #[test]
    fn a_hardware_switch_cannot_be_undone_from_software() {
        let tree = Tree::new("hard-refuses");
        tree.adapter("hci0", "00:1A:7D:DA:71:13").switch("hci0", 3, 1, 1);
        let mut bluetooth = Bluetooth::new(tree.sysfs(), Recorder::new());
        let adapter = bluetooth.adapter("hci0").unwrap();
        let refusal = bluetooth.power_on(&adapter).unwrap_err();
        assert!(matches!(refusal, Error::Blocked { hardware: true }));
        assert!(refusal.to_string().contains("hardware switch"));
        // Nothing was attempted: there is nothing software can do about it.
        assert!(actions(&mut bluetooth).is_empty());

        let adapter = bluetooth.adapter("hci0").unwrap();
        assert!(matches!(
            bluetooth.set_blocked(&adapter, false).unwrap_err(),
            Error::Blocked { hardware: true }
        ));
    }

    #[test]
    fn powering_on_an_adapter_that_is_already_up_is_not_an_error() {
        let tree = Tree::new("already-up");
        tree.adapter("hci0", "00:1A:7D:DA:71:13");
        let control = Recorder::new()
            .with_flags(0, HCI_UP)
            .failing_to_power_up(libc::EALREADY);
        let mut bluetooth = Bluetooth::new(tree.sysfs(), control);
        let adapter = bluetooth.adapter("hci0").unwrap();
        assert!(bluetooth.power_on(&adapter).is_ok());
    }

    #[test]
    fn a_refusal_for_want_of_privilege_says_so() {
        let tree = Tree::new("eperm");
        tree.adapter("hci0", "00:1A:7D:DA:71:13");
        let control = Recorder::new().failing_to_power_up(libc::EPERM);
        let mut bluetooth = Bluetooth::new(tree.sysfs(), control);
        let adapter = bluetooth.adapter("hci0").unwrap();
        let refusal = bluetooth.power_on(&adapter).unwrap_err();
        assert!(matches!(refusal, Error::NotPermitted));
        assert!(refusal.to_string().contains("root"));
    }

    #[test]
    fn an_adapter_blocked_while_we_were_not_looking_is_reported_as_blocked() {
        let tree = Tree::new("erfkill");
        tree.adapter("hci0", "00:1A:7D:DA:71:13");
        // 132 is ERFKILL, which the kernel answers when a block got there first.
        let control = Recorder::new().failing_to_power_up(132);
        let mut bluetooth = Bluetooth::new(tree.sysfs(), control);
        let adapter = bluetooth.adapter("hci0").unwrap();
        assert!(matches!(
            bluetooth.power_on(&adapter).unwrap_err(),
            Error::Blocked { hardware: false }
        ));
    }

    #[test]
    fn powering_off_leaves_the_kill_switch_alone() {
        let tree = Tree::new("down");
        tree.adapter("hci0", "00:1A:7D:DA:71:13").switch("hci0", 3, 0, 0);
        let control = Recorder::new().with_flags(0, HCI_UP);
        let mut bluetooth = Bluetooth::new(tree.sysfs(), control);
        let adapter = bluetooth.adapter("hci0").unwrap();
        bluetooth.power_off(&adapter).unwrap();
        assert_eq!(
            actions(&mut bluetooth),
            vec!["down hci0"],
            "turning Bluetooth off is not the same request as blocking it"
        );
    }

    #[test]
    fn blocking_an_adapter_writes_a_soft_block() {
        let tree = Tree::new("block");
        tree.adapter("hci0", "00:1A:7D:DA:71:13").switch("hci0", 3, 0, 0);
        let mut bluetooth = Bluetooth::new(tree.sysfs(), Recorder::new());
        let adapter = bluetooth.adapter("hci0").unwrap();
        bluetooth.set_blocked(&adapter, true).unwrap();
        assert_eq!(actions(&mut bluetooth), vec!["rfkill 3 soft=1"]);
    }

    #[test]
    fn an_adapter_with_no_kill_switch_cannot_be_blocked() {
        let tree = Tree::new("no-switch");
        tree.adapter("hci0", "00:1A:7D:DA:71:13");
        let mut bluetooth = Bluetooth::new(tree.sysfs(), Recorder::new());
        let adapter = bluetooth.adapter("hci0").unwrap();
        assert!(matches!(
            bluetooth.set_blocked(&adapter, true).unwrap_err(),
            Error::NoAdapter(_)
        ));
    }

    // -- links --------------------------------------------------------------

    #[test]
    fn the_links_the_kernel_holds_are_listed_with_their_kind() {
        let tree = Tree::new("links");
        tree.adapter("hci0", "00:1A:7D:DA:71:13").switch("hci0", 3, 0, 0);
        tree.file("/sys/class/bluetooth/hci0/hci0:256/address", "4C:87:5D:11:22:33\n")
            .file("/sys/class/bluetooth/hci0/hci0:256/type", "ACL\n");
        tree.file("/sys/class/bluetooth/hci0/hci0:257/address", "00:0C:8A:AA:BB:CC\n")
            .file("/sys/class/bluetooth/hci0/hci0:257/type", "LE\n");
        let bluetooth = Bluetooth::new(tree.sysfs(), Recorder::new());
        let links = bluetooth.connections("hci0");
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].address, "4C:87:5D:11:22:33");
        assert_eq!(links[0].handle, 256);
        assert_eq!(links[0].kind, LinkKind::Acl);
        assert_eq!(links[0].kind.label(), "data");
        assert_eq!(links[1].kind, LinkKind::Le);
    }

    #[test]
    fn a_child_directory_that_is_not_a_link_is_not_listed() {
        let tree = Tree::new("not-links");
        tree.adapter("hci0", "00:1A:7D:DA:71:13").switch("hci0", 3, 0, 0);
        tree.dir("/sys/class/bluetooth/hci0/subsystem");
        // A link directory with nothing readable in it is not a device either.
        tree.dir("/sys/class/bluetooth/hci0/hci0:260");
        // Another adapter's link, which is not this adapter's business.
        tree.file("/sys/class/bluetooth/hci0/hci1:256/address", "00:11:22:33:44:55\n");
        let bluetooth = Bluetooth::new(tree.sysfs(), Recorder::new());
        assert!(bluetooth.connections("hci0").is_empty());
    }

    // -- inquiry ------------------------------------------------------------

    #[test]
    fn scanning_asks_the_controller_and_names_what_answered() {
        let tree = Tree::new("scan");
        tree.adapter("hci0", "00:1A:7D:DA:71:13");
        let control = Recorder::new()
            .with_flags(0, HCI_UP)
            .finding("4C:87:5D:11:22:33", 0x240404);
        let mut bluetooth = Bluetooth::new(tree.sysfs(), control);
        let adapter = bluetooth.adapter("hci0").unwrap();
        let found = bluetooth.scan(&adapter, 8).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind(), DeviceKind::AudioVideo);
        assert_eq!(found[0].kind().label(), "audio");
        assert_eq!(actions(&mut bluetooth), vec!["inquiry hci0 8s"]);
    }

    #[test]
    fn scanning_an_adapter_that_is_off_refuses_before_touching_the_socket() {
        let tree = Tree::new("scan-off");
        tree.adapter("hci0", "00:1A:7D:DA:71:13").switch("hci0", 3, 1, 0);
        let mut bluetooth = Bluetooth::new(tree.sysfs(), Recorder::new());
        let adapter = bluetooth.adapter("hci0").unwrap();
        assert!(matches!(
            bluetooth.scan(&adapter, 8).unwrap_err(),
            Error::Blocked { hardware: false }
        ));

        let tree = Tree::new("scan-down");
        tree.adapter("hci0", "00:1A:7D:DA:71:13");
        let mut bluetooth = Bluetooth::new(tree.sysfs(), Recorder::new());
        let adapter = bluetooth.adapter("hci0").unwrap();
        assert!(matches!(
            bluetooth.scan(&adapter, 8).unwrap_err(),
            Error::NotPowered(_)
        ));
        assert!(actions(&mut bluetooth).is_empty());
    }

    #[test]
    fn the_class_of_a_device_names_what_it_is() {
        // Major class is bits 8 to 12 of the 24 bit class of device.
        assert_eq!(DeviceKind::from_class(0x1C010C), DeviceKind::Computer);
        assert_eq!(DeviceKind::from_class(0x5A020C), DeviceKind::Phone);
        assert_eq!(DeviceKind::from_class(0x240404), DeviceKind::AudioVideo);
        assert_eq!(DeviceKind::from_class(0x000540), DeviceKind::Peripheral);
        // 31 is "uncategorised", and zero is the miscellaneous class.
        assert_eq!(DeviceKind::from_class(0x001F00), DeviceKind::Unknown);
        assert_eq!(DeviceKind::from_class(0), DeviceKind::Unknown);
        assert_eq!(DeviceKind::Unknown.label(), "device");
    }

    #[test]
    fn inquiry_results_are_read_out_of_the_buffer_the_kernel_wrote() {
        // One `inquiry_info`: address least significant byte first, three
        // scan bytes, then the class of device and a clock offset.
        let entry = [
            0x33, 0x22, 0x11, 0x5D, 0x87, 0x4C, // 4C:87:5D:11:22:33
            0x01, 0x02, 0x00, // page scan fields
            0x04, 0x04, 0x24, // class 0x240404
            0x00, 0x00, // clock offset
        ];
        let found = parse_inquiry_results(&entry, 1);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].address, "4C:87:5D:11:22:33");
        assert_eq!(found[0].class, 0x240404);
        assert_eq!(found[0].kind(), DeviceKind::AudioVideo);
    }

    #[test]
    fn a_count_the_kernel_wrote_is_not_believed_past_the_end_of_the_buffer() {
        let entry = [0u8; std::mem::size_of::<InquiryInfo>()];
        // The count comes out of the same buffer the results do, so a wrong
        // one has to be survivable rather than fatal.
        assert_eq!(parse_inquiry_results(&entry, 255).len(), 1);
        assert_eq!(parse_inquiry_results(&entry[..7], 255).len(), 0);
        assert_eq!(parse_inquiry_results(&[], 255).len(), 0);
        assert_eq!(parse_inquiry_results(&entry, 0).len(), 0);
        // A trailing part of an entry is not an entry.
        let ragged = [0u8; std::mem::size_of::<InquiryInfo>() + 6];
        assert_eq!(parse_inquiry_results(&ragged, 2).len(), 1);
    }

    #[test]
    fn an_inquiry_is_counted_in_the_units_the_controller_uses() {
        // The controller counts in units of 1.28 seconds.
        assert_eq!(inquiry_length(8), 6);
        assert_eq!(inquiry_length(13), 10);
        // Never zero, which would be no inquiry at all, and never beyond the
        // longest the specification allows.
        assert_eq!(inquiry_length(0), 1);
        assert_eq!(inquiry_length(255), 0x30);
    }

    // -- bytes on the way to the kernel -------------------------------------

    #[test]
    fn an_address_is_printed_the_way_the_label_on_the_device_prints_it() {
        // The kernel keeps it backwards, so this is the test that catches the
        // most likely mistake in the whole module.
        assert_eq!(
            format_address(&[0x33, 0x22, 0x11, 0x5D, 0x87, 0x4C]),
            "4C:87:5D:11:22:33"
        );
        assert_eq!(format_address(&[0; 6]), "00:00:00:00:00:00");
        assert!(looks_like_address("4C:87:5D:11:22:33"));
        assert!(!looks_like_address("4C:87:5D:11:22"));
        assert!(!looks_like_address("4C:87:5D:11:22:3G"));
        assert!(!looks_like_address(""));
    }

    #[test]
    fn an_rfkill_event_is_the_eight_bytes_the_kernel_reads() {
        let bytes = rfkill_event_bytes(3, true);
        assert_eq!(bytes.len(), std::mem::size_of::<RfkillEvent>());
        assert_eq!(&bytes[0..4], &3u32.to_ne_bytes());
        assert_eq!(bytes[4], RFKILL_TYPE_BLUETOOTH);
        assert_eq!(bytes[5], RFKILL_OP_CHANGE);
        assert_eq!(bytes[6], 1);
        assert_eq!(bytes[7], 0, "the hardware switch is never ours to set");
        assert_eq!(rfkill_event_bytes(3, false)[6], 0);
    }

    #[test]
    fn an_inquiry_request_is_the_ten_bytes_the_kernel_reads() {
        let bytes = inquiry_request_bytes(1, 8);
        assert_eq!(bytes.len(), std::mem::size_of::<HciInquiryReq>());
        assert_eq!(&bytes[0..2], &1u16.to_ne_bytes());
        assert_eq!(&bytes[2..4], &IREQ_CACHE_FLUSH.to_ne_bytes());
        assert_eq!(&bytes[4..7], &GIAC, "anything discoverable answers on GIAC");
        assert_eq!(bytes[7], inquiry_length(8));
        assert_eq!(bytes[8], 0, "zero asks for as many answers as there are");
    }

    #[test]
    fn the_ioctl_numbers_match_the_kernel_macros() {
        // The literals are what `hci_sock.h` expands to, and are what a strace
        // of `hciconfig` shows. Nothing at run time would tell us these were
        // wrong except the kernel refusing every call with `EINVAL`.
        assert_eq!(HCIDEVUP, 0x4004_48C9);
        assert_eq!(HCIDEVDOWN, 0x4004_48CA);
        assert_eq!(HCIGETDEVINFO, 0x8004_48D3);
        assert_eq!(HCIINQUIRY, 0x8004_48F0);
    }

    #[test]
    fn the_kernel_structures_are_the_size_the_headers_say() {
        assert_eq!(std::mem::size_of::<HciDevInfo>(), 92);
        assert_eq!(std::mem::size_of::<HciDevStats>(), 40);
        assert_eq!(std::mem::size_of::<HciInquiryReq>(), 10);
        assert_eq!(std::mem::size_of::<InquiryInfo>(), 14);
        assert_eq!(std::mem::size_of::<RfkillEvent>(), 8);
        // Where the results start is the size of the request, so getting that
        // wrong would read every address off by a couple of bytes.
        assert_eq!(inquiry_request_bytes(0, 8).len(), 10);
    }

    #[test]
    fn a_build_without_bluetooth_refuses_rather_than_pretending() {
        // On Linux this opens a real socket, which on a machine with no
        // Bluetooth fails; everywhere else the stub refuses. Either way the
        // answer is an error rather than a made up state.
        let tree = Tree::new("system-control");
        tree.adapter("hci0", "00:1A:7D:DA:71:13");
        let mut bluetooth = Bluetooth::new(tree.sysfs(), SystemControl);
        let adapter = bluetooth.adapter("hci0").unwrap();
        assert_eq!(adapter.name, "hci0");
        assert_eq!(adapter.address, "00:1A:7D:DA:71:13");
        if !adapter.state_known {
            assert_eq!(adapter.state(), "unknown");
        }
    }
}
