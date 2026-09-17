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
    curl ca-certificates unzip \
    "$KERNEL_PKG" \
    grub-common grub2-common $GRUB_PKGS xorriso mtools \
    fdisk dosfstools e2fsprogs

# Two things on the image do not come from the Debian archive — the face the
# compositor draws with and the file manager — so there is one way to fetch
# them and it verifies what it got before anything uses it.
#
# The Debian mirror above is plain http on purpose: apt checks the archive's
# signature whatever transport carried it, and https would add a dependency on
# a clock this machine may not have (see the MIRROR note below). Neither of
# these two has a signature to check, so the checksum written beside the URL is
# the whole of the integrity, and it is why the fetch is https *and* pinned: a
# mirror that serves something else, an upstream that moves a tag, and a
# release whose bytes changed under it all stop the build here rather than
# shipping. Updating either of them means changing a version and a hash
# together, in one commit, which is the review this arrangement is for.
fetch_pinned() {
    url=$1
    want=$2
    out=$3
    curl -fsSL --retry 3 -o "$out" "$url" || {
        echo "mkiso: cannot fetch $url" >&2
        exit 1
    }
    got=$(sha256sum "$out" | cut -d' ' -f1)
    if [ "$got" != "$want" ]; then
        echo "mkiso: $url is not the file this build was written against" >&2
        echo "mkiso:   expected $want" >&2
        echo "mkiso:   got      $got" >&2
        exit 1
    fi
}

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

# And the four words a person turns a computer off with, for the same reason
# and by the same rule: one file in the tree, on both sides of the pivot. Here
# it answers the rescue session, where busybox has halt, poweroff and reboot
# applets and no `shutdown` at all — so the one word everybody types is the one
# word that was missing — and where the applets it does have signal a PID 1
# that is a shell and returns 0 having done nothing. iso/shutdown says the
# whole of it; #158 is the issue.
mkdir -p "$ROOT/usr/local/sbin"
cp iso/shutdown "$ROOT/usr/local/sbin/shutdown"
chmod 755 "$ROOT/usr/local/sbin/shutdown"
for verb in poweroff reboot halt; do
    ln -sf shutdown "$ROOT/usr/local/sbin/$verb"
done

