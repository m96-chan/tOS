# tOS ISO

Bootable image where the compositor **is** userspace, on top of a real Debian:

```text
live       GRUB -> Linux -> /init -> squashfs + tmpfs overlay
                         -> switch_root -> tos-session -> tos
installed  GRUB -> Linux -> /init -> switch_root -> init -> tos
rescue     GRUB -> Linux -> /init -> tos-session -> tos   (initramfs only)
```

`/init` looks for three roots in that order and hands the machine to the first
one it finds. An installed machine names its root filesystem with `root=` on
the kernel command line, which is what the installer writes and what the live
image deliberately does not; busybox init then reads the `/etc/inittab` the
installer left, runs `/etc/rc` and respawns the session. A live medium carries
its root as one squashfs file, which `/init` mounts with a tmpfs stacked in
front so the session can be written to. A machine that finds neither stays in
the initramfs, which is a rescue session rather than a system.

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
installs GRUB, and writes `/etc/inittab` so the installed machine starts the
compositor on the console.

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
| `profile`   | sourced by every shell; prints the banner and the install hint |
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
- The session still runs `/bin/sh`, which in the rootfs is dash. `bash` is
  installed, and switching to it is two lines rather than one (#82): `SHELL` in
  `iso/live-session` for a live session, and the same export in the installer's
  `SESSION_SCRIPT` for an installed machine, which runs what the installer
  wrote and never reads `iso/live-session`. #82 is about the second of those.
  The `shell =` key in `tos.conf` outranks both.
- An installed machine's PID 1 is busybox `init`, symlinked over the rootfs's
  empty `/sbin`. Debian's essential set contains no init at all — an init
  system is a package, and tOS installs none.
- The installer has not been run against real hardware. Its logic is
  covered by tests, including the whole sequence against a recorded
  backend, but the commands it drives have only been checked for what they
  are, not for what they do to a physical disk.
