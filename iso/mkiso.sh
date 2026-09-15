#!/bin/sh
# Build a bootable tOS ISO. Runs INSIDE a rust:1-bookworm (Debian)
# container; use iso/build.sh on the host to launch it.
#
# Layout of the result:
#   kernel     Debian linux-image (virtio + DRM as modules)
#   initramfs  busybox + /sbin/tos (static musl build) + needed modules
#   rootfs     minimal Debian bookworm as a squashfs: glibc, dpkg, apt, bash,
#              systemd
#   bootloader GRUB (BIOS + UEFI hybrid via grub-mkrescue)
#
# Boot flow: GRUB -> kernel -> /init -> squashfs under a tmpfs overlay ->
# switch_root -> systemd -> tos-session.service -> /sbin/tos-session -> tos
# (DRM backend). An installed machine is the same from switch_root on, which
# is the point of #110: one answer to "what is PID 1 here", not two.
#
# The README's target userspace is Debian, and the squashfs is it. The
# initramfs is no longer the system: it is the few megabytes that find the
# medium, put a writable Debian together out of it, and get out of the way.
# It keeps a session of its own only as a rescue path, for a machine where
# the rootfs cannot be mounted at all.
set -eu

ARCH=$(uname -m)
case "$ARCH" in
x86_64)
    RUST_TARGET=x86_64-unknown-linux-musl
    KERNEL_PKG=linux-image-amd64
    GRUB_PKGS="grub-pc-bin grub-efi-amd64-bin"
    DEB_ARCH=amd64
    ;;
aarch64)
    RUST_TARGET=aarch64-unknown-linux-musl
    KERNEL_PKG=linux-image-arm64
    GRUB_PKGS="grub-efi-arm64-bin"
    DEB_ARCH=arm64
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
    musl-tools busybox-static cpio kmod skkdic \
    mmdebstrap squashfs-tools \
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

# dist/ is written by this container, which is root, into a directory bind
# mounted from the host — so everything in it comes out owned by root. The host
# has no passwordless sudo, so a root-owned dist/ cannot be deleted and
# `git worktree remove` fails on the worktree the image was built in, until
# somebody works out why.
#
# On EXIT rather than at the end, because a build that failed is the one that
# leaves it that way most often: the mirror was unreachable, an assertion about
# the rootfs fired, cargo did not compile. build.sh says who invoked it; a
# build run some other way skips this and keeps what it had.
hand_dist_back() {
    if [ -n "${HOST_UID:-}" ] && [ -n "${HOST_GID:-}" ]; then
        chown -R "$HOST_UID:$HOST_GID" dist 2>/dev/null || true
    fi
}
trap hand_dist_back EXIT

# --- initramfs ---------------------------------------------------------
mkdir -p "$ROOT/bin" "$ROOT/sbin" "$ROOT/dev" "$ROOT/proc" "$ROOT/sys" \
    "$ROOT/tmp" "$ROOT/root" "$ROOT/etc" "$ROOT/lib/modules/$KVER"
# The rescue session's whole toolbox, and one thing about it belongs on the
# record rather than in an issue nobody reads twice: `busybox wget -T SEC`
# stores the parsed number through a null pointer and dies of SIGSEGV before
# it has looked at the URL (#96). Nothing about this image causes it. Debian's
# busybox and busybox-static do it alike, a plain bookworm container does it,
# and so does upstream 1.37: networking/wget.c declares the option `T:+`
# whatever the configuration, then hands getopt32 a NULL destination for it
# wherever FEATURE_WGET_TIMEOUT is compiled out — which is everywhere Debian
# builds — and getopt32.c:567 writes an int through that pointer without the
# null check the string case one line below it has. A fetch that has to give
# up on time is `timeout SEC wget ...`, through busybox's own timeout applet.
cp /bin/busybox "$ROOT/bin/busybox"
cp "$TOS_BIN" "$ROOT/sbin/tos"
cp "$INSTALLER_BIN" "$ROOT/sbin/tos-install"
cp "$PREVIEW_BIN" "$ROOT/sbin/tos-preview"
cp iso/init "$ROOT/init"
# The session script goes on both sides of the pivot: this copy runs when the
# rootfs could not be mounted, the copy in the squashfs when it could. One file
# in the tree so the two cannot drift apart.
cp iso/live-session "$ROOT/sbin/tos-session"
chmod 755 "$ROOT/init" "$ROOT/sbin/tos" "$ROOT/sbin/tos-install" \
    "$ROOT/sbin/tos-preview" "$ROOT/sbin/tos-session"

