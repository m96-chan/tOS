# tOS ISO

Bootable image where the compositor **is** userspace:

```text
live       GRUB -> Linux -> initramfs /init -> tos (DRM backend)
installed  GRUB -> Linux -> initramfs /init -> switch_root -> init -> tos
```

`/init` takes the second path when the kernel command line names a `root=`,
which is what the installer writes and what the live image deliberately does
not. On an installed machine busybox init then reads the `/etc/inittab` the
installer left, runs `/etc/rc` and respawns the session.

Per the top-level README, tOS targets a **Debian** userspace; this image
is the Debian-based kernel/compositor half of that. The initramfs holds
a static `tos` binary, busybox, and the display/input driver modules.
`tos` falls back to `/bin/sh` (busybox) for its panes until the real
Debian rootfs stage (squashfs via `rootfs/debian/`) exists.

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
with a boot partition and an ext4 root, copies the live system onto it,
installs GRUB, and writes `/etc/inittab` so the installed machine starts the
compositor on the console.

It will not touch a disk until the disk's own name has been typed, and it
refuses the medium the live session booted from, anything mounted, and
anything read only.

```sh
tos-install --list       # what it can see
tos-install --plan       # every command it would run, without running any
tos-install --dry-run    # the whole interface, writing nothing
```

The kernel and initramfs are copied from the medium rather than from the
running filesystem, because a live session *is* an initramfs and does not
contain the kernel that unpacked it. `/init` mounts the medium at
`/run/live/medium` for exactly that reason; if it is not mounted the
installer stops rather than leaving a disk that cannot boot.

Pick "tOS (verbose)" in GRUB to keep kernel messages visible while
debugging boot problems. If the compositor exits, `/init` drops to an
emergency busybox shell on the console.

## Files

| file        | role                                                          |
|-------------|---------------------------------------------------------------|
| `build.sh`  | host entry point: runs `mkiso.sh` in a container               |
| `mkiso.sh`  | container-side build: static binaries, initramfs, `grub-mkrescue` |
| `init`      | initramfs PID 1: mounts, modprobe, `switch_root` into `root=`, or find the medium and exec `tos` |
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
- No real rootfs yet: the installer copies the busybox initramfs world
  onto the disk, so an installed machine is the same small system the ISO
  boots — it is a root filesystem of its own, and `/init` switches into it,
  but the contents are busybox rather than Debian. The next step per the
  top-level README is a Debian rootfs (mmdebstrap → squashfs); the installer
  is what will put that on disk once it exists, and `/init` already knows how
  to hand a machine over to whatever is there.
- The installer has not been run against real hardware. Its logic is
  covered by tests, including the whole sequence against a recorded
  backend, but the commands it drives have only been checked for what they
  are, not for what they do to a physical disk.
