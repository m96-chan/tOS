//! The machine's network interfaces, straight from the kernel.
//!
//! There is no NetworkManager here and no `systemd-networkd` to ask, so what a
//! status bar or a settings pane wants is read out of `/sys/class/net` and
//! `/proc/net`, and the one thing this module can change — whether a link is
//! administratively up — is a single ioctl.
//!
//! Addresses come from `getifaddrs(3)` rather than `/proc/net/fib_trie`.
//! `fib_trie` is readable text, but it names no interfaces: it lists prefixes
//! and the table they belong to, so an address found there cannot be
//! attributed to a device without inferring it from the routes, and that
//! inference is wrong the moment two interfaces share a subnet. `getifaddrs`
//! hands back the interface name, the address and the netmask for both
//! families in one pass. It is a libc call, so it sits behind [`Kernel`] with
//! the ioctl and a test supplies addresses the same way it supplies a fake
//! `/sys`.
//!
//! Joining a wireless network is deliberately not here. Reading a wireless
//! interface's state is; see [`Wireless`].

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use crate::sysfs::Sysfs;

/// What an interface is, which is mostly a question of what to show a person.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `lo`, useful to nobody but the machine itself.
    Loopback,
    /// Ethernet and anything else with a cable.
    Wired,
    /// Has a `wireless/` directory or says `DEVTYPE=wlan`.
    Wireless,
    /// Bridges, bonds, VLANs, tunnels, container veth pairs: real to the
    /// kernel, noise to a user.
    Virtual,
}

impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::Loopback => "loopback",
            Kind::Wired => "wired",
            Kind::Wireless => "wireless",
            Kind::Virtual => "virtual",
        }
    }
}

/// The kernel's `operstate`, which is not the same question as "is it switched
/// on": a link can be administratively up and still have no carrier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    Up,
    Down,
    /// Associated enough to talk to, not enough to route: `dormant` and
    /// `testing` both land here.
    Dormant,
    /// The driver declines to say, which is normal for tunnels and for `lo`.
    Unknown,
}

impl LinkState {
    fn parse(text: &str) -> LinkState {
        match text.trim() {
            "up" => LinkState::Up,
            "down" | "lowerlayerdown" | "notpresent" => LinkState::Down,
            "dormant" | "testing" => LinkState::Dormant,
            _ => LinkState::Unknown,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            LinkState::Up => "up",
            LinkState::Down => "down",
            LinkState::Dormant => "dormant",
            LinkState::Unknown => "unknown",
        }
    }
}

/// One address on one interface, with the length of its prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Address {
    pub ip: IpAddr,
    pub prefix_len: u8,
}

impl Address {
    /// `192.168.1.5/24` or `fe80::1/64`. Answers `None` rather than an error,
    /// because everything that produces one of these is a best effort read.
    pub fn parse(text: &str) -> Option<Address> {
        let (ip, prefix) = match text.split_once('/') {
            Some((ip, prefix)) => (ip, Some(prefix)),
            None => (text, None),
        };
        let ip: IpAddr = ip.trim().parse().ok()?;
        let widest = if ip.is_ipv4() { 32 } else { 128 };
        let prefix_len = match prefix {
            Some(prefix) => prefix.trim().parse().ok()?,
            None => widest,
        };
        if prefix_len > widest {
            return None;
        }
        Some(Address { ip, prefix_len })
    }

    pub fn is_ipv4(&self) -> bool {
        self.ip.is_ipv4()
    }

    /// Link local: IPv4 `169.254/16` and IPv6 `fe80::/10`. The standard
    /// library's IPv6 predicate is still unstable, so this is spelled out.
    pub fn is_link_local(&self) -> bool {
        match self.ip {
            IpAddr::V4(v4) => v4.is_link_local(),
            IpAddr::V6(v6) => v6.segments()[0] & 0xffc0 == 0xfe80,
        }
    }

    /// An address worth putting in front of a person: one that could carry
    /// traffic off this machine.
    pub fn is_routable(&self) -> bool {
        !self.ip.is_loopback() && !self.is_link_local() && !self.ip.is_unspecified()
    }
}

impl std::fmt::Display for Address {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.ip, self.prefix_len)
    }
}

/// An address together with the interface it is on, which is what
/// `getifaddrs(3)` gives and what `/proc/net/fib_trie` cannot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfaceAddress {
    pub interface: String,
    pub address: Address,
}

/// What can be known about a wireless link without associating with anything.
///
/// The SSID is whatever the kernel hands over for free: `SIOCGIWESSID`, the
/// old wireless extensions ioctl, which cfg80211 still answers when the kernel
/// was built with its compatibility layer. Signal strength comes out of
/// `/proc/net/wireless`. Neither scans, and neither can join a network.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Wireless {
    /// The network this interface is associated with, when it is associated
    /// and when the kernel will say so.
    pub ssid: Option<String>,
    /// The driver's own link quality figure, usually out of 70.
    pub link_quality: Option<u32>,
    /// Signal level in dBm, when the driver reports a real one.
    pub signal_dbm: Option<i32>,
}

impl Wireless {
    pub fn is_associated(&self) -> bool {
        self.ssid.is_some()
    }
}

/// One interface, as completely as sysfs describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interface {
    pub name: String,
    pub kind: Kind,
    /// `operstate`.
    pub state: LinkState,
    /// `carrier`: whether there is a cable in it, or an association. The file
    /// cannot be read on a down interface, which reads as no carrier.
    pub carrier: bool,
    /// `IFF_UP` out of `flags`: whether anyone asked for this link at all.
    pub admin_up: bool,
    pub mac: Option<String>,
    pub mtu: Option<u32>,
    /// Megabits per second, where the driver reports one. Absent for wireless,
    /// for virtual devices, and for anything that is down.
    pub speed_mbps: Option<u32>,
    /// Routable addresses first, IPv4 before IPv6.
    pub addresses: Vec<Address>,
    /// This interface carries a default route.
    pub is_default: bool,
    /// The default route's gateway, when this interface carries one and that
    /// route has a gateway rather than being on-link.
    pub gateway: Option<Ipv4Addr>,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    /// Present exactly when [`Interface::kind`] is [`Kind::Wireless`].
    pub wireless: Option<Wireless>,
}

impl Interface {
    /// Whether this is a link a person would expect to see listed.
    pub fn is_worth_showing(&self) -> bool {
        matches!(self.kind, Kind::Wired | Kind::Wireless)
    }

    /// Up, carrying, and holding an address that goes somewhere.
    pub fn is_online(&self) -> bool {
        self.admin_up && self.carrier && self.addresses.iter().any(Address::is_routable)
    }

    /// The first routable IPv4 address, which is what a status bar shows.
    pub fn ipv4(&self) -> Option<Address> {
        self.addresses
            .iter()
            .find(|address| address.is_ipv4() && address.is_routable())
            .copied()
    }