# The message of the day, which is where a person is told that this is a live
# session and how to put it on a disk, and the pictures beside it. Both are
# compiled into tos as well, so the screens have one whatever happens to /etc;
# splash.png is additionally what a pane is shown over the graphics protocol,
# which needs a path rather than a payload. They are two files because they are
# two screens: splash.png is the frontispiece over the login box and the
# greeting at the head of a pane (#132), and lock.png is the picture in the
# corner of a locked screen. A machine can replace either on its own.
mkdir -p "$ROOT/etc/tos" "$ROOT/run/live/medium"
cp .motd_art "$ROOT/etc/tos/motd_art"
cp compositor/tos-compositor/assets/splash.png "$ROOT/etc/tos/splash.png"
cp compositor/tos-compositor/assets/lock.png "$ROOT/etc/tos/lock.png"
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
# everything apt installs later. Copyright files stay — they are the terms
# under which this image may be handed to anybody at all.
#
# The manual pages used to be on this list, and came off it in #151. The line
# that put them there said a squashfs "should not spend it on manual pages it
# has no pager story for", and that reason expired the moment `less` went on
# the image. What remained was the size, and the size turned out to be
# 7,876,608 bytes of the squashfs — measured by building this rootfs both ways,
# rather than the much larger number it was assumed to be.
#
# The argument for paying it is not really about the image at all. A
# `path-exclude` here is not "this image has no manuals"; it is a line in the
# installed machine's dpkg configuration, so it is "this machine can never have
# manuals" — `apt install` a package on a tOS laptop a year from now and its
# manual is thrown away on the way in. That is a property somebody discovers
# the first time they type `man git`, cannot explain, and cannot easily undo,
# and it is not worth 3% of the medium.
#
# /usr/share/locale stays excluded and the reason is intact: the rootfs has no
# `locales` package, so no locale is generated and every program falls back to
# C. Those 31.8 MB are translations nothing can display.
cat >"$WORK/tos-minimal" <<'EOF'
# Written by iso/mkiso.sh. See the note there.
path-exclude=/usr/share/doc/*
path-include=/usr/share/doc/*/copyright
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
#   iw                      the one wireless diagnostic a person reaches for
#                           (`iw dev`, `iw wlan0 link`), 95 kB, and the only
#                           thing that can move a radio into a network
#                           namespace — which is how iso/wifi-witness.sh makes
#                           its access point a different host from the client
#                           it is testing, on a machine that has one kernel.
#   wpasupplicant           the other half of a radio. docs/design/network.md
#                           chose this supplicant and docs/design/wifi.md
#                           drives it over its control socket, one datagram
#                           at a time, with no D-Bus anywhere near it. The
#                           package also carries `wpa_cli`, which nothing in
#                           tOS shells out to: it is there for
#                           iso/wifi-witness.sh, which is where the text the
#                           three parsers are tested against comes from.
#   firmware-iwlwifi        Intel, Realtek, Qualcomm/Atheros, Broadcom and
#   firmware-realtek        MediaTek, out of the `non-free-firmware`
#   firmware-atheros        component the mmdebstrap call below now names. A
#   firmware-brcm80211      driver that binds to a radio and then cannot
#   firmware-misc-nonfree   start it is what these prevent, and it is the one
#                           failure a machine cannot get itself out of: a
#                           laptop with no network cannot `apt install` the
#                           firmware that would give it one. So all five,
#                           deliberately, and they are not cheap: 222,867,547
#                           bytes unpacked and 82,505,728 of the squashfs
#                           between them, which nearly doubles the image. The
#                           design guessed 35 MB by adding up .deb sizes, and
#                           a .deb is xz where this squashfs is zstd. The
#                           per-package split is in iso/README.md.
#
#                           The last name is the MediaTek one, and it is not
#                           a mistake. bookworm has no `firmware-mediatek` —
#                           trixie introduced that package — and there is no
#                           `mediatek/WIFI_MT7921*` anywhere in the archive
#                           either: MT7921 cards ask for the blobs of the
#                           silicon they are, `WIFI_RAM_CODE_MT7961_1.bin`
#                           and `WIFI_MT7922_patch_mcu_1_1_hdr.bin`, which is
#                           what `modinfo -F firmware mt7921e` lists and what
#                           firmware-misc-nonfree ships. Read out of the
#                           kernel module and looked up in bookworm's
#                           Contents with apt-file, rather than guessed from
#                           the chip's name.
#
# And then the applications, which are #151 and are a different kind of
# argument from everything above. Nothing up to here is on the image because
# somebody would enjoy it; each of those is a thing without which the machine
# does not boot, does not install itself, or cannot reach a network. These are
# on it because a machine that boots to a shell and has no editor is a machine
# nobody can do anything on, and `apt install` is not an answer on the first
# day of a laptop whose network is the thing you were going to configure.
#
# docs/design/applications.md is the decision and the reasoning; the one-line
# versions:
#
#   git                     "must git." — the issue's own words, and the only
#                           name on it that arrived that way. Pulls
#                           libcurl3-gnutls and perl's liberror, which is
#                           where most of its cost is.
#   curl                    the other thing the issue asked for by name, and
#                           the tool every install-this-shell-script expects,
#                           Homebrew's included. `wget` is already here for
#                           "does this reach the network"; curl is here
#                           because it is what a `curl | sh` line says, and
#                           what a program that wants an API speaks.
#   less                    the pager. git needs one, and without it `git log`
#                           writes a repository's history at the screen and
#                           keeps going. It is also what reopened the
#                           manual-page decision above: the exclusion's stated
#                           reason was that there was no pager to read them
#                           with, and now there is.
#   neovim                  the editor. bookworm has 0.7.2, which is old, and
#                           it is shipped anyway: see applications.md, where
#                           the alternative — an upstream tarball nothing
#                           updates — is the thing being turned down.
#   ripgrep fzf             search, and choosing from what it found. Both are
#                           a single static-ish binary with no dependencies,
#                           and both are what the file manager below reaches
#                           for when they are there.
#   btop                    what is this machine doing. `ps` and `free` are
#                           already here and answer a narrower question; this
#                           is the one somebody actually opens.
#   openssh-client rsync    off this machine and onto another one.
#   unzip file              what a downloaded archive is, and how to open it.
#                           `file` is also yazi's one hard dependency.
#   man-db                  and the manual pages it reads, which the dpkg
#                           configuration above stopped throwing away for it.
#                           Both halves or neither: a man(1) with nothing to
#                           show answers every question with "No manual entry",
#                           which is worse than a machine that plainly has
#                           none. 7,876,608 bytes of the squashfs for the
#                           pages, and the note above the dpkg configuration is
#                           why they are worth it.
#   libatomic1              nothing in Debian's set pulls it, and Node from
#                           v25 links it: a `nvm install node` on a tOS
#                           machine gets to 100%, unpacks, and then dies with
#                           `libatomic.so.1: cannot open shared object file`.
#                           45 kB, no dependencies of its own. It is here and
#                           not left to the person because the failure names a
#                           *file* and not a package, so apt cannot tell them
#                           what to type — see applications.md, which is also
#                           where Homebrew's place on this machine is settled.
#
# Not here, and on purpose: `tmux`, which #6 already decided is the person's to
# install, since a session that survives a detach is what tOS's own panes are
# for; and `lazygit`, which is a taste over git rather than a thing git cannot
# do, and which unlike yazi has no .deb to pin. applications.md has both in
# full, along with where Homebrew stands.
ROOTFS_PACKAGES="debian-archive-keyring,ca-certificates,bash,systemd-sysv,udev,\
busybox,iproute2,procps,iputils-ping,wget,ncurses-base,\
e2fsprogs,dosfstools,\
fdisk,util-linux,mount,kmod,sudo,squashfs-tools,grub2-common,\
wpasupplicant,iw,\
firmware-iwlwifi,firmware-realtek,firmware-atheros,firmware-brcm80211,\
firmware-misc-nonfree,\
git,curl,less,neovim,ripgrep,fzf,btop,openssh-client,rsync,unzip,file,\
man-db,libatomic1,\
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
#
# And `non-free-firmware` beside `main`, on all three, which is where every
# blob in the list above lives: bookworm moved firmware out of `non-free` into
# a component of its own precisely so that an installer could enable it
# without enabling non-free software in general, and this is that. It is
# written into the rootfs's sources as well as used to build it, so a machine
# whose radio wants a firmware package nobody packed can still install one —
# which is the whole reason the component exists.
#
# The suites are spelled out as `deb` lines rather than handed over as bare
# mirror URLs, because a bare URL is `main` and nothing else.
mmdebstrap \
    --mode=root \
    --variant=apt \
    --architectures="$DEB_ARCH" \
    --include="$ROOTFS_PACKAGES" \
    --aptopt='Acquire::Retries "3"' \
    --setup-hook="copy-in $WORK/tos-minimal /etc/dpkg/dpkg.cfg.d" \
    "$SUITE" "$ROOTFS" \
    "deb $MIRROR $SUITE main non-free-firmware" \
    "deb $SECURITY_MIRROR $SUITE-security main non-free-firmware" \
    "deb $MIRROR $SUITE-updates main non-free-firmware"

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

