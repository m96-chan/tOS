# Networking

Design for [#17](https://github.com/m96-chan/tOS/issues/17).

Three decisions, in the order they have to be made.

**Wired links get an address from a DHCP client written in tree, not from
`dhclient`.** `compositor/tos-system/src/net/dhcp.rs`.

**Wireless links will be joined through `wpa_supplicant`, driven over its
UNIX control socket, not through iwd and not through D-Bus.** Nothing is
built for it yet; it is gated on [#20](https://github.com/m96-chan/tOS/issues/20).

**Nothing scans for access points until the supplicant is there.**
`SIOCGIWSCAN` is not being used, and the reason is measured rather than
stylistic.

---

## What exists today

```text
net.rs      /sys/class/net + /proc/net/route + /proc/net/wireless, getifaddrs(3)
            reads: kind, operstate, carrier, admin_up, addresses, default
            route, gateway, associated SSID, signal
            writes: SIOCSIFFLAGS — one link, up or down
system.rs   Machine polls a Reading once a second; reading().link is the
            interface worth naming
```

That is a status bar and nothing more. A link can be switched on and it has
no address when it comes up, so switching it on accomplishes nothing a person
would notice. The rest of this document is about closing that gap.

---

## Wired: why a DHCP client rather than a DHCP client package

There is no `dhclient` on a tOS machine and there is nowhere obvious to put
one. The live ISO is a busybox initramfs; the installed system is the same
initramfs copied onto a disk. Even after #20 lands and both become a Debian
rootfs, adding `isc-dhcp-client` means adding a daemon, a configuration
language, a hook directory and a supervision story for something the
compositor has to be able to start and stop from a menu.

The protocol is smaller than that infrastructure. DHCPv4 is one packet shape,
four message types and a handshake that fits on a page:

```text
DISCOVER  broadcast, "is there a server"
OFFER     broadcast back, "here is an address you may have"
REQUEST   broadcast, "I am taking that one, from that server"
ACK       broadcast back, "it is yours for N seconds"
```

`tos-term` parses PNG and DEFLATE rather than take a crate for either, and
`lock.rs` implements SHA-512 crypt rather than link `libcrypt`. This is the
same call for the same reason: the thing being avoided is larger than the
thing being written, and what is written is testable in a way that a
subprocess is not.

### A UDP socket, not a raw one

This is the one decision in the DHCP client that could reasonably have gone
the other way.

A client in SELECTING has no address. A server is therefore entitled to
unicast its OFFER to the address it is about to hand out, which is an address
this machine does not have and whose ARP the kernel will not answer. `dhcpcd`,
`udhcpc` and `dhclient` all solve this with a raw `AF_PACKET` socket: they
build the Ethernet, IP and UDP headers by hand, compute the UDP checksum by
hand, and read frames below the IP stack.

RFC 2131 §4.1 has a bit for exactly this situation. Setting `BROADCAST` in the
`flags` field asks the server to broadcast its reply to 255.255.255.255:68,
which arrives in an ordinary `SOCK_DGRAM` socket bound to `0.0.0.0:68`. That
is what `dhcp.rs` does.

The cost is honest and bounded: against a server that ignores the flag, the
offer goes somewhere this client cannot hear, nothing arrives, and acquisition
ends in a timeout after fifteen seconds. The failure mode is *no address*,
never *a wrong address*. What the raw socket would buy is compatibility with a
class of server that is rare and out of spec; what it would cost is about two
hundred lines of header construction and checksum arithmetic whose only test
would be one that constructs the same bytes the code does.

Three socket options are not optional, and one of them is why this is a
per-interface operation at all:

- `SO_REUSEADDR`, set before the bind, which is why the descriptor is built
  with `libc::socket` and wrapped in `std::net::UdpSocket` afterwards.
- `SO_BROADCAST`, without which sending to 255.255.255.255 fails with
  `EACCES` — an error that reads like a permission problem and is not one.
- `SO_BINDTODEVICE`. A machine asking for its first address has no route to
  anywhere, so nothing in the routing table can tell the kernel which
  interface a broadcast should leave by. Naming the device is the only way to
  say "ask on this cable", and it is also what stops a laptop with a cable and
  a radio from configuring the wrong one.

### The seam, and what is actually tested

Everything above the wire is a function over bytes:

```text
Message::parse / Message::encode      no socket
Client::receive -> Step               no socket, no clock
Transport (2 methods)                 the socket
FakeServer: Transport                 a server that is not on a network
BroadcastSocket: Transport            the real one, Linux only
```

So the whole of DISCOVER → OFFER → REQUEST → ACK is exercised in `cargo test`
on a machine with no DHCP server, no privileges and no network. The branches
that are worth having and that would otherwise only be hit on somebody else's
network — a reply carrying another machine's transaction id, a reply carrying
another machine's MAC, a second server's offer arriving after the first was
taken up, a NAK, an ACK naming an address that was never offered, an ACK with
no netmask on it — are each a named test.

The retry schedule is *not* the RFC's. RFC 2131 asks for waits of 4, 8, 16 and
64 seconds, which is more than a minute of a menu that has gone quiet.
`dhcp.rs` waits 1, 2, 4 and 8 — fifteen seconds to conclude there is no
server, where a server that is there answers the first attempt in
milliseconds. The waiting is the receive timeout rather than a sleep, which is
also what keeps the tests instant.

### Applying the lease

The ACK is four numbers and they are applied through the same `net::Kernel`
seam as `SIOCSIFFLAGS`:

```text
lease.address, lease.netmask  ->  SIOCSIFADDR then SIOCSIFNETMASK
lease.router                  ->  SIOCDELRT then SIOCADDRT, 0.0.0.0/0
lease.resolvers, lease.domain ->  /etc/resolv.conf, through Sysfs
```

Three details that are not arbitrary:

- **Address before route.** `SIOCADDRT` for a gateway on a subnet this machine
  is not on yet answers `ENETUNREACH`. `RecordingKernel` writes its changes
  into one ordered list precisely so that a test can assert the order; three
  separate lists could not have.
- **Netmask with the address, always.** The kernel derives a classful mask the
  moment `SIOCSIFADDR` lands, so `10.0.0.5` alone gives the interface a `/8`.
  A missing option 1 in the ACK is refused rather than guessed at, because the
  symptom of a wrong netmask is "the network is slow", not "the netmask is
  wrong".
- **`SIOCDELRT` before `SIOCADDRT`, ignoring its result.** `SIOCADDRT` will
  not replace a route; it answers `EEXIST`. The ordinary case is that there
  was nothing to delete, which is `ESRCH`, and treating that as a failure
  would mean the first lease on a fresh machine could never be applied.

`/etc/resolv.conf` is overwritten rather than merged, and goes through
`Sysfs::write` so that a test can point it at a temporary directory rather
than at the resolver of the machine running the suite. The file has no syntax
for "these lines are mine and those are yours"; every other DHCP client on
Linux replaces it wholesale. The header line naming tOS and the interface is
the compromise, so that somebody who finds their own resolver gone can see who
took it.

### What is not built: renewal

A lease carries the seconds it is good for and `Lease::renew_after()` says
when T1 falls. Nothing wakes up at T1.

Acquisition is a thing somebody pressed a key for; renewal is a timer that
fires in four hours, and those are different pieces of machinery. What renewal
would take, when it is worth doing:

- a `Client` that starts in `Bound` rather than `Init` and **unicasts** a
  REQUEST to `Lease::server` with `ciaddr` set to the address already held.
  This is the one message in the protocol that is not broadcast, so
  `Transport` grows a `send_to`;
- a deadline on `Network`, folded into the wait the frame loop already does
  for `Machine::next_poll`, in the same way animation deadlines and the idle
  deadline already are;
- a fall back to a broadcast REBINDING at T2, and to a fresh DISCOVER when the
  lease finally expires.

Until then, a machine whose lease runs out keeps an address the server
believes is free. That is wrong, and it is quiet, and asking for DHCP again
from the network menu fixes it. It is the right thing to leave out of an issue
whose wired requirement is "DHCP on link-up is enough to start".

### What is not built: IPv6

Stateless autoconfiguration needs no client at all — the kernel does it from
router advertisements. DHCPv6 is a separate protocol with a separate packet
shape, and nobody whose first problem is getting a laptop onto a café network
is asking for it.

---

## Wi-Fi: wpa_supplicant, not iwd

The issue asks for a decision and a reason. The decision is wpa_supplicant,
and the reason is not about Wi-Fi.

### tOS has no D-Bus and is not going to grow one

`tos-system`'s module header says it out loud: "There is no D-Bus here, and no
system daemon to ask." Power is `reboot(2)` and `/sys/power/state`; audio is
ALSA ioctls on `/dev/snd/controlC0`; Bluetooth is an `AF_BLUETOOTH` socket.
Every one of those had a D-Bus answer available — `logind`, PulseAudio,
BlueZ — and every one of them was written against the kernel instead.

**iwd's only control interface is D-Bus.** `iwctl` is a D-Bus client.
`iwd` has no text protocol and no control socket; the `wpa_supplicant`
compatibility shim it ships (`iwd -i`) exists precisely because that is the
interface everything else speaks. Talking to iwd from tOS would mean
implementing a D-Bus client in tree: the SASL EXTERNAL handshake on
`/run/dbus/system_bus_socket`, the message header and body marshalling with
its alignment rules, signature parsing, `org.freedesktop.DBus.Properties`, and
`ObjectManager` to enumerate devices and networks. That is a protocol
implementation considerably larger than the DHCP client above, in support of a
daemon that is doing the interesting work anyway.

**wpa_supplicant has a control socket that is lines of text.** A UNIX datagram
socket at `/run/wpa_supplicant/<interface>`, and a request/reply protocol that
is legible from a shell:

```text
->  SCAN
<-  OK
<-  <3>CTRL-EVENT-SCAN-RESULTS            (unsolicited, on the same socket)
->  SCAN_RESULTS
<-  bssid / frequency / signal level / flags / ssid   (tab separated)
->  ADD_NETWORK
<-  0
->  SET_NETWORK 0 ssid "kitchen-table"
<-  OK
->  SET_NETWORK 0 psk "correct horse battery staple"
<-  OK
->  SELECT_NETWORK 0
<-  OK
->  STATUS
<-  wpa_state=COMPLETED ...
```

`std::os::unix::net::UnixDatagram` is in the standard library. The protocol is
text, so a `Supplicant` trait over it has a `RecordingSupplicant` that is a
table of canned replies, in exactly the shape `RecordingKernel` and
`FakeServer` already have — and the scan-result parser is a pure function over
a string, testable the way `parse_proc_net_route` is.

That is the whole argument. It is not that wpa_supplicant is better software
than iwd; by most measures it is not. It is that one of them can be spoken to
with the standard library and a seam, and the other cannot.

### The secondary reasons, which agree

- **Debian.** Both are in bookworm main. `wpasupplicant` is what `ifupdown`
  and NetworkManager already expect, which matters for a rootfs (#20) that a
  user is expected to be able to `apt install` things into afterwards.
- **The supplicant is not "shelling out".** The house rule is not to ask a
  subprocess for something the kernel will answer. The kernel will not do a
  WPA2 four-way handshake: EAPOL, PBKDF2 over the passphrase, the group key
  exchange and the PMKSA cache are a supplicant's job, and tOS is not going to
  implement EAPOL. This is the same category as the kernel itself — a thing
  underneath tOS that does work tOS does not want to do — not the same
  category as parsing `ip addr` output.
- **Size.** iwd is smaller and leans on the kernel's crypto where
  wpa_supplicant links OpenSSL and libnl. This is a real point in iwd's favour
  and it is the only one; it will be worth measuring when #20 makes the rootfs
  real, and it does not outweigh a D-Bus implementation.

### What would have to be true before any of it is written

#20 has to land. Until the rootfs is a Debian squashfs there is nowhere to put
a supplicant binary, nowhere to put `/etc/wpa_supplicant`, and no firmware for
the radio either. A supplicant client written before then would be untested
against a real supplicant and unrunnable on either image.

---

## Why there is no scan yet

The obvious thing to build in the meantime is a network list: `SIOCSIWSCAN` to
trigger a scan, `SIOCGIWSCAN` to collect the results. tOS already uses the
wireless-extensions compatibility layer for `SIOCGIWESSID`, so the precedent
is there. It is not being built, and the reason is specific.

`SIOCGIWSCAN` returns a packed stream of `struct iw_event`:

```c
struct iw_event { __u16 len; __u16 cmd; union iwreq_data u; };
struct iw_point { void __user *pointer; __u16 length; __u16 flags; };
```

An SSID arrives as an `iw_point`, and the kernel does not put the pointer on
the wire. `iwe_stream_add_point` copies the 4-byte header, then
`sizeof(struct iw_point) - IW_EV_POINT_OFF` bytes starting at
`&iwe->u + IW_EV_POINT_OFF`, then the payload. On a 64-bit userspace
`IW_EV_POINT_OFF` is `offsetof(iw_point, length)` = 8, and `sizeof(iw_point)`
is 12 rounded up to 16 for alignment — so the body is 8 bytes of which the
last 4 are structure padding, and the payload starts 12 bytes into the event.
On a 32-bit userspace the same arithmetic gives 4 and 8. The kernel's
`iw_handler.h` has a `IW_EV_COMPAT_*` family for the case where a 32-bit
process is talking to a 64-bit kernel, which is a third set of offsets again.

None of that can be verified from this side. A parser written to those numbers
would be tested against a byte stream constructed from the same numbers, which
proves the parser agrees with itself and nothing else. `net.rs` already
carries a comment about this exact hazard, on `parse_proc_net_route`: the
addresses in `/proc/net/route` are byte-swapped, "getting this the wrong way
round produces addresses that look entirely plausible, which is why it is
tested against captured text". A scan parser got wrong produces a list of
plausible-looking SSIDs made of the wrong bytes, and there is no captured text
to test it against without hardware.

Against that, what the list would be worth: `SIOCSIWSCAN` needs `CAP_NET_ADMIN`
to trigger, so the live ISO cannot run one anyway, and nothing in tOS can join
what it finds. A menu of networks where every row does nothing is worse than
no menu — it is a promise the system cannot keep.

So the honest surface today is the one that exists: the SSID a wireless
interface is *already* associated with, which `SIOCGIWESSID` gives for free
and which `Interface::summary` already shows. The link menu offers a wireless
interface a row that says joining is not yet possible and points here, rather
than leaving a wireless interface looking identical to a wired one.

When the supplicant lands, the scan comes with it — `SCAN_RESULTS` over the
control socket, tab-separated text, no offsets to guess.

---

## The control surface

A launcher-shaped menu, because the launcher's overlay is the compositor's one
modal surface and the power, Bluetooth and audio menus are meant to be the
same box over a different list.

```text
super+shift+n           OverlayKind::Networks   the interfaces
  -> choose a link      OverlayKind::Link       what can be done to it
       bring the link up / take the link down   SIOCSIFFLAGS
       ask for an address                       DHCP
       join a wireless network                  says why not, links here
```

Two levels rather than one, because a single list crossing interfaces with
actions is a list of names nobody reads. The chosen interface is held in
`Compositor::network_target` rather than inside the `OverlayKind` variant, so
that `OverlayKind` stays a `Copy` tag: it is copied out of the open overlay on
every keystroke that closes one, and the other five menus should not start
paying for a payload they have not got.

### Asking is explicit, and does not happen on its own

The issue's wording is "DHCP on link-up is enough to start", which is a
statement about scope rather than a trigger. Bringing a link up and asking it
for an address are two rows, not one, and nothing watches the poll for a
carrier appearing.

Doing it automatically is a policy, and it is the wrong one to take unasked.
A machine on a segment with no DHCP server would spend fifteen seconds per
link every time a cable moved; a machine given a static address by hand would
have it taken away by a cable being replugged; and a laptop with a cable and a
radio would race to configure both. When there is a reason to add it — the
obvious one is a first boot that should reach the network without anybody
learning a key binding — it is a carrier transition seen in `Machine::poll`
calling the same `request_address` the menu row calls, and the decision to
make is which links it is allowed to do it to, not how.

### The DHCP request is on a thread

A server that is there answers in milliseconds; a network with no server on it
is fifteen seconds. The frame loop cannot spend fifteen seconds anywhere — the
clock would stop, the cursor would stop blinking, and the keyboard would
appear to have died, on a machine whose owner has just been told something is
being asked for.

Only the waiting is on the thread. The ioctls that put the lease on the link
happen back on the thread that owns the `Machine`, so nothing is shared but
the answer, over an `mpsc::Receiver` polled from `Compositor::tick` — which
already runs at least once a second because the machine poll is on that
deadline. No new timer, no shared `Machine`, no lock.

---

## The live ISO, and everything that can be refused

The issue's last line is that the live ISO only needs status, and that
configuring a network may require an installed system. The compositor does not
know what live media is and is not going to learn; what it does instead is
make every privileged step report its own refusal.

```text
reading interfaces        /sys/class/net, getifaddrs(3)   no privilege
opening either menu       reading only                    no privilege
bring up / take down      SIOCSIFFLAGS                    CAP_NET_ADMIN
ask for an address        bind(68), SO_BINDTODEVICE       CAP_NET_BIND_SERVICE,
                                                          CAP_NET_RAW
                          SIOCSIFADDR, SIOCADDRT          CAP_NET_ADMIN
                          /etc/resolv.conf                write access
```

Every one of the privileged rows returns `io::Error` all the way up to
`Notifications::status`, in the words the kernel used. A refused
`SIOCSIFFLAGS` reads `tosfake0 up failed: Operation not permitted`. A DHCP
acquisition that got a lease it is not allowed to apply says both halves —
what was offered and what stopped it being taken — because "cannot take
192.168.1.50/24 via 192.168.1.1: Operation not permitted" tells you the
network is fine and the privileges are not, and "failed" does not.

Nothing panics and nothing silently does nothing, which is the actual
requirement: a machine where these calls fail is a machine that says so on the
status line.

---

## What landed and what did not

| | |
|---|---|
| Interface and link state in the status interface | already done by `system.rs`'s poll (#66) |
| Wired: DHCP on link-up | acquisition, address, netmask, route, `resolv.conf` |
| Wired: lease renewal | **not built** — see above for what it takes |
| Wi-Fi: supplicant decision | wpa_supplicant, recorded above |
| Wi-Fi: scan / join | **not built**, gated on #20; scan reasoned against above |
| TUI control surface | `super+shift+n`, two overlays |
| Live ISO needs only status | every privileged step reports its refusal |
