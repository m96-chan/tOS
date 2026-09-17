//! DHCPv4, because there is nothing on this machine to ask for an address.
//!
//! A tOS install is a kernel, a compositor and a shell. There is no
//! `dhclient`, no `dhcpcd` and no `systemd-networkd`, and putting one there
//! would mean shipping a C daemon, a configuration language for it and a way
//! to supervise it, in order to obtain four numbers. DHCP is a small
//! protocol — one packet shape, four messages, and a handshake that fits on a
//! page — so it is written here, the way `tos-term` parses its own PNGs
//! rather than take a crate for it.
//!
//! This module lives under [`crate::net`] rather than beside it because DHCP
//! is not a thing in its own right: it is one of the ways an interface in
//! `net.rs` gets an address, and it is `net.rs`'s [`crate::net::Kernel`] seam
//! that applies what it learns. A sibling `dhcp.rs` would have had to import
//! half of `net.rs` and export a [`Lease`] back into it, which is a line
//! drawn through something that is already one piece.
//!
//! Everything above the wire is a function over bytes. [`Message::parse`] and
//! [`Message::encode`] have no socket in them, [`Client`] is a state machine
//! that is handed datagrams and answers with datagrams, and the socket itself
//! is [`Transport`], a two-method trait. So the whole of DISCOVER → OFFER →
//! REQUEST → ACK is exercised by this crate's tests against [`FakeServer`],
//! on a machine with no DHCP server, no privileges and no network at all.
//!
//! So is everything *below* the wire, which there is some of since #156:
//! sends go out of a raw socket, because a UDP socket on a link with no
//! address of its own puts another link's address in the IP header.
//! [`broadcast_frame`] is the Ethernet, IPv4 and UDP headers as one more
//! function over bytes, and the only thing the socket does with it is hand it
//! to `sendto`.
//!
//! # What is here and what is not
//!
//! Acquisition is complete: a client that has never had an address gets one,
//! with its netmask, its default gateway and its resolvers. **Renewal is
//! not.** A lease carries the seconds it is good for and
//! [`Lease::renew_after`] says when T1 falls, but nothing here wakes up at
//! T1 — the compositor has one thread and a frame loop, and a timer that
//! fires in four hours is a different piece of machinery from a menu entry
//! somebody just pressed. What renewal would take, for whoever does it: a
//! `Client` that starts in `Bound` rather than `Init` and unicasts a REQUEST
//! to [`Lease::server`] with `ciaddr` set to the address it already holds
//! (this is the one case where the packet is not broadcast, so [`Transport`]
//! would grow a `send_to`); a deadline on [`crate::net::Network`] that the
//! compositor folds into the wait it already does for the poll interval, the
//! way `Machine::next_poll` is folded in today; and a fall back to a
//! broadcast REBINDING at T2 and to a fresh DISCOVER when the lease finally
//! expires. Until then a machine whose lease runs out keeps an address the
//! server believes is free — wrong, but quiet — and asking for DHCP again
//! from the network menu fixes it.
//!
//! Nothing here does IPv6. Stateless autoconfiguration needs no client at
//! all, since the kernel does it from router advertisements on its own, and
//! DHCPv6 is a separate protocol with a separate packet shape wanted by
//! approximately nobody whose first problem is getting a laptop onto a café
//! network.

use std::io;
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

/// The port a client listens on.
///
/// It is below 1024, which is the first thing that makes this privileged:
/// binding 68 wants `CAP_NET_BIND_SERVICE` before any of `net.rs`'s ioctls
/// want `CAP_NET_ADMIN`.
pub const CLIENT_PORT: u16 = 68;
/// The port a server listens on.
pub const SERVER_PORT: u16 = 67;

/// `BOOTREQUEST`, the op code on everything a client sends.
const BOOTREQUEST: u8 = 1;
/// `BOOTREPLY`, the op code on everything a server sends. A datagram that is
/// not one is somebody else's DHCP conversation, overheard on the broadcast
/// address.
const BOOTREPLY: u8 = 2;
/// `htype`: 1 is 10mb Ethernet, which is what every card this will ever run
/// on still calls itself.
const ETHERNET: u8 = 1;
/// `hlen`: six bytes of MAC.
const MAC_LEN: u8 = 6;

/// The four bytes that tell a DHCP message from the BOOTP message it is
/// wearing the clothes of. Without it the options field is a BOOTP vendor
/// area and means nothing at all.
const MAGIC_COOKIE: [u8; 4] = [99, 130, 83, 99];

/// Everything before the cookie: `op` through `file`, 236 bytes.
const FIXED_LEN: usize = 236;
/// The shortest a message can be and still be one.
const MIN_LEN: usize = FIXED_LEN + 4;

/// `flags`: ask the server to broadcast its reply rather than unicast it.
///
/// This bit is the whole reason an ordinary UDP socket is enough to *hear*
/// on. A client in SELECTING has no address, so a server that unicasts the
/// OFFER to the address it is about to hand out is talking to a machine that
/// does not have it yet: the reply would then have to be caught below the IP
/// stack, on the raw socket that [`broadcast_frame`] already sends from.
/// RFC 2131 §4.1 put this flag in the protocol for exactly that case, and a
/// server that honours it broadcasts to 255.255.255.255:68, which lands in a
/// socket bound to `0.0.0.0:68` like any other datagram.
///
/// What goes wrong without it, or against a server that ignores it: the offer
/// goes somewhere this client cannot hear, nothing arrives, and [`acquire`]
/// gives up with a timeout. A wrong answer is not one of the outcomes, which
/// is what makes the trade worth taking.
const FLAG_BROADCAST: u16 = 0x8000;

/// The shortest datagram a relay agent is obliged to forward.
///
/// RFC 951's BOOTP message is 300 bytes of payload and some relays and
/// switches still drop anything shorter, so a DISCOVER with four options on
/// it is padded back up rather than sent at its natural size. The padding is
/// `PAD` options, which every parser on earth skips.
const MIN_PAYLOAD: usize = 300;

/// Option codes, from the IANA registry. Only the ones this asks for or reads
/// are named; an option that is not here is carried through [`Message`] as
/// bytes and ignored.
mod option {
    pub const PAD: u8 = 0;
    pub const SUBNET_MASK: u8 = 1;
    pub const ROUTER: u8 = 3;
    pub const DNS_SERVER: u8 = 6;
    pub const DOMAIN_NAME: u8 = 15;
    pub const REQUESTED_ADDRESS: u8 = 50;
    pub const LEASE_TIME: u8 = 51;
    pub const MESSAGE_TYPE: u8 = 53;
    pub const SERVER_ID: u8 = 54;
    pub const PARAMETER_LIST: u8 = 55;
    pub const MESSAGE: u8 = 56;
    pub const MAX_MESSAGE_SIZE: u8 = 57;
    pub const RENEWAL_TIME: u8 = 58;
    pub const CLIENT_ID: u8 = 61;
    pub const END: u8 = 255;
}

/// What this client asks every server for, in option 55.
///
/// Servers may send whatever they like and most send more than this, but a
/// server that trims its replies to the parameter list — which is what the
/// list is for — must be told that a netmask, a gateway, resolvers and a
/// domain are the four things a machine needs in order to be on the network.
/// Leaving the list off is how you get an ACK with an address in it and
/// nothing to do with it.
const PARAMETERS: &[u8] = &[
    option::SUBNET_MASK,
    option::ROUTER,
    option::DNS_SERVER,
    option::DOMAIN_NAME,
];

/// The largest datagram this client will accept, in option 57.
///
/// The receive buffer is an Ethernet frame's worth, so this is the honest
/// number to advertise: a server that would otherwise have sent a longer
/// reply trims it rather than sending something that arrives cut in half.
const MAX_MESSAGE_SIZE: u16 = 1500;

/// The message types this speaks, and the ones it only ever reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageType {
    Discover,
    Offer,
    Request,
    Decline,
    Ack,
    Nak,
    Release,
    Inform,
}

impl MessageType {
    pub fn code(self) -> u8 {
        match self {
            MessageType::Discover => 1,
            MessageType::Offer => 2,
            MessageType::Request => 3,
            MessageType::Decline => 4,
            MessageType::Ack => 5,
            MessageType::Nak => 6,
            MessageType::Release => 7,
            MessageType::Inform => 8,
        }
    }

    pub fn from_code(code: u8) -> Option<MessageType> {
        Some(match code {
            1 => MessageType::Discover,
            2 => MessageType::Offer,
            3 => MessageType::Request,
            4 => MessageType::Decline,
            5 => MessageType::Ack,
            6 => MessageType::Nak,
            7 => MessageType::Release,
            8 => MessageType::Inform,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            MessageType::Discover => "DISCOVER",
            MessageType::Offer => "OFFER",
            MessageType::Request => "REQUEST",
            MessageType::Decline => "DECLINE",
            MessageType::Ack => "ACK",
            MessageType::Nak => "NAK",
            MessageType::Release => "RELEASE",
            MessageType::Inform => "INFORM",
        }
    }
}

/// One DHCP message, with the BOOTP fields nobody uses left out.
///
/// `sname` and `file` are not here. They are 192 bytes of the packet that
/// exist so a diskless workstation can be told which TFTP image to boot, they
/// go out as zeroes, and the one modern use for them — option overload, where
/// a server short of room spills its options into them — is declined for the
/// reason given at [`Message::parse`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// [`BOOTREQUEST`] or [`BOOTREPLY`].
    pub op: u8,
    /// The transaction this belongs to. A reply that does not carry ours is
    /// another machine's conversation, heard because both arrive on the same
    /// broadcast address.
    pub xid: u32,
    /// Seconds since the client started trying, which a server may use to
    /// decide that its backup should answer.
    pub secs: u16,
    /// [`FLAG_BROADCAST`] is set.
    pub broadcast: bool,
    /// The address the client already holds, which is zero until it does.
    pub ciaddr: Ipv4Addr,
    /// "Your" address: the one a server is offering or confirming.
    pub yiaddr: Ipv4Addr,
    /// The next server in a boot sequence. Not the DHCP server — that is
    /// option 54 — and a well worn way to configure a machine wrongly.
    pub siaddr: Ipv4Addr,
    /// The relay agent that forwarded this, if any.
    pub giaddr: Ipv4Addr,
    /// The client's hardware address, which is the first six bytes of
    /// `chaddr`.
    pub mac: [u8; 6],
    /// Options in the order they were on the wire, code and body. Kept as
    /// bytes so that an option this does not understand survives a parse and
    /// can be looked at by something that does.
    pub options: Vec<(u8, Vec<u8>)>,
}

impl Message {
    /// An empty request from this MAC, which every message a client sends is
    /// a few options on top of.
    pub fn request(mac: [u8; 6], xid: u32) -> Message {
        Message {
            op: BOOTREQUEST,
            xid,
            secs: 0,
            broadcast: true,
            ciaddr: Ipv4Addr::UNSPECIFIED,
            yiaddr: Ipv4Addr::UNSPECIFIED,
            siaddr: Ipv4Addr::UNSPECIFIED,
            giaddr: Ipv4Addr::UNSPECIFIED,
            mac,
            options: Vec::new(),
        }
    }

