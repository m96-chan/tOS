#!/usr/bin/env bash
# Build the native Android executable and a Termux install bundle on Linux.
set -euo pipefail
cd "$(dirname "$0")/.."

target=aarch64-linux-android
sdk=${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/Android/Sdk}}
ndk=${ANDROID_NDK_HOME:-${ANDROID_NDK_ROOT:-}}
if [[ -z $ndk ]]; then
    shopt -s nullglob
    candidates=("$sdk"/ndk/*)
    if ((${#candidates[@]} == 0)); then
        echo 'Set ANDROID_NDK_HOME to an installed Android NDK (r28 or newer recommended).' >&2
        exit 1
    fi
    ndk=$(printf '%s\n' "${candidates[@]}" | sort -V | tail -n 1)
fi
toolchain=$ndk/toolchains/llvm/prebuilt/linux-x86_64/bin
linker=$toolchain/aarch64-linux-android24-clang
if [[ ! -x $linker ]]; then
    echo "Android compiler not found: $linker" >&2
    exit 1
fi
if ! rustup target list --installed | grep -qx "$target"; then
    echo "Install the Rust target first: rustup target add $target" >&2
    exit 1
fi

# Match Android's newer 16 KiB page-size devices as well as 4 KiB devices.
CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$linker" \
    RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-C link-arg=-Wl,-z,max-page-size=16384" \
    cargo build --locked --release --target "$target" -p tos-compositor

bundle=dist/android/tos-termux-aarch64
mkdir -p "$bundle"
cp "${CARGO_TARGET_DIR:-target}/$target/release/tos" "$bundle/tos-bin"
"$toolchain/llvm-strip" "$bundle/tos-bin"
cp android/termux-launcher.sh "$bundle/tos"
cp android/install-termux.sh "$bundle/install.sh"
chmod 755 "$bundle/tos-bin" "$bundle/tos" "$bundle/install.sh"
tar -C dist/android -czf dist/android/tos-termux-aarch64.tar.gz tos-termux-aarch64
(
    cd dist/android
    sha256sum tos-termux-aarch64.tar.gz > tos-termux-aarch64.tar.gz.sha256
)
echo 'Built dist/android/tos-termux-aarch64.tar.gz'