    /// The first routable IPv6 address.
    pub fn ipv6(&self) -> Option<Address> {
        self.addresses
            .iter()
            .find(|address| !address.is_ipv4() && address.is_routable())
            .copied()
    }

    pub fn ssid(&self) -> Option<&str> {
        self.wireless.as_ref()?.ssid.as_deref()
    }

    /// One line for a status bar: `wlan0  kitchen-table  192.168.1.5/24`.
    pub fn summary(&self) -> String {
        let mut text = self.name.clone();
        if let Some(ssid) = self.ssid() {
            text.push_str("  ");
            text.push_str(ssid);
        }
        text.push_str("  ");
        match self.ipv4().or_else(|| self.ipv6()) {
            Some(address) => text.push_str(&address.to_string()),
            None if self.admin_up && !self.carrier => text.push_str("no carrier"),
            None => text.push_str(self.state.as_str()),
        }
        text
    }
}

/// The route packets take when nothing more specific matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultRoute {
    pub interface: String,
    /// A default route can be on-link — a point to point link has no gateway
    /// to speak of — so this is optional.
    pub gateway: Option<Ipv4Addr>,
    pub metric: u32,
}

/// Everything in this module that is not a file read.
///
/// Two libc calls live behind here, for the same reason the installer puts
/// every command behind `exec::Backend`: a test can assert that `eth0` was
/// asked to come up without the developer's own networking flinching.
pub trait Kernel {
    /// Every address on every interface, from `getifaddrs(3)`.
    fn addresses(&self) -> Vec<InterfaceAddress>;

    /// The SSID a wireless interface is already associated with, if the kernel
    /// will say. Not a scan, and not a connection.
    fn essid(&self, interface: &str) -> Option<String>;

    /// Set or clear `IFF_UP` on an interface, with `SIOCSIFFLAGS`. Wants
    /// `CAP_NET_ADMIN`, so it fails with a permission error for anyone else.
    fn set_link_up(&mut self, interface: &str, up: bool) -> io::Result<()>;
}

/// The real kernel.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemKernel;

impl Kernel for SystemKernel {
    fn addresses(&self) -> Vec<InterfaceAddress> {
        read_interface_addresses()
    }

    fn essid(&self, interface: &str) -> Option<String> {
        read_essid(interface)
    }

    fn set_link_up(&mut self, interface: &str, up: bool) -> io::Result<()> {
        set_interface_flags(interface, up)
    }
}

/// A link change somebody asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkChange {
    pub interface: String,
    pub up: bool,
}

impl LinkChange {
    pub fn describe(&self) -> String {
        format!("{} {}", self.interface, if self.up { "up" } else { "down" })
    }
}

/// A kernel that writes down what it was asked to do and answers reads from a
/// table. This is what the tests assert against, and it is also how a UI can
/// be driven with no hardware underneath it.
#[derive(Debug, Clone, Default)]
pub struct RecordingKernel {
    pub addresses: Vec<InterfaceAddress>,
    pub essids: Vec<(String, String)>,
    pub changes: Vec<LinkChange>,
    /// Interfaces whose `SIOCSIFFLAGS` should fail, so the error path can be
    /// exercised.
    pub refusing: Vec<String>,
}

impl RecordingKernel {
    pub fn new() -> RecordingKernel {
        RecordingKernel::default()
    }

    /// Give an interface an address, written the way a person writes one:
    /// `with_address("eth0", "192.168.1.5/24")`.
    pub fn with_address(mut self, interface: &str, address: &str) -> RecordingKernel {
        if let Some(address) = Address::parse(address) {
            self.addresses.push(InterfaceAddress {
                interface: interface.to_string(),
                address,
            });
        }
        self
    }

    pub fn with_essid(mut self, interface: &str, ssid: &str) -> RecordingKernel {
        self.essids.push((interface.to_string(), ssid.to_string()));
        self
    }

    /// Make the ioctl fail for an interface, the way it does without
    /// `CAP_NET_ADMIN`.
    pub fn refusing(mut self, interface: &str) -> RecordingKernel {
        self.refusing.push(interface.to_string());
        self
    }

    /// Everything it was asked to do, as text: `["eth0 up"]`.
    pub fn transcript(&self) -> Vec<String> {
        self.changes.iter().map(LinkChange::describe).collect()
    }

    pub fn did(&self, needle: &str) -> bool {
        self.transcript().iter().any(|line| line == needle)
    }
}

impl Kernel for RecordingKernel {
    fn addresses(&self) -> Vec<InterfaceAddress> {
        self.addresses.clone()
    }

    fn essid(&self, interface: &str) -> Option<String> {
        self.essids
            .iter()
            .find(|(name, _)| name == interface)
            .map(|(_, ssid)| ssid.clone())
    }

    fn set_link_up(&mut self, interface: &str, up: bool) -> io::Result<()> {
        self.changes.push(LinkChange {
            interface: interface.to_string(),
            up,
        });
        if self.refusing.iter().any(|name| name == interface) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("not allowed to change {interface}"),
            ));
        }
        Ok(())
    }
}

/// The network, as this machine sees it.
///
/// Reading goes through a [`Sysfs`] and acting through a [`Kernel`], so the
/// whole of it can be driven over a directory of text files.
#[derive(Debug, Clone)]
pub struct Network<K: Kernel = SystemKernel> {
    sysfs: Sysfs,
    kernel: K,
}

impl Network<SystemKernel> {
    /// The machine this is running on.
    pub fn system() -> Network<SystemKernel> {
        Network {
            sysfs: Sysfs::system(),
            kernel: SystemKernel,
        }
    }
}

impl<K: Kernel> Network<K> {
    pub fn new(sysfs: Sysfs, kernel: K) -> Network<K> {
        Network { sysfs, kernel }
    }

    pub fn kernel(&self) -> &K {
        &self.kernel
    }

    /// Every interface the kernel has, in the order to show them: the ones a
    /// person cares about first, loopback and the plumbing last.
    ///
    /// Interfaces come and go — a USB dongle unplugged, a container starting —
    /// so a name that no longer resolves is dropped rather than reported as an
    /// interface with nothing in it.
    pub fn interfaces(&self) -> Vec<Interface> {
        let routes = self.routes();
        let addresses = self.kernel.addresses();
        let wireless = self.wireless_status();

        let mut interfaces: Vec<Interface> = self
            .sysfs
            .list("/sys/class/net")
            .into_iter()
            .filter_map(|name| self.read_interface(&name, &routes, &addresses, &wireless))
            .collect();

        interfaces.sort_by_key(|interface| {
            let rank = match interface.kind {
                Kind::Wired | Kind::Wireless => 0,
                Kind::Virtual => 1,
                Kind::Loopback => 2,
            };
            (rank, interface.name.clone())
        });
        interfaces
    }