    /// Read a datagram.
    ///
    /// Every failure is `None` rather than an error, for the same reason a
    /// missing sysfs file is: this is a socket bound to a broadcast address,
    /// so the ordinary reason a parse fails is that the datagram was never
    /// meant for us. There is nothing to report and nothing to do but wait
    /// for the next one.
    ///
    /// Option overload — RFC 2132's option 52, "the rest of my options are in
    /// `sname` and `file`" — is not implemented. It exists for servers that
    /// cannot fit a reply in 576 bytes, this client asks for four options, and
    /// a server that needs more than 300 bytes to answer four options has
    /// gone wrong in a way no amount of parsing will fix. A message carrying
    /// option 52 still parses; the spilled options are simply not found,
    /// which surfaces as a lease missing its netmask and refused by
    /// [`Lease::from_ack`] rather than as a lease that is quietly wrong.
    pub fn parse(datagram: &[u8]) -> Option<Message> {
        if datagram.len() < MIN_LEN || datagram[FIXED_LEN..MIN_LEN] != MAGIC_COOKIE {
            return None;
        }
        // `hlen` may be anything up to 16. Six is Ethernet, and it is the only
        // kind of link this will ever be put on; anything else is a token
        // ring, a firewire bus or a corrupt packet, and the MAC comparison
        // that filters replies could not be done on it in any case.
        if datagram[1] != ETHERNET || datagram[2] != MAC_LEN {
            return None;
        }

        let word =
            |at: usize| u32::from_be_bytes([at, at + 1, at + 2, at + 3].map(|i| datagram[i]));
        let address = |at: usize| Ipv4Addr::from(word(at));
        let mut mac = [0u8; 6];
        mac.copy_from_slice(&datagram[28..34]);

        Some(Message {
            op: datagram[0],
            xid: word(4),
            secs: u16::from_be_bytes([datagram[8], datagram[9]]),
            broadcast: u16::from_be_bytes([datagram[10], datagram[11]]) & FLAG_BROADCAST != 0,
            ciaddr: address(12),
            yiaddr: address(16),
            siaddr: address(20),
            giaddr: address(24),
            mac,
            options: parse_options(&datagram[MIN_LEN..]),
        })
    }

    /// Write a datagram, padded up to [`MIN_PAYLOAD`].
    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![0u8; FIXED_LEN];
        out[0] = self.op;
        out[1] = ETHERNET;
        out[2] = MAC_LEN;
        // `hops` is zero from a client; a relay agent increments it.
        out[3] = 0;
        out[4..8].copy_from_slice(&self.xid.to_be_bytes());
        out[8..10].copy_from_slice(&self.secs.to_be_bytes());
        let flags = if self.broadcast { FLAG_BROADCAST } else { 0 };
        out[10..12].copy_from_slice(&flags.to_be_bytes());
        out[12..16].copy_from_slice(&self.ciaddr.octets());
        out[16..20].copy_from_slice(&self.yiaddr.octets());
        out[20..24].copy_from_slice(&self.siaddr.octets());
        out[24..28].copy_from_slice(&self.giaddr.octets());
        out[28..34].copy_from_slice(&self.mac);

        out.extend_from_slice(&MAGIC_COOKIE);
        for (code, body) in &self.options {
            // An option's length is one byte, so a body that does not fit
            // cannot be written at all. Nothing this module builds comes near
            // it; the clamp is here so that a future caller handing over
            // something long truncates visibly rather than writing a length
            // that lies about the bytes after it.
            let length = body.len().min(u8::MAX as usize);
            out.push(*code);
            out.push(length as u8);
            out.extend_from_slice(&body[..length]);
        }
        out.push(option::END);
        out.resize(out.len().max(MIN_PAYLOAD), option::PAD);
        out
    }

    /// Put an option on, replacing one already there with the same code.
    pub fn set(&mut self, code: u8, body: Vec<u8>) {
        match self.options.iter_mut().find(|(have, _)| *have == code) {
            Some((_, existing)) => *existing = body,
            None => self.options.push((code, body)),
        }
    }

    pub fn set_message_type(&mut self, kind: MessageType) {
        self.set(option::MESSAGE_TYPE, vec![kind.code()]);
    }

    pub fn option(&self, code: u8) -> Option<&[u8]> {
        self.options
            .iter()
            .find(|(have, _)| *have == code)
            .map(|(_, body)| body.as_slice())
    }

    pub fn message_type(&self) -> Option<MessageType> {
        match self.option(option::MESSAGE_TYPE)? {
            [code] => MessageType::from_code(*code),
            _ => None,
        }
    }

    /// An option that is exactly one address.
    pub fn ipv4(&self, code: u8) -> Option<Ipv4Addr> {
        match self.option(code)? {
            [a, b, c, d] => Some(Ipv4Addr::new(*a, *b, *c, *d)),
            _ => None,
        }
    }

    /// An option that is a list of addresses. A body whose length is not a
    /// multiple of four keeps the addresses that are whole, because three
    /// usable resolvers and a stub are better than nothing.
    pub fn ipv4_list(&self, code: u8) -> Vec<Ipv4Addr> {
        self.option(code)
            .unwrap_or_default()
            .chunks_exact(4)
            .map(|chunk| Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]))
            .collect()
    }

    /// An option that is one big-endian 32 bit number: the timers.
    pub fn seconds(&self, code: u8) -> Option<u32> {
        match self.option(code)? {
            [a, b, c, d] => Some(u32::from_be_bytes([*a, *b, *c, *d])),
            _ => None,
        }
    }

    /// An option that is text.
    ///
    /// Servers put all sorts in here, and a domain name arriving with a
    /// trailing NUL is common enough to be normal, so this trims rather than
    /// refuses. Anything that is not UTF-8 after that is dropped: a domain
    /// name that cannot be printed cannot be written into `resolv.conf`
    /// either.
    pub fn text(&self, code: u8) -> Option<String> {
        let body = self.option(code)?;
        let text = String::from_utf8(body.to_vec()).ok()?;
        let text = text.trim_end_matches('\0').trim().to_string();
        if text.is_empty() {
            None
        } else {
            Some(text)
        }
    }
}

/// Walk the options field.
///
/// `PAD` is one byte with no length after it and `END` stops the walk, which
/// is what makes this a loop rather than a slice of tuples. A truncated
/// option — a code and a length with fewer bytes after them than the length
/// claims — ends the walk too, keeping everything read up to that point: a
/// datagram cut short still carries the options that arrived whole.
fn parse_options(mut body: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut out = Vec::new();
    while let Some((code, rest)) = body.split_first() {
        match *code {
            option::END => break,
            option::PAD => {
                body = rest;
                continue;
            }
            _ => {}
        }
        let Some((length, rest)) = rest.split_first() else {
            break;
        };
        let length = *length as usize;
        if rest.len() < length {
            break;
        }
        out.push((*code, rest[..length].to_vec()));
        body = &rest[length..];
    }
    out
}

/// What a server agreed to, once the ACK is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    pub address: Ipv4Addr,
    pub netmask: Ipv4Addr,
    /// The default gateway. Optional because a network can genuinely have
    /// none — a lab segment, a machine on a switch with nothing beyond it —
    /// and because a server is allowed to send an empty router list to say
    /// exactly that.
    pub router: Option<Ipv4Addr>,
    pub resolvers: Vec<Ipv4Addr>,
    pub domain: Option<String>,
    /// The server that granted it, from option 54. Where a renewal would be
    /// sent, and the only reason this is kept.
    pub server: Ipv4Addr,
    /// How long the lease is good for, in seconds. `None` is an infinite
    /// lease, which option 51 spells `0xffffffff`.
    pub seconds: Option<u32>,
    /// Option 58, the renewal time, when the server named one.
    pub renewal_seconds: Option<u32>,
}

impl Lease {
    /// Read a lease out of an ACK, or say why the ACK is not usable.
    ///
    /// A missing netmask is fatal rather than guessed at. The classful
    /// default — a 10.x address is a /8 — has been wrong since CIDR, and
    /// guessing it puts the machine on a subnet of the wrong size in a way
    /// nothing announces: traffic to a neighbour goes to the gateway, or
    /// traffic to the gateway is ARPed for on a segment it is not on, and the
    /// symptom is "the network is slow" rather than "the netmask is wrong".
    /// Every DHCP server sends option 1; one that does not has not been given
    /// a subnet to hand out, and refusing is the honest answer.
    pub fn from_ack(ack: &Message) -> Result<Lease, String> {
        if ack.yiaddr.is_unspecified() {
            return Err("the server acknowledged no address".to_string());
        }
        let Some(netmask) = ack.ipv4(option::SUBNET_MASK) else {
            return Err("the server sent no subnet mask".to_string());
        };
        if !is_contiguous_mask(netmask) {
            return Err(format!("{netmask} is not a subnet mask"));
        }
        Ok(Lease {
            address: ack.yiaddr,
            netmask,
            // The first router is the default gateway; the rest are
            // alternatives only a routing daemon could choose between.
            router: ack.ipv4_list(option::ROUTER).first().copied(),
            resolvers: ack.ipv4_list(option::DNS_SERVER),
            domain: ack.text(option::DOMAIN_NAME),
            // A server that did not identify itself is still a server, and
            // `siaddr` is the next best guess at where a renewal would go.
            server: ack.ipv4(option::SERVER_ID).unwrap_or(ack.siaddr),
            seconds: ack
                .seconds(option::LEASE_TIME)
                .filter(|seconds| *seconds != u32::MAX),
            renewal_seconds: ack.seconds(option::RENEWAL_TIME),
        })
    }

    /// The netmask as a prefix length, which is how everything above this
    /// prints an address.
    pub fn prefix_len(&self) -> u8 {
        u32::from(self.netmask).count_ones() as u8
    }

    /// When a renewal would be due: option 58 if the server named one, else
    /// half the lease, which is what RFC 2131 §4.4.5 says T1 defaults to.
    ///
    /// Nothing acts on this yet — see the module header — but the number is
    /// carried from the moment the lease is read, so that adding the timer
    /// later is a timer rather than a second parse of the ACK.
    pub fn renew_after(&self) -> Option<Duration> {
        let seconds = self
            .renewal_seconds
            .or_else(|| self.seconds.map(|seconds| seconds / 2))?;
        Some(Duration::from_secs(u64::from(seconds)))
    }

    /// One line for a notification: `192.168.1.50/24 via 192.168.1.1`.
    pub fn describe(&self) -> String {
        let mut text = format!("{}/{}", self.address, self.prefix_len());
        if let Some(router) = self.router {
            text.push_str(" via ");
            text.push_str(&router.to_string());
        }
        text
    }

    /// The `/etc/resolv.conf` this lease implies.
    ///
    /// Written out here, next to the lease it comes from, so that the file's
    /// contents are a value a test can compare rather than something only a
    /// filesystem can be asked about.
    ///
    /// The header line matters: this file is the one piece of an installed
    /// system tOS overwrites without being asked, and somebody who put their
    /// own resolver in it deserves to be told who took it away and why.
    pub fn resolv_conf(&self, interface: &str) -> String {
        let mut text = format!("# written by tOS from the DHCP lease on {interface}\n");
        if let Some(domain) = &self.domain {
            text.push_str(&format!("search {domain}\n"));
        }
        for resolver in &self.resolvers {
            text.push_str(&format!("nameserver {resolver}\n"));
        }
        text
    }
}

/// Whether a netmask is a run of ones followed by a run of zeroes.
///
/// `255.255.0.255` has 24 bits set and would pass a `count_ones` test while
/// meaning nothing at all, and the kernel accepts it through `SIOCSIFNETMASK`
/// and produces a routing table nobody can explain afterwards.
fn is_contiguous_mask(mask: Ipv4Addr) -> bool {
    // The complement of a contiguous mask is one less than a power of two,
    // and `x & (x + 1) == 0` is the cheap way to ask that. It is also true of
    // the all-ones mask, whose complement is zero.
    let holes = !u32::from(mask);
    holes & holes.wrapping_add(1) == 0
}

/// Where a client is in the handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Nothing sent.
    Init,
    /// A DISCOVER is out and an OFFER is what would answer it.
    Selecting,
    /// An offer has been taken up and a REQUEST is out.
    Requesting,
    /// An ACK arrived and was usable.
    Bound,
    /// A NAK arrived, or the ACK was not usable.
    Failed,
}

