#!/bin/sh
# Build a bootable tOS ISO. Runs INSIDE a rust:1-bookworm (Debian)
# container; use iso/build.sh on the host to launch it.
#
# Layout of the result:
#   kernel     Debian linux-image (virtio + DRM as modules)
#   initramfs  busybox + /sbin/tos (static musl build) + needed modules
#   bootloader GRUB (BIOS + UEFI hybrid via grub-mkrescue)
#
# Boot flow: GRUB -> kernel -> /init -> exec tos (DRM backend).
#
# The README's target userspace is a Debian rootfs; this image is the
# kernel/compositor half of that story, with busybox standing in for the
# rootfs until the squashfs stage exists.
set -eu

ARCH=$(uname -m)
case "$ARCH" in
x86_64)
    RUST_TARGET=x86_64-unknown-linux-musl
    KERNEL_PKG=linux-image-amd64
    GRUB_PKGS="grub-pc-bin grub-efi-amd64-bin"
    ;;
aarch64)
    RUST_TARGET=aarch64-unknown-linux-musl
    KERNEL_PKG=linux-image-arm64
    GRUB_PKGS="grub-efi-arm64-bin"
    ;;
*)
    echo "mkiso: unsupported architecture $ARCH" >&2
    exit 1
    ;;
esac

export DEBIAN_FRONTEND=noninteractive
apt-get update
# --no-install-recommends keeps initramfs-tools and firmware out: the
# kernel package then skips generating its own initrd, which would fail in
# a container anyway.
apt-get install -y --no-install-recommends \
    musl-tools busybox-static cpio kmod \
    "$KERNEL_PKG" \
    grub-common $GRUB_PKGS xorriso mtools

# Fully static binary: it runs as PID 1's child with no libc on disk.
rustup target add "$RUST_TARGET" 2>/dev/null || true
cargo build --release --target "$RUST_TARGET" -p tos-compositor
TOS_BIN="target/$RUST_TARGET/release/tos"

KVER=$(basename /lib/modules/*)
WORK=$(mktemp -d)
ROOT="$WORK/root"
ISODIR="$WORK/iso"
mkdir -p "$ROOT" "$ISODIR/boot/grub" dist

# --- initramfs ---------------------------------------------------------
mkdir -p "$ROOT/bin" "$ROOT/sbin" "$ROOT/dev" "$ROOT/proc" "$ROOT/sys" \
    "$ROOT/tmp" "$ROOT/root" "$ROOT/etc" "$ROOT/lib/modules/$KVER"
cp /bin/busybox "$ROOT/bin/busybox"
cp "$TOS_BIN" "$ROOT/sbin/tos"
cp iso/init "$ROOT/init"
chmod 755 "$ROOT/init"
# The kernel opens /dev/console for PID 1 before devtmpfs is mounted.
mknod -m 600 "$ROOT/dev/console" c 5 1
mknod -m 666 "$ROOT/dev/null" c 1 3

# Only the display/input drivers /init loads, plus their dependency
# closure — the full Debian module tree would be hundreds of megabytes.
MODULES="bochs virtio_gpu simpledrm cirrus vmwgfx \
    evdev atkbd i8042 psmouse virtio_input hid_generic usbhid virtio_pci"
for mod in $MODULES; do
    modprobe -S "$KVER" --show-depends "$mod" 2>/dev/null || true
done | sed -n 's/^insmod //p' | sort -u | while read -r path; do
    rel="${path#/lib/modules/$KVER/}"
    mkdir -p "$ROOT/lib/modules/$KVER/$(dirname "$rel")"
    cp "$path" "$ROOT/lib/modules/$KVER/$rel"
done
# depmod needs the metadata files next to the pruned tree.
cp "/lib/modules/$KVER/modules.order" "/lib/modules/$KVER/modules.builtin" \
    "$ROOT/lib/modules/$KVER/"
cp "/lib/modules/$KVER/modules.builtin.modinfo" \
    "$ROOT/lib/modules/$KVER/" 2>/dev/null || true
depmod -b "$ROOT" "$KVER"

(cd "$ROOT" && find . | cpio -o -H newc --quiet | gzip -9) \
    >"$ISODIR/boot/initramfs.gz"

# --- ISO ---------------------------------------------------------------
cp "/boot/vmlinuz-$KVER" "$ISODIR/boot/vmlinuz"
cat >"$ISODIR/boot/grub/grub.cfg" <<'EOF'
set timeout=1
set default=0

menuentry "tOS" {
    linux /boot/vmlinuz console=tty0 console=ttyS0 quiet
    initrd /boot/initramfs.gz
}

menuentry "tOS (verbose)" {
    linux /boot/vmlinuz console=tty0 console=ttyS0
    initrd /boot/initramfs.gz
}
EOF

ISO="dist/tos-$ARCH.iso"
grub-mkrescue -o "$ISO" "$ISODIR" --quiet
rm -rf "$WORK"
ls -lh "$ISO"