# The message of the day, which is where a person is told that this is a live
# session and how to put it on a disk.
mkdir -p "$ROOT/etc/tos" "$ROOT/run/live/medium"
cp .motd_art "$ROOT/etc/tos/motd_art"
cp iso/profile "$ROOT/etc/profile"

# Every name this machine can resolve without asking a nameserver. Debian gets
# the file from base-files; the initramfs had nothing at all, so `localhost`
# was not a name a rescue session could resolve and wget and ping both
# answered "bad address" for anything running on the machine itself. It goes
# with the loopback interface /init now brings up: either one alone still
# leaves that fetch failing, the name for want of an address and the address
# for want of a route.
#
# This file and nothing beside it, each omission measured rather than assumed.
# glibc looks in files before dns with no /etc/nsswitch.conf telling it to —
# checked by pinning a real name here and watching the lookup take the pinned
# address — so that file would only repeat what glibc already does. The NSS
# modules everyone reaches for first are a dead end for a nearby reason: since
# glibc 2.34 nss_files and nss_dns are inside libc itself, so this static
# busybox dlopens nothing and resolves names here with no module on disk.
# /etc/services is absent because nothing asks for it — busybox's wget never
# calls getservbyname, it carries 80 and 443 itself. 336 bytes gzipped.
cat >"$ROOT/etc/hosts" <<'EOF'
127.0.0.1	localhost
::1	localhost ip6-localhost ip6-loopback
EOF

# The font and the SKK dictionary used to be copied in here, and are not any
# more: they live in the Debian rootfs below, where the session that reads them
# now runs. Together they cost 4,539,466 bytes of the gzipped initramfs, which
# is what paying for them twice would mean. What stays behind is a rescue
# session that draws Latin from the compositor's built-in ASCII face and cannot
# type Japanese — an acceptable thing for a path taken only when the rootfs
# will not mount, and not worth 4.5 MB to avoid.

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

need_tool sfdisk partx mkfs.ext4 mkfs.vfat mount umount sync
maybe_tool blkid

# GRUB is deliberately not among them. grub-install plus the module and
# template trees it reads out of /usr/lib/grub and /usr/share/grub came to
# 24,453,120 bytes raw and about 10 MB of this gzip — near half the
# initramfs — and the rootfs carries the same files again for the installer,
# which has lived there since #20. So a live session and an installed machine
# lose nothing at all.
#
# The one session that loses something is the rescue one, which is the
# initramfs and has no rootfs to reach: it can still partition a disk and copy
# a system onto it, and what it left behind would have nothing to start it. So
# tos-install looks for a grub-install before it offers to erase anything —
# see Bootloader::detect and Plan::refusal in installer/src/plan.rs — and a
# rescue session now refuses rather than producing that disk.

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
#
# Then the network cards. Everything tOS can do to a network — enumerating
# /sys/class/net, SIOCSIFFLAGS, the DHCP client, the whole super+shift+n menu
# — needs an interface to do it to, and until these were packed a tOS VM had
# `lo` and nothing else: the adapter sat on the PCI bus with no driver bound
# and the menu answered "no wired or wireless interfaces". virtio_net and
# e1000 are the two emulations QEMU and VirtualBox actually hand out; e1000e,
# r8169 and igb are the cards a desktop or laptop is likely to have. The list
# stays a closure and not the tree: these five pull seven more between them —
# libphy, realtek, mdio_devres, i2c-algo-bit, dca, failover, net_failover —
# and the twelve together cost 618 KiB of the compressed initramfs, measured
# at 25,925,814 bytes before and 26,558,126 after. Nothing here is firmware:
# r8169 asks for rtl_nic blobs this image does not carry, so a Realtek card
# that needs one gets whatever its PHY does by default, which is untested.
# And `autofs4`, which is not hardware and is not for /init. systemd loads it
# itself at startup — `kmod-setup` carries a fixed list, and autofs is on it
# because automounts have to work before udev has made any device nodes — and
# it fails, because there is no module tree on the rootfs at all and never will
# be (see the note beside /lib/modules below). A machine that booted correctly
# has said `Failed to find module 'autofs4'` on its way in ever since systemd
# became PID 1, followed by an `[UNSUPP]` for the one automount unit Debian
# ships (#129).
#
# Loading it here is what makes both go away, and it is the only place it can
# be loaded from: the module exists only while the initramfs does. systemd
# skips its own load when `/sys/class/misc/autofs` already exists, which is
# what a loaded autofs creates, so the check passes silently and the automount
# is supported rather than unsupported. Nothing else changes — the rootfs keeps
# no modules, and this costs the one module.
MODULES="bochs virtio_gpu simpledrm cirrus vmwgfx vboxvideo \
    evdev atkbd i8042 psmouse virtio_input hid_generic usbhid virtio_pci \
    sd_mod sr_mod cdrom ata_piix ahci libahci virtio_blk virtio_scsi \
    nvme usb_storage uas xhci_pci ehci_pci ohci_pci sdhci_pci mmc_block \
    virtio_net e1000 e1000e r8169 igb \
    isofs ext4 vfat nls_cp437 nls_iso8859_1 nls_ascii \
    crc32c_generic crc32c_intel autofs4"
