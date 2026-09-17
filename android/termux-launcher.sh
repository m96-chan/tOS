#!/data/data/com.termux/files/usr/bin/sh
set -eu
: "${PREFIX:?Run tos inside Termux}"
# Android owns screen locking and blanking. The smallest built-in font keeps
# the development backend usable in the host terminal's limited pixel grid.
exec "$PREFIX/libexec/tos/tos" \
    --backend nested --bitmap-scale 1 --idle-lock 0 --idle-blank 0 "$@"
