#!/usr/bin/env bash
# Google's AVF Debian kernel includes virtio, ext4 and TUN built in.
set -euo pipefail
cd "$(dirname "$0")/../.."
out=dist/android/vm
archive=$out/avf-latest-images.tar.gz
archive_sha=765cec51cd3e16ea1745b7ac8f9b3816d21ab3ed069bea355a59e3ade6bdfc41
kernel_sha=121fcef9c308fa5bbf2722417880bc805d6c0858608634fe4ee834f567e4ac07
mkdir -p "$out/kernel"
if printf '%s  %s\n' "$kernel_sha" "$out/kernel/vmlinuz" | sha256sum -c --status 2>/dev/null; then
    exit 0
fi
if [[ ! -f $archive ]]; then
    curl --fail --location --retry 3 --output "$archive.part" \
        https://dl.google.com/android/ferrochrome/latest/aarch64/images.tar.gz
    mv "$archive.part" "$archive"
fi
if ! printf '%s  %s\n' "$archive_sha" "$archive" | sha256sum -c; then
    echo 'The upstream archive changed. Review and update the pinned kernel hashes before rebuilding.' >&2
    exit 1
fi
tar -xzf "$archive" -C "$out/kernel" vmlinuz build_id
printf '%s  %s\n' "$kernel_sha" "$out/kernel/vmlinuz" | sha256sum -c
