# Sound server, or no sound server

Design for [#19](https://github.com/m96-chan/tOS/issues/19), fourth item:
*"Decide whether an installed system gets PipeWire from the Debian rootfs
([#20](https://github.com/m96-chan/tOS/issues/20)) or stays ALSA-only; record
why."*

**tOS stays ALSA-only, and this is not a "for now".** The compositor's volume
control drives the kernel's control device by ioctl and will keep doing that.
tOS does not start a sound server, does not require one, does not link one and
does not speak to one. An installed system is free to run PipeWire — the
volume keys keep working when it does, and that is measured below rather than
hoped for.

The reason is not that PipeWire is bad. It is that tOS is a session with no
systemd, no logind and no D-Bus, and PipeWire from Debian is shipped as two
systemd user units and nothing else. Carrying it means tOS becoming a service
supervisor, which is the one thing the README says it is not.

> **One premise of that sentence moved: #110 put systemd on the image, on both
> the live medium and every installed machine.** The decision is not revisited
> here and the conclusion is unchanged, because the reasons that do the work
> below are not the missing supervisor: the units carry `ConditionUser=!root`
> and a tOS session is root's, there is still no D-Bus and no logind session,
> and tOS sets a knob rather than playing a sound. What is now true is that a
> person who installs PipeWire on their own machine has an init that will
> start it, which is exactly the "an installed system is free to run PipeWire"
> above. If that is ever wanted by default, this document is the one to
> reopen, and `docs/design/init.md` is what changed under it.

Everything below was read out of the packages bookworm actually ships and the
sources those packages are built from, not recalled. Every claim names where
it came from.

---

## What exists today

```text
tos-system/src/audio.rs   Control trait (:571) over SNDRV_CTL_IOCTL_*
                          Card (:628) = open(/dev/snd/controlC<N>) + ioctl
                          PREFERRED (:919) = Master, PCM, Speaker, Headphone
                          STEP_PERCENT (:902) = 5
                          open_default_in (:1282) -> Ok(None) with no card
tos-compositor/system.rs  Machine::mixer (:192), opened once (:268)
                          Reading::volume (:59), re-read once a second
tos-compositor/compositor.rs
                          change_volume (:1067), volume_status (:112)
tos-input/src/keymap.rs   scancodes 113/114/115 (:161) -> KeyCode::Media
iso/mkiso.sh              MODULES: no snd_* module of any kind
iso/init                  modprobe list: no snd_* module of any kind
```

Nothing in the tree links `libasound`, and nothing in the tree ever opens a
PCM device. That second fact is the hinge of this whole document and is easy
to miss: tOS does not play sound. It sets a knob. The only file it opens is
`/dev/snd/controlC<N>`, the mixer, and the only thing it does with it is read
and write control elements.

The live ISO cannot make a sound at all today, because `iso/mkiso.sh` copies
only the display, input, storage and filesystem modules into the initramfs and
`snd_hda_intel`, `snd_pcm` and the rest are not among them. So none of what
follows is observable on the ISO until that is fixed; it is a separate,
mechanical piece of work and it is named at the end.

---

## The question, stated properly

"PipeWire or ALSA-only" sounds like a choice between two ways of doing the
same job. It is not. There are three jobs, and only one of them is tOS's:

```text
1. set the hardware volume        the compositor, on a key press
2. play audio                     applications, in panes
3. mix two applications together  somebody, if 2 ever happens twice at once
```

tOS does (1). Nothing in tOS does (2), and (3) only exists as a consequence of
(2). A sound server is an answer to (2) and (3). Choosing one on tOS's behalf
would be choosing on behalf of applications that do not exist yet, in a rootfs
that does not exist yet (#20).

So the real question is narrower and answerable: **does the compositor's own
volume control need a sound server underneath it, and does it break if the
installed system has one?**

---

## What PipeWire would cost tOS

### It arrives as systemd units and nothing else

`pipewire` 0.3.65-3+deb12u1 is the bookworm version. The `pipewire` binary
package contains, in full:

```text
/usr/lib/systemd/user/pipewire.service
/usr/lib/systemd/user/pipewire.socket
/usr/share/doc/pipewire/{NEWS.gz,README.Debian.gz,changelog.Debian.gz,copyright}
```

That is the whole package: `Installed-Size: 97` kB of unit files. There is no
`/etc/init.d` script in `pipewire`, `pipewire-bin`, `pipewire-pulse` or
`wireplumber`. The `init-system-helpers (>= 1.52)` dependency exists only so
`postinst` can run `deb-systemd-helper --user unmask 'pipewire.service'`.

`pipewire.service` and `pipewire.socket` both carry `ConditionUser=!root`.
tOS runs as root — `tos-session.service` starts the compositor with no login
in front of it, on the live image and on an installed disk alike. So even
now that the machine does have systemd, the units as shipped would decline to
start for the only user tOS has.

### The session bus it wants cannot be installed

`pipewire-bin` has `Recommends: dbus-user-session, wireplumber |
pipewire-media-session, rtkit`. `dbus-user-session` has `Depends: ...
libpam-systemd, systemd`. It is not installable on a machine without systemd,
which is every tOS machine.

### Without a runtime directory the daemon exits

`src/modules/module-protocol-native.c` looks for `PIPEWIRE_RUNTIME_DIR`, then
`XDG_RUNTIME_DIR`, then `USERPROFILE`, and with none of them set:

```c
pw_log_error("server %p: name %s is not an absolute path and no runtime dir "
             "found. Set one of PIPEWIRE_RUNTIME_DIR, XDG_RUNTIME_DIR or "
             "USERPROFILE in the environment", s, name);
return -ENOENT;
```

`libpipewire-module-protocol-native` is loaded from the shipped
`pipewire.conf` *without* the `nofail` flag, and `src/pipewire/conf.c:593`
makes a module without that flag fatal. No runtime directory, no daemon.

There is nothing in tOS that sets `XDG_RUNTIME_DIR`, because there is nothing
in tOS that is a session manager. Setting it would be the first line of
becoming one.

### So carrying PipeWire means writing a service supervisor

Adding up: tOS would have to create a runtime directory, set the environment
for it, spawn `/usr/bin/pipewire` and `/usr/bin/wireplumber`, notice when
either dies and restart it, and tear both down when the session ends. That is
a service supervisor with a hardcoded unit list. It is perhaps two hundred
lines, and it would be the second-largest thing in the compositor that has
nothing to do with drawing panes — after which the compositor would still have
to *talk* to it, which means either `libpipewire` (a C dependency, in a tree
whose whole dependency surface is `libc` and `fontdue`) or the native protocol
written out by hand. `audio.rs` hand-writes about ten ioctls. The native
protocol is not in that weight class.

### And it costs 26 MB, some of it libX11

From the bookworm `Packages` index, resolving the `Depends` closure against an
`mmdebstrap` base:

```text
pipewire + wireplumber              +36 packages   ~26 MiB
  + libspa-0.2-bluetooth + bluez + dbus  +54 pkgs  ~34 MiB
alsa-utils (amixer, alsactl)         +8 packages    ~8 MiB
libasound2 alone                     +2 packages   ~1.3 MiB
```

`libpipewire-0.3-modules` depends on `libpulse0` for `module-pulse-tunnel`,
and `libpulse0` depends on `libx11-6`. A headless PipeWire on bookworm drags
in libX11 and GLib. #20 already warns that "ISO size will jump"; this would be
26 MB of the jump spent on a thing the compositor cannot talk to.

---

## What ALSA-only costs, honestly

The costs are real. They are also, every one of them, somebody else's.

**No software mixing for a program that bypasses `libasound`.** `dmix` is
`src/pcm/pcm_dmix.c` in *alsa-lib*, not in the kernel: it mixes in the client
process through a SysV shared memory ring. A process that opens
`/dev/snd/pcmC0D0p` itself gets the raw kernel substream, which is exclusive.
But `dmix` is already the configured default for the cards this matters on —
`/usr/share/alsa/cards/HDA-Intel.conf` defines `HDA-Intel.pcm.default` as
`type plug` over `softvol` over `dmix:$CARD`, commented "default with
dmix+softvol & dsnoop", and `USB-Audio.conf` has `use_dmix` defaulting to
`yes`. Two ordinary applications in two panes will mix, because both will use
`libasound` and `libasound` will put them through `dmix`. That costs
`libasound2` + `libasound2-data`, 1.3 MB, in the rootfs — not in tOS.

(Not universal: a card with no `/usr/share/alsa/cards/*.conf` entry falls
through to `/usr/share/alsa/pcm/default.conf`, which is `plughw` and does not
mix. A card with hardware mixing, like EMU10K1, deliberately has no entry
because it does not need one.)

**No per-application volume.** There is one knob and it is the card's. This is
a genuine loss and it is the loss tOS is choosing. A tiling terminal session
is not where somebody balances a game against a music player.

**No Bluetooth audio.** Covered below; it is not a cost of this decision,
because PipeWire would not have bought it either.

**No hot-plug of a USB headset into the running mixer.** `Machine::open_mixer`
opens the default card once and never looks again (`system.rs:268`), which is
right for the case it was written for — a card that is not there does not
appear later — and wrong for a headset plugged in mid-session. That is a bug
in the tOS side, fixable in the tOS side, and unrelated to sound servers.

---

## Does an installed system that runs PipeWire anyway break the volume keys?

No, and this is the fact that makes the decision cheap rather than a gamble.
Three things had to be checked.

**The control device is not exclusive.** `snd_ctl_open()` in
`sound/core/control.c` allocates a per-open `struct snd_ctl_file`, appends it
to `card->ctl_files`, and has no busy check and no `-EBUSY` path. tOS's fd and
PipeWire's mixer handle can be open at the same time. (The *PCM* device is
exclusive; tOS never opens one.)

**PipeWire drives the same hardware mixer by default.** The `api.alsa.soft-
mixer` property is documented as: *"Setting this option to true will disable
the hardware mixer for volume control and mute. All volume handling will then
use software volume and mute, leaving the hardware mixer untouched."* It
defaults to false, twice confirmed: bookworm's
`/usr/share/wireplumber/main.lua.d/50-alsa-config.lua` ships it commented out
with `false` spelled out, and `acp.c:1571` only sets `impl->soft_mixer` when
the property is present, on a zero-initialised struct. So PipeWire's device
Route volume *is* the `Master` control tOS is writing.

**And it notices when something else moves it.** `acp.c:1065`:

```c
static int mixer_callback(snd_mixer_elem_t *elem, unsigned int mask)
{
	if (mask & SND_CTL_EVENT_MASK_VALUE) {
		if (dev->read_volume) dev->read_volume(dev);
		if (dev->read_mute)   dev->read_mute(dev);
```

which re-reads the hardware level and emits `volume_changed` up into the
device's Route. A tOS volume key press propagates into PipeWire's own idea of
the volume rather than being fought.

One caveat worth writing down rather than discovering: WirePlumber 0.4.13's
`src/scripts/policy-device-routes.lua` stores route props and calls
`restoreRoute()` when a route is newly activated — a device resume or a
profile change will write WirePlumber's remembered level back over whatever
tOS last set. It does not fight continuously; it clobbers at transitions.

---

## The decision

**tOS is ALSA-only. Permanently, for the controls tOS provides.**

1. The compositor's volume control stays `SNDRV_CTL_IOCTL_*` on
   `/dev/snd/controlC<N>`. No `libasound`, no `libpipewire`, no `amixer`, no
   daemon.
2. tOS does not start, supervise, require or detect a sound server. There is
   no code path in the compositor that knows what PipeWire is, and there will
   not be one.
3. An installed system may install and run PipeWire — it is a Debian rootfs
   (#20) and that is the user's business. The volume keys keep working when it
   does, for the three reasons measured above.
4. The rootfs should ship `libasound2` so applications get `dmix` and a
   working `default` PCM. 1.3 MB, no daemon, no bus. That is #20's decision to
   execute, not this one's, but it is what this one assumes.

The one-line argument: **tOS does not play audio, so it does not need the
thing that plays audio.** Everything a sound server is for happens above tOS,
in applications, in a rootfs that has not been built yet. Deciding it now
would be deciding it for them.

---

## What this means for the neighbouring issues

**#20, the Debian rootfs.** Nothing to add to the ISO for tOS's sake except
kernel modules (below). `libasound2` for applications' sake. No PipeWire, no
D-Bus, no `dbus-user-session` — which is convenient, since the last of those
cannot be installed without systemd anyway.

**#18, Bluetooth.** This document does *not* answer #18's "does tOS carry a
D-Bus?", but it narrows it considerably, because the bus Bluetooth needs is
not a bus PipeWire needs:

```text
                           needs a bus?   which bus
PipeWire, ALSA playback    no             spa/plugins/alsa has zero
                                          references to dbus
WirePlumber, ALSA          no             "WirePlumber does not require a
                                          D-Bus connection to work"
PipeWire, Bluetooth        yes            system bus; bluez5-dbus.c:5024
                                          fails -EIO with "no dbus connection"
bluetoothd itself          yes            src/main.c: "Unable to get on
                                          D-Bus" then exit(1)
A2DP audio path            yes            org.bluez.MediaTransport1.Acquire()
                                          returns the fd
```

So a D-Bus system bus, if tOS ever carries one, is carried for BlueZ's sake
and for nothing else. PipeWire would then become cheap — but still optional,
and still not something the compositor talks to. If #18 concludes that tOS
does carry a system bus and does want Bluetooth audio, the sound server
question reopens *at that point*, with the bus already paid for, and this
document should be revised rather than worked around.

This is also why output-device selection is not built in #19. See the comment
above `Compositor::change_volume` in `compositor.rs`: a card is not an output,
and the second output that genuinely is a different device is a Bluetooth
sink, which has no control device at all.

---

## What would reopen this

Honest triggers, so that the next person does not have to guess whether the
decision still holds:

- **#18 decides tOS carries a D-Bus system bus** and wants Bluetooth audio.
  Then a Bluetooth sink exists, it is not an ALSA card, and something has to
  route to it. That is the strongest trigger and the likely one.
- **tOS itself starts playing audio.** An audible bell is the obvious
  candidate — `compositor.rs` currently turns `TermEvent::Bell` into a visible
  notification precisely because "a display server with no audio stack has
  nothing to ring". The moment tOS opens a PCM, it is a client, and being a
  client that bypasses `dmix` would make it a client that stops every other
  program from playing.
- **Somebody measures two applications in two panes failing to mix** on real
  hardware. The `dmix` reasoning above is read from config files, not observed.
- **A machine where `PREFERRED` finds no usable control.** That is a gap in
  `audio.rs`'s element search, not an argument for a sound server, but it is
  how one would be argued for, so it is worth naming as *not* a reason.

---

## What still has to happen before any of this makes a noise

Not decisions — work, listed so it is not mistaken for done:

- `iso/mkiso.sh` and `iso/init` ship and load no `snd_*` module, so a tOS ISO
  has no `/dev/snd` and `Machine::mixer()` correctly answers `None` on every
  machine it boots. `snd_hda_intel`, `snd_hda_codec_*`, `snd_usb_audio` and
  their closure need to join `MODULES`, with the same "whatever the machine
  has, missing hardware is a harmless failure" handling the display drivers
  get. This is the only thing standing between the bindings and a sound.
- `audio.rs` has never been run against real hardware. Every test drives a
  `Control` built out of structures a test wrote. The layouts are size-checked
  against the kernel's own ioctl numbers, which is the part that would fail
  loudly rather than quietly, but "works on a real HDA codec" is still
  unverified.
- A hot-plugged USB headset does not become the mixer, because the mixer is
  opened once. See the cost list above.
