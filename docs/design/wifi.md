# Wi-Fi

**Issue #137.** Decided: the supplicant `docs/design/network.md` chose, driven
over its control socket by a client in `tos-system`; the wireless link joined
from the same menu the wired one is configured from; the radio's driver and
firmware carried in the rootfs and loaded by udev after the pivot; the wired
link preferred over the radio by route metric, and the radio used the moment
the cable is not.

`network.md` already argued *which* supplicant and *why not* D-Bus. Nothing
here reopens that. This is what is built on top of the decision, and every
section is a rule a test can be written against.

## The shape

```text
tos-system/net/wpa.rs      the client: one round trip on a UNIX datagram
                           socket, typed commands over it, parsers as pure
                           functions, and a RecordingSupplicant
tos-system/net/auto.rs     the policy grows one rule: a radio that is
                           associated and has no address asks for one
tos-system/net.rs          the default route gets a metric, chosen by the
                           kind of link it goes through
tos-compositor/wifi.rs     the state between keystrokes: the scan that is
                           running, the join that is waiting, and what to
                           say about either on the next tick
tos-compositor/compositor.rs
                           the join row does something; two overlays under
                           it — the networks in range, and a passphrase
iso/mkiso.sh               wpasupplicant, five firmware families, the
                           wireless driver closure in the rootfs, a unit,
                           a udev rule, one sysctl
```

## The client — `net::wpa`

### One seam, one method

```rust
pub trait Supplicant {
    /// One request, one reply. The reply is the text the supplicant sent,
    /// trailing newline removed, and nothing has been made of it yet.
    fn request(&mut self, command: &str) -> io::Result<String>;
}
```

