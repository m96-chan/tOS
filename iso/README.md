# tOS ISO

Bootable image where the compositor **is** userspace, on top of a real Debian:

```text
live       GRUB -> Linux -> /init -> squashfs + tmpfs overlay
                         -> switch_root -> systemd -> tos-session -> tos
installed  GRUB -> Linux -> /init -> switch_root -> systemd -> tos-session
                         -> tos
rescue     GRUB -> Linux -> /init -> tos-session -> tos   (initramfs only)
```

`/init` looks for three roots in that order and hands the machine to the first
one it finds. An installed machine names its root filesystem with `root=` on
the kernel command line, which is what the installer writes and what the live
image deliberately does not. A live medium carries its root as one squashfs
file, which `/init` mounts with a tmpfs stacked in front so the session can be
written to. Either way what it execs is `/sbin/init`, which is **systemd**
(#110): the same PID 1 on the live image and on the disk it installs, reading
the same `tos-session.service`. A machine that finds neither root stays in the
initramfs, which is a rescue session rather than a system — the one path with
no init at all, where `/sbin/tos-session` is PID 1 and restarts the compositor
itself — and, since GRUB moved into the rootfs, one to look at a broken
machine from rather than one to install from.

The GRUB menu has three entries: `tOS`, `tOS (verbose)` and
`tOS (rescue shell)`. Only the last passes `tos.rescue`, which is what the
session wants before it execs a shell when the compositor exits — it used to
be on all of them, which made an unauthenticated root shell the default way to
boot the image (#112).

The session is a unit, `/etc/systemd/system/tos-session.service`, written into
the rootfs by `mkiso.sh` and `Restart=always`. The gettys are masked — a
`getty@tty1` would draw over the compositor and a serial getty is a login
prompt on a line anybody with the cable can reach — so nothing but tOS is on
the console. `docs/design/init.md` has the reasoning, including what systemd
costs and what "apt install a daemon and it runs" is worth.

Per the top-level README, tOS targets a **Debian** userspace, and the squashfs
is it: a minimal bookworm with glibc, dpkg, apt and bash, built by
`mmdebstrap`. The static `tos` binary sits on top of it rather than inside it,
so nothing the compositor needs can be broken by an upgrade within the rootfs.
The initramfs is now only the few megabytes that find the medium, assemble the
root out of it and get out of the way.

## Building

Needs Docker or Podman on the host; the build itself runs in a
`rust:1-bookworm` container.

```sh
iso/build.sh          # -> dist/tos-x86_64.iso
```

On Apple Silicon this cross-builds via the container's amd64 emulation,
which is slow but hands-off. CI (`.github/workflows/iso.yml`) builds the
same ISO on every push that touches the compositor and uploads it as an
artifact.

### What Debian costs

Measured on x86_64, bookworm. The `before` column is the last image built
without a rootfs *and with* the network drivers, which landed first — measuring
against the image before both would credit the rootfs with 618 KiB of NIC
modules; `docs/design/network.md` has that measurement separately.

| | before | after |
|---|---|---|
| ISO | 55,054,336 | 108,115,968 |
| initramfs (gzip) | 26,558,126 | 21,539,919 |
| rootfs, unpacked | — | 203,123,805 |
| rootfs, squashed (zstd-19) | — | 58,073,088 |

The image roughly doubles. Debian itself is the 58 MB squashfs; the initramfs
gets 5.0 MB *smaller*, because the font and the SKK dictionary are in the
rootfs now and were previously carried in both places — `mkiso.sh` puts the
pair at 4,539,466 bytes uncompressed.

203 MB unpacked against 58 MB squashed is the ratio worth knowing: an
installed machine spends the unpacked figure on its disk, a live one only the
squashed figure on the medium. Trimming `/usr/share/{man,locale,info,doc}` was
worth 16,650,240 bytes of that squashfs — most of it translations nothing on
the image can currently display. `man` came back off that list in #151; the
other three are still trimmed.

### What carrying them twice cost

The image has two root filesystems, and until #99 the two largest things in
either were in both. The initramfs carried GRUB for an installer that has run
from the rootfs since #20, and the rootfs carried the pruned kernel module
tree for a `modprobe` nothing runs. Measured on the commit before this change
and the commit after it, same machine, same kernel:

| | before | after | |
|---|---|---|---|
| ISO | 109,256,704 | 93,167,616 | −16,089,088 |
| initramfs (gzip) | 22,176,128 | 9,497,076 | −12,679,052 |
| rootfs, unpacked | 205,654,526 | 188,674,630 | −16,979,896 |
| rootfs, squashed (zstd-19) | 58,576,896 | 55,169,024 | −3,407,872 |

The initramfs loses 57 per cent of itself, and it is the one file here that is
gunzipped into tmpfs on every boot — live, installed and rescue alike — and
written onto every disk the installer touches. The module tree comes off the
rootfs at both the sizes that matter: 17 MB of an installed machine's disk and
3.4 MB of the medium.

Neither copy was free to remove; what each cost is in Known limits below.

## Wireless

A radio is the one piece of hardware this image carries a driver for that
nothing about booting needs, so it is packed the other way round from
everything else. The display, input, disk and NIC modules are in the
initramfs and loaded by name in `/init` before the pivot, because a machine
has to have them to *reach* its root. The wireless ones are in the **rootfs**,
in a `/lib/modules/<kver>` of its own, and nothing loads them by name at all:
udev is in there, systemd starts it, and it modprobes whatever the bus's
modalias asks for, the way it does on any Debian machine. The initramfs does
not grow by a byte for any of this.

Seventeen names, checked against the kernel in the build container before
being written down, their closure 44 modules:

| | |
|---|---|
| the stack | `cfg80211` `mac80211` |
| Intel | `iwlwifi` `iwlmvm` `iwldvm` |
| Realtek | `rtw88_8821ce` `rtw88_8822be` `rtw88_8822ce` `rtw89_8852ae` `rtl8xxxu` |
| Qualcomm / Atheros | `ath9k` `ath10k_pci` `ath11k_pci` |
| Broadcom | `brcmfmac` |
| MediaTek | `mt7921e` `mt7921u` |
| a radio made out of nothing | `mac80211_hwsim` |

Their firmware comes from Debian's `non-free-firmware` component, which the
rootfs's sources now name on all three suites — so a machine can also install
a blob nobody packed: `firmware-iwlwifi`, `firmware-realtek`,
`firmware-atheros`, `firmware-brcm80211` and `firmware-misc-nonfree`. The last
of those is the MediaTek one, which is not a mistake: bookworm has no
`firmware-mediatek` and no `mediatek/WIFI_MT7921*` file either. An MT7921 card
asks for the blobs of the silicon it is — `WIFI_RAM_CODE_MT7961_1.bin` and
`WIFI_MT7922_patch_mcu_1_1_hdr.bin` — which is what `modinfo -F firmware
mt7921e` lists and where `apt-file` finds them.

All five, deliberately. A machine with no network cannot `apt install` the
firmware that would give it one, so this is the one place on the image where
the space is the wrong thing to save.

`wpasupplicant` is packed beside them, and four files written by `mkiso.sh`
are the whole of the wiring:

| file | what it is |
|---|---|
| `/etc/systemd/system/tos-supplicant@.service` | one supplicant per radio, `Restart=on-failure`, bound to the interface's device unit. It writes `/etc/wpa_supplicant/tos-<if>.conf` if there is none, with the control socket the compositor connects to and `update_config=1`, which is what makes a joined network survive a reboot |
| `/etc/udev/rules.d/80-tos-wireless.rules` | what starts it: a `net` device with `DEVTYPE=wlan` gets `tos-supplicant@<name>.service` pulled in. Instantiated from udev rather than enabled by name, because the name of the radio is the machine's to say |
| `/etc/sysctl.d/80-tos-net.conf` | `net.ipv4.conf.{all,default}.ignore_routes_with_linkdown = 1`, so that a laptop with a cable *and* a radio — two default routes, wired metric 100 and wireless 600 — stops using the cable's route the second the cable is pulled, and uses it again when it is back |
| `/usr/share/tos/wifi-witness.sh` | `iso/wifi-witness.sh`, on the image because the machine it has to run on is one booted from this image |

Debian's own `wpa_supplicant@.service` is in the same package and is not used:
it is written for `ifupdown`, and the config path and control directory are
this design's. `docs/design/wifi.md` has the whole of it, including what the
compositor says to that socket.

### What wireless costs

Measured on x86_64, bookworm, kernel 6.1.0-53-amd64, by building the ISO at
the commit before this change and at this one, same machine:

| | before | after | |
|---|---|---|---|
| ISO | 105,404,416 | 193,746,944 | +88,342,528 |
| rootfs, squashed (zstd-19) | 67,145,728 | 155,484,160 | +88,338,432 |
| rootfs, unpacked | 237,038,226 | 486,468,046 | +249,429,820 |
| initramfs (gzip) | 9,755,570 | 9,755,144 | −426, gzip noise |

The image nearly doubles, and `docs/design/wifi.md` guessed "roughly 35 MB on
a 105 MB image" — which was the sum of four `.deb` files, and a `.deb` is
xz-compressed where this squashfs is zstd. What each package actually costs,
squashed the way the image squashes it:

| | unpacked | squashed |
|---|---|---|
| firmware-iwlwifi | 84,167,644 | 29,749,248 |
| firmware-atheros | 62,303,820 | 22,552,576 |
| firmware-misc-nonfree | 52,404,663 | 17,797,120 |
| firmware-brcm80211 | 18,461,384 | 10,412,032 |
| firmware-realtek | 6,961,976 | 1,994,752 |
| the 44 wireless modules | 19,857,339 | 3,923,968 |
| wpasupplicant | 3,673,298 | 1,413,120 |

The argument for carrying all of it is unchanged by the number being bigger
than the design expected: a laptop whose radio has no firmware has no network,
and a machine with no network cannot fetch its firmware. The number that is
worth looking at twice is the third row. `firmware-misc-nonfree` is packed for
MediaTek and MediaTek only, its `mediatek/` subtree is 5,898,240 bytes
squashed of the 17,797,120 it costs, and the two files an MT7921 card actually
loads are a couple of megabytes of that; the rest is i915, nvidia, cxgb4 and
every other blob Debian could not find a better home for. Dropping that one
package would take about 17 MB off the medium and MediaTek off the list of
radios this image can start.

### What the applications cost

**#151.** The image had everything it needed to boot, install itself and join a
network, and nothing to work in once it had. Thirteen Debian packages, a file
manager and a face later, measured by building the ISO at `origin/main` and at
that change on the same machine:

| | before | after | |
|---|---|---|---|
| ISO | 194,025,472 | 240,781,312 | +46,755,840 |
| rootfs, squashed (zstd-19) | 155,623,424 | 202,375,168 | +46,751,744 |
| rootfs, unpacked | 486,614,158 | 623,082,598 | +136,468,440 |
| initramfs (gzip) | 9,893,959 | 9,893,964 | +5, gzip noise |

About a quarter more image. The initramfs is untouched, which is the shape this
was always meant to have: the few megabytes that find the medium do not care
what is on it. Where the rest goes, each measured on its own:

| | unpacked | squashed |
|---|---|---|
| git, curl, less, neovim, ripgrep, fzf, btop, openssh-client, rsync, unzip, file, libatomic1 | 84,098,952 | 23,908,352 |
| yazi | 33,929,663 | 12,378,112 |
| the manual pages | 7,938,894 | 7,876,608 |
| HackGen Console NF, less the `fonts-vlgothic` it replaces | 8,834,072 | 3,428,352 |

Two rows are worth a second look.

**The manual pages do not compress**, because they arrive gzipped: the squashfs
gains nothing on them and the unpacked and squashed figures are the same number
twice. They are on the image because the `path-exclude` that kept them off it
was not a statement about the image — it was a line in the installed machine's
dpkg configuration, so it meant that machine could never have a manual page for
anything it installed afterwards either.

**Two things here do not come from Debian**, and both are fetched at build time
against a sha256 written into `mkiso.sh` beside the URL. That buys integrity
and nothing else: neither has a line in the package database, so `apt upgrade`
will never touch them and a fix in either is a commit and a new image.
`docs/design/applications.md` has the whole argument, including why the list of
things arriving this way is two long and not ten.

### The witness

`mac80211_hwsim` is a kernel module that makes radios out of nothing and lets
them hear each other, which is how any of this is tried on a machine with no
radio in it. On the booted image, in a pane, as root:

```sh
/usr/share/tos/wifi-witness.sh
```

It loads the module with `radios=2`, leaves `wlan0` to the machine, turns
`wlan1` into an access point (`wpa_supplicant`'s own AP mode: WPA2-PSK, ssid
`hwsim-ap`, passphrase `correct horse`) with busybox `udhcpd` behind it on
`10.99.0.1/24`, and prints `SCAN_RESULTS`, `STATUS` and `LIST_NETWORKS` as
`wpa_cli` read them — to the pane and to `/dev/ttyS0`, so a headless boot's
serial log has them. That text is what the compositor's three parsers are
tested against, copied in verbatim rather than typed from memory.

Then `super+shift+n`, `wlan0`, `join a wireless network`, `hwsim-ap`, the
passphrase — and the same with a wrong one, which has to say so rather than
time out. `wpa_cli` is used in that script and nowhere else on the image;
tOS talks to the same socket itself.

## Running

```sh
iso/run.sh            # VirtualBox, headless, serial log on stdout
iso/run.sh --gui      # ... in a window as well
```

The VM is created and destroyed around the boot, so nothing it does survives
and the name is free for the next run. It has no disk, which means it is for
watching the image boot rather than for testing `tos-install` — installing
needs a machine you keep.

CI boots the same image under QEMU (`iso.yml` and `release.yml` drive it
inline), so the image is proved on two hypervisors and neither is the only
witness. That is also why this script does not have to stay QEMU: the developer
path and the gate are different things.

Any virtual machine will do: the initramfs carries the DRM drivers QEMU,
VirtualBox and VMware put in front of a guest (`bochs`, `virtio_gpu`,
`vboxvideo`, `vmwgfx`, `cirrus`, `simpledrm`). The GRUB menu waits ten
seconds, which is long enough to pick "tOS (verbose)" instead.

The screen is the console: `tty0` comes last on the kernel command line, so
`/init`'s messages and the emergency shell land where a person is looking.
The same messages also go to `ttyS0` for anyone capturing a headless boot,
which is how CI watches this image.

## Installing

Every shell in the live session prints the banner from `.motd_art` and one
line that matters:

```text
  Type tos-install to install tOS on this machine.
```

The banner is `/etc/tos/motd_art` on the machine, so it can be changed without
rebuilding anything. It does not have to be letters: a picture turned into
coloured blocks, the way `chafa image.png` does it, works in both the shell and
the installer, which reads the colours rather than printing them. The installer
falls back to the built-in banner when the one it finds needs more of the
screen than the welcome text can spare.

In a pane the banner is not what a shell prints at all. `/etc/tos/splash.png`
is on the image beside it — the picture the login screen draws — and a shell
that finds itself in a tOS pane sends the terminal that file's *path* over the
graphics protocol, so the real picture arrives for the price of a hundred byte
escape. What decides is whether `TOS` is in the environment and whether the
terminal filled in the pixel fields of its `winsize`, which tOS does for every
pane and the kernel's VT does not. Replacing `splash.png` changes the login
screen and the greeting together, which is why there is one file.

A terminal that cannot be sent the picture is shown it drawn in cells instead,
where there is room: `/etc/tos/motd_ascii`, the same picture rendered once into
half blocks, printed whenever the whole greeting fits the terminal — 120 cells
by 31, which is a maximized window at the far end of an `ssh` and not an
80-column console. Below that it is the drawn banner, as it has always been:
the serial console, the kernel VT the rescue session lands on, and anybody
logged in from another machine on a smaller screen.

`/etc/tos/lock.png` is beside it and is the other picture: the one in the
bottom right corner of a locked screen. It is a second file rather than a
second use of the first because it is composed for a different place, and
because replacing one screen's picture should not silently replace two.

`tos-install` is a TUI that runs in a pane, which makes installing tOS the
first real use of the platform as a platform. It picks a disk, writes a GPT
with a boot partition and an ext4 root, unpacks the Debian rootfs onto it,
installs GRUB, and writes one drop-in for `tos-session.service` naming the
person whose machine it is — the unit itself comes with the rootfs, so an
installed machine starts the compositor because the image it came from does.

The rootfs is unpacked from the medium rather than copied out of the running
session, which is the same image with a tmpfs over it: copying that would put
whatever the live session happened to write — a DHCP resolver, a half-finished
`apt install` — onto a disk somebody expected to be clean. What the installer
writes afterwards is only `/etc`, and it *adds* the machine's account to
Debian's `/etc/passwd` rather than replacing the file, because `_apt` is in
there and apt cannot fetch anything without it.

It will not touch a disk until the disk's own name has been typed, and it
refuses the medium the live session booted from, anything mounted, and
anything read only.

It also refuses the session, when the session is a rescue one. GRUB is in the
rootfs, so a rescue session could partition a disk, format it and fill it and
still have nothing to make it bootable with — an erased disk in exchange for a
machine that does not start. `tos-install` looks for a `grub-install` before it
offers anything, and says what it found at the plan, at the screen where the
disk's name would be typed, and in the dry run.

```sh
tos-install --list       # what it can see
tos-install --plan       # every command it would run, without running any
tos-install --dry-run    # the whole interface, writing nothing
```

The kernel and initramfs are copied from the medium rather than from the
running filesystem, because neither is in the running filesystem: the live
session is a squashfs of a Debian rootfs, and the kernel that unpacked the
initramfs is on the medium beside it. `/init` mounts the medium at
`/run/live/medium` for exactly that reason; if it is not mounted the
installer stops rather than leaving a disk that cannot boot.

Pick "tOS (verbose)" in GRUB to keep kernel messages visible while
debugging boot problems. If the compositor exits, `live-session` restarts it —
and drops to a shell on the console instead when `tos.rescue` is on the kernel
command line, which the live image's own GRUB entry puts there. After the pivot
that shell is the rootfs's `sh`, which is dash; in a rescue session it is
busybox. An installed machine has no `tos.rescue` on its command line, by the
argument in the installer beside `CMDLINE`.

## Files

| file        | role                                                          |
|-------------|---------------------------------------------------------------|
| `build.sh`  | host entry point: runs `mkiso.sh` in a container               |
| `mkiso.sh`  | container-side build: static binaries, initramfs, Debian rootfs, `grub-mkrescue` |
| `init`      | initramfs PID 1: mounts, modprobe, then `switch_root` into `root=`, into the squashfs overlay, or into neither |
| `live-session` | PID 1 of a live session either way: `/init` execs the copy inside the rootfs after pivoting, and the initramfs copy when there was no rootfs to pivot into. The session's environment and the loop that restarts the compositor |
| `profile`   | `/etc/profile.d/tos.sh` in the rootfs, `/etc/profile` in the initramfs: the banner and the install hint, for a login shell or for an ash pane through `ENV` |
| `bashrc`    | `/root/.bashrc` and `/etc/skel/.bashrc`: history, prompt, colour and the banner, for the interactive non-login shell a pane actually runs |
| `dot-profile` | `/root/.profile` and `/etc/skel/.profile`: hands `~/.bashrc` to a login shell, which is the one kind of shell that does not read it |
| `run.sh`    | boots `dist/tos-<arch>.iso` in VirtualBox, and cleans up after |
| `wifi-witness.sh` | run on the booted image: makes two radios out of `mac80211_hwsim`, turns one into an access point, and prints the supplicant's own `SCAN_RESULTS`, `STATUS` and `LIST_NETWORKS` for the compositor's parsers to be tested against |

## Known limits

- x86_64 only for now; `ARCH=aarch64` is plumbed through `build.sh` and
  `mkiso.sh` but untested, and arm64 needs a different boot path anyway.
- The initramfs carries only the virtual machines' display/input modules
  and their dependency closure. Real hardware needs its GPU driver added
  to the `MODULES` list in `mkiso.sh` (and matching firmware, which is packed
  for the wireless families and for nothing else — see Wireless above).
  Without a driver there is no `/dev/dri/card0`, and the compositor falls back
  to running inside the console rather than owning the screen.
- The rootfs is a package manager, and now a network to reach with it.
  `dpkg` and `apt` are on every installed machine, `apt install ./something.deb`
  works off a local file, and the image carries and loads drivers for virtio,
  e1000/e1000e, r8169 and igb — so a VM gets a link, a DHCP lease and a
  `/etc/resolv.conf` written from it. Only virtio-net and e1000 have been seen
  to bind to anything; the rest are packed and untried, and `r8169` ships
  without its `rtl_nic` firmware. `docs/design/network.md` records what was
  actually observed.
- No radio has been seen to associate. The drivers, the firmware, the
  supplicant, the unit and the udev rule are on the image and asserted by the
  build; what has been proved is that they are *packed*. Whether a card comes
  up, whether udev starts a supplicant on it and whether the menu can join
  anything is what `iso/wifi-witness.sh` is for, and until that has been run
  on a booted image the whole of Wireless above is a claim about a directory
  listing. `firmware-misc-nonfree` is also the largest thing on the image that
  is mostly not wireless: it is packed for six MediaTek files.
- The session runs `bash` where the filesystem has one and `/bin/sh` where it
  does not, which is the difference between a Debian rootfs and the initramfs
  rescue session. Both `iso/live-session` and the installer's `SESSION_SCRIPT`
  ask rather than assume, and `/etc/passwd` names whichever one actually landed
  on the disk. The `shell =` key in `tos.conf` still outranks both.
- An installed machine's PID 1 is busybox `init`, symlinked over the rootfs's
  empty `/sbin`. Debian's essential set contains no init at all — an init
  system is a package, and tOS installs none.
- A rescue session cannot install. GRUB is in the Debian rootfs, which is the
  thing a rescue session could not mount, and nothing on the medium carries a
  second copy any more. `tos-install` refuses from such a session rather than
  erasing a disk it could not finish with, which is the one place it refuses
  where it otherwise degrades: a rescue install used to leave the busybox
  world on the disk, which is a worse tOS but a tOS that boots. A live session
  and an installed machine are unaffected — both have the rootfs.
- An installed machine can `modprobe` only the wireless tree. That is the
  whole of what the rootfs's `/lib/modules` holds; every other module is in
  the initramfs and nowhere else, and `switch_root` deletes the initramfs, so
  everything but a radio is loaded by `/init` before the pivot or not at all.
  That is why the three `nls_` modules are in its list: nothing has a device
  that needs them, but the kernel asks for a codepage by name the first time a
  FAT filesystem is mounted, and by then there is nowhere to look. Adding
  hardware to a running tOS still means adding its driver to `MODULES` in
  `mkiso.sh` and rebuilding the image — the two lists are separate, and the
  one a radio goes in is `WIRELESS_MODULES`.
- The installer has not been run against real hardware. Its logic is
  covered by tests, including the whole sequence against a recorded
  backend, but the commands it drives have only been checked for what they
  are, not for what they do to a physical disk.
