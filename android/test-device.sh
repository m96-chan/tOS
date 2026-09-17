#!/usr/bin/env bash
# Install the test-only instrumentation APK, run it as tOS's UID, then remove it.
set -euo pipefail
cd "$(dirname "$0")/.."
serial=${1:?Usage: bash android/test-device.sh DEVICE_SERIAL}
fresh=${2:-no}
[[ $fresh == no || $fresh == --fresh ]] || { echo 'Optional second argument: --fresh (preserve old disk as backup)' >&2; exit 1; }
[[ $fresh != --fresh ]] || fresh=yes
sdk=${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/Android/Sdk}}
build_tools=$(printf '%s\n' "$sdk"/build-tools/* | sort -V | tail -n 1)
android_jar=$sdk/platforms/android-35/android.jar
adb=$sdk/platform-tools/adb
stage=$(mktemp -d)
installed=false
cleanup() {
    if $installed; then "$adb" -s "$serial" uninstall io.github.m96chan.tos.tests >/dev/null || true; fi
    rm -rf -- "$stage"
}
trap cleanup EXIT
mkdir -p "$stage/classes" "$stage/dex"
javac --release 8 -classpath "$android_jar" -d "$stage/classes" android/tests/Smoke.java
mapfile -t classes < <(find "$stage/classes" -name '*.class' -type f)
"$build_tools/d8" --min-api 26 --lib "$android_jar" --output "$stage/dex" "${classes[@]}"
"$build_tools/aapt2" link -o "$stage/test.apk" --manifest android/tests/AndroidManifest.xml -I "$android_jar"
(cd "$stage/dex" && zip -q "$stage/test.apk" classes.dex)
"$build_tools/zipalign" -f 4 "$stage/test.apk" "$stage/aligned.apk"
export TOS_ANDROID_STORE_PASSWORD=${TOS_ANDROID_STORE_PASSWORD:-android}
export TOS_ANDROID_KEY_PASSWORD=${TOS_ANDROID_KEY_PASSWORD:-$TOS_ANDROID_STORE_PASSWORD}
"$build_tools/apksigner" sign --ks "${TOS_ANDROID_KEYSTORE:-dist/android/development.keystore}" \
    --ks-key-alias "${TOS_ANDROID_KEY_ALIAS:-tos-development}" --ks-pass env:TOS_ANDROID_STORE_PASSWORD \
    --key-pass env:TOS_ANDROID_KEY_PASSWORD "$stage/aligned.apk"
"$adb" -s "$serial" install --no-incremental -r "$stage/aligned.apk"
installed=true
"$adb" -s "$serial" shell am instrument -w -e fresh "$fresh" -e previous_logs "${TOS_VM_DIAGNOSTICS:-no}" -e reboots "${TOS_VM_REBOOTS:-1}" io.github.m96chan.tos.tests/.Smoke | tee "$stage/results"
"$adb" -s "$serial" uninstall io.github.m96chan.tos.tests
installed=false
rg -q 'tOS device smoke tests: 0 failures' "$stage/results"