/// What a datagram made the client do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Not ours, or not interesting in this state. Nothing changed, so the
    /// caller goes back to waiting on the same deadline.
    Ignore,
    /// Put these bytes on the wire and keep waiting.
    Send(Vec<u8>),
    /// Done. Boxed because a lease is much larger than the other variants and
    /// every `Step` would otherwise be that size.
    Bound(Box<Lease>),
    /// The server said no, or said yes to something unusable. Sending the
    /// same thing again will not help, so the caller stops.
    Refused(String),
}

/// The DISCOVER → OFFER → REQUEST → ACK state machine, with no socket in it.
///
/// Handed datagrams, answers with datagrams. That is the whole interface, and
/// it is what lets every branch below — a reply for another transaction, a
/// second server's offer arriving after the first was taken, a NAK, an ACK
/// with no netmask on it — be a test rather than something that happens
/// occasionally on somebody else's network.
#[derive(Debug, Clone)]
pub struct Client {
    mac: [u8; 6],
    xid: u32,
    state: State,
    /// The offer taken up, kept so that the REQUEST can name it and so that
    /// an ACK for a different address can be told from an ACK for ours.
    taken: Option<Offer>,
    /// The `secs` last handed to [`Client::discover`], kept so that the
    /// REQUEST can carry the same count. Only the retry loop knows how long
    /// this client has been asking, and the REQUEST is built here, in the
    /// middle of a `receive`, where there is nobody to ask.
    secs: u16,
}

/// The two numbers out of an OFFER that the REQUEST has to repeat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Offer {
    address: Ipv4Addr,
    server: Option<Ipv4Addr>,
}

impl Client {
    pub fn new(mac: [u8; 6], xid: u32) -> Client {
        Client {
            mac,
            xid,
            state: State::Init,
            taken: None,
            secs: 0,
        }
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn xid(&self) -> u32 {
        self.xid
    }

    /// The DISCOVER, and the move into SELECTING.
    ///
    /// Takes `secs` because the field is the number of seconds this client
    /// has been trying and only the caller, which owns the retry loop, knows
    /// it. A server with a backup uses it to decide when the backup should
    /// answer, so a client that always sends zero is one a failover pair
    /// never gets round to helping.
    ///
    /// Calling it again is how a retransmission is built: the same
    /// transaction, the same options, a later `secs`. It returns to SELECTING
    /// and forgets any offer taken, so the caller has to know it is still in
    /// that stage before asking — [`acquire`] does that with
    /// [`Client::state`].
    pub fn discover(&mut self, secs: u16) -> Vec<u8> {
        self.state = State::Selecting;
        self.taken = None;
        self.secs = secs;
        let mut message = Message::request(self.mac, self.xid);
        message.secs = secs;
        message.set_message_type(MessageType::Discover);
        self.add_identity(&mut message);
        message.encode()
    }

    /// Offer one datagram to the client.
    pub fn receive(&mut self, datagram: &[u8]) -> Step {
        let Some(message) = Message::parse(datagram) else {
            return Step::Ignore;
        };
        // Three filters. A socket bound to the broadcast address hears every
        // DHCP conversation on the segment, so without them a busy network
        // hands this client an address that belongs to the machine next to
        // it — which is the one failure that does not announce itself.
        if message.op != BOOTREPLY || message.xid != self.xid || message.mac != self.mac {
            return Step::Ignore;
        }
        let Some(kind) = message.message_type() else {
            // A BOOTP reply with no option 53 on it. Some very old servers
            // answer a DISCOVER this way; there is no lease in it, so there
            // is nothing to do but let the retry run out.
            return Step::Ignore;
        };

        match (self.state, kind) {
            (State::Selecting, MessageType::Offer) => self.take(&message),
            // A NAK in SELECTING is a server declining to talk to this client
            // at all: an unknown MAC on a network with a fixed table.
            (State::Selecting, MessageType::Nak) => self.refuse(&message),
            (State::Requesting, MessageType::Ack) => self.acknowledge(&message),
            (State::Requesting, MessageType::Nak) => self.refuse(&message),
            // A second server's offer, arriving after the first was taken up.
            // RFC 2131 is explicit that the client picks one and that the
            // others are released by the broadcast REQUEST naming a server
            // id, so there is nothing to do with this but drop it.
            (State::Requesting, MessageType::Offer) => Step::Ignore,
            _ => Step::Ignore,
        }
    }

    /// Take up an offer: record it, and answer with the REQUEST that claims
    /// it.
    ///
    /// The REQUEST is broadcast rather than unicast to the server that
    /// offered, and `ciaddr` stays zero. Both are required of a client in
    /// SELECTING: the broadcast is what tells every *other* server that
    /// offered that its offer was not taken and can go back in the pool, and
    /// `ciaddr` is for a client that already holds the address it is asking
    /// about, which one halfway through its first handshake does not.
    ///
    /// `secs` is this client's own count of how long it has been asking,
    /// carried over from the DISCOVER that drew the offer, and not the `secs`
    /// on the offer itself. RFC 2131's table of what a server fills in says a
    /// reply's `secs` is zero, so echoing it puts a REQUEST on the wire
    /// claiming a client that has been retrying for a minute has only just
    /// started — the one thing the field exists to tell a server apart.
    /// Unlike the DISCOVER it is not rebuilt on a retransmission: by the time
    /// a REQUEST is out some server has already answered, so there is no
    /// backup left to fail over to, and §4.1 asks for the same packet again.
    fn take(&mut self, offer: &Message) -> Step {
        if offer.yiaddr.is_unspecified() {
            return Step::Ignore;
        }
        let taken = Offer {
            address: offer.yiaddr,
            server: offer.ipv4(option::SERVER_ID),
        };
        self.taken = Some(taken);
        self.state = State::Requesting;

        let mut message = Message::request(self.mac, self.xid);
        message.secs = self.secs;
        message.set_message_type(MessageType::Request);
        message.set(option::REQUESTED_ADDRESS, taken.address.octets().to_vec());
        if let Some(server) = taken.server {
            message.set(option::SERVER_ID, server.octets().to_vec());
        }
        self.add_identity(&mut message);
        Step::Send(message.encode())
    }

    /// An ACK: check it is for the address that was offered, then read the
    /// lease out of it.
    ///
    /// A server acknowledging an address other than the one it offered has
    /// changed its mind mid-handshake, which is not something to accept
    /// quietly: the address in hand would then not be the one the REQUEST was
    /// broadcast about, and every other server that heard that broadcast has
    /// released the wrong lease.
    fn acknowledge(&mut self, ack: &Message) -> Step {
        if let Some(taken) = self.taken {
            if ack.yiaddr != taken.address {
                return Step::Ignore;
            }
        }
        match Lease::from_ack(ack) {
            Ok(lease) => {
                self.state = State::Bound;
                Step::Bound(Box::new(lease))
            }
            Err(why) => {
                self.state = State::Failed;
                Step::Refused(why)
            }
        }
    }

    fn refuse(&mut self, nak: &Message) -> Step {
        self.state = State::Failed;
        // Option 56 is a human readable reason, and the servers that set it
        // ("address in use", "no free leases") save a great deal of guessing.
        Step::Refused(match nak.text(option::MESSAGE) {
            Some(why) => format!("the server refused: {why}"),
            None => "the server refused the request".to_string(),
        })
    }

    /// The options every message from this client carries.
    ///
    /// The client identifier is the hardware type and the MAC, which is what
    /// RFC 2131 §9.14 says to send when there is nothing better. It matters
    /// because a server keys its lease table on it: a client that sends one
    /// on the DISCOVER and not on the REQUEST looks like two machines, and
    /// gets NAKed by the very server that just offered it an address.
    fn add_identity(&self, message: &mut Message) {
        let mut identifier = vec![ETHERNET];
        identifier.extend_from_slice(&self.mac);
        message.set(option::CLIENT_ID, identifier);
        message.set(option::PARAMETER_LIST, PARAMETERS.to_vec());
        message.set(
            option::MAX_MESSAGE_SIZE,
            MAX_MESSAGE_SIZE.to_be_bytes().to_vec(),
        );
    }
}

/// The socket the handshake rides on.
///
/// Two methods, because that is all acquisition needs: broadcast a datagram,
/// and wait a while for one. It is a trait for the reason every other seam in
/// this crate is one — the state machine above is driven from tests by
/// [`FakeServer`], with no socket, no privileges and no server.
pub trait Transport {
    /// Broadcast one datagram to `255.255.255.255:67`.
    fn send(&mut self, datagram: &[u8]) -> io::Result<()>;

    /// Wait up to `timeout` for a datagram on port 68.
    ///
    /// `Ok(None)` is the timeout expiring, which is the ordinary answer on a
    /// network with no DHCP server on it, and so is not an error.
    fn receive(&mut self, timeout: Duration) -> io::Result<Option<Vec<u8>>>;

    /// The monotonic clock [`acquire`] measures an attempt's budget against.
    ///
    /// The default is the real one, which is the whole answer for anything
    /// that is really a socket, so this is a third method on an existing seam
    /// rather than a `Clock` trait of its own. A separate clock would have to
    /// be handed to the transport as well: the only thing in the retry loop
    /// that takes any time is [`Transport::receive`], so a fake network that
    /// wants its fake delays to show up on the deadline has to be the thing
    /// that moves the clock. Two seams that have to be kept in step are worse
    /// than one, and the one that already knows how long it waited is this.
    ///
    /// [`FakeServer`] overrides it, which is what lets a test watch a fifteen
    /// second retry schedule run all the way out in no time at all.
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// How many times [`acquire`] sends before giving up.
///
/// RFC 2131 §4.1 asks for four seconds, then eight, sixteen and sixty-four,
/// which adds up to more than a minute of a menu that has gone quiet.
/// Somebody who has just pressed "ask for an address" is standing there
/// watching, so this starts at a second and doubles: 1 + 2 + 4 + 8 is fifteen
/// seconds to conclude there is no server, and a server that is there answers
/// the first attempt in milliseconds. A retry is a resend of the same packet
/// under the same transaction id, so a server that was merely slow is not
/// confused by it.
pub const ATTEMPTS: u32 = 4;

/// The first wait, doubled on each attempt.
pub const FIRST_WAIT: Duration = Duration::from_secs(1);

/// The most datagrams read in one attempt before the attempt is abandoned.
///
/// The deadline in [`acquire`] bounds how long an attempt takes but not how
/// much work it does, and those are different failures. A segment handing
/// over datagrams as fast as they can be dropped — every one of them somebody
/// else's DHCP, every one of them ignored — would keep this loop spinning for
/// the whole of the attempt's budget on a thread the compositor also draws
/// frames with. Sixteen is far more than a handshake needs and far fewer than
/// a busy network produces in a second, so an ordinary conversation never
/// meets it.
///
/// It cannot be the only bound. Counting datagrams without a deadline to go
/// with it multiplies the budget rather than caps it: each datagram then
/// carries a fresh timeout of its own, and sixteen of them is sixteen waits.
const MAX_DATAGRAMS: u32 = 16;

/// Run the whole handshake and come back with a lease.
///
/// Fifteen seconds is the worst this can cost: [`ATTEMPTS`] budgets of
/// [`FIRST_WAIT`] doubling, 1 + 2 + 4 + 8, and nothing that arrives in
/// between can add to them. That number is a promise made above this module
/// as well as in it — while an acquisition is in flight the compositor holds
/// the link's `dhcp` slot and answers a second request with "already asking
/// on …", so an attempt that overruns is not a slow menu entry, it is a menu
/// entry that cannot be pressed again until it finishes.
///
/// The waiting here is the receive timeout rather than a sleep, which is what
/// makes it testable: [`FakeServer`] answers at once, and the retry schedule
/// never costs a test a second. Time is read through [`Transport::now`] for
/// the same reason, so that the fifteen seconds above is something a test
/// asserts rather than something a test sits through.
pub fn acquire<T: Transport>(transport: &mut T, mac: [u8; 6], xid: u32) -> io::Result<Lease> {
    let mut client = Client::new(mac, xid);
    let started = transport.now();
    let mut elapsed = 0u16;
    // Filled by the first pass below, which is always in INIT and so always
    // builds one.
    let mut pending = Vec::new();

    for attempt in 0..ATTEMPTS {
        // A retransmitted DISCOVER is rebuilt rather than resent, because
        // `secs` has to say how long this client has been asking — see
        // [`Client::discover`] for the server that is waiting to hear a big
        // enough number. Not once a REQUEST is pending: rebuilding then would
        // drop the offer that was taken and start the conversation over,
        // against a server that has already reserved an address for it.
        if client.state() != State::Requesting {
            pending = client.discover(elapsed);
        }
        transport.send(&pending)?;

        // A deadline, not a timeout per datagram. Each receive gets what is
        // left of the attempt's budget, so that a segment carrying DHCP for
        // other machines — the case [`MAX_DATAGRAMS`] exists for — cannot
        // make one attempt cost MAX_DATAGRAMS attempts' worth of waiting.
        // Handing the whole budget to every receive instead lets each ignored
        // datagram start the clock again, and the fifteen seconds this module
        // documents becomes four minutes on exactly the kind of network that
        // is hardest to get an address on.
        let deadline = transport.now() + FIRST_WAIT * 2u32.pow(attempt);
        for _ in 0..MAX_DATAGRAMS {
            let remaining = deadline.saturating_duration_since(transport.now());
            if remaining.is_zero() {
                break;
            }
            let Some(datagram) = transport.receive(remaining)? else {
                break;
            };
            match client.receive(&datagram) {
                Step::Ignore => continue,
                Step::Send(next) => {
                    pending = next;
                    transport.send(&pending)?;
                }
                Step::Bound(lease) => return Ok(*lease),
                Step::Refused(why) => {
                    return Err(io::Error::new(io::ErrorKind::ConnectionRefused, why))
                }
            }
        }
        // Read off the clock rather than added up from the schedule. An
        // attempt cut short by [`MAX_DATAGRAMS`] spends less than its budget,
        // and a `secs` counted off the schedule anyway would tell a server the
        // client has been asking for longer than it has, and would end below
        // with an error naming fifteen seconds nobody waited.
        elapsed =
            u16::try_from(transport.now().duration_since(started).as_secs()).unwrap_or(u16::MAX);
    }

    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!("no DHCP server answered in {elapsed} seconds"),
    ))
}