    /// One interface by name, or `None` when it is not there any more.
    pub fn interface(&self, name: &str) -> Option<Interface> {
        self.read_interface(
            name,
            &self.routes(),
            &self.kernel.addresses(),
            &self.wireless_status(),
        )
    }

    /// The interfaces worth putting in front of a person: wired and wireless,
    /// no bridges and no loopback.
    pub fn visible_interfaces(&self) -> Vec<Interface> {
        self.interfaces()
            .into_iter()
            .filter(Interface::is_worth_showing)
            .collect()
    }

    /// The default route, lowest metric winning, from `/proc/net/route`.
    pub fn default_route(&self) -> Option<DefaultRoute> {
        default_route(&self.routes())
    }

    /// The interface traffic actually leaves by, which is what a status bar
    /// means by "the network".
    pub fn active_interface(&self) -> Option<Interface> {
        let route = self.default_route()?;
        self.interface(&route.interface)
    }

    /// Ask the kernel to bring a link up. Wants `CAP_NET_ADMIN`.
    pub fn bring_up(&mut self, interface: &str) -> io::Result<()> {
        self.kernel.set_link_up(interface, true)
    }

    /// Ask the kernel to take a link down.
    pub fn take_down(&mut self, interface: &str) -> io::Result<()> {
        self.kernel.set_link_up(interface, false)
    }

    fn routes(&self) -> Vec<RouteEntry> {
        parse_proc_net_route(&self.sysfs.read("/proc/net/route").unwrap_or_default())
    }

    fn wireless_status(&self) -> Vec<(String, Wireless)> {
        parse_proc_net_wireless(&self.sysfs.read("/proc/net/wireless").unwrap_or_default())
    }

    /// Build one interface out of its sysfs directory.
    ///
    /// `flags` is the one file every live interface has and the one this
    /// cannot do without, so its absence is how a vanished interface is
    /// recognised. Everything else may be missing: drivers differ, and a file
    /// can evaporate between two reads of the same directory.
    fn read_interface(
        &self,
        name: &str,
        routes: &[RouteEntry],
        addresses: &[InterfaceAddress],
        wireless: &[(String, Wireless)],
    ) -> Option<Interface> {
        let attribute = |file: &str| self.sysfs.read(&format!("/sys/class/net/{name}/{file}"));
        let flags = parse_flags(&attribute("flags")?)?;

        let kind = self.kind_of(name, flags, attribute("uevent").as_deref());
        let mac = attribute("address").filter(|mac| mac != "00:00:00:00:00:00");
        // `speed` is -EINVAL on a down link and -1 on a driver that does not
        // know, so only a positive number means anything.
        let speed_mbps = attribute("speed")
            .and_then(|text| text.trim().parse::<i64>().ok())
            .filter(|speed| *speed > 0)
            .map(|speed| speed as u32);

        let mut addresses: Vec<Address> = addresses
            .iter()
            .filter(|entry| entry.interface == name)
            .map(|entry| entry.address)
            .collect();
        // Routable first and IPv4 before IPv6, so the first one is the one to
        // show; then by address, so a listing does not shuffle between frames.
        addresses.sort_by_key(|address| {
            (
                !address.is_routable(),
                !address.is_ipv4(),
                address.ip.to_string(),
            )
        });
        addresses.dedup();

        let default = routes
            .iter()
            .filter(|route| route.interface == name && route.is_default())
            .min_by_key(|route| route.metric);

        let wireless = match kind {
            Kind::Wireless => {
                let mut status = wireless
                    .iter()
                    .find(|(interface, _)| interface == name)
                    .map(|(_, status)| status.clone())
                    .unwrap_or_default();
                status.ssid = self.kernel.essid(name);
                Some(status)
            }
            _ => None,
        };

        Some(Interface {
            name: name.to_string(),
            kind,
            state: LinkState::parse(&attribute("operstate").unwrap_or_default()),
            carrier: attribute("carrier")
                .map(|text| text.trim() == "1")
                .unwrap_or(false),
            admin_up: flags & IFF_UP != 0,
            mac,
            mtu: attribute("mtu").and_then(|text| text.trim().parse().ok()),
            speed_mbps,
            addresses,
            is_default: default.is_some(),
            gateway: default.and_then(RouteEntry::gateway),
            rx_bytes: attribute("statistics/rx_bytes")
                .and_then(|text| text.trim().parse().ok())
                .unwrap_or(0),
            tx_bytes: attribute("statistics/tx_bytes")
                .and_then(|text| text.trim().parse().ok())
                .unwrap_or(0),
            wireless,
        })
    }

    /// What sort of interface this is.
    ///
    /// The useful signal for "is this real hardware" is the `device` link:
    /// bridges, tunnels, bonds and container veth pairs have none, because
    /// there is no device under them.
    fn kind_of(&self, name: &str, flags: u32, uevent: Option<&str>) -> Kind {
        let devtype = uevent.and_then(uevent_devtype);
        let arp_type: u32 = self
            .sysfs
            .read_number(&format!("/sys/class/net/{name}/type"))
            .unwrap_or(0);

        if flags & IFF_LOOPBACK != 0 || arp_type == ARPHRD_LOOPBACK {
            return Kind::Loopback;
        }
        if devtype.as_deref() == Some("wlan")
            || self
                .sysfs
                .exists(&format!("/sys/class/net/{name}/wireless"))
            || self
                .sysfs
                .exists(&format!("/sys/class/net/{name}/phy80211"))
        {
            return Kind::Wireless;
        }
        if devtype
            .as_deref()
            .map(|devtype| VIRTUAL_DEVTYPES.contains(&devtype))
            .unwrap_or(false)
        {
            return Kind::Virtual;
        }
        if !self.sysfs.exists(&format!("/sys/class/net/{name}/device")) {
            return Kind::Virtual;
        }
        Kind::Wired
    }
}

/// `IFF_UP`, from `linux/if.h`. Written out rather than taken from libc so the
/// parsing builds anywhere.
const IFF_UP: u32 = 0x1;
/// `IFF_LOOPBACK`.
const IFF_LOOPBACK: u32 = 0x8;
/// `ARPHRD_LOOPBACK`, the value in `/sys/class/net/lo/type`.
const ARPHRD_LOOPBACK: u32 = 772;
/// `RTF_UP`, from `linux/route.h`.
const RTF_UP: u32 = 0x1;
/// `RTF_GATEWAY`.
const RTF_GATEWAY: u32 = 0x2;

/// `DEVTYPE` values that mean the kernel made this interface up.
const VIRTUAL_DEVTYPES: &[&str] = &[
    "bridge",
    "bond",
    "vlan",
    "veth",
    "tun",
    "tap",
    "gre",
    "gretap",
    "ip6gre",
    "ip6tnl",
    "sit",
    "vxlan",
    "macvlan",
    "macvtap",
    "ipvlan",
    "wireguard",
    "dummy",
    "geneve",
    "ppp",
    "team",
];

