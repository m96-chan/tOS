# Networking

Design for [#17](https://github.com/m96-chan/tOS/issues/17).

Three decisions, in the order they have to be made.

**Wired links get an address from a DHCP client written in tree, not from
`dhclient`.** `compositor/tos-system/src/net/dhcp.rs`.

**Wireless links will be joined through `wpa_supplicant`, driven over its
UNIX control socket, not through iwd and not through D-Bus.** Nothing is
built for it yet. It was gated on [#20](https://github.com/m96-chan/tOS/issues/20),
which has landed: the rootfs is a Debian squashfs now, so there is somewhere to
put a supplicant and something to install one with. What is left is the client
and the firmware, not the place to keep them.

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

There is no `dhclient` on a tOS machine, and the argument for keeping it that
way outlived the reason it started. When this was written the live ISO was a
busybox initramfs and the installed system was that same initramfs copied onto
a disk, so there was nowhere to put one; since #20 both are a Debian rootfs and
`apt install isc-dhcp-client` would work. It is still not wanted, for the
second reason rather than the first: adding it means adding a daemon, a configuration
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

#20 has landed, which was the first condition: the rootfs is a Debian squashfs,
so there is somewhere to put a supplicant binary and `/etc/wpa_supplicant`, and
apt to put them there with. What is still missing is the firmware for the radio
— the image packs no firmware at all — and a machine with a real wireless
adapter to write the client against. A supplicant client written without one
would be untested against a real supplicant.

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

### Asking is explicit at the menu, and automatic for a wired link — [#124](https://github.com/m96-chan/tOS/issues/124)

The issue's wording is "DHCP on link-up is enough to start", which was read
here as a statement about scope rather than a trigger, and for a while nothing
watched the poll for a carrier appearing: bringing a link up and asking it for
an address were two rows a person pressed, and a machine nobody pressed them
on had no network.

What settled it was [#110](https://github.com/m96-chan/tOS/issues/110) landing.
A tOS machine can install `openssh-server` now, and it does everything right —
the unit is enabled, `sshd` is listening three seconds into the boot, nothing
has failed — on a machine with no address for anybody to reach it at. The only
way to one was to sit down at the console, log in, and press the two rows. A
machine that has to be physically logged into before it can be reached over
the network is not a machine anything can be served from, and a daemon with no
network is the same as no daemon.

So it happens by itself, in `tos-system`'s `net::auto`. The three objections
that held it back were real, and each one is a rule rather than a reason not
to:

| Objection | The rule that answers it |
|---|---|
| Fifteen seconds per link every time a cable moves | One acquisition per carrier, not per poll: a server that does not answer costs one conversation, on a thread, and the next one needs the cable pulled out and put back |
| A static address taken away by a replug | Nothing is ever done to a link that already holds a routable address, wherever it came from |
| A laptop racing to configure a cable and a radio | Wired links only — there is no supplicant, so a radio brought up has nothing to associate with — and one acquisition at a time |

The policy is separate from the mechanism, which is what makes it a table test
rather than a machine with two cards in it: `Autoconfigure::next` is handed the
interfaces as they were last read and answers with at most one step, and the
compositor carries it out through the same `Network::bring_up` and
`Compositor::request_address` the menu rows go through. There is one way to
get an address on this machine, and the automatic path is the menu with nobody
at it.

Two smaller decisions come with it. Bringing a link up is not announced —
nobody asked, so the address it leads to is the answer worth a line, and
`collect_address` says that one exactly as it does for a keystroke — while a
link the kernel *refuses* to bring up is announced, because that is the whole
reason the machine is not reachable and the only part of the path a person can
do anything about. And a blanked screen is looked at anyway, once a minute
instead of once a second: `Machine::poll` leaves a dark machine alone on the
grounds that there is nothing on the screen to be out of date, which is right
for a battery and wrong for the one thing here that is not about the screen at
all. A cable plugged into a server whose screen went dark ten minutes ago must
not wait for a keystroke nobody is coming to make. A minute rather than a
number of its own because the dark loop already wakes on that interval to read
its signal flags, so this costs no wakeup that was not going to happen.

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
| Wired: brought up and addressed at boot | `net::auto`, once per carrier, wired only (#124) |
| Wired: lease renewal | **not built** — see above for what it takes |
| Wi-Fi: supplicant decision | wpa_supplicant, recorded above |
| Wi-Fi: scan / join | **not built**; #20 landed, so what is left is firmware and a radio to test against |
| TUI control surface | `super+shift+n`, two overlays |
| Live ISO needs only status | every privileged step reports its refusal |

---

## The first time any of this ran — [#84](https://github.com/m96-chan/tOS/issues/84)

Everything above this line was designed, written and unit tested against a
`FakeServer`, and had never touched an interface, because a tOS VM had no
interface to touch. This section is the record of the first boot that did:
what was wrong, what it cost to fix, what was seen, and what is still only a
design. It is deliberately specific — the point of an experiment is that
somebody can tell whether it was really run.

Measured 2026-09-12, on the ISO built from this tree: Debian bookworm,
kernel 6.1.0-53-amd64, busybox 1.35.0.

### Two faults, stacked, and the second was only visible past the first

`iso/mkiso.sh` packed a module closure with nothing under `drivers/net` in it
at all — no `virtio_net`, no `e1000`, nothing. A VirtualBox VM given a NAT
adapter booted to `lo` alone, and `super+shift+n` did not open a menu: it put
`no wired or wireless interfaces` on the status bar, which is the compositor
correctly reporting that there was nothing to offer. The adapter was really
there. On that machine `/sys/bus/pci/devices/0000:00:03.0` was vendor `0x1af4`
device `0x1000`, class `0x020000`, and it had no `driver` symlink: a network
controller on the bus with nothing bound to it.

Packing the five drivers did not fix it. The next image still showed `lo`
alone, because **nothing on this machine ever asks for a module.** There is no
udev in the initramfs, and this kernel is built without `CONFIG_UEVENT_HELPER`
— `/sys/kernel/uevent_helper` does not exist, which is checkable from a pane
and was. The only thing that loads a module is the `modprobe` loop in
`iso/init`, so a module that is packed and not named there is dead weight.
Packing a driver and loading it are two separate edits in two separate files,
and the first one alone looks exactly like the bug it was meant to fix.

And then `modprobe virtio_net` by hand *still* produced no interface, which is
the part most worth writing down. `virtio_net` binds virtio devices, not PCI
ones. The PCI adapter needs `virtio_pci` bound to it before a virtio device
exists for `virtio_net` to claim, and `virtio_pci` — though it had been in the
packed list for a long time — had never been in `/init`'s loop either. Nothing
had ever needed it: the machines this image is booted on take their disk from
IDE and their screen from VGA or vmsvga, so the one virtio device that
mattered was the one nobody had. `modprobe virtio_pci`, and `eth0` appeared in
the same second, with `0000:00:03.0/driver` now pointing at `virtio-pci`.

### What the drivers cost

Five names — `virtio_net`, `e1000`, `e1000e`, `r8169`, `igb` — pull seven more
through `modprobe --show-depends`: `libphy`, `realtek`, `mdio_devres`,
`i2c-algo-bit`, `dca`, `failover`, `net_failover`. Twelve modules.

```text
initramfs.gz   25,925,814 -> 26,558,126     +632,312 bytes   +2.4%
tos-x86_64.iso 54,423,552 -> 55,054,336     +630,784 bytes
```

618 KiB of compressed initramfs for the ability to be on a network at all.
That is the argument for keeping the list a closure rather than taking
`drivers/net` entire, which is the same argument the display and storage lists
were already making, and the number is small enough that the next card
somebody needs is not worth agonising over.

No firmware came with any of it — `--no-install-recommends` keeps
`firmware-realtek` and friends off the image on purpose. The kernel package
says so at build time, warning about `rtl_nic/rtl8168*.fw` for `r8169`. A
Realtek card that needs its blob will get whatever its PHY does by default,
and that is untested here (see below).

### What was seen

Driven headless: VirtualBox with the serial console to a file, the compositor
menu driven by `VBoxManage controlvm keyboardputscancode`, and the screen read
back with `controlvm screenshotpng`. Command output from a pane was redirected
to `/dev/ttyS0`, because `Notifications::status` writes only to the screen —
there is no log file, no `/dev/kmsg`, nothing on serial — so the status bar is
readable only as a picture, and anything needing exact text has to be asked
for from a shell.

On a NAT `virtio-net` adapter, in order, all of it through the menu:

```text
super+shift+n            network:  eth0   wired  down
choose eth0              eth0  down:  bring the link up / ask for an address
bring the link up        eth0: <BROADCAST,MULTICAST,UP,LOWER_UP>
                         operstate up, carrier 1, 08:00:27:54:ee:7d
ask for an address       status bar, about two seconds later:
                         eth0 10.0.2.15/24 via 10.0.2.2
```

and on the machine afterwards, in `ip addr` and `ip route` form — which that
image could only print as `busybox ip`, because its rootfs carried no `ip` of
that name at all ([#97](https://github.com/m96-chan/tOS/issues/97), since
answered by putting `iproute2` and `procps` in the rootfs, so the same lines
come out of `ip` called by its own name now):

```text
inet 10.0.2.15/24 brd 10.0.2.255 scope global eth0
default via 10.0.2.2 dev eth0
10.0.2.0/24 dev eth0 scope link  src 10.0.2.15

/etc/resolv.conf:
# written by tOS from the DHCP lease on eth0
search tail33b9af.ts.net
nameserver 100.100.100.100
nameserver 0.100.100.100
```

The netmask is the one the server sent and not a classful guess — `/24` on a
`10.` address is precisely the case the "netmask with the address, always"
rule above was written for, and a `/8` here would have been the symptom it
predicts. Address before route, route installed, resolvers written.

Reachability from a pane: `ping -c 3 10.0.2.2` and `ping -c 2 1.1.1.1` both
0% loss, 0.14 ms to the gateway against 3.5 ms off-network, which is the shape
of a real round trip rather than a local one. `nslookup example.com` used
`100.100.100.100` — the resolver out of the lease, read from the file tOS
wrote — and got real answers back, so `/etc/resolv.conf` is not merely written
but usable.

The same run repeated on a NAT `82540EM` adapter, to exercise the other
emulation that was packed: `e1000` bound it, `eth0` appeared, and the menu
took it to the same `10.0.2.15/24 via 10.0.2.2`.

### The one cross-check worth having

`nameserver 0.100.100.100` in that file looks like a parser that lost a byte,
and the honest thing was to assume it was ours. It is not. busybox `udhcpc`,
run against the same server on the same machine, reports the identical pair:

```text
EV=bound ip=10.0.2.15 mask=255.255.255.0 router=10.0.2.2
         dns=[100.100.100.100 0.100.100.100] domain=[tail33b9af.ts.net]
         server=10.0.2.2 lease=86400
```

Every field agrees with what `dhcp.rs` produced. The malformed resolver is
VirtualBox's NAT, which passes the host's resolvers through and had an IPv6
one (`fd7a:115c:a1e0::53`, Tailscale) to fit into a four-byte DHCPv4 option 6.
So the first real-server check of the option parser is that it matches a
mature implementation byte for byte, including on a server's bad output —
which is a better result than a clean one would have been.

### The QEMU witness, and a capture

The same image under the `qemu-system-x86_64` line the CI smoke boot now uses
— `-netdev user -device virtio-net-pci` — reaches the compositor, and driving
the same four keystrokes over the QEMU monitor produces this on the netdev,
dumped with `filter-dump`:

```text
DHCP 68->67  DISCOVER  yiaddr=0.0.0.0
DHCP 67->68  OFFER     yiaddr=10.0.2.15
DHCP 68->67  REQUEST   yiaddr=0.0.0.0
DHCP 67->68  ACK       yiaddr=10.0.2.15
```

That is the handshake at the top of this document, on a wire, against a server
nobody in this repository wrote, captured rather than asserted. Two different
NAT implementations — VirtualBox's and QEMU's slirp — both answer the
`BROADCAST` flag, which is the bet "A UDP socket, not a raw one" made.

### What did not work

busybox `wget` segfaults. Every fetch: by hostname, by IP, and against an
address with nothing listening, always exit 139. Bare `wget` with no arguments
prints its usage and exits 1, so the applet is there and starts. Crashing
against an unreachable address means it dies before any connection succeeds,
which puts the fault in the busybox-static build on the image and not in the
network — `ping` and `nslookup`, which are the same binary, both work over the
same link. It wants its own issue; it is not evidence about anything above.

### Is NAT enough?

For this issue, yes, and it should stay the gate.

NAT proved the whole path end to end against two independent server
implementations: the interface exists and enumerates, `SIOCSIFFLAGS` brings
the link up, DISCOVER/OFFER/REQUEST/ACK completes, the options parse the same
as `udhcpc` parses them, the address and netmask and route land on a real
kernel in an order it accepts, `/etc/resolv.conf` is written and is usable,
and packets reach the internet. It is free, it is deterministic, it needs no
privilege on a CI runner, and it now runs on both witnesses.

What it does not exercise, stated so that nobody mistakes a green boot for
more than it is:

- **Renewal.** Still not built, and this run made that concrete rather than
  theoretical: the lease is 86400 seconds, T1 falls twelve hours in, and
  nothing wakes up. A boot that lasts a minute could never have shown it.
- **A server that ignores the broadcast flag.** Both NATs honour it, so the
  one acknowledged risk in the socket decision above remains untested by
  construction. Only an out-of-spec server can test it, and neither of these
  is one.
- **A slow or hostile server.** Both answered in milliseconds, so the 1/2/4/8
  retry schedule and the fifteen-second give-up never ran. Nor did NAK, nor a
  second server racing the first, nor an ACK for an address that was never
  offered — all of which have `FakeServer` tests and no field evidence.
- **Carrier transitions.** No cable was pulled; nothing here says what the
  poll does when a link goes away and comes back with a different lease.
- **Being addressable.** NAT is one-way. Nothing tOS does today wants an
  inbound connection, but a bridged adapter is the only thing that would show
  DHCP from a real server on a real segment, ARP from other machines, or a
  second client contending for the same pool.
- **Real hardware.** `e1000e`, `r8169` and `igb` were packed and have never
  bound to anything — there was no such card to boot on. Only `virtio_net`
  and `e1000` are known to work. `r8169` in particular is packed without its
  firmware, and whether that matters depends on a chip nobody here has.

The shape of the answer, then: NAT is the right gate because it is the one
that can run unattended on every build, and the list above is the argument for
a bridged rig rather than against this one. The overlap is not accidental —
almost everything NAT cannot show is something that is **not built yet**
(renewal above all). When renewal is written it will need a test rig that can
hold a lease and move a clock, and that is the moment to build the bridged
witness, not before.