/// A transaction id.
///
/// Its whole job is to tell this client's conversation from the one happening
/// on the same broadcast address for the machine next to it, so it has to be
/// unpredictable to nobody and merely unlikely to collide. The clock, the
/// process id and the MAC give that without a random number generator, which
/// this tree does not have and would not take a crate for.
pub fn transaction_id(mac: [u8; 6]) -> u32 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.subsec_nanos() ^ since.as_secs() as u32)
        .unwrap_or(0);
    let tail = u32::from_be_bytes([mac[2], mac[3], mac[4], mac[5]]);
    nanos.rotate_left(7) ^ tail ^ std::process::id().rotate_left(19)
}

/// `aa:bb:cc:dd:ee:ff` as six bytes.
///
/// sysfs hands the MAC over as text in `/sys/class/net/<name>/address`, and it
/// is the one thing a DHCP client cannot make up: it is this client's name as
/// far as every server on the segment is concerned.
pub fn hardware_address(text: &str) -> Option<[u8; 6]> {
    let mut mac = [0u8; 6];
    let mut parts = text.trim().split(':');
    for byte in mac.iter_mut() {
        *byte = u8::from_str_radix(parts.next()?, 16).ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    // The all-zero address belongs to interfaces that have not got one, and a
    // DISCOVER sent from it is a DISCOVER no server can answer.
    if mac == [0; 6] {
        return None;
    }
    Some(mac)
}

/// The Ethernet address a DISCOVER is sent to: everybody on the link.
const BROADCAST_MAC: [u8; 6] = [0xff; 6];

/// `ETH_P_IP`, the ethertype of a frame carrying IPv4.
///
/// Written out rather than taken from `libc` because everything from here to
/// [`broadcast_frame`] is bytes rather than Linux, and is tested on any
/// machine that can run `cargo test`.
const ETHERTYPE_IPV4: u16 = 0x0800;

/// IPv4's protocol number for UDP.
const PROTOCOL_UDP: u8 = 17;

/// The TTL on the way out.
///
/// A limited broadcast never leaves the link — no router forwards
/// 255.255.255.255 — so any value at all would arrive, and this could be 1.
/// It is 64 because that is what the Linux stack puts on an ordinary datagram
/// and what `udhcpc` puts on this one, and a packet that looks like every
/// other packet is a packet nothing on the way has a special opinion about.
const IPV4_TTL: u8 = 64;

/// Destination, source, ethertype.
const ETHERNET_HEADER: usize = 14;
/// An IPv4 header with no options on it.
const IPV4_HEADER: usize = 20;
/// Two ports, a length and a checksum.
const UDP_HEADER: usize = 8;

/// One DHCP message wrapped in the Ethernet, IPv4 and UDP headers that a
/// client with no address has to write for itself.
///
/// This is the header construction `docs/design/network.md` decided not to
/// write, under "A UDP socket, not a raw one", and what changed is that the
/// machine has two links now and had one then. `SO_BINDTODEVICE` fixes which
/// interface a broadcast leaves by; it does not fix the address the kernel
/// puts in the IP header. For a limited broadcast out of a link that has no
/// address the kernel goes looking — `inet_select_addr` falls back to the
/// first suitable address on *any* device — so a laptop with a cable in it
/// sends the radio's DISCOVER from the cable's address. RFC 2131 §4.1 says a
/// client in SELECTING sends from `0.0.0.0`; so does every other client; and
/// `0.0.0.0:68 -> 255.255.255.255:67` is how the rule that admits DHCP
/// through a firewall is usually written. There is no socket option that
/// makes a UDP socket send from an address the host does not have, so the
/// header is written here instead.
///
/// Only the sending side is built. Replies still arrive on the UDP socket,
/// because [`FLAG_BROADCAST`] asks the server to broadcast them and a
/// broadcast to 255.255.255.255:68 lands in a socket bound to `0.0.0.0:68`
/// like any other datagram. Receiving is the half with no source address to
/// get wrong, and leaving it alone keeps everything the kernel's UDP stack
/// does for free: the checksum verified rather than computed, the port
/// demultiplexed, anything that arrived in pieces put back together.
///
/// The payload goes in untouched. These are forty-two bytes in front of what
/// [`Message::encode`] already built, which is what
/// `a_frame_is_the_payload_with_forty_two_bytes_in_front_of_it` says on the
/// bytes rather than in prose.
pub fn broadcast_frame(mac: [u8; 6], payload: &[u8]) -> Vec<u8> {
    let datagram = udp_datagram(
        Ipv4Addr::UNSPECIFIED,
        Ipv4Addr::BROADCAST,
        CLIENT_PORT,
        SERVER_PORT,
        payload,
    );
    let header = ipv4_header(Ipv4Addr::UNSPECIFIED, Ipv4Addr::BROADCAST, datagram.len());

    let mut frame = Vec::with_capacity(ETHERNET_HEADER + header.len() + datagram.len());
    frame.extend_from_slice(&BROADCAST_MAC);
    frame.extend_from_slice(&mac);
    frame.extend_from_slice(&ETHERTYPE_IPV4.to_be_bytes());
    frame.extend_from_slice(&header);
    frame.extend_from_slice(&datagram);
    frame
}

/// The IPv4 header over a UDP datagram of `length` bytes.
///
/// The fields left zero, and why, because a header that is mostly zeroes is
/// the part that looks unfinished:
///
/// - **DSCP and ECN.** Nothing on this link is doing differentiated services,
///   and a DISCOVER asking for low delay is asking a switch that has one
///   queue.
/// - **Identification.** It exists so that fragments of one datagram can be
///   told from fragments of another, and this datagram is 328 bytes: under
///   every MTU there has ever been, including the 576 bytes RFC 791 makes
///   everything accept.
/// - **Flags and fragment offset.** Zero, which is "may fragment, and this is
///   the first fragment". Setting `DF` would be truer to the line above and
///   would also ask any hop that could not forward it to send an ICMP back to
///   `0.0.0.0`, which is not an address anything can answer.
///
/// The header checksum covers this header alone, which is why it can be
/// finished here without ever seeing the payload.
fn ipv4_header(source: Ipv4Addr, destination: Ipv4Addr, length: usize) -> [u8; IPV4_HEADER] {
    let mut header = [0u8; IPV4_HEADER];
    // Version 4 in the high nibble; in the low one the header's own length in
    // 32 bit words, which is five because there are no options on it.
    header[0] = 0x45;
    // The clamp is unreachable from this module — the caller's datagram is
    // 328 bytes — and is here because a total length that has wrapped round
    // does not describe a shorter packet, it describes a different one.
    let total = u16::try_from(IPV4_HEADER + length).unwrap_or(u16::MAX);
    header[2..4].copy_from_slice(&total.to_be_bytes());
    header[8] = IPV4_TTL;
    header[9] = PROTOCOL_UDP;
    // 10..12 is the checksum itself, which is computed over a header carrying
    // zero in it and written in afterwards.
    header[12..16].copy_from_slice(&source.octets());
    header[16..20].copy_from_slice(&destination.octets());
    let sum = checksum(&[&header]);
    header[10..12].copy_from_slice(&sum.to_be_bytes());
    header
}

/// A UDP datagram with its checksum computed.
///
/// IPv4 allows that checksum to be zero, meaning "not computed", and it is
/// tempting to leave it there: the payload is about to be checked by
/// [`Message::parse`] anyway, and the frame is not going further than the
/// wire it is put on. It is computed because the machines this whole change
/// is for are the ones that look — the firewall that inspects DHCP before it
/// forwards it, the server that logs what it dropped — and a datagram with no
/// checksum is one some of them have already decided not to trust.
///
/// A checksum that comes out zero goes on the wire as `0xffff`. The two are
/// the same number in ones complement arithmetic, and only one of them means
/// "there is no checksum here".
fn udp_datagram(
    source: Ipv4Addr,
    destination: Ipv4Addr,
    from: u16,
    to: u16,
    payload: &[u8],
) -> Vec<u8> {
    // Same clamp, same unreachability, same reason as in `ipv4_header`.
    let length = u16::try_from(UDP_HEADER + payload.len()).unwrap_or(u16::MAX);
    let mut datagram = Vec::with_capacity(UDP_HEADER + payload.len());
    datagram.extend_from_slice(&from.to_be_bytes());
    datagram.extend_from_slice(&to.to_be_bytes());
    datagram.extend_from_slice(&length.to_be_bytes());
    // The checksum's own two bytes, zero while it is being computed over
    // them.
    datagram.extend_from_slice(&[0, 0]);
    datagram.extend_from_slice(payload);

    let pseudo = pseudo_header(source, destination, length);
    let sum = match checksum(&[&pseudo, &datagram]) {
        0 => 0xffff,
        sum => sum,
    };
    datagram[6..8].copy_from_slice(&sum.to_be_bytes());
    datagram
}

/// The twelve bytes RFC 768 checksums and nobody sends.
///
/// The addresses out of the IP header, the protocol, and the UDP length
/// again. It is there so that a datagram delivered to the wrong host, or
/// handed to the wrong protocol, fails its checksum instead of arriving —
/// which is worth more here than usual, because the two addresses it covers
/// are `0.0.0.0` and `255.255.255.255` and they are the entire claim this
/// change makes about the packet.
fn pseudo_header(source: Ipv4Addr, destination: Ipv4Addr, length: u16) -> [u8; 12] {
    let mut pseudo = [0u8; 12];
    pseudo[0..4].copy_from_slice(&source.octets());
    pseudo[4..8].copy_from_slice(&destination.octets());
    // pseudo[8] is a zero byte that exists to line the protocol up.
    pseudo[9] = PROTOCOL_UDP;
    pseudo[10..12].copy_from_slice(&length.to_be_bytes());
    pseudo
}

/// RFC 1071's internet checksum, over as many runs of bytes as it is handed.
///
/// One function for the IPv4 header's checksum and for the UDP one, because
/// they are the same arithmetic over different bytes: add the big-endian 16
/// bit words up, fold the carries back in until none is left, complement.
/// Taking a list of runs is what lets the UDP checksum cover a pseudo-header
/// that is never sent followed by a datagram that is, without joining 340
/// bytes together first.
///
/// A run of odd length does not pad itself. The byte left over is carried
/// into the next run and pairs with its first byte, because the checksum is
/// defined over the concatenation and not over the pieces; only a byte left
/// over at the very end is the high half of a word whose low half is zero.
/// Splitting the same bytes differently therefore cannot change the answer,
/// which is what `the_checksum_does_not_care_where_the_runs_are_split` is
/// for.
fn checksum(runs: &[&[u8]]) -> u16 {
    let mut total: u32 = 0;
    // The high half of a word whose low half is in the next run.
    let mut half: Option<u8> = None;

    for run in runs {
        let mut bytes = *run;
        if let Some(high) = half.take() {
            let Some((low, rest)) = bytes.split_first() else {
                // An empty run in the middle: the byte is still waiting.
                half = Some(high);
                continue;
            };
            total += u32::from(u16::from_be_bytes([high, *low]));
            bytes = rest;
        }
        let mut words = bytes.chunks_exact(2);
        for word in &mut words {
            total += u32::from(u16::from_be_bytes([word[0], word[1]]));
        }
        if let [tail] = words.remainder() {
            half = Some(*tail);
        }
    }
    if let Some(high) = half {
        total += u32::from(u16::from_be_bytes([high, 0]));
    }

    // Twice is enough — the first fold cannot carry more than once — but the
    // loop says what it is doing without asking anyone to prove that.
    while total > 0xffff {
        total = (total & 0xffff) + (total >> 16);
    }
    !(total as u16)
}

/// A DHCP server that is not on a network.
///
/// Answers out of a small table: one address, one netmask, and whatever else
/// it was told to send. Public rather than `#[cfg(test)]` for the same reason
/// [`crate::net::RecordingKernel`] is — it is how the acquisition path is
/// driven from a test in another crate, and how a UI can be shown getting an
/// address on a machine that has no network at all.
#[derive(Debug, Clone)]
pub struct FakeServer {
    pub address: Ipv4Addr,
    pub netmask: Ipv4Addr,
    pub router: Option<Ipv4Addr>,
    pub resolvers: Vec<Ipv4Addr>,
    pub domain: Option<String>,
    /// What the server calls itself in option 54.
    pub identity: Ipv4Addr,
    pub lease_seconds: u32,
    /// Answer the REQUEST with a NAK carrying this reason, instead of an ACK.
    pub refuse: Option<String>,
    /// Send no subnet mask, which is how the "unusable ACK" path is reached.
    pub omit_netmask: bool,
    /// Every datagram the client put on the wire, in order.
    pub sent: Vec<Vec<u8>>,
    /// Datagrams handed over before any real reply: other machines' traffic,
    /// for the tests about what a broadcast socket overhears.
    pub noise: Vec<Vec<u8>>,
    /// How long each datagram takes to turn up, on the fake clock.
    ///
    /// Zero — the default — is a server that answers the instant it is asked,
    /// which is what every test about the handshake itself wants. Setting it
    /// is how a test asks what a busy segment does to the retry schedule, and
    /// it costs nothing: the delay is only ever added to [`Transport::now`],
    /// so a simulated minute of other people's DHCP goes by between two
    /// instructions.
    pub arrival: Duration,
    /// Replies waiting to be read.
    pending: Vec<Vec<u8>>,
    /// The fake clock: a real [`Instant`] to hang it off, because one cannot
    /// be built from nothing, and the simulated time added to it.
    started: Instant,
    spent: Duration,
}

impl FakeServer {
    /// A server on 192.168.1.1 handing out 192.168.1.50.
    pub fn new() -> FakeServer {
        FakeServer {
            address: Ipv4Addr::new(192, 168, 1, 50),
            netmask: Ipv4Addr::new(255, 255, 255, 0),
            router: Some(Ipv4Addr::new(192, 168, 1, 1)),
            resolvers: vec![Ipv4Addr::new(192, 168, 1, 1)],
            domain: Some("example.lan".to_string()),
            identity: Ipv4Addr::new(192, 168, 1, 1),
            lease_seconds: 3600,
            refuse: None,
            omit_netmask: false,
            sent: Vec::new(),
            noise: Vec::new(),
            arrival: Duration::ZERO,
            pending: Vec::new(),
            started: Instant::now(),
            spent: Duration::ZERO,
        }
    }

    /// A server that answers the REQUEST with a NAK.
    pub fn refusing(mut self, why: &str) -> FakeServer {
        self.refuse = Some(why.to_string());
        self
    }

    /// How much of the fake clock the conversation so far has used.
    ///
    /// What a test asserts against when the question is how long an
    /// acquisition took, rather than what it said.
    pub fn spent(&self) -> Duration {
        self.spent
    }

    /// What the client sent, parsed.
    pub fn seen(&self) -> Vec<Message> {
        self.sent
            .iter()
            .filter_map(|datagram| Message::parse(datagram))
            .collect()
    }

    /// The types the client sent, in order: `["DISCOVER", "REQUEST"]`.
    pub fn transcript(&self) -> Vec<&'static str> {
        self.seen()
            .iter()
            .filter_map(|message| message.message_type())
            .map(MessageType::as_str)
            .collect()
    }

    /// The reply to one request, or `None` for a request it does not answer.
    fn reply_to(&self, request: &Message) -> Option<Vec<u8>> {
        let answer = match request.message_type()? {
            MessageType::Discover => MessageType::Offer,
            MessageType::Request if self.refuse.is_some() => MessageType::Nak,
            MessageType::Request => MessageType::Ack,
            _ => return None,
        };

        let mut message = Message::request(request.mac, request.xid);
        message.op = BOOTREPLY;
        message.set_message_type(answer);
        message.set(option::SERVER_ID, self.identity.octets().to_vec());

        if answer == MessageType::Nak {
            let why = self.refuse.clone().unwrap_or_default();
            message.set(option::MESSAGE, why.into_bytes());
            return Some(message.encode());
        }

        message.yiaddr = self.address;
        if !self.omit_netmask {
            message.set(option::SUBNET_MASK, self.netmask.octets().to_vec());
        }
        if let Some(router) = self.router {
            message.set(option::ROUTER, router.octets().to_vec());
        }
        if !self.resolvers.is_empty() {
            let body = self
                .resolvers
                .iter()
                .flat_map(|resolver| resolver.octets())
                .collect();
            message.set(option::DNS_SERVER, body);
        }
        if let Some(domain) = &self.domain {
            message.set(option::DOMAIN_NAME, domain.clone().into_bytes());
        }
        message.set(
            option::LEASE_TIME,
            self.lease_seconds.to_be_bytes().to_vec(),
        );
        Some(message.encode())
    }
}

