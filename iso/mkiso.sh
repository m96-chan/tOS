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
    musl-tools busybox-static cpio kmod fonts-vlgothic skkdic \
    "$KERNEL_PKG" \
    grub-common grub2-common $GRUB_PKGS xorriso mtools \
    fdisk dosfstools e2fsprogs

# Fully static binaries: they run as PID 1's children with no libc on disk.
rustup target add "$RUST_TARGET" 2>/dev/null || true
cargo build --release --target "$RUST_TARGET" \
    -p tos-compositor -p tos-install -p tos-preview
TOS_BIN="target/$RUST_TARGET/release/tos"
INSTALLER_BIN="target/$RUST_TARGET/release/tos-install"
# The one program on the image that can show a picture. Without it the
# graphics protocol is something the compositor implements and nothing in a
# session ever asks for.
PREVIEW_BIN="target/$RUST_TARGET/release/tos-preview"

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
cp "$PREVIEW_BIN" "$ROOT/sbin/tos-preview"
cp iso/init "$ROOT/init"
chmod 755 "$ROOT/init" "$ROOT/sbin/tos" "$ROOT/sbin/tos-install" \
    "$ROOT/sbin/tos-preview"

# The message of the day, which is where a person is told that this is a live
# session and how to put it on a disk.
mkdir -p "$ROOT/etc/tos" "$ROOT/run/live/medium"
cp .motd_art "$ROOT/etc/tos/motd_art"
cp iso/profile "$ROOT/etc/profile"

# The font. Without one on the image the compositor finds nothing to load and
# falls back to its built-in ASCII face, which draws every kana as a hollow
# box; an image that cannot show Japanese is not much of a Japanese desktop.
#
# VL Gothic is the cheapest face Debian has that covers both Latin and
# Japanese. It is 4,088,728 bytes on disk and costs 2,544,069 of them once the
# initramfs is gzipped, which is 2.4 MiB on a 49.5 MiB image: measured by
# regzipping the shipped archive with the file taken out. IPAGothic would cost
# 4.3 MB and the smallest usable cut of Noto Sans CJK 13.6 MB, for a repertoire
# this image has no use for.
#
# It is also exactly fixed pitch — every Latin glyph half an em, every kana and
# kanji a full em — which is the 1:2 ratio tos-term's width table already
# assumes, so a wide character lands on two cells with nothing rescaled. That
# makes it the primary face here rather than only a fallback.
#
# Licence: M+ / Sazanami / BSD-3-Clause, all redistributable.
VLGOTHIC=usr/share/fonts/truetype/vlgothic/VL-Gothic-Regular.ttf
mkdir -p "$ROOT/$(dirname "$VLGOTHIC")"
cp "/$VLGOTHIC" "$ROOT/$VLGOTHIC"

# The dictionary. The font is what makes Japanese legible; this is what makes
# it typable. SKK-JISYO.L is a sorted text file the IME binary searches, which
# is why tOS needs no conversion daemon at all — see docs/design/ime.md.
#
# Converted here, once, rather than at run time: tOS is UTF-8 throughout and
# is not going to learn a second encoding to read one file.
#
# Debian's skkdic (20230109-1) ships it in EUC-JP — checked, not assumed.
# file(1) gets no further than "ISO-8859 text", but the dictionary's own first
# line is `;; -*- mode: fundamental; coding: euc-jp -*-`, it is not valid UTF-8
# (iconv stops at byte 1366), and `iconv -f EUC-JP` reads all 4,489,936 bytes
# of it without a single illegal sequence. That first line still says euc-jp in
# the converted copy: it is a comment inside the dictionary, not something tOS
# reads.
#
# 4,489,936 bytes of EUC-JP become 6,156,948 of UTF-8 and cost 1,995,397 of
# them once the initramfs is gzipped — measured the way the font's number was,
# by regzipping the shipped archive with the file taken out: 25,880,398 with
# it against 23,885,001 without. That is 3.7% of the image against VL Gothic's
# 2,544,069, so the data that makes Japanese input possible is a quarter
# cheaper than the face that makes it visible. The image goes from 52,379,648
# bytes to 54,376,448.
#
# After #20 (the Debian rootfs) this copy goes away: the rootfs installs
# skkdic itself, and its own /usr/share/skk/SKK-JISYO.L becomes the system
# dictionary. The path below is the initramfs stand-in until then.
#
# Licence: GPL-2+ (skk-dev/dict), redistributable.
SKKDIC=/usr/share/skk/SKK-JISYO.L
mkdir -p "$ROOT/usr/share/tos"
iconv -f EUC-JP -t UTF-8 "$SKKDIC" >"$ROOT/usr/share/tos/SKK-JISYO.L"

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

# The kernel loads a module it needs mid-syscall by running the program named
# in kernel.modprobe, which is /sbin/modprobe and not on any PATH — /init
# installs busybox's applets into /bin, so without this the kernel asks for a
# module and finds nothing to ask with.
#
# Two mounts the installer makes need it. ext4 wants crc32c from the crypto
# API, because Debian's mke2fs turns metadata_csum on: without the driver the
# root filesystem it just made answers "Cannot load crc32c driver" and refuses
# to mount. FAT wants its codepage the same way, which is the ESP under UEFI.
# Both modules are already on the image; nothing could reach them.
ln -sf /bin/busybox "$ROOT/sbin/modprobe"
(cd "$ROOT" && find . | cpio -o -H newc --quiet | gzip -9) \
    >"$ISODIR/boot/initramfs.gz"

# --- ISO ---------------------------------------------------------------
cp "/boot/vmlinuz-$KVER" "$ISODIR/boot/vmlinuz"
# The last console= on the command line is the one userspace gets as
# /dev/console, so tty0 comes last: a person watching a screen is the common
# case, and ttyS0 still carries the kernel log for anyone capturing it.
#
# sysctl.kernel.sysrq=438 is 0x1b6, which is this kernel's own
# CONFIG_MAGIC_SYSRQ_DEFAULT_ENABLE. It changes nothing the image does today;
# it means a kernel bump cannot move tOS's SysRq policy without somebody
# editing a line that says what the policy is. There is no `sysrq=` boot
# parameter — the mask is the kernel.sysrq sysctl, set through the generic
# sysctl.*= form, which the kernel applies just before it starts /init.
#
# tos.rescue is what /init wants before it execs a shell when the compositor
# exits. The live image asks for it: it has no credential, so by the rule in
# docs/design/screen-lock.md it never locks, and a root shell on an image with
# no locked session to protect is a rescue tool. The installer's command line
# does not ask for it. See docs/design/lock-other-doors.md.
cat >"$ISODIR/boot/grub/grub.cfg" <<'EOF'
set timeout=10
set default=0

menuentry "tOS" {
    linux /boot/vmlinuz console=ttyS0 console=tty0 sysctl.kernel.sysrq=438 tos.rescue quiet
    initrd /boot/initramfs.gz
}

menuentry "tOS (verbose)" {
    linux /boot/vmlinuz console=ttyS0 console=tty0 sysctl.kernel.sysrq=438 tos.rescue
    initrd /boot/initramfs.gz
}
EOF

ISO="dist/tos-$ARCH.iso"
grub-mkrescue -o "$ISO" "$ISODIR" --quiet
rm -rf "$WORK"
ls -lh "$ISO"