# And the applications, for the same reason (#151): an image that reached here
# without them is one somebody boots and cannot work on, and finding that out
# at the login screen is finding it out too late.
for program in /usr/bin/git /usr/bin/curl /usr/bin/less /usr/bin/nvim \
    /usr/bin/rg /usr/bin/fzf /usr/bin/btop /usr/bin/ssh /usr/bin/rsync \
    /usr/bin/unzip /usr/bin/file /usr/bin/man; do
    if [ ! -x "$ROOTFS$program" ]; then
        echo "mkiso: the rootfs has no $program" >&2
        exit 1
    fi
done

# The face the compositor draws with.
#
# It was fonts-vlgothic until #151, which is where somebody asked for this one
# by name. The swap is not only taste: HackGen Console NF is a Nerd Font, so
# the private-use glyphs that a modern TUI draws its icons out of are in the
# face rather than being the hollow box they were, and the file manager below
# is full of them.
#
# A file copied in rather than a package, because Debian has no HackGen and is
# not going to. That reverses what the vlgothic line used to say for itself —
# that a face dpkg owns is one whose absence is survivable, since the
# compositor falls back to its built-in ASCII face and only Japanese breaks.
# The trade is taken on purpose: a fetch that does not produce exactly the
# bytes this build was written against stops the build (`fetch_pinned`), so
# there is no image that quietly shipped without a face, and carrying
# vlgothic as well would be 2,318,336 bytes of squashfs against a case that
# cannot happen. It is the arrangement `splash.png`, `lock.png` and the SKK
# dictionary are already on.
#
# The plain `HackGen`, not `HackGen35`: tOS's width table says a wide
# character is exactly two cells, and only the 1:2 cut is. HackGen35 is 3:5,
# which would put every kana half a cell wrong across a line. `Console` is the
# cut without the programming ligatures, which a terminal that composites its
# own cells cannot use anyway. `Regular` alone, no `Bold`: tos-font
# synthesizes bold from the regular face when a cut is missing, which is what
# it already did for vlgothic, and the Bold file is another 13,464,288 bytes.
#
# 12,922,800 bytes on disk, 5,746,688 of the squashfs — against vlgothic's
# 2,318,336, so the face costs 3,428,352 more than the one it replaces.
#
# Licence: SIL Open Font License 1.1 (Hack, MIT; GenJyuu Gothic, OFL),
# redistributable. The copyright file goes beside it, because shipping an OFL
# face without its licence is not something an ISO may do.
HACKGEN_VERSION=2.10.0
HACKGEN_SHA256=f8abd483d5edfad88a78ed511978f43c83b43c48e364aa29ebe4a68217474428
fetch_pinned \
    "https://github.com/yuru7/HackGen/releases/download/v$HACKGEN_VERSION/HackGen_NF_v$HACKGEN_VERSION.zip" \
    "$HACKGEN_SHA256" "$WORK/hackgen.zip"
