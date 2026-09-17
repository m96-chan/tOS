#!/bin/bash
set -eu
export PATH=/usr/sbin:/usr/bin:/sbin:/bin HOME=/root TERM=xterm-256color
mountpoint -q /proc || mount -t proc proc /proc
mountpoint -q /sys || mount -t sysfs sysfs /sys
mkdir -p /dev/pts /run
mountpoint -q /dev/pts || mount -t devpts devpts /dev/pts
mountpoint -q /run || mount -t tmpfs tmpfs /run
if [ ! -e /var/lib/tos-ready ]; then
    echo 'Preparing Debian tools…'
    export DEBIAN_FRONTEND=noninteractive
    mapfile -t packages < /usr/lib/tos/packages.txt
    apt-get -y --no-download --reinstall --no-install-recommends install "${packages[@]}"
    mkdir -p /root/.config/btop
    cp /usr/lib/tos/btop.conf /root/.config/btop/btop.conf
    rm -f /var/cache/apt/archives/*.deb
    touch /var/lib/tos-ready
    sync
fi
rm -f /usr/sbin/policy-rc.d
exec /sbin/init --show-status=no --log-target=journal --log-level=warning </dev/null >/dev/null 2>&1
