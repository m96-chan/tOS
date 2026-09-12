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
# it in the checkout so it has somewhere to mount the named volume over, which
# means the host ends up with a root-owned target/ that the container can no
# longer see past its own mount. A `cargo build` on the host then fails on a
# directory it cannot write, and `git worktree remove` fails on one it cannot
# delete, and neither says why. Nothing here may use sudo: the hosts this is
# run from have no passwordless one, which is the whole reason the build is in
# a container. So the fix is another container, which is already root.
#
# Docker only. Under rootless podman the container's root is already the
# invoking user on the host, so nothing is root-owned to begin with — and a
# `chown` to this uid *inside* that container names a subuid, handing target/
# to an id the user can neither write to nor chown back. That is the symptom
# this exists to prevent, arrived at from the other side.
#
# On EXIT rather than after the build, because a build that failed is the case
# that leaves target/ root-owned most often, and `set -e` would have skipped
# the repair exactly then. Its own failure — no alpine image cached, nothing to
# pull from — says so and is not allowed to fail a build that worked.
hand_back_target() {
    status=$?
    if [ "$engine" = docker ]; then
        "$engine" run --rm -v "$PWD":/src alpine \
            chown "$(id -u):$(id -g)" /src/target 2>/dev/null ||
            echo "iso/build.sh: target/ may still be root-owned" >&2
    fi
    return "$status"
}
trap hand_back_target EXIT

# Named volumes keep the registry and target dir warm between builds, and
# keep the container's Linux artifacts out of the host target/.
# The container runs as root and writes dist/ into the checkout. Passing the
# invoking user in lets mkiso.sh hand it back, which matters on a host with no
# passwordless sudo — see the note beside the chown there.
"$engine" run --rm --platform "$platform" \
    -e HOST_UID="$(id -u)" \
    -e HOST_GID="$(id -g)" \
    -v "$PWD":/src \
    -v "tos-iso-cargo-$ARCH":/usr/local/cargo/registry \
    -v "tos-iso-target-$ARCH":/src/target \
    -w /src \
    rust:1-bookworm \
    sh iso/mkiso.sh
