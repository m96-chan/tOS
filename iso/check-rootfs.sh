#!/usr/bin/env bash
# Assert that a built ISO carries a Debian rootfs an installed machine can use.
#
#   iso/check-rootfs.sh dist/tos-x86_64.iso
#
# Called by both the iso workflow and the release workflow. It was inline in
# the first of those and absent from the second, so a tagged release could ship
# an image whose rootfs had lost apt, the installer or the Japanese face while
# the same commit was green on every other gate — the release boots, and a boot
# cannot tell you what is missing from a filesystem it does not read.
#
# mkiso.sh makes the same assertions against the tree before it is squashed,
# where a failure is cheaper and names the file. This is the other half of the
# question: that what it checked is what mksquashfs actually put on the medium.
set -euo pipefail

iso=${1:?usage: iso/check-rootfs.sh <iso>}
mount_point=$(mktemp -d)
list=$(mktemp)
cleanup() {
    sudo umount "$mount_point" 2>/dev/null || true
    rmdir "$mount_point" 2>/dev/null || true
    rm -f "$list"
}
trap cleanup EXIT

sudo mount -o loop,ro "$iso" "$mount_point"
unsquashfs -l "$mount_point/live/filesystem.squashfs" >"$list"
sudo umount "$mount_point"

# Debian is usr-merged, so /usr/bin is where /bin/apt really lives. tos-session
# is what /init execs after the pivot, and init is what an installed machine's
# inittab is read by; without either, the disk boots to nothing.
for path in usr/bin/dpkg usr/bin/apt usr/bin/bash usr/bin/mount \
    usr/sbin/init usr/sbin/tos usr/sbin/tos-install \
    usr/sbin/tos-session var/lib/dpkg/status; do
    grep -qx "squashfs-root/$path" "$list" || {
        echo "the rootfs has no /$path" >&2
        exit 1
    }
done

# The Japanese face, which nothing else here would miss: the compositor falls
# back to its built-in bitmap and every kana turns into a hollow box, with the
# boot and every assertion above still green.
grep -q "^squashfs-root/usr/share/fonts/truetype/vlgothic/" "$list" || {
    echo "the rootfs carries no Japanese face" >&2
    exit 1
}

echo "rootfs entries: $(wc -l <"$list")"