mkdir -p "$ROOTFS/usr/share/fonts/truetype/hackgen" "$WORK/hackgen"
unzip -q -j -o "$WORK/hackgen.zip" "*/HackGenConsoleNF-Regular.ttf" -d "$WORK/hackgen"
cp "$WORK/hackgen/HackGenConsoleNF-Regular.ttf" \
    "$ROOTFS/usr/share/fonts/truetype/hackgen/HackGenConsoleNF-Regular.ttf"
mkdir -p "$ROOTFS/usr/share/doc/hackgen"
cat >"$ROOTFS/usr/share/doc/hackgen/copyright" <<EOF
HackGen Console NF $HACKGEN_VERSION
https://github.com/yuru7/HackGen

SIL Open Font License 1.1. Built from Hack (MIT) and GenJyuu Gothic (OFL).
Fetched by iso/mkiso.sh against sha256 $HACKGEN_SHA256.
EOF

if [ ! -f "$ROOTFS/usr/share/fonts/truetype/hackgen/HackGenConsoleNF-Regular.ttf" ]; then
    echo "mkiso: the rootfs carries no Japanese face" >&2
    exit 1
fi

# The file manager (#151), and the first program on this image that tOS did
# not write and that draws pictures through tOS's own graphics protocol.
#
# That is most of why it is here rather than left to the person. The protocol
# has been exercised by `tos-preview`, which tOS wrote, and by its own tests;
# a third-party program that was never told what tOS is, previewing a photo in
# a pane because the pane answered like a terminal that can show one, is the
# protocol being a protocol. `docs/design/graphics-file-transmission.md` is
# what it is speaking.
#
# Upstream's own .deb, unpacked rather than installed: it carries no
# maintainer scripts, so `dpkg-deb -x` puts exactly what `dpkg -i` would have
# and needs no chroot to run in. What it does not get is a line in the package
# database, which is the honest cost of every name on this image that Debian
# does not have — see applications.md, where who owns its updates is written
# down.
#
# The musl build, which is statically linked: it then does not care what libc
# the rootfs has, and the rootfs is not asked to carry a second one.
#
# `Depends: file`, which is above. Everything else in its control file is a
# Recommends — ffmpeg, 7zip, poppler-utils, imagemagick, zoxide — and stays
# off: image previews are decoded by yazi itself and go over the graphics
# protocol, so the previewers a terminal without one needs are previewers this
# terminal does not. `ripgrep` and `fzf` are on that Recommends list too, and
# are here for their own reasons above.
#
# 33,929,663 bytes unpacked, 12,378,112 of the squashfs — the single largest
# thing #151 adds, and the one most likely to be argued with.
#
# Licence: MIT, redistributable.
YAZI_VERSION=26.9.1
case "$ARCH" in
x86_64) YAZI_SHA256=77d9d41441eaa8f17a555ccd3961ea128321dceac5fc06cf53db131aa60fde8a ;;
aarch64) YAZI_SHA256=39ae427eb0f0275c4302429b7a8fd48d1b862a2ee40d68d37b23f336be025164 ;;
esac
fetch_pinned \
    "https://github.com/sxyazi/yazi/releases/download/v$YAZI_VERSION/yazi-$ARCH-unknown-linux-musl.deb" \
    "$YAZI_SHA256" "$WORK/yazi.deb"