# The rootfs stack, kept in its own list because it is not about what hardware
# the machine has: these three are how /init turns one read-only file on the
# medium into a writable Debian. `loop` makes the squashfs a block device,
# `squashfs` reads it, `overlay` puts a tmpfs in front so the live session can
# be written to. Without any one of them the machine falls back to the
# initramfs session.
ROOTFS_MODULES="loop squashfs overlay"

# And that the image packs everything /init will reach for. The list is
# written down twice — here, and in the loop at the top of iso/init — because
# one of them says what to carry and the other says when to load it, and only
# this one is read by the thing that packs. This one may hold more: libahci is
# named here because it is a dependency and never loaded by name. It may not
# hold less.
#
# The way it comes to hold less is somebody adding a module to iso/init
# because a boot needed it, which is a change that appears to work: modprobe
# says nothing about a module it cannot find, /init throws its output away,
# and what breaks is a mount three steps into erasing somebody's disk.
packed=" $MODULES $ROOTFS_MODULES "
missing=
for mod in $(sed -n '/^for mod in /,/; do$/p' iso/init |
    sed 's/^for mod in //; s/; do$//; s/\\$//'); do
    case "$packed" in
    *" $mod "*) ;;
    *) missing="$missing $mod" ;;
    esac
done
if [ -n "$missing" ]; then
    echo "mkiso: iso/init loads modules this image does not pack:$missing" >&2
    exit 1
fi
for mod in $MODULES $ROOTFS_MODULES; do
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

# Asserted here rather than left to a boot test, because losing it costs
# nothing anybody watches: the image still builds, still boots, and both CI
# workflows stay green on a machine that cannot resolve its own name. Checking
# for the name and not the file, since an /etc/hosts without `localhost` in it
# is the same machine with a longer path to the same surprise. The fetch that
# would prove the rest of this needs a network, and a smoke boot that reaches
# the internet is a gate that fails on somebody else's outage.
if ! grep -q '[[:space:]]localhost' "$ROOT/etc/hosts" 2>/dev/null; then
    echo "mkiso: the initramfs has no /etc/hosts naming localhost" >&2
    exit 1
fi

(cd "$ROOT" && find . | cpio -o -H newc --quiet | gzip -9) \
    >"$ISODIR/boot/initramfs.gz"

