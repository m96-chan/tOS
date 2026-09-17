#!/usr/bin/env bash
# Standalone ARM64 APK: Rust compositor + a small Java/NDK Android view.
# No Gradle plugins or third-party Android UI libraries are required.
set -euo pipefail
cd "$(dirname "$0")/.."
sdk=${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/Android/Sdk}}
ndk=${ANDROID_NDK_HOME:-${ANDROID_NDK_ROOT:-}}
shopt -s nullglob
if [[ -z $ndk ]]; then
    candidates=("$sdk"/ndk/*)
    ((${#candidates[@]})) || { echo 'Install the Android NDK or set ANDROID_NDK_HOME.' >&2; exit 1; }
    ndk=$(printf '%s\n' "${candidates[@]}" | sort -V | tail -n 1)
fi
candidates=("$sdk"/build-tools/*)
((${#candidates[@]})) || { echo 'Install Android SDK build-tools.' >&2; exit 1; }
build_tools=$(printf '%s\n' "${candidates[@]}" | sort -V | tail -n 1)
android_jar=$sdk/platforms/android-35/android.jar
[[ -f $android_jar ]] || { echo 'Install the Android SDK platform android-35.' >&2; exit 1; }
toolchain=$ndk/toolchains/llvm/prebuilt/linux-x86_64/bin
cc=$toolchain/aarch64-linux-android24-clang
[[ -x $cc ]] || { echo "Compiler not found: $cc" >&2; exit 1; }
target=aarch64-linux-android
if ! rustup target list --installed | grep -qx "$target"; then
    echo "Run: rustup target add $target" >&2; exit 1
fi

python3 android/userland/prepare.py

CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$cc" \
    RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-C link-arg=-Wl,-z,max-page-size=16384" \
    cargo build --locked --release --target "$target" -p tos-android

# A fresh staging directory prevents stale classes/resources entering the APK.
mkdir -p dist/android
stage=$(mktemp -d "$PWD/dist/android/apk-build.XXXXXX")
trap 'rm -rf -- "$stage"' EXIT
mkdir -p "$stage/classes" "$stage/dex" "$stage/lib/arm64-v8a"
cp -a dist/android/userland/lib/arm64-v8a/. "$stage/lib/arm64-v8a/"
cp -a dist/android/userland/assets "$stage/assets"
"$cc" -shared -fPIC -O2 -Wall -Wextra -Werror -Wl,-z,max-page-size=16384 \
    android/app/jni/paths.c -ldl -o "$stage/lib/arm64-v8a/libtos_paths.so"
"$cc" -fPIE -pie -O2 -Wall -Wextra -Werror -Wl,-z,max-page-size=16384 \
    android/app/jni/motd.c -o "$stage/lib/arm64-v8a/libtos_motd.so"
"$cc" -shared -fPIC -O2 -Wall -Wextra -Werror \
    -Wl,-z,max-page-size=16384 -Wl,--exclude-libs,ALL \
    android/app/jni/bridge.c "${CARGO_TARGET_DIR:-target}/$target/release/libtos_android.a" \
    -landroid -llog -ldl -lm -o "$stage/lib/arm64-v8a/libtos_android.so"
"$toolchain/llvm-strip" "$stage/lib/arm64-v8a/libtos_android.so"

"$build_tools/aapt2" compile --dir android/app/res -o "$stage/resources.zip"
"$build_tools/aapt2" link -o "$stage/base.apk" \
    --manifest android/app/AndroidManifest.xml -I "$android_jar" "$stage/resources.zip"
javac -encoding UTF-8 --release 8 \
    -classpath "$android_jar" -d "$stage/classes" android/app/src/io/github/m96chan/tos/*.java
mapfile -t classes < <(find "$stage/classes" -name '*.class' -type f | sort)
"$build_tools/d8" --min-api 24 --lib "$android_jar" --output "$stage/dex" "${classes[@]}"
cp "$stage/dex/classes.dex" "$stage/classes.dex"
(
    cd "$stage"
    zip -q -r base.apk classes.dex lib assets
)
"$build_tools/zipalign" -f -P 16 4 "$stage/base.apk" "$stage/aligned.apk"

# Keep a local development identity between builds so updates retain app data.
# Distribution builds can supply their own identity through these variables.
keystore=${TOS_ANDROID_KEYSTORE:-$PWD/dist/android/development.keystore}
alias=${TOS_ANDROID_KEY_ALIAS:-tos-development}
if [[ -z ${TOS_ANDROID_KEYSTORE:-} ]]; then
    export TOS_ANDROID_STORE_PASSWORD=android
    export TOS_ANDROID_KEY_PASSWORD=android
    if [[ ! -f $keystore ]]; then
        keytool -genkeypair -keystore "$keystore" -storepass:env TOS_ANDROID_STORE_PASSWORD \
            -keypass:env TOS_ANDROID_KEY_PASSWORD -alias "$alias" -keyalg RSA -keysize 2048 \
            -validity 10000 -dname 'CN=tOS Development,O=tOS,C=JP' -storetype PKCS12
    fi
else
    : "${TOS_ANDROID_STORE_PASSWORD:?Set the keystore password in this environment variable}"
    export TOS_ANDROID_STORE_PASSWORD
    export TOS_ANDROID_KEY_PASSWORD=${TOS_ANDROID_KEY_PASSWORD:-$TOS_ANDROID_STORE_PASSWORD}
fi
apk=$PWD/dist/android/tos-android-arm64.apk
"$build_tools/apksigner" sign --ks "$keystore" --ks-key-alias "$alias" \
    --ks-pass env:TOS_ANDROID_STORE_PASSWORD --key-pass env:TOS_ANDROID_KEY_PASSWORD \
    --out "$apk" "$stage/aligned.apk"
"$build_tools/apksigner" verify "$apk"
(
    cd dist/android
    sha256sum tos-android-arm64.apk > tos-android-arm64.apk.sha256
)
echo "Built $apk"