impl Default for FakeServer {
    fn default() -> FakeServer {
        FakeServer::new()
    }
}

impl Transport for FakeServer {
    fn send(&mut self, datagram: &[u8]) -> io::Result<()> {
        self.sent.push(datagram.to_vec());
        if let Some(request) = Message::parse(datagram) {
            if let Some(reply) = self.reply_to(&request) {
                self.pending.push(reply);
            }
        }
        Ok(())
    }

    fn receive(&mut self, timeout: Duration) -> io::Result<Option<Vec<u8>>> {
        let waiting = !self.noise.is_empty() || !self.pending.is_empty();
        if !waiting || self.arrival > timeout {
            // Nothing waiting is the timeout expiring, not an error: a
            // network with no server on it is an ordinary network. So is a
            // datagram still in flight when the caller gives up on it, and
            // both cost the caller the whole of what it was prepared to wait,
            // which is the part a deadline is made of.
            self.spent += timeout;
            return Ok(None);
        }
        self.spent += self.arrival;
        if !self.noise.is_empty() {
            return Ok(Some(self.noise.remove(0)));
        }
        Ok(Some(self.pending.remove(0)))
    }

    fn now(&self) -> Instant {
        self.started + self.spent
    }
}

/// The real transport: a raw socket to send on, a UDP socket to hear on,
/// both pinned to one interface.
///
/// Two sockets rather than one because the two directions have different
/// problems. A send out of a link with no address has to say `0.0.0.0` and no
/// UDP socket can be made to — see [`broadcast_frame`], which is where the
/// headers for it are built — so sends go out of an `AF_PACKET` socket below
/// the IP stack. Replies are broadcast to 255.255.255.255:68 by a server
/// honouring [`FLAG_BROADCAST`], which is an ordinary datagram arriving at an
/// ordinary socket, so receives stay where they were.
///
/// Three socket options on the UDP half, none of which it can do without:
///
/// - `SO_REUSEADDR`, because 68 is a fixed well known port and a second
///   attempt while a previous socket is still being torn down would otherwise
///   fail to bind. It has to be set before the bind, which is why the
///   descriptor is built with libc and handed to [`std::net::UdpSocket`]
///   afterwards rather than the other way round.
/// - `SO_BROADCAST`, without which the kernel refuses to send to
///   255.255.255.255 at all. It refuses with `EACCES`, which reads like a
///   privilege problem and is not one. Nothing is sent from this socket any
///   more, but a socket that cannot send to the broadcast address cannot be
///   asked to later, and the option costs one syscall.
/// - `SO_BINDTODEVICE`, which is why this is per interface. A machine asking
///   for its first address has no route to anywhere, so nothing in the
///   routing table can tell the kernel which interface a datagram should
///   leave by or arrive on; naming the device is the only way to say "ask on
///   this cable". On the receiving side it is also what stops the answer to
///   the radio's DISCOVER being read as the cable's.
#[cfg(target_os = "linux")]
pub struct BroadcastSocket {
    socket: std::net::UdpSocket,
    raw: PacketSocket,
    interface: String,
}