# --- Debian rootfs -----------------------------------------------------
# What a tOS machine is once it has finished booting. Before this the answer
# was "the initramfs", which meant no dpkg, no apt and no way to add a single
# program to a machine for the rest of its life (#83). A real glibc, a real
# dpkg and a working apt are the whole point of choosing Debian, and this is
# where they come from.
#
# mmdebstrap rather than debootstrap because it needs no privilege the build
# container does not already have: it notices it cannot mount /proc inside the
# chroot, says so, and carries on. debootstrap would need a privileged
# container, and the ISO build is something CI has to be able to run.
ROOTFS="$WORK/rootfs"
# http and not https, deliberately. apt checks the archive's signature against
# debian-archive-keyring whichever transport carried it, so TLS would add no
# integrity this machine does not already have; what it would add is a
# dependency on the clock. tOS runs no time sync of any kind, and on a board
# with a flat RTC battery it comes up in 1970, where every certificate is not
# yet valid and apt fails on every mirror. A machine that cannot reach
# security.debian.org is the whole of #98, and https is one more way to arrive
# there. ca-certificates stays installed for everything else the machine will
# want to talk to.
MIRROR=${MIRROR:-http://deb.debian.org/debian}
SECURITY_MIRROR=${SECURITY_MIRROR:-http://security.debian.org/debian-security}
SUITE=${SUITE:-bookworm}

# Never unpacked rather than deleted afterwards, so the rule also governs
# everything apt installs later: a machine whose whole disk is a squashfs
# should not spend it on manual pages it has no pager story for. Copyright
# files stay — they are the terms under which this image may be handed to
# anybody at all.
cat >"$WORK/tos-minimal" <<'EOF'
# Written by iso/mkiso.sh. See the note there.
path-exclude=/usr/share/doc/*
path-include=/usr/share/doc/*/copyright
path-exclude=/usr/share/man/*
path-exclude=/usr/share/locale/*
path-exclude=/usr/share/info/*
EOF

# `--variant=apt` is essential plus apt, which is the smallest thing that can
# still install a package. Everything beyond it is named here with a reason:
#
#   debian-archive-keyring  apt verifies the archive signature against it, and
#                           without it `apt update` fails on every mirror.
#   ca-certificates         and the same for an https mirror.
#   bash                    #82, and the shell people actually expect. The
#                           session still runs /bin/sh; see iso/live-session.
#   systemd-sysv            PID 1, on this image and on every machine
#                           installed from it (#110). Debian's essential set
#                           contains no init at all, and what stood here was
#                           busybox's, reading an /etc/inittab the installer
#                           wrote: three lines, enough for exactly one
#                           program. A Debian package that ships a unit — and
#                           newer ones increasingly ship nothing else — now
#                           works when it is installed, with nothing written
#                           on tOS's side. 13 packages and about 13.5 MB
#                           unpacked, measured the same way as the pair
#                           below, which is what `ip`, `ps` and `free`
#                           already cost. docs/design/init.md has the whole
#                           of the decision.
#   busybox                 not PID 1 any more, and nothing else here needs
#                           it: the installer runs Debian's sfdisk, mkfs,
#                           mount, unsquashfs and grub-install, and a pane's
#                           shell is bash or dash. It is on the image because
#                           the initramfs's busybox-static is a different
#                           build and #109 is still open about what a tOS
#                           machine should be able to type; it is not load
#                           bearing here.
#   iproute2 procps         `ip`, `ps` and `free`: the first things anybody
#                           types at a machine they cannot see inside (#97).
#                           busybox carries applets by all three names and
#                           Debian's package leaves them off PATH; symlinking
#                           them there is free and was turned down anyway,
#                           because busybox's `ip` prints a format no recipe
#                           and no manual page agrees with, and somebody who
#                           learns this machine's `ip` learns it wrong. 18
#                           packages and 13,445,033 bytes unpacked, measured
#                           in a bookworm container with recommends off and
#                           the excludes above in place.
#   iputils-ping wget       whether this machine reaches anything, and whether
#                           it can fetch (#109). The same argument as the line
#                           above and the same answer: busybox carries `ping`
#                           and `wget` and leaves both off PATH, and naming
#                           them there was turned down again — busybox's
#                           `wget` segfaults on `-T`, the first flag anybody
#                           reaches for (#96), which is a `wget` that is not
#                           the `wget` people know failing in a way no manual
#                           page describes. 3 packages and 644,402 bytes
#                           unpacked between them, measured the same way: five
#                           per cent of what `iproute2 procps` already cost to
#                           answer the neighbouring question.
#                           Not `curl`: ten packages and five times `wget` on
#                           its own, saying nothing `wget` does not for "does
#                           this reach the network", so it is worth its own
#                           argument rather than riding in on this one. And
#                           not an `nslookup`, because names already resolve —
#                           `getent hosts` is in libc-bin, which is essential,
#                           so a DNS fault can already be told from the other
#                           three without adding anything.
#   ncurses-base            the terminfo for the TERM tOS advertises. Without
#                           it apt's own progress bar has nothing to draw on.
#   fonts-vlgothic          the face the compositor loads, now dpkg's problem
#                           rather than a file copied past it.
#   e2fsprogs dosfstools    the installer makes these filesystems, and it now
#   fdisk util-linux        runs here rather than in the initramfs.
#   mount                   and so it needs a mount(8): Debian split it out of
#                           util-linux, and nothing essential pulls it back
#                           in. A rootfs without it installs as far as the
#                           Mount step and stops there — found by booting one.
#   grub2-common $GRUB_PKGS the installer's bootloader step, likewise.
#   squashfs-tools          how the installer unpacks this very rootfs.
#   kmod                    modprobe for a machine that has pivoted.
#   sudo                    the only way to be root on an installed machine.
#                           A session's panes run as the person now (#112), and
#                           Debian's root carries `*`, so `su` has nothing to
#                           accept and without this the machine cannot install
#                           a package on itself. It authenticates against the
#                           `/etc/shadow` line #111 put there, through PAM,
#                           which is the reason that decision was taken. The
#                           installer writes the drop-in that names the person;
#                           this carries the program and creates the
#                           `/etc/sudoers.d` the drop-in goes in.
#   udev                    the thing that makes `/dev/disk/by-label` exist.
#                           Debian ships it as its own binary package and
#                           nothing here pulls it: `systemd-sysv` depends on
#                           `systemd`, and `systemd` only *recommends* `udev`,
#                           which mmdebstrap does not install. Without it a
#                           machine boots with systemd as PID 1 and no device
#                           units at all, so every `LABEL=` line in fstab that
#                           is not already mounted waits 90 seconds for a
#                           device node nobody will create and then fails
#                           `local-fs.target`. `/` survives that — the
#                           initramfs mounted it before systemd started — so
#                           the one line that actually breaks is the ESP, and
#                           only a UEFI install has one. That install then
#                           lands in emergency mode, where `sulogin` says
#                           "the root account is locked" and means it: Debian's
#                           root carries `*` and tOS never changes it, so there
#                           is no way back in. Found by installing 37322ed8 and
#                           rebooting it; `blkid` could read both labels the
#                           whole time, which is what made it look like a
#                           filesystem problem rather than a missing package.
ROOTFS_PACKAGES="debian-archive-keyring,ca-certificates,bash,systemd-sysv,udev,\
busybox,iproute2,procps,iputils-ping,wget,ncurses-base,fonts-vlgothic,\
e2fsprogs,dosfstools,\
fdisk,util-linux,mount,kmod,sudo,squashfs-tools,grub2-common,\
$(echo "$GRUB_PKGS" | tr ' ' ',')"

# Three suites and not one. `bookworm` is the frozen release: a point release
# folds security fixes back into it, so an image built later picks some of them
# up, but a machine already installed from an older one never does. It is
# subscribed to a suite that does not move between point releases, and `apt
# update` tells it it is up to date while every fix published since goes past
# it (#98). -security is where those appear first; -updates carries the changes
# that are not security but cannot wait for the point release either.
#
# mmdebstrap takes the extra suites as further mirror arguments and installs
# from them as well as writing them to sources.list, so the image ships the
# fixed package rather than merely being able to fetch it afterwards. Measured
# against the package set above that costs no additional packages and 6.7 kB
# more to download — the whole of it being a newer ca-certificates.
mmdebstrap \
    --mode=root \
    --variant=apt \
    --architectures="$DEB_ARCH" \
    --include="$ROOTFS_PACKAGES" \
    --aptopt='Acquire::Retries "3"' \
    --setup-hook="copy-in $WORK/tos-minimal /etc/dpkg/dpkg.cfg.d" \
    "$SUITE" "$ROOTFS" "$MIRROR" \
    "deb $SECURITY_MIRROR $SUITE-security main" \
    "deb $MIRROR $SUITE-updates main"

# The build has one job and it is this; a rootfs that reached here without the
# programs the issue is about is not worth putting on an image. The mount and
# unsquashfs lines are here because tos-install runs inside this rootfs now,
# and a missing one of those is an installation that stops halfway across a
# disk it has already partitioned.
for program in /usr/bin/apt /usr/bin/dpkg /bin/bash /bin/busybox \
    /bin/mount /bin/umount /usr/bin/unsquashfs /usr/sbin/sfdisk \
    /usr/sbin/mkfs.ext4 /usr/sbin/grub-install; do
    if [ ! -x "$ROOTFS$program" ]; then
        echo "mkiso: the rootfs has no $program" >&2
        exit 1
    fi
done

# The face the compositor loads is a package in here now rather than a file
# copied past dpkg, which means nothing in the tree fails if it goes missing:
# the compositor falls back to its built-in ASCII face and every kana becomes
# a hollow box, with the boot and every other assertion still green.
if [ ! -f "$ROOTFS/usr/share/fonts/truetype/vlgothic/VL-Gothic-Regular.ttf" ]; then
    echo "mkiso: the rootfs carries no Japanese face" >&2
    exit 1
fi

# And that there is a PID 1 in here, and that it is the one this image means.
# A root with no init is a disk that panics on every boot — and the live image
# would panic with it now that it execs the same program (#110).
#
# Every one of these is a symlink that systemd-sysv ships absolute:
# /sbin/init -> /lib/systemd/systemd, and reboot, poweroff and halt ->
# /bin/systemctl. A plain `[ -x ]` resolves an absolute symlink against the
# *build container's* root, which has no systemd at all, so it answers about
# the wrong machine — the same trap iso/init documents at `executable_in_root`,
# and it answers "missing" for a rootfs that is perfectly good. Follow them by
# hand instead, re-rooting every absolute target as we go.
rootfs_exec() {
    target=$1
    hops=0
    while [ -L "$ROOTFS$target" ] && [ "$hops" -lt 8 ]; do
        link=$(readlink "$ROOTFS$target")
        case $link in
        /*) target=$link ;;
        *) target="${target%/*}/$link" ;;
        esac
        hops=$((hops + 1))
    done
    [ -x "$ROOTFS$target" ]
}

init=$(readlink "$ROOTFS/sbin/init" 2>/dev/null || true)
case $init in
*/systemd) ;;
*)
    echo "mkiso: /sbin/init in the rootfs is ${init:-not a symlink}, not systemd" >&2
    exit 1
    ;;