That is the whole of what has to be faked. `SocketSupplicant` is the one that
talks: a `UnixDatagram` bound to `/run/tos/wpa-<pid>-<interface>` (a datagram
socket with no address of its own gets no reply, because the supplicant
answers with `sendto` on the sender's address), connected to
`/run/wpa_supplicant/<interface>`, with a read timeout of half a second so
that a supplicant that has died mid-conversation costs half a second and not
the session. The bound path is unlinked on drop. `RecordingSupplicant` is a
table of `(command, reply)` pairs and a transcript, in exactly the shape
`RecordingKernel` and `FakeServer` already have.

Everything typed sits on top of that method, generic over the trait, so that
every test of the typed layer runs against the table:

```rust
pub struct Client<S: Supplicant> { .. }

impl<S: Supplicant> Client<S> {
    pub fn ping(&mut self) -> io::Result<()>;                 // PING -> PONG
    pub fn scan(&mut self) -> io::Result<()>;                 // SCAN -> OK | FAIL-BUSY (also Ok)
    pub fn scan_results(&mut self) -> io::Result<Vec<Found>>; // SCAN_RESULTS
    pub fn status(&mut self) -> io::Result<Status>;           // STATUS
    pub fn networks(&mut self) -> io::Result<Vec<Known>>;     // LIST_NETWORKS
    pub fn add_network(&mut self) -> io::Result<u32>;         // ADD_NETWORK -> id
    pub fn set_ssid(&mut self, id: u32, ssid: &str) -> io::Result<()>;
    pub fn set_passphrase(&mut self, id: u32, passphrase: &str) -> io::Result<()>;
    pub fn set_open(&mut self, id: u32) -> io::Result<()>;    // key_mgmt NONE
    pub fn select(&mut self, id: u32) -> io::Result<()>;      // ENABLE_NETWORK, SELECT_NETWORK
    pub fn disable(&mut self, id: u32) -> io::Result<()>;     // DISABLE_NETWORK
    pub fn remove(&mut self, id: u32) -> io::Result<()>;      // REMOVE_NETWORK
    pub fn disconnect(&mut self) -> io::Result<()>;           // DISCONNECT
    pub fn save(&mut self) -> io::Result<()>;                 // SAVE_CONFIG
}
```

A reply of `FAIL` to anything that expected `OK` is an `io::Error` carrying
the command it was a reply to, so that "SET_NETWORK 0 psk: FAIL" reaches the
status line rather than "failed".

### What is parsed, and from what

Three parsers, each a pure function over a `&str`, each tested against text
captured from a running `wpa_supplicant` (the witness at the end of this
document is where the text comes from; the format below is what the
supplicant's own `ctrl_iface.c` writes and what `wpa_cli` reads).

**`SCAN_RESULTS`** — a header line, then one tab-separated line per BSS:

```text
bssid / frequency / signal level / flags / ssid
02:00:00:00:01:00	2412	-30	[WPA2-PSK-CCMP][ESS]	kitchen-table
02:00:00:00:02:00	5180	-52	[ESS]	cafe
```

```rust
pub struct Found {
    pub bssid: String,
    pub frequency_mhz: u32,
    pub signal_dbm: i32,
    pub security: Security,
    pub ssid: String,
}
pub enum Security { Open, Psk, Eap, Wep, Unknown }
```

`Security` is read out of the flags: `WPA2-PSK` or `WPA-PSK` or `SAE` in any
flag is `Psk`; `EAP` is `Eap`; `WEP` is `Wep`; a line whose only flags are
things like `[ESS]`, `[WPS]`, `[P2P]` is `Open`; anything else is `Unknown`,
which the menu offers a row for that says it cannot join it. An SSID is the
supplicant's printable rendering, which escapes bytes that are not printable
ASCII as `\xNN` — it is shown as sent and joined by the hex form below, so
nothing has to be unescaped. A line with fewer than five fields is skipped,
not an error: one malformed BSS is not a reason to lose the list. Results are
returned in the order sent, which is strongest signal first.

**`STATUS`** — `key=value` lines:

```text
bssid=02:00:00:00:01:00
freq=2412
ssid=kitchen-table
id=0
mode=station
pairwise_cipher=CCMP
group_cipher=CCMP
key_mgmt=WPA2-PSK
wpa_state=COMPLETED
address=02:00:00:00:00:00
```

```rust
pub struct Status { pub state: State, pub ssid: Option<String>, pub id: Option<u32> }
pub enum State {
    Disconnected, Inactive, Scanning, Authenticating, Associating,
    Associated, FourWayHandshake, GroupHandshake, Completed, Interface_disabled,
    Unknown(String),
}
```

`wpa_state` is the only field that decides anything; the rest is what the
status line says. A `STATUS` with no `wpa_state` line is `State::Unknown("")`.

**`LIST_NETWORKS`** — a header, then `id / ssid / bssid / flags`:

```text
network id / ssid / bssid / flags
0	kitchen-table	any	[CURRENT]
1	cafe	any	[DISABLED]
2	office	any	[TEMP-DISABLED]
```

```rust
pub struct Known { pub id: u32, pub ssid: String, pub current: bool,
                   pub disabled: bool, pub temp_disabled: bool }
```

`[TEMP-DISABLED]` is how a wrong passphrase looks from outside: the supplicant
tries the four-way handshake, fails it, and disables the network for a while
rather than trying forever. That flag, on the network that was just selected,
*is* "wrong passphrase" — there is no other way to learn it without attaching
to the event stream, and attaching is what is being avoided.

### What is sent, and how it is quoted

The SSID goes as hex — `SET_NETWORK 0 ssid 6b69746368656e2d7461626c65` —
never as a quoted string. The supplicant accepts both, and the quoted form
has quoting rules (a `"` inside, a backslash, a non-ASCII byte) that would
otherwise have to be implemented here and tested against the supplicant's
parser. Hex has none. It is also what makes a hidden network or an SSID with
a tab in it join at all.

The passphrase goes quoted, `SET_NETWORK 0 psk "correct horse battery
staple"`, because that is the form the supplicant derives a key from; the
unquoted form is a 64-digit hex key already derived. A passphrase is accepted
only if it is 8 to 63 characters of printable ASCII with no `"` in it, which
is what WPA2 allows and the whole of what the quoted form can carry
unambiguously; anything else is refused *here*, with a message saying which
rule it broke, before a byte reaches the supplicant. A 64-digit hex string is
sent unquoted as the key it is. An open network sets `key_mgmt NONE` and no
`psk`.

### Polling, not attaching

The control socket can `ATTACH` and receive unsolicited events —
`<3>CTRL-EVENT-SCAN-RESULTS`, `<3>CTRL-EVENT-CONNECTED` — on the same
socket, interleaved with replies. Not used. The compositor already wakes at
least once a second for the machine poll, and everything the events would say
can be asked for: a scan's results arrive by asking `SCAN_RESULTS` on the next
tick, and a join's outcome is `STATUS` reaching `COMPLETED` or `LIST_NETWORKS`
showing `[TEMP-DISABLED]`. One socket, one direction, no demultiplexing, and
a `RecordingSupplicant` that is a table rather than a script.

## The policy — `net::auto` and `Network::configure`

### One more rule

`Autoconfigure` stays what it is, with its fourth rule rewritten:

> **A radio is never brought up and never scanned by this.** The supplicant
> owns the link's administrative state and does its own scanning; a radio
> with no supplicant on it is a radio nothing here can do anything with.
> **A radio that is associated asks for an address**, once per association,
> exactly as a cable asks once per carrier — because association *is* carrier
> on a wireless link: `/sys/class/net/wlan0/carrier` reads `1` the moment the
> four-way handshake completes and `0` when it drops.

So `Step::Ask` is offered for `Kind::Wireless` on the same conditions as
`Kind::Wired` — up, carrying, no routable address, not already asked this
carrier — and `Step::BringUp` is offered for `Kind::Wired` only. "Never over
an address" and "one at a time" hold unchanged.

### Two default routes, and which one the kernel uses

A laptop with a cable in it and a radio associated has two links that could
carry the default route, and until now `Network::configure` set one with no
metric and would have got `EEXIST` setting the second. Now:

```rust
fn set_default_route(&mut self, interface: &str, gateway: Ipv4Addr, metric: u32)
    -> io::Result<()>;
```

on the `Kernel` seam, with `configure` choosing the metric by kind — **wired
100, wireless 600**, NetworkManager's numbers, so that `ip route` on a tOS
machine reads the way it does everywhere else and a lower number wins. Two
default routes with different metrics coexist; the kernel routes by the lower.
Setting a route that already exists with the same metric through the same
gateway is not an error here (`EEXIST` is absorbed), because a lease renewed
on the same link is the ordinary case and not a fault.

That leaves the cable being pulled: the wired route is still in the table,
still lower, and now goes nowhere. One sysctl is the answer and the image sets
it — `net.ipv4.conf.all.ignore_routes_with_linkdown = 1` (and `.default`),
which makes the kernel skip a route whose link has no carrier. Cable in, the
cable carries traffic; cable out, the radio does, in the same second; cable
back, the cable again. Nobody's address is touched, which is the rule that
mattered most.

## The compositor — `wifi.rs` and the menus

### The rows

The link menu for a wireless interface, which used to end in a row that said
"not yet":

```text
wlan0  kitchen-table  192.168.1.5/24         (associated, addressed)
  take the link down
  ask for an address
  leave kitchen-table          DISABLE_NETWORK id, DISCONNECT
  forget kitchen-table         REMOVE_NETWORK id, SAVE_CONFIG
  join a wireless network      -> OverlayKind::Wireless

wlan0  down                                  (not associated)
  bring the link up
  ask for an address
  join a wireless network      -> OverlayKind::Wireless

wlan0  down                                  (no supplicant socket)
  bring the link up
  ask for an address
  no supplicant on wlan0       says: "is wpasupplicant installed?"
```

`leave` and `forget` appear only when `STATUS` names a network (`id=`), and
name it. The last shape is the one a machine gets when `apt remove
wpasupplicant` has happened, or on the initramfs rescue session, and it says
so rather than offering a row that fails.

### The scan

Choosing `join a wireless network` opens `OverlayKind::Wireless` at once,
titled `wireless — scanning`, holding whatever `SCAN_RESULTS` already had
(the supplicant scans on its own and keeps the last results, so the list is
rarely empty even on the first open), and sends `SCAN`. On every tick while
the overlay is open, `SCAN_RESULTS` is asked again and the rows are replaced
with `set_items`, the way the Bluetooth menu takes in an inquiry — so a query
typed while the radio was listening survives the answer. The title drops
`— scanning` when the results change or after ten seconds, whichever is
first. `SCAN` is not repeated; the supplicant's own periodic scan keeps the
list fresh and a menu that hammers the radio is a menu that drains a battery.

One row per SSID, strongest first, duplicates (the same SSID on two bands or
two access points) folded into the strongest:

```text
kitchen-table       -30 dBm  WPA2
cafe                -52 dBm  open
office              -67 dBm  enterprise — cannot join
```

A BSS with an empty SSID (a hidden network) is not a row. Joining a hidden
network needs its name typed and is not in this milestone; the limit is
written down below rather than half built.

### The join

Choosing a row:

- `Security::Open` — joins at once.
- `Security::Psk` — opens `OverlayKind::Passphrase`, a prompt titled
  `kitchen-table — passphrase`, **masked**: the line shows one `•` per
  character, the way the lock screen does, because a passphrase typed in
  front of somebody is a passphrase. `Overlay::secret_prompt` is the same
  prompt with the query drawn masked; enter accepts, escape closes. The SSID
  being joined is held in `Compositor::wifi` beside the interface, not in the
  `OverlayKind`, for the reason `network_target` is held that way.
- `Security::Eap`, `Wep`, `Unknown` — the row says so and choosing it puts
  "cannot join <ssid>: enterprise networks are not supported" (or "WEP", or
  "unknown security") on the status line. A row that does nothing without
  saying why is the thing `network.md` refused to ship.

Joining is, in order and stopping at the first error: if `LIST_NETWORKS`
already has this SSID, its id is reused (a second `SET_NETWORK psk` on it
replaces the key, which is what retyping a passphrase means); otherwise
`ADD_NETWORK`. Then `SET_NETWORK id ssid <hex>`, `SET_NETWORK id psk "…"` or
`key_mgmt NONE`, `ENABLE_NETWORK id`, `SELECT_NETWORK id`, `SAVE_CONFIG`.
Every other known network is left exactly as it was — `SELECT_NETWORK`
disables the others for this association, which is the supplicant's own
behaviour and the right one. The status line says `joining kitchen-table`,
and `Compositor::wifi` remembers `(interface, id, ssid, deadline)`.

Then, on each tick until settled:

| seen | said | and |
|---|---|---|
| `STATUS` `wpa_state=COMPLETED` with that `id` | `joined kitchen-table` | done; `net::auto` sees carrier and asks for an address, and the lease is announced as it always is |
| `LIST_NETWORKS` shows that id `[TEMP-DISABLED]` | `kitchen-table: wrong passphrase` | `REMOVE_NETWORK id`, `SAVE_CONFIG`, so a wrong key is not kept and tried again at every boot |
| thirty seconds pass | `kitchen-table: could not join` | the network is left configured, because a network that was out of range is one the supplicant will join by itself when it is back |
| the socket errors | the error, named | done |

Nothing here is on a thread. Every request is a local datagram answered in
microseconds by a daemon on the same machine, with a half-second timeout
behind it for the day the daemon is gone; the waiting — for a scan to finish,
for a handshake to complete — is done by the tick that was going to happen
anyway.

### Where the supplicant comes from

`Compositor` holds a `wifi: Wifi`, and `Wifi` is built with an opener:

```rust
pub type Opener = Box<dyn FnMut(&str) -> io::Result<Box<dyn Supplicant>>>;
```

The default opens `SocketSupplicant` on the interface's socket; a test hands
in a closure returning a `RecordingSupplicant`, which is how the whole path —
menu, scan, passphrase, join, wrong passphrase, tick — runs with no radio and
no daemon, the way `Scan::spawn` takes a `Control`. A socket is opened when a
row needs it and closed when the menu closes; nothing is held open between
keystrokes, so a supplicant restarted under the session is simply talked to
again next time.

## The image — `iso/mkiso.sh`

### The driver is in the rootfs, and udev loads it

`iso/init`'s loop is "the last moment in the life of the machine when a
module can be loaded", because the rootfs carries no `/lib/modules`. That was
the right trade for a display and a disk: everything the machine has to have
to *reach* the rootfs is loaded before it is reached. A radio is not that. It
is wanted after the pivot, by a daemon that lives in the rootfs, and nothing
about the radio has to work for the rootfs to come up.

So the rootfs gets a `/lib/modules/<kver>` of its own, holding the wireless
closure and nothing else, with its `modules.dep` from `depmod`:

```text
cfg80211 mac80211
iwlwifi iwlmvm iwldvm                 Intel
rtw88_8821ce rtw88_8822be rtw88_8822ce rtw89_8852ae rtl8xxxu   Realtek
ath9k ath10k_pci ath11k_pci           Qualcomm / Atheros
brcmfmac                              Broadcom
mt7921e mt7921u                       MediaTek
mac80211_hwsim                        the witness
```

and udev — already in the rootfs, already started by systemd's
`sysinit.target` — loads whichever of them the bus has a device for, by
modalias, as it does on every Debian machine. The initramfs does not grow by
a byte, the rescue session (which has no supplicant either) is unchanged, and
the comment in `mkiso.sh` that says a pivoted machine can never modprobe again
is rewritten to say what is true now: it can load what the rootfs carries,
and the rootfs carries the hardware that is wanted after the pivot.

Firmware goes to `/lib/firmware` in the rootfs the same way, from Debian's
`non-free-firmware` component, which the rootfs's sources gain:

| package | of the squashfs | for |
|---|---|---|
| firmware-iwlwifi | 29.7 MB | Intel — most laptops |
| firmware-atheros | 22.6 MB | Qualcomm ath9k/ath10k/ath11k — most AMD laptops |
| firmware-misc-nonfree | 17.8 MB | MediaTek MT7921/MT7922 — newer AMD laptops (5.9 MB of it; bookworm has no `firmware-mediatek`, and the blobs `mt7921e` asks for by name are in this package) |
| firmware-brcm80211 | 10.4 MB | Broadcom — Macs, Raspberry Pi |
| firmware-realtek | 2.0 MB | Realtek — most cheap laptops and USB dongles |

**Measured, not estimated — and the estimate was wrong.** The first draft of
this document said "roughly 35 MB", which was the sum of the `.deb` sizes; a
`.deb` is xz and this squashfs is zstd, and the difference is 2.5×. Built and
counted (2026-09-15, kernel 6.1.0-53-amd64): the squashfs goes from
67,145,728 to 155,484,160 bytes and the ISO from 105,404,416 to
193,746,944 — **+88 MB, and the image nearly doubles**. The module tree is
3.9 MB of that and `wpasupplicant` 1.4 MB; the firmware is the rest.

All five anyway, decided with that number on the table rather than the wrong
one: a machine with no network cannot `apt install` the firmware that would
give it one, so the place to save this space is not here. The lever, should
it ever be pulled, is `firmware-misc-nonfree` — 17.8 MB carried for a 5.9 MB
`mediatek/` subtree — and `iso/README.md` says so next to the numbers.

### The supplicant is a unit, started by udev

```ini
# /etc/systemd/system/tos-supplicant@.service
[Unit]
Description=tOS wireless supplicant on %I
BindsTo=sys-subsystem-net-devices-%i.device
After=sys-subsystem-net-devices-%i.device

[Service]
Type=simple
ExecStartPre=/bin/sh -c 'test -f /etc/wpa_supplicant/tos-%I.conf || \
    printf "ctrl_interface=/run/wpa_supplicant\nupdate_config=1\n" \
    > /etc/wpa_supplicant/tos-%I.conf'
ExecStart=/sbin/wpa_supplicant -Dnl80211 -i%I -c/etc/wpa_supplicant/tos-%I.conf
Restart=on-failure
```

```text
# /etc/udev/rules.d/80-tos-wireless.rules
ACTION=="add", SUBSYSTEM=="net", ENV{DEVTYPE}=="wlan", TAG+="systemd", \
    ENV{SYSTEMD_WANTS}+="tos-supplicant@$name.service"
```

A unit rather than a child of the compositor, for the reason
`docs/design/init.md` gave for systemd: supervision is what an init is for,
and a supplicant that crashes is restarted by the thing that restarts things.
Instantiated from a udev rule rather than enabled by name, because the name
of the radio is the machine's to say. tOS's own unit rather than Debian's
`wpa_supplicant@.service`, because the config path and the control directory
are this design's and Debian's unit is written for `ifupdown`. `update_config=1`
is what makes `SAVE_CONFIG` write the joined network back to the file, and
the file is what makes the machine rejoin at the next boot without being
asked. On the live image `/etc` is a tmpfs overlay and the file is gone at
reboot, which is right for a live image. The installer copies `/etc` onto the
disk, so an installed machine keeps its networks.

The socket directory is root's. The compositor is root. The live image is
root. Nothing has to be a member of `netdev`.

**Debian's own `wpa_supplicant.service` is masked.** The package enables a
singleton beside the per-radio units, and its `ExecStart` is `wpa_supplicant
-u` — the D-Bus interface, on an image that ships no D-Bus — so it failed, in
red, on every boot (#140), before it had looked at any hardware. Masked
rather than given a drop-in without the `-u`, because a second supplicant
owning the same sockets under `/run/wpa_supplicant` is the thing this design
already decided it did not want.

**The witness is on the image.** `iso/wifi-witness.sh` is copied to
`/usr/share/tos/wifi-witness.sh`, because the machine it has to run on is one
that was booted from this image, and a witness a person has to retype into a
pane is one nobody runs. Off `PATH`, because it turns the machine into an
access point.

### The sysctl

```text
# /etc/sysctl.d/80-tos-net.conf
net.ipv4.conf.all.ignore_routes_with_linkdown = 1
net.ipv4.conf.default.ignore_routes_with_linkdown = 1
```

Applied by `systemd-sysctl` at boot. See "Two default routes" above.

## What is deliberately not in this milestone

- **Hidden networks.** Needs an SSID prompt and `scan_ssid=1`; the row is not
  there rather than there and broken.
- **Enterprise (EAP) networks.** Said as such in the list.
- **Lease renewal**, which `network.md` already lists as not built and which a
  radio makes no more or less urgent.
- **Removing a route when a cable is pulled.** The sysctl makes the kernel
  skip it, which is the behaviour wanted; the stale entry in `ip route` is
  cosmetic and stays.
- **A regulatory domain.** The supplicant and the kernel default to the
  world regulatory domain, which is the conservative one.
- **Signal in the status bar.** `Interface::summary` shows the SSID; a
  signal figure is a status-bar decision and not a networking one.

## The witness

Nothing above has met a radio. The witness is the QEMU boot the ISO workflow
already runs, plus `mac80211_hwsim`, which is a kernel module that makes
radios out of nothing and lets them hear each other. On the booted image,
from a pane:

```sh
modprobe mac80211_hwsim radios=2           # wlan0 and wlan1 appear; udev
                                           # starts tos-supplicant@wlan0
                                           # and tos-supplicant@wlan1
iso/wifi-witness.sh                        # on wlan1: an access point
                                           # (wpa_supplicant's own AP mode,
                                           # mode=2, WPA2-PSK, ssid
                                           # "hwsim-ap", psk "correct horse")
                                           # and busybox udhcpd on 10.99.0.1/24
```

Then `super+shift+n`, `wlan0`, `join a wireless network`, `hwsim-ap`, the
passphrase; the status line says `joining hwsim-ap`, then `joined hwsim-ap`,
then `wlan0 hwsim-ap 10.99.0.x/24 via 10.99.0.1`, and `ping 10.99.0.1` from a
pane answers. Then the same with a wrong passphrase, which has to say so.
Then a reboot, which has to rejoin without a keystroke. Then `wlan1`'s AP
taken down, which has to show `wlan0` losing its carrier in the menu.

The script also dumps `SCAN_RESULTS`, `STATUS` at each state it passes
through, and `LIST_NETWORKS` before and after a wrong passphrase, to the
serial console; that text is what the three parsers are tested against, and
it goes into `wpa.rs`'s tests verbatim, labelled with the supplicant version
that produced it. What was seen goes at the end of this document in the shape
`network.md` used for #84 — specific enough that somebody can tell whether it
was really run.

---

## What was seen — the witness, run

Everything above this line was designed, built and unit tested against
tables, and had never met a radio. This is the record of the runs that did:
the ISO built from this tree, booted in VirtualBox with a NAT `virtio-net`
adapter for the cable, and `mac80211_hwsim` for the radios, driven headless —
the serial line to a file, the compositor by `keyboardputscancode` and
`keyboardputstring`, the screen read back with `screenshotpng` every second
or two, because a status line lives a few seconds and a screenshot is the
only way to read it. Measured 2026-09-16: Debian bookworm, kernel
6.1.0-53-amd64, wpa_supplicant v2.10, busybox 1.35.0.

### Three faults, in the order they were found, each only visible past the last

**The image had every radio driver and no cipher.** The first run: hwsim
loaded out of the rootfs's own module tree, udev started
`tos-supplicant@wlan0` and `tos-supplicant@wlan1`, the witness took the
second one off its radio and started an access point on it — and the access
point failed to come up, with the supplicant's debug log reading
`nl80211: NEW_KEY`, `kernel reports: key addition failed`, `WPA: group state
machine entering state FATAL_FAILURE`. `ls /lib/modules/*/kernel/crypto/`
answered `No such file or directory`. mac80211 depends on none of the
ciphers; it asks the crypto API for `ccm(aes)` by name at the moment the
first key is set, which is the shape of #84 again, and a real WPA2 join on a
real radio would have died in the same line. `ccm gcm cmac ctr ghash_generic`
are in the tree now and the build asserts `ccm.ko` is there.

**The witness was one host pretending to be two, and the kernel knew.** With
the cipher in, the access point came up, the list showed
`hwsim-ap  -30 dBm  WPA2`, the passphrase prompt drew bullets, the four-way
handshake completed, the link menu grew `leave hwsim-ap` and
`forget hwsim-ap`, `net::auto` said `asking for an address on wlan0` — and
fifteen seconds later `wlan0: no DHCP server answered in 15 seconds`, while
busybox's `udhcpc` on the same radio got `10.99.0.49` at once. `tcpdump` on
the access point's radio:

```text
02:00:00:00:00:00 > ff:ff:ff:ff:ff:ff, ethertype IPv4, length 342:
    10.0.2.15.68 > 255.255.255.255.67: BOOTP/DHCP, Request from 02:00:00:00:00:00
```

`10.0.2.15` is the cable's address. `wlan0` had none, so the kernel chose one
from another link — and a packet whose source is one of the receiving host's
own addresses is a martian, dropped before `udhcpd` saw it. Allowing it
(`accept_local=1`) got the lease onto `wlan0` and then
`cannot take 10.99.0.49/24 via 10.99.0.1: Network unreachable`, because the
gateway was an address of the same host. Neither is what a laptop meets. The
first is a real finding about the client on its own and is #156; the second
is the witness's construction. The fix for the witness was to make the access
point a different host: its radio is moved into a network namespace by its
phy (`iw phy ... set netns`, which is why `iw` is on the image now), and the
supplicant, the address and `udhcpd` run in there.

**`pkill wpa_supplicant` in the namespace killed both supplicants.** Pids are
not namespaced the way the network is. The link menu answered with its third
shape — `no supplicant on wlan0 / is wpasupplicant installed?` — which was
the right thing to say and the wrong test; the instruction in the script's
header now kills by config-file pattern.

### What was seen, from the script the image ships

`/usr/share/tos/wifi-witness.sh` from a pane, then the keystrokes in its
header, on a fresh boot of the image:

```text
super+shift+n, wlan0, join a wireless network
                          wireless — scanning   ->   wireless
                          hwsim-ap              -30 dBm  WPA2
hwsim-ap                  hwsim-ap — passphrase   > ••••••••••••••
correct horse             status bar: joining hwsim-ap
                          status bar, ~8 s later: wlan0 10.99.0.49/24 via 10.99.0.1
```

and from a pane afterwards, on the serial line:

```text
wpa_state=COMPLETED      key_mgmt=WPA2-PSK     0  hwsim-ap  any  [CURRENT]
wlan0  UP  10.99.0.49/24 fe80::ff:fe00:0/64
default via 10.0.2.2 dev enp0s3 metric 100
default via 10.99.0.1 dev wlan0 metric 600
10.0.2.0/24 dev enp0s3 proto kernel scope link src 10.0.2.15
10.99.0.0/24 dev wlan0 proto kernel scope link src 10.99.0.49
# written by tOS from the DHCP lease on wlan0
nameserver 10.99.0.1
ping 10.99.0.1: 3 packets transmitted, 3 received, 0% packet loss, rtt avg 2.204 ms
ip route get 8.8.8.8:  8.8.8.8 via 10.0.2.2 dev enp0s3 src 10.0.2.15
```

Two default routes, the cable's the lower, and the ping is two milliseconds
across a simulated air rather than the sixty microseconds of loopback the
first witness measured. The link menu for `wlan0` then reads
`wlan0  hwsim-ap  up` over `take the link down / ask for an address /
leave hwsim-ap / forget hwsim-ap / join a wireless network`.

**The cable pulled** — `VBoxManage controlvm ... setlinkstate1 off`:

```text
/sys/class/net/enp0s3/carrier: 0
ip route get 8.8.8.8:  8.8.8.8 via 10.99.0.1 dev wlan0 src 10.99.0.49
ping 10.99.0.1: 2 received, 0% packet loss
```

and put back: carrier `1`, `via 10.0.2.2 dev enp0s3` again. The stale wired
route was in the table throughout, as the design said it would be, and
`ignore_routes_with_linkdown` is what made the kernel step over it.

**The wrong passphrase** — `forget hwsim-ap` from the link menu, the same
join with `wrong horse battery`: `joining hwsim-ap`, and about twelve seconds
later `hwsim-ap: wrong passphrase`. `LIST_NETWORKS` afterwards is the header
alone and `wpa_state=INACTIVE`: the network was removed and not saved, so a
wrong key is not tried again at every boot.

**The access point taken down** — `pkill -f 'wpa_supplicant.*ap.conf'`: the
link's carrier goes and the menu offers `bring the link up` and no network
to leave.

### What the supplicant wrote

Copied out of the serial line into `wpa.rs`'s tests verbatim, labelled
`captured`:

```text
bssid / frequency / signal level / flags / ssid
02:00:00:00:01:00	2412	-30	[WPA2-PSK-CCMP][WPS][ESS]	hwsim-ap
```

`[WPS]` beside the security flags on the first real scan line ever parsed —
which is why the flag reader treats the WPS family as saying nothing about
security — and a `STATUS` carrying `p2p_device_address`, `address`, `uuid`
and, once a lease has been held, `ip_address`, none of which the parser
wants and all of which it now has been shown to step over.

### What was not seen

- **A reboot rejoining.** The live image's `/etc` is a tmpfs, so the network
  `SAVE_CONFIG` wrote is gone at reboot by design; this is an installed
  machine's test and was not run on one.
- **A real radio.** Everything here is `mac80211_hwsim`, which is mac80211
  with no hardware under it. The five driver families and their firmware are
  packed and have never bound to anything; whether Intel's `iwlmvm` finds
  its `.ucode` on this image is the next machine's question.
- **Hidden networks, EAP, WEP** — deliberately outside this milestone, and
  the list says so on the row.
- **Two access points with one name.** Folding to the strongest is unit
  tested and was not exercised; hwsim had one.

