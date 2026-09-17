#!/usr/bin/env bash
# Create the initial Debian disk without executing ARM64 code on the host.
set -euo pipefail
cd "$(dirname "$0")/../.."
bash android/vm/prepare-kernel.sh
sdk=${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/Android/Sdk}}
ndk=${ANDROID_NDK_HOME:-${ANDROID_NDK_ROOT:-$(printf '%s\n' "$sdk"/ndk/* | sort -V | tail -n1)}}
cc=$ndk/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android24-clang
mkdir -p dist/android/vm/lists/partial
"$cc" -static -O2 -Wall -Wextra -Werror android/vm/agent.c -o dist/android/vm/agent
image=debian@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171
if [[ ! -f dist/android/vm/base.tar ]]; then
    container=$(docker create --platform linux/arm64 "$image")
    trap 'docker rm "$container" >/dev/null' EXIT
    docker export "$container" -o dist/android/vm/base.tar
    docker rm "$container" >/dev/null
    trap - EXIT
fi
docker run --rm --platform linux/amd64 \
    -v "$PWD:/src:ro" -v "$PWD/dist/android/vm:/out" \
    debian:bookworm bash /src/android/vm/prepare-rootfs.sh