esac
# And the three programs a person turns the machine off with, which belong to
# the init system and arrive with systemd-sysv rather than being linked here.
for program in /sbin/init /sbin/reboot /sbin/poweroff /sbin/halt; do
    if ! rootfs_exec "$program"; then
        echo "mkiso: the rootfs cannot execute $program" >&2
        exit 1
    fi
done

# And that mmdebstrap wrote the suites it was handed. It is free to arrange
# them as it likes — a one-line sources.list today, deb822 under some future
# version — and a rootfs subscribed to bookworm alone is indistinguishable
# from a correct one until a CVE is published, which is too late to find out.
for suffix in security updates; do
    if ! grep -qr -- "$SUITE-$suffix" "$ROOTFS/etc/apt/sources.list" \
        "$ROOTFS/etc/apt/sources.list.d"; then
        echo "mkiso: the rootfs is not subscribed to $SUITE-$suffix" >&2
        exit 1
    fi
done

# tOS itself, on top of Debian. The binaries are the same static musl ones the
# initramfs carries: nothing here links against the rootfs, which is what makes
# a broken upgrade inside the rootfs survivable.
cp "$TOS_BIN" "$ROOTFS/sbin/tos"
cp "$INSTALLER_BIN" "$ROOTFS/sbin/tos-install"
cp "$PREVIEW_BIN" "$ROOTFS/sbin/tos-preview"
cp iso/live-session "$ROOTFS/sbin/tos-session"
chmod 755 "$ROOTFS/sbin/tos" "$ROOTFS/sbin/tos-install" \
    "$ROOTFS/sbin/tos-preview" "$ROOTFS/sbin/tos-session"