/// `flags` is hex with a `0x` on the front.
fn parse_flags(text: &str) -> Option<u32> {
    let text = text.trim();
    let digits = text.strip_prefix("0x").unwrap_or(text);
    u32::from_str_radix(digits, 16).ok()
}

/// `DEVTYPE=` out of a `uevent` file.
fn uevent_devtype(uevent: &str) -> Option<String> {
    uevent.lines().find_map(|line| {
        line.trim()
            .strip_prefix("DEVTYPE=")
            .filter(|value| !value.is_empty())
            .map(|value| value.to_string())
    })
}

/// One line of `/proc/net/route`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RouteEntry {
    interface: String,
    destination: Ipv4Addr,
    raw_gateway: Ipv4Addr,
    flags: u32,
    metric: u32,
    prefix_len: u8,
}

impl RouteEntry {
    fn is_default(&self) -> bool {
        self.flags & RTF_UP != 0 && self.prefix_len == 0 && self.destination.is_unspecified()
    }

    /// The gateway, unless the route is on-link and so has none.
    fn gateway(&self) -> Option<Ipv4Addr> {
        if self.flags & RTF_GATEWAY == 0 || self.raw_gateway.is_unspecified() {
            None
        } else {
            Some(self.raw_gateway)
        }
    }
}

/// Parse `/proc/net/route`.
///
/// The addresses in that file are the kernel's network-order words printed as
/// host-order hex, which on every machine tOS runs on means the bytes come out
/// backwards: `0102A8C0` is 192.168.2.1, not 1.2.168.192. Getting this the
/// wrong way round produces addresses that look entirely plausible, which is
/// why it is tested against captured text.
fn parse_proc_net_route(text: &str) -> Vec<RouteEntry> {
    let mut routes = Vec::new();
    for line in text.lines() {
        // Iface Destination Gateway Flags RefCnt Use Metric Mask ...
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 8 || fields[0] == "Iface" {
            continue;
        }
        let (Some(destination), Some(gateway), Some(mask)) = (
            parse_route_address(fields[1]),
            parse_route_address(fields[2]),
            parse_route_address(fields[7]),
        ) else {
            continue;
        };
        let (Ok(flags), Ok(metric)) =
            (u32::from_str_radix(fields[3], 16), fields[6].parse::<u32>())
        else {
            continue;
        };
        routes.push(RouteEntry {
            interface: fields[0].to_string(),
            destination,
            raw_gateway: gateway,
            flags,
            metric,
            prefix_len: u32::from(mask).count_ones() as u8,
        });
    }
    routes
}

/// Eight hex digits, least significant byte first.
fn parse_route_address(field: &str) -> Option<Ipv4Addr> {
    if field.len() != 8 || !field.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let word = u32::from_str_radix(field, 16).ok()?;
    Some(Ipv4Addr::from(word.to_le_bytes()))
}

/// The default route with the lowest metric, which is the one the kernel will
/// actually use.
fn default_route(routes: &[RouteEntry]) -> Option<DefaultRoute> {
    routes
        .iter()
        .filter(|route| route.is_default())
        .min_by_key(|route| route.metric)
        .map(|route| DefaultRoute {
            interface: route.interface.clone(),
            gateway: route.gateway(),
            metric: route.metric,
        })
}

/// Parse `/proc/net/wireless`: two header lines, then one line an interface,
/// with the numbers carrying a trailing full stop.
fn parse_proc_net_wireless(text: &str) -> Vec<(String, Wireless)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() || name.contains(char::is_whitespace) {
            continue;
        }
        // status, link, level, noise.
        let fields: Vec<&str> = rest.split_whitespace().collect();
        if fields.len() < 4 {
            continue;
        }
        let number = |field: &str| field.trim_end_matches('.').parse::<i32>().ok();
        let link_quality = number(fields[1])
            .filter(|value| *value >= 0)
            .map(|value| value as u32);
        // A driver reporting a relative 0..255 level rather than dBm is not
        // worth showing as a signal strength, so only negatives count.
        let signal_dbm = number(fields[2]).filter(|value| *value < 0 && *value > -200);
        out.push((
            name.to_string(),
            Wireless {
                ssid: None,
                link_quality,
                signal_dbm,
            },
        ));
    }
    out
}

/// A netmask's bytes as a prefix length, for `getifaddrs`.
fn prefix_from_mask(bytes: &[u8]) -> u8 {
    bytes.iter().map(|byte| byte.count_ones() as u8).sum()
}

// getifaddrs(3) is POSIX, so this path is exercised on the machine the code is
// written on as well as the one it runs on.
#[cfg(unix)]
fn read_interface_addresses() -> Vec<InterfaceAddress> {
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs either fills in the pointer or returns non-zero, and
    // the list it builds is handed straight back to freeifaddrs below.
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return Vec::new();
    }

    let mut out = Vec::new();
    let mut cursor = head;
    while !cursor.is_null() {
        // SAFETY: cursor is non-null and owned by the list getifaddrs built.
        let entry = unsafe { &*cursor };
        cursor = entry.ifa_next;
        if entry.ifa_addr.is_null() || entry.ifa_name.is_null() {
            continue;
        }
        // SAFETY: ifa_name is a NUL terminated string in the same allocation.
        let name = unsafe { std::ffi::CStr::from_ptr(entry.ifa_name) }
            .to_string_lossy()
            .into_owned();
        // SAFETY: sa_family is the first field of every sockaddr. The reads
        // below are unaligned, so the sockaddr's own alignment does not have
        // to be taken on trust either.
        let family = i32::from(unsafe { (*entry.ifa_addr).sa_family });

        let address = if family == libc::AF_INET {
            let sockaddr: libc::sockaddr_in =
                unsafe { std::ptr::read_unaligned(entry.ifa_addr.cast()) };
            let prefix_len = if entry.ifa_netmask.is_null() {
                32
            } else {
                let mask: libc::sockaddr_in =
                    unsafe { std::ptr::read_unaligned(entry.ifa_netmask.cast()) };
                prefix_from_mask(&mask.sin_addr.s_addr.to_ne_bytes())
            };
            Address {
                ip: IpAddr::V4(Ipv4Addr::from(u32::from_be(sockaddr.sin_addr.s_addr))),
                prefix_len,
            }
        } else if family == libc::AF_INET6 {
            let sockaddr: libc::sockaddr_in6 =
                unsafe { std::ptr::read_unaligned(entry.ifa_addr.cast()) };
            let prefix_len = if entry.ifa_netmask.is_null() {
                128
            } else {
                let mask: libc::sockaddr_in6 =
                    unsafe { std::ptr::read_unaligned(entry.ifa_netmask.cast()) };
                prefix_from_mask(&mask.sin6_addr.s6_addr)
            };
            Address {
                ip: IpAddr::V6(Ipv6Addr::from(sockaddr.sin6_addr.s6_addr)),
                prefix_len,
            }
        } else {
            // AF_PACKET and friends carry the hardware address, which sysfs
            // has already given us.
            continue;
        };

        out.push(InterfaceAddress {
            interface: name,
            address,
        });
    }

    // SAFETY: head is exactly what getifaddrs returned, and is not used again.
    unsafe { libc::freeifaddrs(head) };
    out
}