dpkg-deb -x "$WORK/yazi.deb" "$ROOTFS"

if [ ! -x "$ROOTFS/usr/bin/yazi" ]; then
    echo "mkiso: the rootfs has no /usr/bin/yazi" >&2
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

# `shutdown` and the three words beside it, in /usr/local/sbin where they are
# found before Debian's (#158).
#
# On an installed machine /sbin/shutdown is systemd-sysv's symlink to
# systemctl, and a pane runs as the person rather than as root (#119). There
# is no way for that person to reach PID 1 on this image: no D-Bus, so no
# logind to ask and no polkit to ask it — the note beside the masked
# wpa_supplicant.service below is the same fact from the other end — and
# /run/systemd/private is `srwx------ root root`. So the command everybody
# turns a computer off with exited 1 saying `Failed to connect to bus`, and
# the machine stayed up. The live image was unaffected only because its
# session is root.
#
# /usr/local/sbin rather than replacing /sbin/shutdown: that symlink is
# dpkg's, and a file tOS writes over it is one the next `apt upgrade` of
# systemd-sysv takes back without telling anybody. /usr/local is the
# directory the FHS keeps for exactly this, and it is first on the PATH
# iso/live-session exports.
#
# It is half the answer; the other half is the installer's sudoers drop-in,
# which names this very file — not /sbin/shutdown, because sudo matches a
# command by inode as well as by name and those four /sbin names are one
# systemctl, so granting them would be granting `sudo systemctl <anything>`.
# The shim re-runs itself under `sudo -n` and the root pass hands over to
# /sbin, which keeps the passwordless part bounded by what this file will do.
# `POWER_PROGRAM` in installer/src/install.rs is the whole of it. Neither is
# D-Bus and polkit,
# which is what the rest of the world does and what #158 turned down for now:
# that needs a logind session this compositor does not create, and the seat
# is #22's question as well.
mkdir -p "$ROOTFS/usr/local/sbin"
cp iso/shutdown "$ROOTFS/usr/local/sbin/shutdown"
chmod 755 "$ROOTFS/usr/local/sbin/shutdown"
for verb in poweroff reboot halt; do
    ln -sf shutdown "$ROOTFS/usr/local/sbin/$verb"
done

# And the wireless witness, which is a test rather than a part of the system —
# on the image because the machine it has to run on is one that was booted
# from this image, and a witness a person has to retype into a pane is one
# nobody runs. 7 KB, in /usr/share/tos beside the dictionary rather than on
# PATH, because it is not a thing to reach for by accident: it makes this
# machine into an access point. See docs/design/wifi.md, "The witness".
mkdir -p "$ROOTFS/usr/share/tos"
cp iso/wifi-witness.sh "$ROOTFS/usr/share/tos/wifi-witness.sh"
chmod 755 "$ROOTFS/usr/share/tos/wifi-witness.sh"

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

# The supplicant, one per radio, started by udev when the radio appears.
#
# A unit rather than a child of the compositor, for the reason
# docs/design/init.md gave for having an init at all: supervision is what an
# init is for, and a supplicant that dies is a machine that quietly stops
# being able to join anything until somebody notices. Instantiated from a udev
# rule rather than enabled by name, because the name of the radio is the
# machine's to say and this image is assembled long before it has one.
#
# tOS's own unit and not Debian's `wpa_supplicant@.service`, which is in the
# same package: Debian's is written for ifupdown, reads
# /etc/wpa_supplicant/wpa_supplicant-%I.conf, and leaves the control interface
# to whatever that file says — which is nothing, on a file nobody wrote. This
# one creates the file if it is not there, with the two lines the compositor's
# client needs: the control socket it connects to, and `update_config=1`,
# which is what makes SAVE_CONFIG write a joined network back so the machine
# rejoins at the next boot without being asked. On the live image /etc is a
# tmpfs overlay and that file is gone at reboot, which is right for a live
# image; the installer copies /etc onto the disk, so an installed machine
# keeps its networks.
#
# The socket directory is root's and the compositor is root, so nothing here
# has to be a member of `netdev`. /sbin/wpa_supplicant is the path dpkg's own
# file list gives — bookworm's /sbin is the merged-usr symlink to /usr/sbin,
# and both resolve.
mkdir -p "$ROOTFS/etc/wpa_supplicant"
cat >"$ROOTFS/etc/systemd/system/tos-supplicant@.service" <<'EOF'
# Written by iso/mkiso.sh. One supplicant per radio; see docs/design/wifi.md.
[Unit]
Description=tOS wireless supplicant on %I
BindsTo=sys-subsystem-net-devices-%i.device
After=sys-subsystem-net-devices-%i.device