# PID 1 on both images, and what starts the session on both (#110).
#
# The unit goes in /etc rather than /lib/systemd/system because tOS writes it
# by hand and dpkg owns that other directory; /etc is also where it wins any
# argument about a name. Enabling it is a symlink, not `systemctl enable`,
# because this is a rootfs being assembled in a container and not a running
# machine: `enable` is that symlink, and there is nothing here it could ask.
#
# Masking the gettys is the other half of the tty1 question the init decision
# had to answer. getty@tty1 would take the console the compositor is drawing
# on, and a serial getty — which systemd's generator adds by itself for every
# console= on the kernel command line — is a login prompt on a line anybody
# with the cable can reach. docs/design/lock-other-doors.md's rule stands
# until #112 decides what a login on this machine is: one door, and the
# compositor is it.
mkdir -p "$ROOTFS/etc/systemd/system/multi-user.target.wants"
cat >"$ROOTFS/etc/systemd/system/tos-session.service" <<'EOF'
# Written by iso/mkiso.sh. The tOS session is the machine's reason to boot.
[Unit]
Description=tOS session
Documentation=https://github.com/m96-chan/tOS
Conflicts=getty@tty1.service
After=systemd-user-sessions.service getty@tty1.service

[Service]
# /sbin/tos-session is iso/live-session: the environment, and then the
# compositor. TOS_USER says whose session it is, and an installed machine
# overrides it with a drop-in naming the person the installer was told about.
ExecStart=/sbin/tos-session
# The restart loop that used to be inside that script, and the respawn line
# that used to be in an inittab. One mechanism, here, where `systemctl status`
# can say how often it has fired.
Restart=always
RestartSec=1
# A controlling terminal, because the rescue shell the session drops to under
# tos.rescue is only a shell if it has one. The compositor takes the VT it
# finds itself on, which is this one.
StandardInput=tty
TTYPath=/dev/tty1
TTYReset=yes
TTYVHangup=yes
UtmpIdentifier=tty1

[Install]
WantedBy=multi-user.target
EOF
ln -sf ../tos-session.service \
    "$ROOTFS/etc/systemd/system/multi-user.target.wants/tos-session.service"
# getty.target is what pulls one in at boot, on tty1 and on every console= the
# generator finds; autovt@ is the one logind starts on demand when somebody
# switches to an empty VT. A symlink to /dev/null is what "masked" is on disk.
for unit in getty.target getty@.service serial-getty@.service autovt@.service; do
    ln -sf /dev/null "$ROOTFS/etc/systemd/system/$unit"
done

# What this machine boots to. Debian's own default is graphical.target, which
# is a target tOS has nothing under: there is no display manager and no X.
# multi-user.target is the truth, and it is what the unit above is wanted by.
ln -sf /lib/systemd/system/multi-user.target "$ROOTFS/etc/systemd/system/default.target"