#[cfg(target_os = "linux")]
impl BroadcastSocket {
    /// Open both sockets for one interface.
    ///
    /// Takes the MAC because the Ethernet header built for every send needs
    /// it, and the caller — [`acquire_on`] — has already had to read it out
    /// of sysfs to put in `chaddr`. Reading it again here, out of the kernel
    /// this time, would be a second answer to a question that already has
    /// one, and the two disagreeing is how a server ends up keying a lease on
    /// a client identifier that belongs to nobody.
    ///
    /// # When it fails
    ///
    /// Everything here wants privilege, and it all wants the same privilege:
    /// `SO_BINDTODEVICE` wants `CAP_NET_RAW`, an `AF_PACKET` socket wants
    /// `CAP_NET_RAW`, and binding port 68 wants `CAP_NET_BIND_SERVICE`. The
    /// compositor is root today and has all of them. If it is ever not, this
    /// fails at the first of the three and says so, exactly as it did before
    /// the raw socket was added — the new socket cannot be the thing that
    /// takes DHCP away from an unprivileged process, because that process
    /// could not have bound port 68 either.
    ///
    /// There is no fall back to sending on the UDP socket when the raw one
    /// cannot be opened, and that is deliberate: sending on the UDP socket
    /// *is* the bug. A fallback would put the wrong source address back on
    /// the wire on exactly the machines nobody was watching, which is how it
    /// got onto the wire in the first place. The error goes straight up
    /// instead, through [`acquire_on`], to the status bar that asked.
    pub fn bind(interface: &str, mac: [u8; 6]) -> io::Result<BroadcastSocket> {
        use std::os::fd::FromRawFd;

        if interface.is_empty() || interface.len() >= libc::IF_NAMESIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{interface:?} is not an interface name"),
            ));
        }

        // SAFETY: a plain socket call.
        let fd = unsafe {
            libc::socket(
                libc::AF_INET,
                libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
                libc::IPPROTO_UDP,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the descriptor was just created and is held nowhere else,
        // so the socket owns it from here. It is wrapped before the
        // setsockopt calls rather than after them so that the early returns
        // below close it rather than leaking it.
        let socket = unsafe { std::net::UdpSocket::from_raw_fd(fd) };

        set_flag(fd, libc::SO_REUSEADDR, true)?;
        set_flag(fd, libc::SO_BROADCAST, true)?;
        bind_to_device(fd, interface)?;
        bind_to_client_port(fd)?;

        // Last, so that the ordinary failures above — a name that is not an
        // interface, port 68 already held — are still the ones reported
        // first, and so that a machine without `AF_PACKET` in its kernel
        // fails with a message about `AF_PACKET` and not about DHCP.
        let raw = PacketSocket::open(interface, mac)?;

        Ok(BroadcastSocket {
            socket,
            raw,
            interface: interface.to_string(),
        })
    }

    pub fn interface(&self) -> &str {
        &self.interface
    }
}

/// The send half: `AF_PACKET`/`SOCK_RAW`, which takes a whole Ethernet frame
/// and puts it on the wire as it is.
///
/// `SOCK_RAW` rather than `SOCK_DGRAM`: the datagram flavour would have the
/// kernel write the Ethernet header, which is the one header out of the three
/// that has nothing wrong with it, and would leave the IP one — the whole
/// point — still unwritten.
#[cfg(target_os = "linux")]
struct PacketSocket {
    fd: libc::c_int,
    /// The interface, by the index `sockaddr_ll` names it with. A packet
    /// socket has no `SO_BINDTODEVICE`; this field is how it is per
    /// interface.
    index: libc::c_int,
    /// The source address of every frame this sends: the link's own.
    mac: [u8; 6],
}

#[cfg(target_os = "linux")]
impl PacketSocket {
    fn open(interface: &str, mac: [u8; 6]) -> io::Result<PacketSocket> {
        let index = interface_index(interface)?;

        // The protocol is in network order because it is an ethertype on the
        // wire and not a number to the kernel. SAFETY: a plain socket call.
        let fd = unsafe {
            libc::socket(
                libc::AF_PACKET,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                libc::c_int::from(ETHERTYPE_IPV4.to_be()),
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let socket = PacketSocket { fd, index, mac };

        // Bound with a protocol of zero, which `packet(7)` defines as "and
        // receive nothing". The socket is created with the ethertype it
        // sends, because that is what a reader of `socket(2)` expects to see,
        // and then narrowed here: nothing ever reads this descriptor — the
        // replies come up the UDP socket — and a socket left listening for
        // `ETH_P_IP` is a copy of every IP frame on a busy link going into a
        // buffer behind nobody, for the fifteen seconds an acquisition lasts.
        let address = link_address(index, 0, None);
        // SAFETY: address is a correctly shaped sockaddr_ll and outlives the
        // call; the length handed over is its own. The descriptor is owned by
        // `socket`, which closes it if this returns early.
        let bound = unsafe {
            libc::bind(
                socket.fd,
                std::ptr::addr_of!(address).cast(),
                std::mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t,
            )
        };
        if bound < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(socket)
    }

    /// Put one DHCP payload on the wire, wrapped.
    fn send(&self, payload: &[u8]) -> io::Result<()> {
        let frame = broadcast_frame(self.mac, payload);
        let address = link_address(self.index, ETHERTYPE_IPV4.to_be(), Some(BROADCAST_MAC));

        // SAFETY: the frame and the address both outlive the call and both
        // lengths handed over are their own.
        let sent = unsafe {
            libc::sendto(
                self.fd,
                frame.as_ptr().cast(),
                frame.len(),
                // No MSG_DONTWAIT: a packet socket blocks only for as long as
                // the driver's queue is full, which is microseconds, and a
                // DISCOVER dropped for EAGAIN would be indistinguishable from
                // a network with no server on it.
                0,
                std::ptr::addr_of!(address).cast(),
                std::mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t,
            )
        };
        if sent < 0 {
            return Err(io::Error::last_os_error());
        }
        // A packet socket writes a frame whole or not at all, so a short
        // count is not something to retry from — it is something that should
        // not have happened, and half a DISCOVER on the wire is worth saying
        // out loud rather than waiting fifteen seconds for.
        if sent as usize != frame.len() {
            return Err(io::Error::other(format!(
                "{} of {} bytes went out",
                sent,
                frame.len()
            )));
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
impl Drop for PacketSocket {
    fn drop(&mut self) {
        // SAFETY: the descriptor is ours and is not used again.
        unsafe { libc::close(self.fd) };
    }
}

/// The `sockaddr_ll` that names an interface, and optionally a hardware
/// address on it.
///
/// One function for both uses because the struct is the same either way: the
/// bind wants the index and nothing else, and the `sendto` wants the index,
/// the ethertype and the destination MAC. Every other field is for the
/// receiving side and is ignored on a send.
#[cfg(target_os = "linux")]
fn link_address(
    index: libc::c_int,
    protocol: u16,
    destination: Option<[u8; 6]>,
) -> libc::sockaddr_ll {
    // SAFETY: sockaddr_ll is plain data with no invalid bit patterns, and all
    // zeroes is the value the fields not set below are meant to have.
    let mut address: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
    address.sll_family = libc::AF_PACKET as libc::sa_family_t;
    address.sll_protocol = protocol;
    address.sll_ifindex = index;
    if let Some(mac) = destination {
        address.sll_halen = mac.len() as libc::c_uchar;
        address.sll_addr[..mac.len()].copy_from_slice(&mac);
    }
    address
}

/// An interface's index, which is what `sockaddr_ll` names it by.
///
/// `if_nametoindex` answers zero for a name that is not an interface and does
/// not always set `errno` doing it, so the zero is turned into an error here
/// rather than handed to `bind` as index zero — which means "every
/// interface", and is the one answer that would put a DISCOVER out of the
/// wrong link silently.
#[cfg(target_os = "linux")]
fn interface_index(interface: &str) -> io::Result<libc::c_int> {
    let name = std::ffi::CString::new(interface).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "interface name has a NUL in it",
        )
    })?;
    // SAFETY: the string is NUL terminated by CString and outlives the call.
    let index = unsafe { libc::if_nametoindex(name.as_ptr()) };
    if index == 0 {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{interface} is not an interface the kernel knows"),
        ));
    }
    Ok(index as libc::c_int)
}