[Service]
Type=simple
ExecStartPre=/bin/sh -c 'test -f /etc/wpa_supplicant/tos-%I.conf || \
    printf "ctrl_interface=/run/wpa_supplicant\nupdate_config=1\n" \
    > /etc/wpa_supplicant/tos-%I.conf'
ExecStart=/sbin/wpa_supplicant -Dnl80211 -i%I -c/etc/wpa_supplicant/tos-%I.conf
Restart=on-failure
EOF

# And Debian's own `wpa_supplicant.service` masked, for the same reason the
# unit above is not Debian's: tOS runs one supplicant per radio, started when
# the radio appears, and has no use for the singleton the package enables.
#
# Left alone it does not merely sit idle — it fails, on every boot, in red.
# Its `ExecStart` is `wpa_supplicant -u`, the D-Bus interface, and this image
# ships no D-Bus: `dpkg -l dbus` says `un`, so `/run/dbus/system_bus_socket`
# is not there and the supplicant exits 255 before it has looked at any
# hardware. Its `After=dbus.service` is no guard, because ordering against a
# unit that does not exist is satisfied by there being nothing to wait for.
#
# Masking rather than a drop-in that drops the `-u`. A drop-in would leave a
# second supplicant running beside the per-radio ones, owning the same control
# sockets under /run/wpa_supplicant that `tos_system::net::wpa` connects to —
# which is the thing this image already decided it did not want. Nothing here
# consumes wpa_supplicant's D-Bus API; the client speaks to the control
# socket. A machine that booted correctly should say nothing on its way in
# (#129), and this was saying `[FAILED]` (#140).
ln -sf /dev/null "$ROOTFS/etc/systemd/system/wpa_supplicant.service"

# What starts it. `DEVTYPE=wlan` is how the kernel's cfg80211 announces a
# wireless interface and how this rule tells one from the wired cards
# iso/init's drivers bring up; TAG+="systemd" is what makes systemd create a
# device unit for it at all, and SYSTEMD_WANTS is the device unit pulling the
# supplicant in. The number is only an ordering among rule files and nothing
# here depends on it; 80 leaves room on both sides. /etc/udev/rules.d and not
# /lib, for the same reason the units go in /etc: dpkg owns the other one.
mkdir -p "$ROOTFS/etc/udev/rules.d"
cat >"$ROOTFS/etc/udev/rules.d/80-tos-wireless.rules" <<'EOF'
# Written by iso/mkiso.sh. A radio appears; its supplicant starts.
ACTION=="add", SUBSYSTEM=="net", ENV{DEVTYPE}=="wlan", TAG+="systemd", \
    ENV{SYSTEMD_WANTS}+="tos-supplicant@$name.service"
EOF

# And the one sysctl a machine with two links needs.
#
# A laptop with a cable in it and a radio associated has two default routes,
# which the compositor now gives different metrics — wired 100, wireless 600,
# NetworkManager's numbers — so the kernel takes the cable. Pull the cable and
# that route is still there, still lower, and now goes nowhere. This makes the
# kernel skip a route whose link has no carrier, so the radio takes over in
# the same second and hands back when the cable returns, with nobody's address
# touched. Applied by systemd-sysctl at boot. See "Two default routes" in
# docs/design/wifi.md.
mkdir -p "$ROOTFS/etc/sysctl.d"
cat >"$ROOTFS/etc/sysctl.d/80-tos-net.conf" <<'EOF'
# Written by iso/mkiso.sh. See docs/design/wifi.md, "Two default routes".
net.ipv4.conf.all.ignore_routes_with_linkdown = 1
net.ipv4.conf.default.ignore_routes_with_linkdown = 1
EOF

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
cp compositor/tos-compositor/assets/splash.png "$ROOTFS/etc/tos/splash.png"
cp compositor/tos-compositor/assets/lock.png "$ROOTFS/etc/tos/lock.png"
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