# An empty machine-id, which is how that file says "not set yet": systemd
# generates one on the first boot and commits it. Shipping a filled-in one
# would give every machine installed from this image the same identity, and
# shipping no file at all leaves systemd deciding the root is not one it can
# initialise.
: >"$ROOTFS/etc/machine-id"

# The message of the day reaches a shell through /etc/profile, and Debian's
# own /etc/profile is a file with opinions this has no business replacing.
# It sources /etc/profile.d/*.sh, so tOS's part goes in there beside it.
mkdir -p "$ROOTFS/etc/tos" "$ROOTFS/etc/profile.d" "$ROOTFS/run/live/medium"
cp .motd_art "$ROOTFS/etc/tos/motd_art"
cp iso/profile "$ROOTFS/etc/profile.d/tos.sh"

# And through ~/.bashrc for the shell a pane actually runs, which is an
# interactive shell that is not a login shell — the one case bash reads that
# file and neither /etc/profile nor $ENV. /root is the one that matters, since
# the session runs as root and every pane inherits its HOME; /etc/skel is for
# the account the installer creates, so that `su -` lands somewhere furnished
# rather than on a bare `bash-5.2$`.
mkdir -p "$ROOTFS/etc/skel" "$ROOTFS/root"
cp iso/bashrc "$ROOTFS/root/.bashrc"
cp iso/bashrc "$ROOTFS/etc/skel/.bashrc"
# And ~/.profile beside it, because a login shell is the one kind of shell
# that does not read ~/.bashrc: `su - tos` would otherwise get none of what
# the pane it was typed in has. This is the file that hands it over.
cp iso/dot-profile "$ROOTFS/root/.profile"
cp iso/dot-profile "$ROOTFS/etc/skel/.profile"

# mmdebstrap leaves the build machine's own /etc/resolv.conf in the rootfs. On
# a container host that is the container's: a resolver address that means
# nothing on any machine this image is carried to, and a search domain that
# tells everybody who boots the ISO what network it was built on. The
# compositor writes this file itself when a DHCP lease arrives — see
# compositor/tos-system — so what belongs here before then is nothing at all.
: >"$ROOTFS/etc/resolv.conf"
# Likewise the host name, which the installer overwrites on a machine it puts
# on a disk. This is the one a live session answers to.
echo tos >"$ROOTFS/etc/hostname"

# And the file that resolves it and the loopback, which no Debian package
# ships: /etc/hosts is written by the installer that put the system there, and
# mmdebstrap is not one. The initramfs got its own copy with #96; this is the
# same hole on the other side of the pivot, where it is easier to miss because
# a live session usually has a DNS server to ask and VirtualBox's answers for
# `localhost`. A machine should not have to ask anyone where its own loopback
# is — without this, `wget http://localhost/` on a live session with no lease
# yet says `bad address`, which reads as a network fault and is not one.
#
# The installer writes the same two names plus the host it was given, so an
# installed machine does not inherit this file; it is for the live session and
# for anything that runs before an install.
cat >"$ROOTFS/etc/hosts" <<'EOF'
127.0.0.1	localhost
127.0.1.1	tos
::1	localhost ip6-localhost ip6-loopback
EOF

# No /lib/modules here either, so the initramfs holds the only copy. The cost
# is worth saying plainly: a machine that has pivoted cannot modprobe anything
# ever again — switch_root deletes the initramfs, and this is the directory the
# modprobe left in the rootfs would have looked in.
#
# What it does not cost is anything the image does by itself. This was a copy
# of the same pruned tree, so the only modules it could ever have offered are
# the ones iso/init loads before the pivot, and iso/init now loads all of them.
# The three NLS modules were the difference, and the note beside them there
# says why the kernel wanted one after the pivot.

# The dictionary, converted out of the build container's skkdic rather than
# installed into the rootfs as a package.
#
# Debian's skkdic (20230109-1) ships SKK-JISYO.L in EUC-JP — checked, not
# assumed. file(1) gets no further than "ISO-8859 text", but the dictionary's
# own first line is `;; -*- mode: fundamental; coding: euc-jp -*-`, it is not
# valid UTF-8 (iconv stops at byte 1366), and `iconv -f EUC-JP` reads all
# 4,489,936 bytes of it without one illegal sequence. tOS is UTF-8 throughout
# and is not going to learn a second encoding to read one file, so the
# conversion happens here, once, rather than at every boot.
#
# Installing the package as well would put both encodings on the image — the
# 4.5 MB original that nothing can read beside the 6.2 MB copy that everything
# can. /usr/share/tos/SKK-JISYO.L is searched before /usr/share/skk, so the
# package would also be the copy that loses. See compositor/tos-ime.
#
# Licence: GPL-2+ (skk-dev/dict), redistributable.
SKKDIC=/usr/share/skk/SKK-JISYO.L
mkdir -p "$ROOTFS/usr/share/tos"
iconv -f EUC-JP -t UTF-8 "$SKKDIC" >"$ROOTFS/usr/share/tos/SKK-JISYO.L"

