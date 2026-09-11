#!/usr/bin/env bash
# Boot the built ISO in QEMU.
#
#   iso/run.sh                  # boots dist/tos-x86_64.iso
#
# -vga std exposes a bochs-drm framebuffer, which is what /init modprobes
# first; serial output mirrors the kernel log to this terminal.
set -euo pipefail
cd "$(dirname "$0")/.."

ARCH=${ARCH:-x86_64}
ISO="dist/tos-$ARCH.iso"
if [[ ! -f $ISO ]]; then
    echo "iso/run.sh: $ISO not found; run iso/build.sh first" >&2
    exit 1
fi
if ! command -v "qemu-system-$ARCH" >/dev/null; then
    echo "iso/run.sh: qemu-system-$ARCH not found (on macOS: brew install qemu)" >&2
    exit 1
fi

exec "qemu-system-$ARCH" \
    -m 1024 \
    -cdrom "$ISO" \
    -vga std \
    -serial mon:stdio \
    "$@"