# The one application on the image that needs a setting before it looks like
# itself. btop has no /etc default to write, so it goes in both homes the way
# the two files above do; the file says why it is there.
for home in "$ROOTFS/root" "$ROOTFS/etc/skel"; do
    mkdir -p "$home/.config/btop"
    cp iso/btop.conf "$home/.config/btop/btop.conf"
done

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

# A /lib/modules of its own, holding the wireless closure and nothing else.
#
# What stood here was the note that there was none, and that a machine which
# had pivoted could therefore never modprobe again: switch_root deletes the
# initramfs, and this is the directory the modprobe left in the rootfs would
# have looked in. That was the right trade twice over, and it stays the right
# trade for everything it covered. A display driver, a disk driver and a
# filesystem are things the machine has to have to *reach* this rootfs, so
# they are loaded before it is reached, by the loop in iso/init, out of the
# initramfs — and carrying a second copy here was 17 MB of an installed
# machine's disk for a modprobe nothing ran (#99).
#
# A radio is none of that. Nothing about it has to work for the rootfs to come
# up; it is wanted *after* the pivot, by a daemon that lives in here, and the
# machine it is wanted on is a laptop rather than a VM. So these modules go on
# this side of the pivot, and the initramfs does not grow by a byte: the list
# below is separate from MODULES above, is not in iso/init's loop, and is not
# checked against it by the guard up there — that guard is about what /init
# reaches for, and /init reaches for none of this.
#
# Nothing loads them by name either. udev is in this rootfs, systemd starts it
# in sysinit.target, and it does here what it does on every Debian machine:
# reads the modalias out of the device the bus announced and modprobes what
# matches. The PCI id of an Intel card names iwlwifi, and iwlwifi is here, so
# the card comes up — and `depmod` below is what makes that lookup possible at
# all, since a tree with no modules.dep and no modules.alias is a directory
# modprobe cannot find anything in.
#
# Five families and one fake one, which are the radios a laptop actually has:
# Intel, Realtek (rtw88 and rtw89 for the recent ones, rtl8xxxu for the USB
# dongles), Qualcomm/Atheros across three generations, Broadcom for Macs and
# Pis, MediaTek for newer AMD machines, and mac80211_hwsim, which is a radio
# made out of nothing and is how iso/wifi-witness.sh proves any of this works
# on a machine with no radio in it. Every name here was checked against this
# kernel with `modprobe --show-depends` before it was written down; the
# seventeen pull twenty-seven more between them — mac80211, cfg80211, rfkill,
# the ath and rtw88 and mt76 cores, mhi, usbcore — and the closure is 44
# modules, 19,407,460 bytes of .ko and 3,923,968 bytes of the squashfs. The
# firmware they ask for is the expensive half, and it is in the package list
# above.
#
# And the ciphers, which no driver depends on and every association needs.
# WPA2 is CCMP and WPA3 is GCMP; the kernel does not link either into
# mac80211 but asks the crypto API for `ccm(aes)` or `gcm(aes)` *by name*, at
# the moment the first key is set — which is the #84 shape again, a module
# nothing depends on and something asks for mid-syscall. The first run of
# iso/wifi-witness.sh found it: the access point came up, the four-way
# handshake ran, and the group key failed with `kernel reports: key addition
# failed`, because the rootfs had every radio driver and no `ccm.ko`. A real
# radio joining a real WPA2 network would have died in the same line. `cmac`
# is what the handshake's key derivation wants and `ctr` and `ghash` are what
# the two AEAD modes are built from; `aes_generic` is named in case this
# kernel makes it a module rather than a builtin, and `modprobe` answers
# `builtin` for one that is not, which the copy below simply skips.
WIRELESS_MODULES="cfg80211 mac80211 \
    iwlwifi iwlmvm iwldvm \
    rtw88_8821ce rtw88_8822be rtw88_8822ce rtw89_8852ae rtl8xxxu \
    ath9k ath10k_pci ath11k_pci \
    brcmfmac \
    mt7921e mt7921u \
    mac80211_hwsim \
    ccm gcm cmac ctr ghash_generic aes_generic"