# The dpkg configuration above governs what apt unpacks from here on, but not
# what is already unpacked: mmdebstrap extracts the essential set with tar
# rather than through dpkg, so those packages' documentation arrives whatever
# dpkg has been told. Take it out by hand to match, or the rule would be one
# that applies only to packages somebody adds later.
#
# Worth 16,650,240 bytes of the squashfs, measured by compressing the same
# rootfs both ways: 74,674,176 with these trees and 58,023,936 without. Most
# of it is /usr/share/locale, 31.8 MB of translated messages that nothing can
# currently display — the rootfs has no `locales` package, so no locale is
# generated and every program falls back to C. Installing `locales` and
# deleting the locale line from the dpkg configuration is the pair of changes
# that would make them worth carrying.
rm -rf "$ROOTFS/usr/share/man" "$ROOTFS/usr/share/locale" \
    "$ROOTFS/usr/share/info"
find "$ROOTFS/usr/share/doc" -mindepth 2 ! -name copyright -delete 2>/dev/null ||
    true
# grub-install opens this directory to find its translations and warns on the
# console when it cannot. The warning is harmless and the installation finishes
# either way, but it lands in the one log a person installing tOS is actually
# reading, where it looks like something went wrong. An empty directory costs
# nothing and says the true thing: there are no translations here.
mkdir -p "$ROOTFS/usr/share/locale"

# zstd rather than the default gzip or the live-image usual xz. The squashfs
# is read while somebody waits — every page of every binary the session starts
# comes through it — and zstd decompresses several times faster than xz for a
# few per cent more image. Debian's kernel builds SQUASHFS_ZSTD in, so nothing
# has to be loaded before the root can be read. 1 MiB blocks because the
# alternative is compressing a rootfs 128 KiB at a time.
mkdir -p "$ISODIR/live"
mksquashfs "$ROOTFS" "$ISODIR/live/filesystem.squashfs" \
    -comp zstd -Xcompression-level 19 -b 1M -noappend -quiet -no-progress

# What minimal Debian costs, which #20 asks for in as many words. Printed
# every build rather than written into a comment that would go stale.
echo "mkiso: rootfs $(du -sb "$ROOTFS" | cut -f1) bytes unpacked, \
$(stat -c%s "$ISODIR/live/filesystem.squashfs") bytes squashed"
echo "mkiso: initramfs $(stat -c%s "$ISODIR/boot/initramfs.gz") bytes gzipped"

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
# tos.rescue is what the session wants before it execs a shell when the
# compositor exits, and it is a menu entry of its own rather than something
# the ordinary ones carry (#112). Both used to name it, so `ctrl+a q` on a
# live image landed on an unauthenticated root shell on tty0 — which was the
# default way to boot the image, on a machine anybody could be standing at.
# What the ordinary entries do now is what an installed machine does: the
# session ends and another one starts. A person who wants the shell chooses
# it, and can see in the menu that they are choosing it.
#
# The live image still asks nobody for a password, and that is the credential
# rule rather than this flag: its only account is Debian's root, which carries
# `*`. See docs/design/login.md and docs/design/lock-other-doors.md.
cat >"$ISODIR/boot/grub/grub.cfg" <<'EOF'
set timeout=10
set default=0

menuentry "tOS" {
    linux /boot/vmlinuz console=ttyS0 console=tty0 sysctl.kernel.sysrq=438 quiet loglevel=3
    initrd /boot/initramfs.gz
}

menuentry "tOS (verbose)" {
    linux /boot/vmlinuz console=ttyS0 console=tty0 sysctl.kernel.sysrq=438
    initrd /boot/initramfs.gz
}

menuentry "tOS (rescue shell)" {
    linux /boot/vmlinuz console=ttyS0 console=tty0 sysctl.kernel.sysrq=438 tos.rescue
    initrd /boot/initramfs.gz
}
EOF

ISO="dist/tos-$ARCH.iso"
grub-mkrescue -o "$ISO" "$ISODIR" --quiet
rm -rf "$WORK"

# Everything above ran as root inside the container, so dist/ and the image in
# it come out owned by root on the host — handed back by the EXIT trap set at
# the top of this file, which runs whether the build got this far or not.
ls -lh "$ISO"
