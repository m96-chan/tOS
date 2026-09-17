#!/data/data/com.termux/files/usr/bin/sh
# Run after unpacking the bundle inside the official Termux app.
set -eu
if [ "${PREFIX:-}" != /data/data/com.termux/files/usr ]; then
    echo 'Run this installer inside Termux (com.termux).' >&2
    exit 1
fi
if [ "$(uname -m)" != aarch64 ]; then
    echo 'This bundle requires an ARM64 Android device.' >&2
    exit 1
fi
bundle=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
# Check the executable before replacing an existing installation.
"$bundle/tos-bin" --version
mkdir -p "$PREFIX/libexec/tos" "$PREFIX/bin"
cp "$bundle/tos-bin" "$PREFIX/libexec/tos/tos"
cp "$bundle/tos" "$PREFIX/bin/tos"
chmod 755 "$PREFIX/libexec/tos/tos" "$PREFIX/bin/tos"
echo 'Installed. Run: tos'
echo 'For the nested display, pinch to reduce the Termux font size first.'
