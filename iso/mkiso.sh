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
    grub-common grub2-common $GRUB_PKGS xorriso mtools \
    fdisk dosfstools e2fsprogs

# Fully static binaries: they run as PID 1's children with no libc on disk.
rustup target add "$RUST_TARGET" 2>/dev/null || true
cargo build --release --target "$RUST_TARGET" -p tos-compositor -p tos-install
TOS_BIN="target/$RUST_TARGET/release/tos"
INSTALLER_BIN="target/$RUST_TARGET/release/tos-install"

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
cp "$INSTALLER_BIN" "$ROOT/sbin/tos-install"
cp iso/init "$ROOT/init"
chmod 755 "$ROOT/init" "$ROOT/sbin/tos" "$ROOT/sbin/tos-install"

# The message of the day, which is where a person is told that this is a live
# session and how to put it on a disk.
mkdir -p "$ROOT/etc/tos" "$ROOT/run/live/medium"
cp .motd_art "$ROOT/etc/tos/motd_art"
cp iso/profile "$ROOT/etc/profile"

# The tools the installer shells out to. Unlike the compositor these are
# Debian binaries, so their libraries have to come along; the installer is
# useless without them and finding that out mid-install is no good.
#
# A tool is looked up once, through these two helpers, so that a missing one
# stops the build with its name rather than turning into `cp ''` further down.
need_tool() {
    for tool in "$@"; do
        path=$(command -v "$tool" 2>/dev/null) || path=""
        if [ -z "$path" ]; then
            echo "mkiso: $tool is missing from the build image" >&2
            exit 1
        fi
        cp "$path" "$ROOT/sbin/$(basename "$tool")"
    done
}

# Tools that improve things when present but are not required.
maybe_tool() {
    for tool in "$@"; do
        path=$(command -v "$tool" 2>/dev/null) || path=""
        [ -n "$path" ] && cp "$path" "$ROOT/sbin/$(basename "$tool")"
    done
    return 0
}

# grub-install is in grub2-common, not grub-common: grub-common carries the
# grub-mkrescue this script already used, which is why it was enough before.
need_tool sfdisk partx mkfs.ext4 mkfs.vfat mount umount sync grub-install
maybe_tool grub-mkimage grub-bios-setup grub-probe grub-mkdevicemap \
    grub-editenv grub-macbless blkid

# grub-install reads its modules and templates out of these trees.
mkdir -p "$ROOT/usr/lib/grub" "$ROOT/usr/share/grub"
cp -a /usr/lib/grub/. "$ROOT/usr/lib/grub/" 2>/dev/null || true
cp -a /usr/share/grub/. "$ROOT/usr/share/grub/" 2>/dev/null || true

# Every shared library those binaries need, resolved transitively by ldd.
copy_libraries() {
    for binary in "$@"; do
        ldd "$binary" 2>/dev/null | sed -n \
            -e 's/^[[:space:]]*\([^ ]*\) => \([^ ]*\).*/\2/p' \
            -e 's/^[[:space:]]*\(\/[^ ]*\) (0x.*/\1/p'
    done | sort -u | while read -r lib; do
        [ -f "$lib" ] || continue
        mkdir -p "$ROOT$(dirname "$lib")"
        cp -L "$lib" "$ROOT$lib"
    done
}
copy_libraries "$ROOT"/sbin/*
# The kernel opens /dev/console for PID 1 before devtmpfs is mounted.
mknod -m 600 "$ROOT/dev/console" c 5 1
mknod -m 666 "$ROOT/dev/null" c 1 3

# Only the display/input drivers /init loads, plus their dependency
# closure — the full Debian module tree would be hundreds of megabytes.
# Display and input, then the storage stack the installer needs: without a
# disk driver it sees no disks, and without the filesystem modules it cannot
# mount what it just created.
MODULES="bochs virtio_gpu simpledrm cirrus vmwgfx vboxvideo \
    evdev atkbd i8042 psmouse virtio_input hid_generic usbhid virtio_pci \
    sd_mod sr_mod cdrom ata_piix ahci libahci virtio_blk virtio_scsi \
    nvme usb_storage uas xhci_pci ehci_pci ohci_pci sdhci_pci mmc_block \
    isofs ext4 vfat nls_cp437 nls_iso8859_1 nls_ascii"
for mod in $MODULES; do
    modprobe -S "$KVER" --show-depends "$mod" 2>/dev/null || true
# `--show-depends` prints the module's default parameters after its path, so
# only the first field is a filename. nvme is the first module in the list
# that has any, which is why this held up until now.
done | sed -n 's/^insmod \([^ ]*\).*/\1/p' | sort -u | while read -r path; do
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

# Busybox provides the small tools the installer expects on PATH but that
# are not worth pulling a Debian binary in for.
for applet in blkid mkdir rm chmod hostname reboot; do
    ln -sf /bin/busybox "$ROOT/bin/$applet" 2>/dev/null || true
done
(cd "$ROOT" && find . | cpio -o -H newc --quiet | gzip -9) \
    >"$ISODIR/boot/initramfs.gz"

# --- ISO ---------------------------------------------------------------
cp "/boot/vmlinuz-$KVER" "$ISODIR/boot/vmlinuz"
# The last console= on the command line is the one userspace gets as
# /dev/console, so tty0 comes last: a person watching a screen is the common
# case, and ttyS0 still carries the kernel log for anyone capturing it.
cat >"$ISODIR/boot/grub/grub.cfg" <<'EOF'
set timeout=10
set default=0

menuentry "tOS" {
    linux /boot/vmlinuz console=ttyS0 console=tty0 quiet
    initrd /boot/initramfs.gz
}

menuentry "tOS (verbose)" {
    linux /boot/vmlinuz console=ttyS0 console=tty0
    initrd /boot/initramfs.gz
}
EOF

ISO="dist/tos-$ARCH.iso"
grub-mkrescue -o "$ISO" "$ISODIR" --quiet
rm -rf "$WORK"
ls -lh "$ISO"
