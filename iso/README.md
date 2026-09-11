# tOS ISO

Bootable image where the compositor **is** userspace:

```text
GRUB -> Linux (Alpine linux-virt) -> initramfs /init -> tos (DRM backend)
```

There is no distribution underneath — the initramfs holds a static `tos`
binary, busybox, and the kernel's driver modules. `tos` falls back to
`/bin/sh` (busybox) for its panes.

## Building

Needs Docker or Podman on the host; the build itself runs in a
`rust:1-alpine` container.

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

- x86_64 only for now; `ARCH=aarch64` is plumbed through `build.sh` but
  the GRUB/kernel side of `mkiso.sh` has only been designed for x86_64.
- The full `linux-virt` module tree is copied into the initramfs. It
  boots everywhere QEMU-shaped but the ISO is larger than it needs to
  be; pruning to gpu/input/virtio subtrees is a later optimization.
- Real-hardware boot (USB stick) should work via GRUB's hybrid image but
  is untested; `linux-virt` lacks most bare-metal drivers, so a
  `linux-lts` variant will be needed for that.
