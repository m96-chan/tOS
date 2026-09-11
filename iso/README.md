# tOS ISO

Bootable image where the compositor **is** userspace:

```text
GRUB -> Linux (Debian linux-image) -> initramfs /init -> tos (DRM backend)
```

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
iso/run.sh            # QEMU, bochs-drm framebuffer, serial log on stdout
```

Pick "tOS (verbose)" in GRUB to keep kernel messages visible while
debugging boot problems. If the compositor exits, `/init` drops to an
emergency busybox shell on the console.

## Files

| file       | role                                                        |
|------------|-------------------------------------------------------------|
| `build.sh` | host entry point: runs `mkiso.sh` in a container            |
| `mkiso.sh` | container-side build: static `tos`, initramfs, `grub-mkrescue` |
| `init`     | initramfs PID 1: mounts, modprobe display/input, exec `tos` |
| `run.sh`   | boots `dist/tos-<arch>.iso` in QEMU                         |

## Known limits

- x86_64 only for now; `ARCH=aarch64` is plumbed through `build.sh` and
  `mkiso.sh` but untested, and arm64 needs a different boot path anyway.
- The initramfs carries only the QEMU-shaped display/input modules and
  their dependency closure. Real hardware needs its GPU driver added to
  the `MODULES` list in `mkiso.sh` (and matching firmware, which is not
  packed at all yet).
- No persistent storage and no real rootfs: the next step per the
  top-level README is a Debian rootfs (mmdebstrap → squashfs) that
  `/init` mounts and pivots into before starting `tos`.