#[cfg(not(unix))]
fn read_interface_addresses() -> Vec<InterfaceAddress> {
    Vec::new()
}

/// The `ifreq` of `linux/if.h`, written out rather than taken from libc so the
/// layout sits next to the ioctl that depends on it. The union on the end is
/// 24 bytes wide; the flags are the first two of them.
#[cfg(target_os = "linux")]
#[repr(C)]
struct IfReq {
    name: [libc::c_char; libc::IF_NAMESIZE],
    flags: libc::c_short,
    padding: [u8; 22],
}

#[cfg(target_os = "linux")]
const SIOCGIFFLAGS: libc::c_ulong = 0x8913;
#[cfg(target_os = "linux")]
const SIOCSIFFLAGS: libc::c_ulong = 0x8914;
#[cfg(target_os = "linux")]
const SIOCGIWESSID: libc::c_ulong = 0x8B1B;
/// `IW_ESSID_MAX_SIZE`.
#[cfg(target_os = "linux")]
const ESSID_MAX: usize = 32;

/// An interface name in the fixed size field the ioctls want.
#[cfg(target_os = "linux")]
fn name_field(interface: &str) -> io::Result<[libc::c_char; libc::IF_NAMESIZE]> {
    let bytes = interface.as_bytes();
    if bytes.is_empty() || bytes.len() >= libc::IF_NAMESIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{interface:?} is not an interface name"),
        ));
    }
    let mut field = [0; libc::IF_NAMESIZE];
    // SAFETY: the length was just checked against the field, and c_char has
    // the size and alignment of u8 on every target Linux runs on.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), field.as_mut_ptr().cast::<u8>(), bytes.len());
    }
    Ok(field)
}

/// A datagram socket, which is only ever a handle for the ioctls to ride on.
#[cfg(target_os = "linux")]
struct IoctlSocket(libc::c_int);

#[cfg(target_os = "linux")]
impl IoctlSocket {
    fn open() -> io::Result<IoctlSocket> {
        // SAFETY: a plain socket call.
        let fd = unsafe {
            libc::socket(
                libc::AF_INET,
                libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
                libc::IPPROTO_IP,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(IoctlSocket(fd))
    }
}

#[cfg(target_os = "linux")]
impl Drop for IoctlSocket {
    fn drop(&mut self) {
        // SAFETY: the descriptor is ours and is not used again.
        unsafe { libc::close(self.0) };
    }
}

/// `SIOCSIFFLAGS`, read-modify-write so that nothing else in `flags` is lost
/// on the way past.
#[cfg(target_os = "linux")]
fn set_interface_flags(interface: &str, up: bool) -> io::Result<()> {
    let socket = IoctlSocket::open()?;
    let mut request = IfReq {
        name: name_field(interface)?,
        flags: 0,
        padding: [0; 22],
    };

    // SAFETY: request is a correctly shaped ifreq and outlives the call.
    if unsafe { libc::ioctl(socket.0, SIOCGIFFLAGS as _, &mut request) } < 0 {
        return Err(io::Error::last_os_error());
    }

    let current = request.flags as u32;
    let wanted = if up {
        current | IFF_UP
    } else {
        current & !IFF_UP
    };
    if wanted == current {
        return Ok(());
    }
    request.flags = wanted as libc::c_short;

    // SAFETY: as above.
    if unsafe { libc::ioctl(socket.0, SIOCSIFFLAGS as _, &request) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn set_interface_flags(interface: &str, _up: bool) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        format!("SIOCSIFFLAGS on {interface} needs a Linux kernel"),
    ))
}

/// The wireless extensions `iw_point`, the half of `iwreq_data` an SSID comes
/// back in.
#[cfg(target_os = "linux")]
#[repr(C)]
struct IwPoint {
    pointer: *mut libc::c_void,
    length: u16,
    flags: u16,
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct IwReq {
    name: [libc::c_char; libc::IF_NAMESIZE],
    point: IwPoint,
}

/// `SIOCGIWESSID`: the network an interface is already associated with.
///
/// This is the old wireless extensions interface, which cfg80211 answers only
/// on a kernel built with its compatibility layer. One without it simply says
/// no and no SSID is shown: it is not worth opening an nl80211 socket to find
/// out, and that is the later Wi-Fi item's job in any case.
#[cfg(target_os = "linux")]
fn read_essid(interface: &str) -> Option<String> {
    let socket = IoctlSocket::open().ok()?;
    let mut buffer = [0u8; ESSID_MAX + 1];
    let mut request = IwReq {
        name: name_field(interface).ok()?,
        point: IwPoint {
            pointer: buffer.as_mut_ptr().cast(),
            length: ESSID_MAX as u16,
            flags: 0,
        },
    };

    // SAFETY: request points at a buffer that outlives the call, and the
    // length handed over is that buffer's own.
    if unsafe { libc::ioctl(socket.0, SIOCGIWESSID as _, &mut request) } < 0 {
        return None;
    }

    let length = (request.point.length as usize).min(ESSID_MAX);
    let ssid = String::from_utf8_lossy(&buffer[..length])
        .trim_end_matches('\0')
        .to_string();
    // An unassociated interface answers with an empty SSID rather than an
    // error.
    if ssid.is_empty() {
        None
    } else {
        Some(ssid)
    }
}

#[cfg(not(target_os = "linux"))]
fn read_essid(_interface: &str) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A directory laid out like `/sys` and `/proc`, which cleans up after
    /// itself. `sysfs.rs` has the same thing for its own tests; this is a
    /// second copy rather than a shared one so that neither file has to move.
    struct Tree {
        root: PathBuf,
    }

