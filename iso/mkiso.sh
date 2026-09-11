#!/bin/sh
# Build a bootable tOS ISO. Runs INSIDE a rust:alpine container; use
# iso/build.sh on the host to launch it.
#
# Layout of the result:
#   kernel     Alpine linux-virt (virtio + DRM as modules)
#   initramfs  busybox + /sbin/tos (static musl build) + kernel modules
#   bootloader GRUB (BIOS + UEFI hybrid via grub-mkrescue)
#
# Boot flow: GRUB -> kernel -> /init -> exec tos (DRM backend).
set -eu

ARCH=$(uname -m)
case "$ARCH" in
x86_64) RUST_TARGET=x86_64-unknown-linux-musl ;;
aarch64) RUST_TARGET=aarch64-unknown-linux-musl ;;
*)
    echo "mkiso: unsupported architecture $ARCH" >&2
    exit 1
    ;;
esac

apk add --no-cache musl-dev linux-virt busybox-static \
    grub grub-bios grub-efi xorriso mtools cpio

# The rust:alpine image sets RUSTFLAGS=-crt-static; we want a fully static
# binary that runs as PID 1 with no libc on disk.
export RUSTFLAGS="-C target-feature=+crt-static"
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
    "$ROOT/tmp" "$ROOT/root" "$ROOT/etc" "$ROOT/lib"
cp /bin/busybox.static "$ROOT/bin/busybox"
cp "$TOS_BIN" "$ROOT/sbin/tos"
cp iso/init "$ROOT/init"
chmod 755 "$ROOT/init"
# The kernel opens /dev/console for PID 1 before devtmpfs is mounted.
mknod -m 600 "$ROOT/dev/console" c 5 1
mknod -m 666 "$ROOT/dev/null" c 1 3
# Full module tree: display and input drivers are modular in linux-virt,
# and /init modprobes what the machine actually has.
cp -a "/lib/modules/$KVER" "$ROOT/lib/modules/"

(cd "$ROOT" && find . | cpio -o -H newc --quiet | gzip -9) \
    >"$ISODIR/boot/initramfs.gz"

# --- ISO ---------------------------------------------------------------
cp "/boot/vmlinuz-virt" "$ISODIR/boot/vmlinuz"
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
