# Bluetooth, and whether tOS carries a D-Bus

The decision behind [#18](https://github.com/m96-chan/tOS/issues/18), which
asked for adapter control "through BlueZ D-Bus — which also forces the 'does
tOS carry a D-Bus?' decision; record the answer".

**tOS does not carry a D-Bus, and does not carry BlueZ. Bluetooth is read out
of `/sys/class/bluetooth` and driven with `AF_BLUETOOTH` ioctls and
`/dev/rfkill`, the same way every other machine-facing thing in tOS is
driven.** The cost is real and is written out below: no pairing, no A2DP, no
OBEX, and therefore no Bluetooth headset. This document says why that is the
right trade today, and what would have to be true to revisit it — which is not
what the issue assumed.

---

## What the issue assumed, and what is actually in the tree

#18 was written against the 0.0.5 roadmap and reads "realistically gated on the
Debian rootfs (#20) providing BlueZ". It is not gated on #20 and never was.
`compositor/tos-system/src/bluetooth.rs` has talked to the kernel directly
since it was written: `HCIGETDEVINFO` for what an adapter is doing, `HCIDEVUP`
and `HCIDEVDOWN` for taking it up and down, `HCIINQUIRY` for a scan, a write to
`/dev/rfkill` for the kill switch, and `/sys/class/bluetooth` for enumeration
and for the live links. No BlueZ, no bus, no daemon, 1,720 lines of `std` and
`libc`.

So the question the issue says has to be forced has in fact already been
answered by the implementation. What was missing was the write-up and the way
in from the session, and this document is the first of those.

## What "carry a D-Bus" would actually mean

It is worth being precise, because "add D-Bus" sounds like adding a library and
is not.

A D-Bus client is not much: the wire format is a straightforward binary
marshalling over a Unix socket with SASL `EXTERNAL` authentication, and writing
one in tree — `std` and `libc`, the way `tos-crypt` did SHA-512 crypt — is a
weekend rather than a project. **That is not the part that costs anything.**

The part that costs is everything a client needs to have something to talk to:

- **A bus daemon.** `org.bluez` is a name on the *system bus*. The system bus
  is a process — `dbus-daemon` or `dbus-broker` — that has to be started before
  anything that wants it, kept running, and given a policy file that says who
  may own `org.bluez` and who may call it. On this developer's machine that is
  `/usr/bin/dbus-daemon`, 215,560 bytes, plus its configuration under
  `/usr/share/dbus-1/`.
- **BlueZ itself.** `bluetoothd` on this machine is bluez 5.87:
  `/usr/lib/bluetooth/bluetoothd`, 2,040,432 bytes, linked against glibc,
  `libglib-2.0`, `libdbus-1`, `libasound`, `libudev` and `libsystemd`. It keeps
  its own state under `/var/lib/bluetooth` and expects to be started as a
  service.
- **Something to start and supervise both.** This is the one that decides it.

## The supervision problem, which is the real answer

tOS has no service manager, and it is not an oversight.

The live image (`iso/mkiso.sh`) is a busybox initramfs. `/init` is
`#!/bin/busybox sh`: it mounts `/proc`, `/sys` and `/dev`, `modprobe`s the
display and input drivers, and execs `/sbin/tos`. The comment in `mkiso.sh`
above the build is the whole shape of the thing — *"Fully static binaries: they
run as PID 1's children with no libc on disk."* There is no libc on the image
for `bluetoothd` to link against, no `/usr/lib`, no `ld.so`.

The installed system (`installer/src/install.rs`) is busybox init with a
three-line `/etc/inittab`:

```text
::sysinit:/etc/rc
::respawn:/sbin/tos
::ctrlaltdel:/sbin/reboot
```

and `/etc/rc` mounts filesystems and sets the hostname. That is the entire boot
of a tOS machine. There is one respawned process and it is the compositor.

So "add BlueZ" is not one package. It is: a libc on the image, a dynamic
loader, glib, libdbus, alsa-lib, udev — `bluetoothd` opens `libudev` and wants
a populated `/sys` device database — a bus daemon with a policy, two more
entries in `inittab` with a startup ordering between them that busybox init
cannot express, and a story for what happens when either dies. It is the Debian
rootfs of #20 and then a service manager on top of it, to get adapter control
that 1,720 lines of `tos-system` already have.

It would also be the first time anything in tOS depended on a process that is
not tOS. Today the compositor is the session: if it is alive the machine works,
and if it dies `inittab` starts another one. A bus in the middle means a second
failure mode — the radio works but the menu says nothing, because `bluetoothd`
is not up — and nothing in the tree is shaped to notice or report that.

## What the "no" costs

This is the honest part, and it is not small.

### Pairing, and therefore connecting

The module's own doc comment already says pairing is out of scope. It is worth
saying *why* it stays out, because the reason is not the same as the reason
above.

The kernel will hold an ACL link, but the link key that authenticates one is
userspace's to keep. On an ordinary Linux machine that userspace is BlueZ, and
its store is `/var/lib/bluetooth/<adapter>/<device>/info`. **A tOS machine
therefore has no paired devices at all** — not "some, from before", not "the
ones the kernel remembers": none, because nothing has ever written a key or
loaded one into the controller.

That makes "connect to an already-paired device" vacuous on tOS rather than
merely unimplemented, which is why the menu built for this issue has no connect
row. It lists the links the adapter is holding, because sysfs is telling the
truth about those, and it says in as many words that pairing is not possible
from here. A greyed-out button that could never work in any state would be
worse than the sentence.

### Bluetooth audio, which is the reason the issue exists

#18 says audio devices are why it exists. They are exactly what this decision
gives up.

A2DP is not in the kernel. It is AVDTP — a userspace protocol over an L2CAP
socket — plus an SBC encoder, plus something to route a playback stream into
it. BlueZ used to carry the audio plugin itself; now it is PipeWire's or
PulseAudio's `module-bluetooth`, or `bluealsa`. HFP and HSP are closer, in that
the kernel does have `BTPROTO_SCO`, but they still need pairing first and a
codec path after.

So the join with audio (#19) is a short one and it is a negative: **a Bluetooth
headset is not an ALSA card, and nothing in tOS can make it one.**
`tos-system/src/audio.rs` drives control elements on a card in `/proc/asound`.
A paired A2DP sink appears there only if `bluealsa` or PipeWire is running and
registers one, and neither can run here for the reasons above. Nothing in this
issue's work should be wired into `audio.rs`, and nothing in #19 should expect
a Bluetooth sink to turn up in its card list. If that changes it will be
because the section below changed, not because the two menus learned about each
other.

### OBEX, and the rest of the profile zoo

File transfer is `obexd`, another daemon on the same bus. Nothing in tOS wants
it today and nothing is planned that does; it is listed here so that the cost
is stated completely rather than only where it hurts.

### What is *not* lost

Worth saying, because it is most of what a person actually does with the menu:

- Whether the machine has an adapter, what its address is, and whether it is up.
- Turning it on and off, including clearing a soft rfkill block first, which is
  the thing `HCIDEVUP` fails on with `ERFKILL` if nobody does it.
- Blocking it — the off that stays off.
- Finding what is in range, with the major device class, which is enough to say
  "that one is audio".
- The links the adapter is currently holding.

## The scan, and the one thread

`HCIINQUIRY` blocks for as long as the controller is told to listen — eight
seconds, as this is configured. The compositor is a single thread and it is the
thread that draws, so an inquiry called from `perform_action` would stop every
pane, the cursor and the clock for those eight seconds, and a session that
stops for eight seconds looks crashed rather than busy.

`compositor/tos-compositor/src/bluetooth.rs` puts it on a thread of its own and
brings the answer back through an `mpsc` channel that `Compositor::tick`
drains. Polled where the frame is already being decided, rather than
interrupting one — the same shape as the once-a-second machine reading, the
animation clock and the idle deadlines, all of which the loop already folds
into the wait it was going to do anyway.

The two alternatives were considered and are worse. There is no non-blocking
`HCIINQUIRY`: the kernel is going to sleep on the controller whatever the
caller asks for. Slicing the inquiry into one short inquiry per frame gives an
inquiry that keeps restarting, which costs the same radio time and finds less,
because a device answers on its own schedule and a restarted inquiry keeps
missing it.

The thread is not joined and cannot be cancelled. There is nothing to cancel —
a thread parked in an ioctl cannot be asked to stop — so closing the menu drops
the receiving end, the send at the far end fails harmlessly, and the thread
ends on its own within the inquiry length, or with the process if the session
went first. This is the only thread tOS starts outside the frame loop, and it
is worth keeping that true: it holds no state anything else can see, it never
touches the session, and it communicates by one message and then exits.

## What would change the answer

The two questions come apart, and the issue's framing joined them. Keeping them
apart is most of the value of writing this down.

### Pairing does not need a bus

The kernel's **management interface** — an `AF_BLUETOOTH`/`BTPROTO_HCI` socket
bound to `HCI_CHANNEL_CONTROL` with `hci_dev` of `HCI_DEV_NONE` — is what
modern BlueZ actually drives the controller with, and it is not a D-Bus API. It
has `MGMT_OP_PAIR_DEVICE`; it delivers the passkey and confirmation requests
that an agent has to answer, and it hands out `MGMT_EV_NEW_LINK_KEY` and
`MGMT_EV_NEW_LONG_TERM_KEY` for userspace to store and to give back with
`MGMT_OP_LOAD_LINK_KEYS` on the next boot.

So a tOS that pairs is: the mgmt protocol as a second `Control`-shaped seam,
an agent in the overlay to show a passkey and take a yes, and a key store on
disk that is tOS's own rather than BlueZ's. That is a real piece of work and a
real security surface — it is a persistent secret, so it belongs next to the
lock credential and under the same argument as `docs/design/screen-lock.md`
makes about `/etc/shadow` — but it needs no daemon, no bus and no rootfs. It is
the next thing to build if Bluetooth is wanted for anything at all, and it is
not what #18 asked for.

### Audio does need more than that

Even with pairing, A2DP needs AVDTP and an SBC encoder written or vendored, and
`audio.rs` needs somewhere other than a card to send a stream. That is the
point at which importing something starts to look better than writing it, and
the point at which the bus question is worth reopening — not before.

### The three things that would make BlueZ the right answer

All three, not any one:

1. **#20 lands and tOS boots a real rootfs**, so that a dynamically linked
   daemon has a libc, a loader and a `/usr` to live in.
2. **tOS gains something that supervises processes other than itself** — a
   service manager, or an `inittab` story with ordering and restart — so that
   "the bus is not up" is a state something can notice and report rather than a
   menu that silently says nothing.
3. **A profile is wanted that cannot be reached from the kernel**, which in
   practice means audio. Adapter power, blocking and discovery are all cheaper
   through the ioctls than through a bus, and would stay that way even if the
   bus were there.

Until all three, a bus is a dependency that buys nothing the tree does not
already have.

## What remains unverified

- **None of the kernel path has been run against a real adapter.** The ioctl
  numbers, the `hci_dev_info` layout, the inquiry request bytes and the rfkill
  event are all checked by tests over bytes, and the tests assert the layouts
  on every target — but the developer machine this was written on has no
  adapter reachable from the build, and the tOS ISO boots under qemu with no
  Bluetooth controller passed through. What is tested is that the right bytes
  are built, not that a controller answered them.
- **The BlueZ and D-Bus figures above are this machine's**, Arch's bluez 5.87
  and its `dbus-daemon`. Debian's builds will differ in size and in exactly
  which libraries are pulled in. The argument does not turn on the numbers, but
  they are not Debian's numbers.
- **The mgmt claims are read, not run.** `MGMT_OP_PAIR_DEVICE`,
  `MGMT_EV_NEW_LINK_KEY` and `MGMT_OP_LOAD_LINK_KEYS` are names out of
  `include/net/bluetooth/mgmt.h`, not out of a session that paired anything.
  What the exchange actually looks like end to end has not been measured, and
  any issue that takes that work on should start by measuring it, the way
  `docs/design/vt-lockswitch.md` did.