    impl Tree {
        fn new(name: &str) -> Tree {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir()
                .join(format!("tos-net-{name}-{}-{unique}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            Tree { root }
        }

        fn file(&self, path: &str, contents: &str) -> &Tree {
            let full = self.root.join(path.trim_start_matches('/'));
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, contents).unwrap();
            self
        }

        fn dir(&self, path: &str) -> &Tree {
            std::fs::create_dir_all(self.root.join(path.trim_start_matches('/'))).unwrap();
            self
        }

        fn remove(&self, path: &str) {
            let _ = std::fs::remove_dir_all(self.root.join(path.trim_start_matches('/')));
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

    /// The files a driver puts under an interface. `flags` and `operstate` are
    /// the interesting ones; the rest are here so the tests look like a real
    /// machine.
    fn add_interface(tree: &Tree, name: &str, admin_up: bool, carrier: bool) {
        let dir = format!("/sys/class/net/{name}");
        tree.file(
            &format!("{dir}/flags"),
            if admin_up { "0x1003\n" } else { "0x1002\n" },
        )
        .file(
            &format!("{dir}/operstate"),
            if carrier { "up\n" } else { "down\n" },
        )
        .file(
            &format!("{dir}/carrier"),
            if carrier { "1\n" } else { "0\n" },
        )
        .file(&format!("{dir}/address"), "aa:bb:cc:dd:ee:01\n")
        .file(&format!("{dir}/mtu"), "1500\n")
        .file(&format!("{dir}/type"), "1\n")
        .file(&format!("{dir}/statistics/rx_bytes"), "4096\n")
        .file(&format!("{dir}/statistics/tx_bytes"), "2048\n");
    }

    /// Real hardware has a `device` link back to the bus it sits on.
    fn make_physical(tree: &Tree, name: &str) {
        tree.file(
            &format!("/sys/class/net/{name}/device/uevent"),
            "DRIVER=e1000e\n",
        );
    }

    fn make_wireless(tree: &Tree, name: &str) {
        make_physical(tree, name);
        tree.dir(&format!("/sys/class/net/{name}/wireless"));
        tree.file(
            &format!("/sys/class/net/{name}/uevent"),
            "DEVTYPE=wlan\nINTERFACE=wlan0\n",
        );
    }

    fn add_loopback(tree: &Tree) {
        tree.file("/sys/class/net/lo/flags", "0x9\n")
            .file("/sys/class/net/lo/operstate", "unknown\n")
            .file("/sys/class/net/lo/type", "772\n")
            .file("/sys/class/net/lo/mtu", "65536\n")
            .file("/sys/class/net/lo/address", "00:00:00:00:00:00\n")
            .file("/sys/class/net/lo/statistics/rx_bytes", "128\n")
            .file("/sys/class/net/lo/statistics/tx_bytes", "128\n");
    }

    /// Captured from a machine with one wired interface on 192.168.2.0/24.
    const WIRED_ROUTES: &str = "\
Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT
eth0\t00000000\t0102A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0
eth0\t0002A8C0\t00000000\t0001\t0\t0\t100\t00FFFFFF\t0\t0\t0
";

    #[test]
    fn a_wired_machine_reports_its_link_and_its_default_route() {
        let tree = Tree::new("wired");
        add_interface(&tree, "eth0", true, true);
        make_physical(&tree, "eth0");
        tree.file("/sys/class/net/eth0/speed", "1000\n");
        add_loopback(&tree);
        tree.file("/proc/net/route", WIRED_ROUTES);

        let kernel = RecordingKernel::new()
            .with_address("eth0", "192.168.2.50/24")
            .with_address("eth0", "fe80::1/64")
            .with_address("lo", "127.0.0.1/8");
        let network = Network::new(tree.sysfs(), kernel);

        let interfaces = network.interfaces();
        assert_eq!(
            interfaces
                .iter()
                .map(|interface| interface.name.as_str())
                .collect::<Vec<_>>(),
            vec!["eth0", "lo"]
        );

        let eth0 = &interfaces[0];
        assert_eq!(eth0.kind, Kind::Wired);
        assert_eq!(eth0.state, LinkState::Up);
        assert!(eth0.admin_up && eth0.carrier);
        assert_eq!(eth0.mac.as_deref(), Some("aa:bb:cc:dd:ee:01"));
        assert_eq!(eth0.mtu, Some(1500));
        assert_eq!(eth0.speed_mbps, Some(1000));
        assert_eq!(eth0.rx_bytes, 4096);
        assert_eq!(eth0.tx_bytes, 2048);
        assert_eq!(
            eth0.ipv4().map(|address| address.to_string()),
            Some("192.168.2.50/24".to_string())
        );
        assert!(eth0.is_default);
        assert_eq!(eth0.gateway, Some(Ipv4Addr::new(192, 168, 2, 1)));
        assert!(eth0.is_online());
        assert!(eth0.wireless.is_none());

        assert_eq!(interfaces[1].kind, Kind::Loopback);
        assert!(!interfaces[1].is_worth_showing());
        assert_eq!(
            network.default_route(),
            Some(DefaultRoute {
                interface: "eth0".to_string(),
                gateway: Some(Ipv4Addr::new(192, 168, 2, 1)),
                metric: 100,
            })
        );
        assert_eq!(
            network.active_interface().map(|interface| interface.name),
            Some("eth0".to_string())
        );
    }

    #[test]
    fn a_laptop_with_the_cable_out_falls_back_to_the_wireless_link() {
        let tree = Tree::new("laptop");
        add_interface(&tree, "eth0", true, false);
        make_physical(&tree, "eth0");
        add_interface(&tree, "wlan0", true, true);
        make_wireless(&tree, "wlan0");
        add_loopback(&tree);
        tree.file(
            "/proc/net/route",
            "\
Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT
wlan0\t00000000\t0101A8C0\t0003\t0\t0\t600\t00000000\t0\t0\t0
wlan0\t0001A8C0\t00000000\t0001\t0\t0\t600\t00FFFFFF\t0\t0\t0
",
        );
        tree.file(
            "/proc/net/wireless",
            "\
Inter-| sta-|   Quality        |   Discarded packets               | Missed | WE
 face | tus | link level noise |  nwid  crypt   frag  retry   misc | beacon | 22
 wlan0: 0000   67.  -43.  -256        0      0      0      0      0        0
",
        );

        let kernel = RecordingKernel::new()
            .with_address("wlan0", "192.168.1.74/24")
            .with_essid("wlan0", "kitchen-table");
        let network = Network::new(tree.sysfs(), kernel);

        let visible = network.visible_interfaces();
        assert_eq!(
            visible
                .iter()
                .map(|interface| interface.name.as_str())
                .collect::<Vec<_>>(),
            vec!["eth0", "wlan0"]
        );

        let eth0 = &visible[0];
        assert!(eth0.admin_up, "the port is still switched on");
        assert!(!eth0.carrier, "but nothing is plugged into it");
        assert!(!eth0.is_online());
        assert_eq!(eth0.summary(), "eth0  no carrier");

        let wlan0 = &visible[1];
        assert_eq!(wlan0.kind, Kind::Wireless);
        assert_eq!(wlan0.ssid(), Some("kitchen-table"));
        let wireless = wlan0.wireless.as_ref().unwrap();
        assert!(wireless.is_associated());
        assert_eq!(wireless.link_quality, Some(67));
        assert_eq!(wireless.signal_dbm, Some(-43));
        assert!(wlan0.is_online());
        assert_eq!(wlan0.summary(), "wlan0  kitchen-table  192.168.1.74/24");
        assert_eq!(
            network.active_interface().map(|interface| interface.name),
            Some("wlan0".to_string())
        );
    }

    #[test]
    fn a_machine_with_only_loopback_has_nothing_worth_showing() {
        let tree = Tree::new("lonely");
        add_loopback(&tree);
        let kernel = RecordingKernel::new().with_address("lo", "127.0.0.1/8");
        let network = Network::new(tree.sysfs(), kernel);

        let interfaces = network.interfaces();
        assert_eq!(interfaces.len(), 1);
        assert_eq!(interfaces[0].kind, Kind::Loopback);
        assert!(!interfaces[0].is_online(), "loopback goes nowhere");
        assert!(
            interfaces[0].mac.is_none(),
            "an all zero hardware address is no address"
        );
        assert!(network.visible_interfaces().is_empty());
        assert!(network.default_route().is_none());
        assert!(network.active_interface().is_none());
    }

    #[test]
    fn an_interface_that_vanishes_mid_read_is_not_a_crash() {
        let tree = Tree::new("vanishing");
        add_interface(&tree, "eth0", true, true);
        make_physical(&tree, "eth0");
        add_interface(&tree, "usb0", true, true);
        make_physical(&tree, "usb0");
        let network = Network::new(tree.sysfs(), RecordingKernel::new());

        // Unplugged between the listing and the read: the directory is still
        // in the listing, its attributes have gone.
        tree.remove("/sys/class/net/usb0");
        tree.dir("/sys/class/net/usb0");
        let names: Vec<String> = network
            .interfaces()
            .into_iter()
            .map(|interface| interface.name)
            .collect();
        assert_eq!(names, vec!["eth0".to_string()]);

        // And gone entirely by the time somebody asks for it by name.
        tree.remove("/sys/class/net/usb0");
        assert!(network.interface("usb0").is_none());
        assert!(network.interface("eth0").is_some());
    }

    #[test]
    fn bridges_and_tunnels_are_kept_out_of_a_users_way() {
        let tree = Tree::new("virtual");
        add_interface(&tree, "eth0", true, true);
        make_physical(&tree, "eth0");
        // A bridge says so in its uevent and has no device behind it.
        add_interface(&tree, "br0", true, true);
        tree.file(
            "/sys/class/net/br0/uevent",
            "DEVTYPE=bridge\nINTERFACE=br0\n",
        );
        // A tunnel says nothing at all, which is the case the missing device
        // link catches.
        add_interface(&tree, "wg0", true, true);
        add_loopback(&tree);

        let network = Network::new(tree.sysfs(), RecordingKernel::new());
        let by_name: Vec<(String, Kind)> = network
            .interfaces()
            .into_iter()
            .map(|interface| (interface.name, interface.kind))
            .collect();
        assert_eq!(
            by_name,
            vec![
                ("eth0".to_string(), Kind::Wired),
                ("br0".to_string(), Kind::Virtual),
                ("wg0".to_string(), Kind::Virtual),
                ("lo".to_string(), Kind::Loopback),
            ]
        );
        assert_eq!(
            network
                .visible_interfaces()
                .into_iter()
                .map(|interface| interface.name)
                .collect::<Vec<_>>(),
            vec!["eth0".to_string()]
        );
    }

    #[test]
    fn a_route_gateway_is_hex_with_its_bytes_the_other_way_round() {
        let routes = parse_proc_net_route(WIRED_ROUTES);
        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0].raw_gateway, Ipv4Addr::new(192, 168, 2, 1));
        assert_eq!(routes[0].destination, Ipv4Addr::UNSPECIFIED);
        assert_eq!(routes[0].prefix_len, 0);
        assert!(routes[0].is_default());
        // The second line is the interface's own subnet, not a default route.
        assert_eq!(routes[1].destination, Ipv4Addr::new(192, 168, 2, 0));
        assert_eq!(routes[1].prefix_len, 24);
        assert!(!routes[1].is_default());
        assert_eq!(routes[1].gateway(), None, "an on-link route has no gateway");
    }