mkdir -p "$ROOTFS/lib/modules/$KVER"
# Asked once for its own sake before anything is copied, because the copy
# below is a pipeline and an `exit` inside one exits a subshell and nothing
# else: a module renamed by a kernel bump would otherwise be a name that
# silently packs nothing, which is the shape of fault #84 was.
for mod in $WIRELESS_MODULES; do
    if ! modprobe -S "$KVER" --show-depends "$mod" >/dev/null 2>&1; then
        echo "mkiso: kernel $KVER has no wireless module $mod" >&2
        exit 1
    fi
done
for mod in $WIRELESS_MODULES; do
    modprobe -S "$KVER" --show-depends "$mod" 2>/dev/null || true
done | sed -n 's/^insmod \([^ ]*\).*/\1/p' | sort -u | while read -r path; do
    rel="${path#/lib/modules/$KVER/}"
    mkdir -p "$ROOTFS/lib/modules/$KVER/$(dirname "$rel")"
    cp "$path" "$ROOTFS/lib/modules/$KVER/$rel"
done
# The same metadata the initramfs tree gets, and for the same reason: depmod
# reads modules.builtin to know which names are in the kernel already rather
# than missing, and writes modules.dep and modules.alias next to the tree.
cp "/lib/modules/$KVER/modules.order" "/lib/modules/$KVER/modules.builtin" \
    "$ROOTFS/lib/modules/$KVER/"
cp "/lib/modules/$KVER/modules.builtin.modinfo" \
    "$ROOTFS/lib/modules/$KVER/" 2>/dev/null || true
depmod -b "$ROOTFS" "$KVER"

# And that the pieces a radio needs all landed, because every one of them
# fails quietly: a missing modules.alias is a card udev never binds, a missing
# firmware tree is a driver that binds and then times out, and a missing
# supplicant is a menu that says "no supplicant on wlan0" on a machine that
# should have had one. None of it is visible from a boot that has no radio,
# which is every boot CI does.
#
# The firmware is checked by family and by pattern rather than by filename,
# because the filenames carry a version the package bumps — `iwlwifi-cc-a0-72`
# becomes `-73` and an assertion naming it fails on a good image.
any_match() {
    # The argument is a pattern, so it is deliberately unquoted here: an
    # unmatched glob stays literal and the test below says no.
    # shellcheck disable=SC2086
    set -- $1
    [ -e "$1" ]
}
for pattern in \
    "$ROOTFS/lib/modules/$KVER/modules.dep" \
    "$ROOTFS/lib/modules/$KVER/modules.alias" \
    "$ROOTFS/lib/modules/$KVER/kernel/net/wireless/cfg80211.ko" \
    "$ROOTFS/lib/modules/$KVER/kernel/crypto/ccm.ko" \
    "$ROOTFS/lib/firmware/iwlwifi-*.ucode" \
    "$ROOTFS/lib/firmware/rtw88/*.bin" \
    "$ROOTFS/lib/firmware/ath10k/QCA*" \
    "$ROOTFS/lib/firmware/brcm/*" \
    "$ROOTFS/lib/firmware/mediatek/WIFI_RAM_CODE_MT7961_1.bin" \
    "$ROOTFS/sbin/wpa_supplicant" \
    "$ROOTFS/sbin/wpa_cli" \
    "$ROOTFS/sbin/iw"; do
    if ! any_match "$pattern"; then
        echo "mkiso: the rootfs has nothing matching ${pattern#"$ROOTFS"}" >&2
        exit 1
    fi
done

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
# Worth 16,650,240 bytes of the squashfs when this was written and all three
# trees went, measured by compressing the same rootfs both ways: 74,674,176
# with them and 58,023,936 without. Most of it is /usr/share/locale, 31.8 MB of
# translated messages that nothing can currently display — the rootfs has no
# `locales` package, so no locale is generated and every program falls back to
# C. Installing `locales` and deleting the locale line from the dpkg
# configuration is the pair of changes that would make them worth carrying.
#
# /usr/share/man is no longer one of the three. It came off the dpkg exclusion
# in #151, and a line here that deleted it anyway would be the essential set's
# manuals — bash's, coreutils', dpkg's, the ones somebody is most likely to
# reach for — going missing while every package installed afterwards kept
# theirs. The note above the dpkg configuration has the argument.
rm -rf "$ROOTFS/usr/share/locale" "$ROOTFS/usr/share/info"
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
