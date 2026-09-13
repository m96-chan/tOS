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
squashed figure on the medium. Trimming `/usr/share/{man,locale,info,doc}`,
which `mkiso.sh` does, is worth 16,650,240 bytes of that squashfs — most of it
translations nothing on the image can currently display.

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

## Known limits

- x86_64 only for now; `ARCH=aarch64` is plumbed through `build.sh` and
  `mkiso.sh` but untested, and arm64 needs a different boot path anyway.
- The initramfs carries only the virtual machines' display/input modules
  and their dependency closure. Real hardware needs its GPU driver added
  to the `MODULES` list in `mkiso.sh` (and matching firmware, which is not
  packed at all yet). Without a driver there is no `/dev/dri/card0`, and
  the compositor falls back to running inside the console rather than
  owning the screen.
- The rootfs is a package manager, and now a network to reach with it.
  `dpkg` and `apt` are on every installed machine, `apt install ./something.deb`
  works off a local file, and the image carries and loads drivers for virtio,
  e1000/e1000e, r8169 and igb — so a VM gets a link, a DHCP lease and a
  `/etc/resolv.conf` written from it. Only virtio-net and e1000 have been seen
  to bind to anything; the rest are packed and untried, and `r8169` ships
  without its `rtl_nic` firmware. `docs/design/network.md` records what was
  actually observed.
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
- An installed machine cannot `modprobe`. The kernel modules are in the
  initramfs and nowhere else, and `switch_root` deletes the initramfs, so
  every module a machine will ever have is loaded by `/init` before the pivot.
  That is why the three `nls_` modules are in its list: nothing has a device
  that needs them, but the kernel asks for a codepage by name the first time a
  FAT filesystem is mounted, and by then there is nowhere to look. Adding
  hardware to a running tOS means adding its driver to `MODULES` in `mkiso.sh`
  and rebuilding the image, which was already true of anything not in it.
- The installer has not been run against real hardware. Its logic is
  covered by tests, including the whole sequence against a recorded
  backend, but the commands it drives have only been checked for what they
  are, not for what they do to a physical disk.