    #[test]
    fn the_lowest_metric_default_route_is_the_one_that_counts() {
        let routes = parse_proc_net_route(
            "\
Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT
wlan0\t00000000\t0101A8C0\t0003\t0\t0\t600\t00000000\t0\t0\t0
eth0\t00000000\t0102A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0
",
        );
        let chosen = default_route(&routes).unwrap();
        assert_eq!(chosen.interface, "eth0");
        assert_eq!(chosen.metric, 100);
        assert_eq!(chosen.gateway, Some(Ipv4Addr::new(192, 168, 2, 1)));
    }

    #[test]
    fn a_route_that_is_not_up_is_not_a_default_route() {
        let routes = parse_proc_net_route(
            "\
Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT
eth0\t00000000\t0102A8C0\t0002\t0\t0\t100\t00000000\t0\t0\t0
",
        );
        assert!(default_route(&routes).is_none());
    }

    #[test]
    fn malformed_route_text_is_skipped_rather_than_believed() {
        for text in [
            "",
            "not a route table at all\n",
            // Too few columns.
            "eth0\t00000000\t0102A8C0\n",
            // Short hex, which would otherwise parse to a plausible address.
            "eth0\t0\t0102A8C0\t0003\t0\t0\t100\t0\t0\t0\t0\n",
            // Hex that is not hex.
            "eth0\tZZZZZZZZ\t0102A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0\n",
            // A metric that is not a number.
            "eth0\t00000000\t0102A8C0\t0003\t0\t0\tlots\t00000000\t0\t0\t0\n",
            // The header on its own.
            "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\n",
        ] {
            let routes = parse_proc_net_route(text);
            assert!(routes.is_empty(), "believed {text:?}");
            assert!(default_route(&routes).is_none());
        }
    }

    #[test]
    fn a_machine_whose_proc_is_not_there_still_lists_its_interfaces() {
        let tree = Tree::new("noproc");
        add_interface(&tree, "eth0", true, true);
        make_physical(&tree, "eth0");
        let network = Network::new(tree.sysfs(), RecordingKernel::new());
        let interfaces = network.interfaces();
        assert_eq!(interfaces.len(), 1);
        assert!(!interfaces[0].is_default);
        assert!(interfaces[0].gateway.is_none());
    }

    #[test]
    fn addresses_are_sorted_with_the_useful_ones_first() {
        let tree = Tree::new("addresses");
        add_interface(&tree, "eth0", true, true);
        make_physical(&tree, "eth0");
        let kernel = RecordingKernel::new()
            .with_address("eth0", "fe80::dead:beef/64")
            .with_address("eth0", "2001:db8::5/64")
            .with_address("eth0", "169.254.7.7/16")
            .with_address("eth0", "10.0.0.5/8")
            .with_address("wlan0", "192.168.9.9/24");
        let network = Network::new(tree.sysfs(), kernel);

        let eth0 = network.interface("eth0").unwrap();
        assert_eq!(
            eth0.addresses
                .iter()
                .map(Address::to_string)
                .collect::<Vec<_>>(),
            vec![
                "10.0.0.5/8",
                "2001:db8::5/64",
                "169.254.7.7/16",
                "fe80::dead:beef/64",
            ],
            "and another interface's address must not leak in"
        );
        assert_eq!(
            eth0.ipv4().map(|address| address.to_string()),
            Some("10.0.0.5/8".to_string())
        );
        assert_eq!(
            eth0.ipv6().map(|address| address.to_string()),
            Some("2001:db8::5/64".to_string())
        );
    }

