#!/usr/bin/env bash
# Build the tOS ISO on the host. Needs Docker or Podman; everything else
# happens inside a Debian-based rust container (see iso/mkiso.sh).
#
#   iso/build.sh                # x86_64 ISO -> dist/tos-x86_64.iso
#   ARCH=aarch64 iso/build.sh   # aarch64 ISO (kernel/GRUB support pending)
set -euo pipefail
cd "$(dirname "$0")/.."

if command -v docker >/dev/null; then
    engine=docker
elif command -v podman >/dev/null; then
    engine=podman
else
    echo "iso/build.sh: needs docker or podman (on macOS: OrbStack, Docker Desktop, or colima)" >&2
    exit 1
fi

ARCH=${ARCH:-x86_64}
case "$ARCH" in
x86_64) platform=linux/amd64 ;;
aarch64) platform=linux/arm64 ;;
*)
    echo "iso/build.sh: unsupported ARCH=$ARCH" >&2
    exit 1
    ;;
esac

# target/ is the one directory mkiso.sh cannot hand back itself. Docker creates
# a bind target it does not find, and creates it as root, so the host ends up
# with a root-owned target/ that the container can no longer see past its own
# mount: a `cargo build` on the host then fails on a directory it cannot write,
# and `git worktree remove` fails on one it cannot delete, and neither says
# why.
#
# Made here rather than repaired afterwards. Docker leaves a mountpoint that
# already exists alone, so this costs nothing, needs no second container and no
# network, works the same under podman, and — unlike a repair after the build —
# does not have to be remembered on the path where the build failed, which is
# the path that used to leave the directory owned by root.
mkdir -p target

# A checkout poisoned by an older build still has to be handed back, and
# nothing here may use sudo: the hosts this is run from have no passwordless
# one, which is the whole reason the build is in a container. So the repair is
# another container, which is already root — once, and only when it is needed.
# Not under rootless podman, where the container's root is already the invoking
# user and a chown to this uid inside it would name a subuid instead, handing
# target/ to an id nobody can write to: the symptom this exists to prevent,
# reached from the other side.
if [ ! -w target ] && [ "$engine" = docker ]; then
    "$engine" run --rm -v "$PWD":/src alpine \
        chown "$(id -u):$(id -g)" /src/target
fi

# Named volumes keep the registry and target dir warm between builds, and
# keep the container's Linux artifacts out of the host target/.
# The container runs as root and writes dist/ into the checkout. Passing the
# invoking user in lets mkiso.sh hand it back, which matters on a host with no
# passwordless sudo — see the note beside the chown there.
exec "$engine" run --rm --platform "$platform" \
    -e HOST_UID="$(id -u)" \
    -e HOST_GID="$(id -g)" \
    -v "$PWD":/src \
    -v "tos-iso-cargo-$ARCH":/usr/local/cargo/registry \
    -v "tos-iso-target-$ARCH":/src/target \
    -w /src \
    rust:1-bookworm \
    sh iso/mkiso.sh