/// `setsockopt` for the options that are one boolean.
#[cfg(target_os = "linux")]
fn set_flag(fd: libc::c_int, option: libc::c_int, on: bool) -> io::Result<()> {
    let value: libc::c_int = i32::from(on);
    // SAFETY: value outlives the call, and the length handed over is its own.
    let result = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            option,
            std::ptr::addr_of!(value).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// `SO_BINDTODEVICE`, which takes the interface name as bytes rather than as
/// an index.
#[cfg(target_os = "linux")]
fn bind_to_device(fd: libc::c_int, interface: &str) -> io::Result<()> {
    let name = interface.as_bytes();
    // SAFETY: the pointer and length describe the same slice, which outlives
    // the call; the caller bounded the length against IF_NAMESIZE.
    let result = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_BINDTODEVICE,
            name.as_ptr().cast(),
            name.len() as libc::socklen_t,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// `bind(2)` to `0.0.0.0:68` on a descriptor that already carries the socket
/// options above.
///
/// [`std::net::UdpSocket::bind`] makes a socket of its own, which would throw
/// those options away, so the bind is done by hand on this one.
#[cfg(target_os = "linux")]
fn bind_to_client_port(fd: libc::c_int) -> io::Result<()> {
    let address = libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: CLIENT_PORT.to_be(),
        sin_addr: libc::in_addr { s_addr: 0 },
        sin_zero: [0; 8],
    };
    // SAFETY: address is a correctly shaped sockaddr_in and outlives the
    // call; the length handed over is its own.
    let result = unsafe {
        libc::bind(
            fd,
            std::ptr::addr_of!(address).cast(),
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
impl Transport for BroadcastSocket {
    fn send(&mut self, datagram: &[u8]) -> io::Result<()> {
        // Down the raw socket, not `self.socket.send_to`: the UDP send is the
        // one that would put another link's address in the IP header.
        self.raw.send(datagram)
    }

    fn receive(&mut self, timeout: Duration) -> io::Result<Option<Vec<u8>>> {
        self.socket.set_read_timeout(Some(timeout))?;
        let mut buffer = vec![0u8; MAX_MESSAGE_SIZE as usize];
        match self.socket.recv_from(&mut buffer) {
            Ok((length, _)) => {
                buffer.truncate(length);
                Ok(Some(buffer))
            }
            // A read timeout is `EAGAIN` on Linux and `ETIMEDOUT` elsewhere,
            // and both mean the same ordinary thing: nobody has answered yet.
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }
}

/// Get an address on one interface, over a real socket.
///
/// The only function in this module that touches the machine, and kept to
/// three lines so that everything worth being wrong about is above it and
/// under test.
#[cfg(target_os = "linux")]
pub fn acquire_on(interface: &str, mac: [u8; 6]) -> io::Result<Lease> {
    let mut socket = BroadcastSocket::bind(interface, mac)?;
    acquire(&mut socket, mac, transaction_id(mac))
}

#[cfg(not(target_os = "linux"))]
pub fn acquire_on(interface: &str, _mac: [u8; 6]) -> io::Result<Lease> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        format!("DHCP on {interface} needs a Linux kernel"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
    const OTHER_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0xaa, 0xbb, 0xcc];
    const XID: u32 = 0x0badcafe;

    /// A server's reply, built the way a server builds one, for the tests
    /// that hand the client a single datagram rather than run a handshake.
    fn reply(kind: MessageType, mac: [u8; 6], xid: u32) -> Message {
        let mut message = Message::request(mac, xid);
        message.op = BOOTREPLY;
        message.set_message_type(kind);
        message.yiaddr = Ipv4Addr::new(192, 168, 1, 50);
        message.set(option::SUBNET_MASK, [255, 255, 255, 0].to_vec());
        message.set(option::SERVER_ID, [192, 168, 1, 1].to_vec());
        message
    }

    #[test]
    fn a_message_survives_being_written_and_read_back() {
        let mut message = Message::request(MAC, XID);
        message.secs = 7;
        message.ciaddr = Ipv4Addr::new(10, 0, 0, 9);
        message.set_message_type(MessageType::Discover);
        message.set(option::DOMAIN_NAME, b"example.lan".to_vec());

        let parsed = Message::parse(&message.encode()).expect("a message");
        assert_eq!(parsed.xid, XID);
        assert_eq!(parsed.secs, 7);
        assert_eq!(parsed.mac, MAC);
        assert_eq!(parsed.ciaddr, Ipv4Addr::new(10, 0, 0, 9));
        assert_eq!(parsed.message_type(), Some(MessageType::Discover));
        assert_eq!(
            parsed.text(option::DOMAIN_NAME).as_deref(),
            Some("example.lan")
        );
        assert!(
            parsed.broadcast,
            "a client with no address asks to be shouted at"
        );
    }

    #[test]
    fn every_datagram_is_padded_up_to_what_a_relay_will_forward() {
        let mut message = Message::request(MAC, XID);
        message.set_message_type(MessageType::Discover);
        assert_eq!(message.encode().len(), MIN_PAYLOAD);
    }

    #[test]
    fn a_datagram_without_the_magic_cookie_is_not_a_dhcp_message() {
        let mut bytes = Message::request(MAC, XID).encode();
        bytes[FIXED_LEN] ^= 0xff;
        assert_eq!(Message::parse(&bytes), None);
    }

    #[test]
    fn a_datagram_too_short_to_hold_the_header_is_refused_rather_than_panicking() {
        for length in 0..MIN_LEN {
            assert_eq!(Message::parse(&vec![0u8; length]), None, "at {length}");
        }
    }

    #[test]
    fn padding_is_skipped_and_end_stops_the_walk() {
        // PAD, a real option, PAD, END, then rubbish that must not be read.
        let body = [
            option::PAD,
            option::MESSAGE_TYPE,
            1,
            MessageType::Offer.code(),
            option::PAD,
            option::END,
            option::ROUTER,
            4,
            9,
            9,
            9,
            9,
        ];
        let options = parse_options(&body);
        assert_eq!(options, vec![(option::MESSAGE_TYPE, vec![2])]);
    }

    #[test]
    fn an_option_cut_off_by_a_truncated_datagram_keeps_the_ones_before_it() {
        // A message type, then a router option claiming four bytes with two.
        let body = [
            option::MESSAGE_TYPE,
            1,
            MessageType::Ack.code(),
            option::ROUTER,
            4,
            192,
            168,
        ];
        assert_eq!(
            parse_options(&body),
            vec![(option::MESSAGE_TYPE, vec![MessageType::Ack.code()])]
        );
    }

    #[test]
    fn a_resolver_list_keeps_the_addresses_that_arrived_whole() {
        let mut message = Message::request(MAC, XID);
        message.set(option::DNS_SERVER, vec![1, 1, 1, 1, 8, 8, 8, 8, 9, 9]);
        assert_eq!(
            message.ipv4_list(option::DNS_SERVER),
            vec![Ipv4Addr::new(1, 1, 1, 1), Ipv4Addr::new(8, 8, 8, 8)]
        );
    }

    #[test]
    fn a_domain_name_with_a_trailing_nul_on_it_is_still_a_domain_name() {
        let mut message = Message::request(MAC, XID);
        message.set(option::DOMAIN_NAME, b"example.lan\0".to_vec());
        assert_eq!(
            message.text(option::DOMAIN_NAME).as_deref(),
            Some("example.lan")
        );
    }

    #[test]
    fn the_whole_handshake_is_discover_offer_request_ack() {
        let mut server = FakeServer::new();
        let lease = acquire(&mut server, MAC, XID).expect("a lease");

        assert_eq!(server.transcript(), vec!["DISCOVER", "REQUEST"]);
        assert_eq!(lease.address, Ipv4Addr::new(192, 168, 1, 50));
        assert_eq!(lease.prefix_len(), 24);
        assert_eq!(lease.router, Some(Ipv4Addr::new(192, 168, 1, 1)));
        assert_eq!(lease.resolvers, vec![Ipv4Addr::new(192, 168, 1, 1)]);
        assert_eq!(lease.domain.as_deref(), Some("example.lan"));
        assert_eq!(lease.server, Ipv4Addr::new(192, 168, 1, 1));
        assert_eq!(lease.seconds, Some(3600));
    }

    #[test]
    fn the_request_names_the_address_and_the_server_that_offered_it() {
        let mut server = FakeServer::new();
        acquire(&mut server, MAC, XID).expect("a lease");

        let request = server.seen().pop().expect("a request");
        assert_eq!(request.message_type(), Some(MessageType::Request));
        assert_eq!(
            request.ipv4(option::REQUESTED_ADDRESS),
            Some(Ipv4Addr::new(192, 168, 1, 50))
        );
        assert_eq!(
            request.ipv4(option::SERVER_ID),
            Some(Ipv4Addr::new(192, 168, 1, 1))
        );
        // Still zero: a client in SELECTING does not hold the address yet,
        // and a REQUEST with ciaddr set means something else entirely.
        assert!(request.ciaddr.is_unspecified());
    }

    #[test]
    fn every_message_carries_the_same_client_identifier() {
        let mut server = FakeServer::new();
        acquire(&mut server, MAC, XID).expect("a lease");

        let identifiers: Vec<Option<Vec<u8>>> = server
            .seen()
            .iter()
            .map(|message| message.option(option::CLIENT_ID).map(<[u8]>::to_vec))
            .collect();
        let expected = Some(vec![ETHERNET, 0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);
        assert_eq!(identifiers, vec![expected.clone(), expected]);
    }

    #[test]
    fn every_message_asks_for_the_four_options_a_machine_needs() {
        let mut server = FakeServer::new();
        acquire(&mut server, MAC, XID).expect("a lease");
        for message in server.seen() {
            assert_eq!(message.option(option::PARAMETER_LIST), Some(PARAMETERS));
        }
    }

    #[test]
    fn a_reply_for_another_transaction_is_not_this_machines_address() {
        let mut client = Client::new(MAC, XID);
        client.discover(0);
        let stranger = reply(MessageType::Offer, MAC, XID ^ 0xffff).encode();
        assert_eq!(client.receive(&stranger), Step::Ignore);
        assert_eq!(client.state(), State::Selecting, "still waiting");
    }

    #[test]
    fn a_reply_for_another_machine_is_not_this_machines_address() {
        let mut client = Client::new(MAC, XID);
        client.discover(0);
        let stranger = reply(MessageType::Offer, OTHER_MAC, XID).encode();
        assert_eq!(client.receive(&stranger), Step::Ignore);
        assert_eq!(client.state(), State::Selecting);
    }

    #[test]
    fn overheard_traffic_does_not_stop_the_handshake_finishing() {
        let mut server = FakeServer::new();
        // Two conversations belonging to the machine next door, and one
        // datagram that is not DHCP at all, all arriving first.
        server.noise = vec![
            reply(MessageType::Offer, OTHER_MAC, XID).encode(),
            reply(MessageType::Ack, MAC, XID ^ 1).encode(),
            b"not a dhcp datagram".to_vec(),
        ];
        let lease = acquire(&mut server, MAC, XID).expect("a lease");
        assert_eq!(lease.address, Ipv4Addr::new(192, 168, 1, 50));
    }

    #[test]
    fn a_nak_is_reported_with_the_reason_the_server_gave() {
        let mut server = FakeServer::new().refusing("no free leases");
        let error = acquire(&mut server, MAC, XID).expect_err("refused");
        assert_eq!(error.kind(), io::ErrorKind::ConnectionRefused);
        assert!(
            error.to_string().contains("no free leases"),
            "lost the reason: {error}"
        );
        // One discover and one request: a refusal is not retried.
        assert_eq!(server.transcript(), vec!["DISCOVER", "REQUEST"]);
    }

    #[test]
    fn an_ack_with_no_subnet_mask_is_refused_rather_than_guessed_at() {
        let mut server = FakeServer::new();
        server.omit_netmask = true;
        let error = acquire(&mut server, MAC, XID).expect_err("no mask");
        assert!(
            error.to_string().contains("subnet mask"),
            "unhelpful: {error}"
        );
    }

    #[test]
    fn a_mask_with_a_hole_in_it_is_not_a_mask() {
        assert!(is_contiguous_mask(Ipv4Addr::new(255, 255, 255, 0)));
        assert!(is_contiguous_mask(Ipv4Addr::new(255, 255, 255, 255)));
        assert!(is_contiguous_mask(Ipv4Addr::new(0, 0, 0, 0)));
        assert!(!is_contiguous_mask(Ipv4Addr::new(255, 255, 0, 255)));
        assert!(!is_contiguous_mask(Ipv4Addr::new(255, 0, 255, 0)));
    }

    #[test]
    fn an_ack_for_an_address_that_was_never_offered_is_ignored() {
        let mut client = Client::new(MAC, XID);
        client.discover(0);
        let offer = reply(MessageType::Offer, MAC, XID).encode();
        assert!(matches!(client.receive(&offer), Step::Send(_)));

        let mut ack = reply(MessageType::Ack, MAC, XID);
        ack.yiaddr = Ipv4Addr::new(192, 168, 1, 99);
        assert_eq!(client.receive(&ack.encode()), Step::Ignore);
        assert_eq!(client.state(), State::Requesting, "still waiting for ours");
    }

    #[test]
    fn a_second_servers_offer_arriving_late_is_dropped() {
        let mut client = Client::new(MAC, XID);
        client.discover(0);
        let offer = reply(MessageType::Offer, MAC, XID).encode();
        assert!(matches!(client.receive(&offer), Step::Send(_)));

        let mut second = reply(MessageType::Offer, MAC, XID);
        second.yiaddr = Ipv4Addr::new(10, 0, 0, 5);
        assert_eq!(client.receive(&second.encode()), Step::Ignore);

        let ack = reply(MessageType::Ack, MAC, XID).encode();
        assert!(matches!(client.receive(&ack), Step::Bound(_)));
    }

    #[test]
    fn a_network_with_no_server_on_it_gives_up_rather_than_waiting_forever() {
        /// A network where nothing ever answers.
        struct Silence {
            sent: usize,
        }

        impl Transport for Silence {
            fn send(&mut self, _datagram: &[u8]) -> io::Result<()> {
                self.sent += 1;
                Ok(())
            }

            fn receive(&mut self, _timeout: Duration) -> io::Result<Option<Vec<u8>>> {
                Ok(None)
            }
        }

        let mut silence = Silence { sent: 0 };
        let error = acquire(&mut silence, MAC, XID).expect_err("nobody answered");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(silence.sent, ATTEMPTS as usize, "one send per attempt");
    }

    /// A segment where nothing that arrives is ours: offers for the machine
    /// next door, half a second apart, far more of them than any attempt can
    /// read. Nothing here can finish a handshake, so the only things that can
    /// end an attempt are the clock and [`MAX_DATAGRAMS`].
    fn a_segment_nobody_answers_on() -> FakeServer {
        let mut server = FakeServer::new();
        server.arrival = Duration::from_millis(500);
        server.noise = (1..=128)
            .map(|n| reply(MessageType::Offer, OTHER_MAC, XID ^ n).encode())
            .collect();
        server
    }

    #[test]
    fn a_segment_full_of_other_machines_traffic_does_not_extend_the_budget() {
        let mut server = a_segment_nobody_answers_on();
        let error = acquire(&mut server, MAC, XID).expect_err("nobody answered");

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        // 1 + 2 + 4 + 8, and not a second more however much arrives in the
        // meantime. Every one of those datagrams is dropped, and a dropped
        // datagram used to buy the attempt another whole wait of its own.
        assert_eq!(server.spent(), Duration::from_secs(15));
        assert_eq!(
            server.transcript().len(),
            ATTEMPTS as usize,
            "still one send per attempt"
        );
    }

    #[test]
    fn a_retransmitted_discover_says_how_long_the_client_has_been_asking() {
        let mut server = a_segment_nobody_answers_on();
        acquire(&mut server, MAC, XID).expect_err("nobody answered");

        let sent = server.seen();
        assert_eq!(
            sent.iter()
                .filter_map(|message| message.message_type())
                .collect::<Vec<_>>(),
            vec![MessageType::Discover; ATTEMPTS as usize],
            "no offer was ever taken, so every send is a DISCOVER"
        );
        // The schedule this client keeps, read off the clock: it asks, waits
        // a second, asks again, waits two. A backup server that is told
        // nothing but zero never decides its turn has come.
        assert_eq!(
            sent.iter().map(|message| message.secs).collect::<Vec<_>>(),
            vec![0, 1, 3, 7]
        );
    }

    #[test]
    fn a_request_carries_the_clients_own_count_rather_than_the_servers_zero() {
        let mut client = Client::new(MAC, XID);
        client.discover(9);

        let mut offer = reply(MessageType::Offer, MAC, XID);
        // What a conforming server sends: RFC 2131 has the server fill in a
        // zero here, so there is nothing in the offer worth echoing.
        offer.secs = 0;
        let Step::Send(request) = client.receive(&offer.encode()) else {
            panic!("the offer was not taken up");
        };

        let request = Message::parse(&request).expect("a request");
        assert_eq!(request.secs, 9);
    }

    #[test]
    fn a_lease_says_when_a_renewal_would_be_due() {
        let mut server = FakeServer::new();
        server.lease_seconds = 7200;
        let lease = acquire(&mut server, MAC, XID).expect("a lease");
        // No option 58, so half the lease, which is what RFC 2131 says T1 is.
        assert_eq!(lease.renew_after(), Some(Duration::from_secs(3600)));
    }

    #[test]
    fn an_infinite_lease_has_no_renewal_to_be_due() {
        let mut server = FakeServer::new();
        server.lease_seconds = u32::MAX;
        let lease = acquire(&mut server, MAC, XID).expect("a lease");
        assert_eq!(lease.seconds, None);
        assert_eq!(lease.renew_after(), None);
    }

    #[test]
    fn a_lease_writes_the_resolv_conf_it_implies() {
        let mut server = FakeServer::new();
        server.resolvers = vec![Ipv4Addr::new(1, 1, 1, 1), Ipv4Addr::new(8, 8, 8, 8)];
        let lease = acquire(&mut server, MAC, XID).expect("a lease");
        assert_eq!(
            lease.resolv_conf("eth0"),
            "# written by tOS from the DHCP lease on eth0\n\
             search example.lan\n\
             nameserver 1.1.1.1\n\
             nameserver 8.8.8.8\n"
        );
    }

    #[test]
    fn a_lease_with_no_resolvers_still_says_who_wrote_the_file() {
        let mut server = FakeServer::new();
        server.resolvers.clear();
        server.domain = None;
        let lease = acquire(&mut server, MAC, XID).expect("a lease");
        assert_eq!(
            lease.resolv_conf("eth0"),
            "# written by tOS from the DHCP lease on eth0\n"
        );
    }

    #[test]
    fn a_hardware_address_is_read_the_way_sysfs_writes_one() {
        assert_eq!(hardware_address("52:54:00:12:34:56"), Some(MAC));
        assert_eq!(hardware_address("52:54:00:12:34:56\n"), Some(MAC));
        assert_eq!(hardware_address("52:54:00:12:34"), None, "too short");
        assert_eq!(hardware_address("52:54:00:12:34:56:78"), None, "too long");
        assert_eq!(hardware_address("not a mac"), None);
        // What an interface with no address of its own reads as, which no
        // server could answer a DISCOVER from.
        assert_eq!(hardware_address("00:00:00:00:00:00"), None);
    }

    #[test]
    fn the_checksum_is_the_worked_example_in_rfc_1071() {
        // §3, "Numerical Examples". The bytes and the answer are both the
        // RFC's, so this is the arithmetic being checked against something
        // other than itself.
        let example: &[u8] = &[0x00, 0x01, 0xf2, 0x03, 0xf4, 0xf5, 0xf6, 0xf7];
        assert_eq!(checksum(&[example]), 0x220d);
    }

    #[test]
    fn the_last_byte_of_an_odd_run_is_the_high_half_of_a_word() {
        let odd: &[u8] = &[0x00, 0x01, 0xf2];
        // Padded on the right, which is what RFC 1071 says.
        assert_eq!(checksum(&[odd]), checksum(&[&[0x00, 0x01, 0xf2, 0x00]]));
        // Not on the left, which is the way to get this wrong that still
        // produces a plausible looking number.
        assert_ne!(checksum(&[odd]), checksum(&[&[0x00, 0x01, 0x00, 0xf2]]));
        // And not dropped: change the byte with no partner and the checksum
        // changes with it.
        assert_ne!(checksum(&[odd]), checksum(&[&[0x00, 0x01, 0xff]]));
    }

    #[test]
    fn the_checksum_does_not_care_where_the_runs_are_split() {
        // The UDP checksum covers a pseudo-header and a datagram handed over
        // separately, so a split that changed the answer would be a checksum
        // no receiver could reproduce.
        let whole: &[u8] = &[0x45, 0x00, 0x01, 0x48, 0x11, 0xff, 0x2a];
        let expected = checksum(&[whole]);
        for at in 0..=whole.len() {
            assert_eq!(checksum(&[&whole[..at], &whole[at..]]), expected, "at {at}");
        }
        // Including across a run with nothing in it, which is what an empty
        // DHCP payload would make the second half.
        assert_eq!(
            checksum(&[&whole[..3], &whole[3..3], &whole[3..]]),
            expected
        );
    }

    #[test]
    fn an_ipv4_header_is_the_twenty_bytes_a_capture_would_show() {
        let header = ipv4_header(
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::BROADCAST,
            UDP_HEADER + MIN_PAYLOAD,
        );
        assert_eq!(
            header,
            [
                0x45, 0x00, // IPv4, five words of header, no DSCP
                0x01, 0x48, // 328 bytes: 20 + 8 + 300
                0x00, 0x00, // no identification, because nothing fragments
                0x00, 0x00, // no flags, no fragment offset
                0x40, // TTL 64
                0x11, // UDP
                0x79, 0xa6, // the checksum over the rest of this
                0x00, 0x00, 0x00, 0x00, // from 0.0.0.0, which is the point
                0xff, 0xff, 0xff, 0xff, // to everybody
            ]
        );
    }

    #[test]
    fn a_receiver_checksumming_the_header_gets_zero() {
        // What the checksum is for: the same sum run over a header that has
        // a correct one in it cancels out. A receiver that gets anything
        // else drops the packet.
        for length in [UDP_HEADER, UDP_HEADER + MIN_PAYLOAD, 1500] {
            let header = ipv4_header(Ipv4Addr::UNSPECIFIED, Ipv4Addr::BROADCAST, length);
            assert_eq!(checksum(&[&header]), 0, "at {length}");
        }
    }

    #[test]
    fn a_receiver_checksumming_the_datagram_gets_zero_however_odd_the_payload() {
        for length in [0, 1, 3, 299, MIN_PAYLOAD] {
            let payload = vec![0xa5u8; length];
            let datagram = udp_datagram(
                Ipv4Addr::UNSPECIFIED,
                Ipv4Addr::BROADCAST,
                CLIENT_PORT,
                SERVER_PORT,
                &payload,
            );
            assert_eq!(datagram.len(), UDP_HEADER + length);
            assert_eq!(
                u16::from_be_bytes([datagram[4], datagram[5]]) as usize,
                datagram.len(),
                "the length field at {length}"
            );
            let pseudo = pseudo_header(
                Ipv4Addr::UNSPECIFIED,
                Ipv4Addr::BROADCAST,
                datagram.len() as u16,
            );
            assert_eq!(checksum(&[&pseudo, &datagram]), 0, "at {length}");
        }
    }

    #[test]
    fn a_checksum_that_comes_out_zero_is_sent_the_other_way_round() {
        // The one payload in this shape whose checksum is zero, found by
        // trying all 65536 of them. Zero on the wire means "there is no
        // checksum here", so it has to go as 0xffff, which is the same
        // number in ones complement and not that sentence.
        let datagram = udp_datagram(
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::BROADCAST,
            CLIENT_PORT,
            SERVER_PORT,
            &[0xff, 0x53],
        );
        assert_eq!(&datagram[6..8], &[0xff, 0xff]);
    }

    #[test]
    fn a_frame_is_the_bytes_a_capture_would_show() {
        // Three bytes of payload rather than a real DHCP message, so that
        // the whole frame fits in one assertion. The shorter payload is what
        // makes the two lengths and both checksums different from every
        // other test here.
        assert_eq!(
            broadcast_frame(MAC, &[0x00, 0x01, 0x02]),
            vec![
                0xff, 0xff, 0xff, 0xff, 0xff, 0xff, // to everybody
                0x52, 0x54, 0x00, 0x12, 0x34, 0x56, // from this link
                0x08, 0x00, // IPv4
                0x45, 0x00, 0x00, 0x1f, 0x00, 0x00, 0x00, 0x00, 0x40, 0x11, 0x7a, 0xcf, 0x00, 0x00,
                0x00, 0x00, 0xff, 0xff, 0xff, 0xff, // 31 bytes, 0.0.0.0 to broadcast
                0x00, 0x44, 0x00, 0x43, 0x00, 0x0b, 0xfd, 0x50, // 68 to 67, 11 bytes
                0x00, 0x01, 0x02, // the payload, untouched
            ]
        );
    }

    #[test]
    fn a_frame_is_the_payload_with_forty_two_bytes_in_front_of_it() {
        let mut client = Client::new(MAC, XID);
        let datagram = client.discover(0);
        let frame = broadcast_frame(MAC, &datagram);

        let headers = ETHERNET_HEADER + IPV4_HEADER + UDP_HEADER;
        assert_eq!(headers, 42);
        assert_eq!(
            &frame[headers..],
            &datagram[..],
            "the payload is not touched"
        );
        // The number in the capture in #156, which is the same DHCP message
        // this wraps: 14 + 20 + 8 + 300.
        assert_eq!(frame.len(), 342);
        // And a server reading the frame finds the message that went in.
        let message = Message::parse(&frame[headers..]).expect("a message");
        assert_eq!(message.message_type(), Some(MessageType::Discover));
        assert_eq!(message.mac, MAC);
        assert_eq!(message.xid, XID);
    }

    #[test]
    fn both_of_the_packets_a_client_sends_go_out_from_nothing_to_everybody() {
        let mut server = FakeServer::new();
        acquire(&mut server, MAC, XID).expect("a lease");
        assert_eq!(server.transcript(), vec!["DISCOVER", "REQUEST"]);

        for datagram in &server.sent {
            let frame = broadcast_frame(MAC, datagram);
            assert_eq!(&frame[0..6], &BROADCAST_MAC, "destination MAC");
            assert_eq!(&frame[6..12], &MAC, "source MAC");
            assert_eq!(&frame[12..14], &ETHERTYPE_IPV4.to_be_bytes());
            // The whole of #156 in one assertion: not the other link's
            // address, which is what SO_BINDTODEVICE left the kernel free to
            // put here.
            assert_eq!(&frame[26..30], &[0, 0, 0, 0], "source address");
            assert_eq!(&frame[30..34], &[255, 255, 255, 255], "destination");
            assert_eq!(&frame[34..36], &CLIENT_PORT.to_be_bytes(), "from port 68");
            assert_eq!(&frame[36..38], &SERVER_PORT.to_be_bytes(), "to port 67");
            assert_ne!(
                &frame[40..42],
                &[0, 0],
                "a checksum, not the absence of one"
            );
            assert_eq!(
                &frame[ETHERNET_HEADER + IPV4_HEADER + UDP_HEADER..],
                &datagram[..]
            );
        }
    }

    #[test]
    fn two_transaction_ids_from_the_same_machine_differ() {
        // Not a randomness test: it only has to be unlikely that two clients
        // on one segment collide, and the clock moving is most of that.
        let first = transaction_id(MAC);
        std::thread::sleep(Duration::from_millis(2));
        assert_ne!(first, transaction_id(MAC));
    }

    #[test]
    fn a_lease_describes_itself_the_way_a_notification_would_say_it() {
        let mut server = FakeServer::new();
        let lease = acquire(&mut server, MAC, XID).expect("a lease");
        assert_eq!(lease.describe(), "192.168.1.50/24 via 192.168.1.1");

        server.router = None;
        server.pending.clear();
        server.sent.clear();
        let lease = acquire(&mut server, MAC, XID).expect("a lease");
        assert_eq!(lease.describe(), "192.168.1.50/24");
    }
}