    #[test]
    fn an_address_is_parsed_with_or_without_its_prefix() {
        assert_eq!(
            Address::parse("192.168.1.5/24"),
            Some(Address {
                ip: IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5)),
                prefix_len: 24,
            })
        );
        assert_eq!(Address::parse("10.0.0.1").unwrap().prefix_len, 32);
        assert_eq!(Address::parse("2001:db8::1").unwrap().prefix_len, 128);
        assert!(Address::parse("192.168.1.5/33").is_none());
        assert!(Address::parse("not an address").is_none());
        assert!(Address::parse("192.168.1.5/wide").is_none());
    }

    #[test]
    fn link_local_and_loopback_addresses_are_not_routable() {
        assert!(!Address::parse("169.254.1.1/16").unwrap().is_routable());
        assert!(!Address::parse("fe80::1/64").unwrap().is_routable());
        assert!(!Address::parse("127.0.0.1/8").unwrap().is_routable());
        assert!(!Address::parse("0.0.0.0/0").unwrap().is_routable());
        assert!(Address::parse("192.168.1.5/24").unwrap().is_routable());
        assert!(Address::parse("2001:db8::1/64").unwrap().is_routable());
    }

    #[test]
    fn a_speed_the_driver_will_not_report_is_absent_rather_than_wrong() {
        let tree = Tree::new("speed");
        add_interface(&tree, "eth0", false, false);
        make_physical(&tree, "eth0");
        // What a down link's `speed` holds, when reading it did not fail
        // outright.
        tree.file("/sys/class/net/eth0/speed", "-1\n");
        let network = Network::new(tree.sysfs(), RecordingKernel::new());
        let eth0 = network.interface("eth0").unwrap();
        assert!(eth0.speed_mbps.is_none());
        assert!(!eth0.admin_up);
        assert_eq!(eth0.state, LinkState::Down);
        assert_eq!(eth0.summary(), "eth0  down");
    }

    #[test]
    fn a_wireless_line_with_a_relative_signal_level_is_not_read_as_dbm() {
        let parsed = parse_proc_net_wireless(
            "\
Inter-| sta-|   Quality        |   Discarded packets               | Missed | WE
 face | tus | link level noise |  nwid  crypt   frag  retry   misc | beacon | 22
 wlan0: 0000   54.  -56.  -256        0      0      0      0      0        0
 wlan1: 0000   40.  148.   68.        0      0      0      0      0        0
",
        );
        assert_eq!(parsed.len(), 2, "the two header lines are not interfaces");
        assert_eq!(parsed[0].0, "wlan0");
        assert_eq!(parsed[0].1.signal_dbm, Some(-56));
        assert_eq!(parsed[1].1.link_quality, Some(40));
        assert_eq!(parsed[1].1.signal_dbm, None);
        assert!(parse_proc_net_wireless("").is_empty());
    }

    #[test]
    fn the_admin_bit_is_the_one_in_the_flags_word() {
        assert_eq!(parse_flags("0x1003\n"), Some(0x1003));
        assert_eq!(parse_flags("1003"), Some(0x1003));
        assert_eq!(parse_flags("nonsense"), None);
        assert!(parse_flags("0x1003").unwrap() & IFF_UP != 0);
        assert!(parse_flags("0x1002").unwrap() & IFF_UP == 0);
        assert!(parse_flags("0x9").unwrap() & IFF_LOOPBACK != 0);
    }

    #[test]
    fn bringing_a_link_up_asks_the_kernel_once_and_says_which() {
        let tree = Tree::new("bringup");
        add_interface(&tree, "eth0", false, false);
        make_physical(&tree, "eth0");
        let mut network = Network::new(tree.sysfs(), RecordingKernel::new());

        network.bring_up("eth0").unwrap();
        network.take_down("eth0").unwrap();

        assert_eq!(network.kernel().transcript(), vec!["eth0 up", "eth0 down"]);
        assert!(network.kernel().did("eth0 up"));
        assert!(!network.kernel().did("wlan0 up"));
    }

    #[test]
    fn a_kernel_that_refuses_reports_why_instead_of_pretending() {
        let tree = Tree::new("refused");
        add_interface(&tree, "eth0", false, false);
        let mut network = Network::new(tree.sysfs(), RecordingKernel::new().refusing("eth0"));
        let error = network.bring_up("eth0").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        // It still wrote down the attempt, which is what a log wants.
        assert!(network.kernel().did("eth0 up"));
    }

    #[test]
    fn a_prefix_length_is_counted_out_of_a_netmask() {
        assert_eq!(prefix_from_mask(&[255, 255, 255, 0]), 24);
        assert_eq!(prefix_from_mask(&[255, 255, 255, 255]), 32);
        assert_eq!(prefix_from_mask(&[0, 0, 0, 0]), 0);
        assert_eq!(prefix_from_mask(&[255; 16]), 128);
    }

    #[test]
    fn a_devtype_is_picked_out_of_a_uevent_file() {
        assert_eq!(
            uevent_devtype("DEVTYPE=wlan\nINTERFACE=wlan0\n").as_deref(),
            Some("wlan")
        );
        assert_eq!(uevent_devtype("INTERFACE=eth0\n"), None);
        assert_eq!(uevent_devtype("DEVTYPE=\n"), None);
    }

    #[test]
    fn the_real_kernel_reads_addresses_without_changing_anything() {
        // getifaddrs is POSIX, so this runs on the machine the code is written
        // on too. It is deliberately undemanding: a sandbox with no interfaces
        // at all still passes, what is being checked is that the pointer walk
        // is sound and the names come back as text.
        for entry in SystemKernel.addresses() {
            assert!(!entry.interface.is_empty());
            assert!(entry.address.prefix_len <= 128);
        }
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn changing_a_link_off_linux_is_refused_rather_than_faked() {
        let error = SystemKernel.set_link_up("eth0", true).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert!(SystemKernel.essid("wlan0").is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn an_impossible_interface_name_never_reaches_the_ioctl() {
        assert!(name_field("").is_err());
        assert!(name_field("an-interface-name-far-too-long-for-the-field").is_err());
        let field = name_field("eth0").unwrap();
        assert_eq!(field[4], 0, "the rest of the field stays zero");
        assert_ne!(
            field,
            name_field("eth1").unwrap(),
            "and the name itself really is copied in"
        );
    }
}
