#!/usr/bin/env bash
# Runs in an x86 Debian build container; ARM64 packages are downloaded, not run.
set -euo pipefail
apt-get update -qq
apt-get install -y --no-install-recommends e2fsprogs ca-certificates curl xz-utils
mkdir -p /rootfs /out/archives/partial
tar xf /out/base.tar -C /rootfs
# This is an interactive VM: retain manuals and use dpkg's normal durable writes.
rm -f /rootfs/etc/dpkg/dpkg.cfg.d/docker /rootfs/etc/dpkg/dpkg.cfg.d/docker-apt-speedup
mapfile -t packages < /src/android/vm/packages.txt
apt-get -o APT::Architecture=arm64 -o Dir::State::lists=/out/lists \
    -o Dir::State::status=/rootfs/var/lib/dpkg/status update
apt-get -o APT::Architecture=arm64 -o Dir::State::lists=/out/lists \
    -o Dir::State::status=/rootfs/var/lib/dpkg/status \
    -o Dir::Cache::archives=/out/archives --download-only --reinstall -y --no-install-recommends install "${packages[@]}"
mkdir -p /rootfs/usr/lib/tos /rootfs/etc/systemd/system/multi-user.target.wants /rootfs/usr/share/tos /rootfs/var/cache/apt/archives /rootfs/var/lib/apt/lists
cp /out/archives/*.deb /rootfs/var/cache/apt/archives/
cp -a /out/lists/. /rootfs/var/lib/apt/lists/
cp /src/android/vm/packages.txt /src/android/vm/boot.sh /src/android/vm/bashrc /rootfs/usr/lib/tos/
cp /out/agent /rootfs/usr/lib/tos/agent
cp /src/android/vm/tos-agent.service /rootfs/etc/systemd/system/
ln -sf ../tos-agent.service /rootfs/etc/systemd/system/multi-user.target.wants/tos-agent.service
cp /src/compositor/tos-compositor/assets/splash.png /rootfs/usr/share/tos/splash.png
cp /src/iso/btop.conf /rootfs/usr/lib/tos/btop.conf
printf '#!/bin/sh\nexit 101\n' > /rootfs/usr/sbin/policy-rc.d
chmod 755 /rootfs/usr/sbin/policy-rc.d /rootfs/usr/lib/tos/boot.sh /rootfs/usr/lib/tos/agent
printf 'tOS\n' > /rootfs/etc/hostname
printf '127.0.0.1 localhost\n127.0.1.1 tOS\n' > /rootfs/etc/hosts
printf 'nameserver 10.0.2.3\n' > /rootfs/etc/resolv.conf
printf '. /usr/lib/tos/bashrc\n' > /rootfs/root/.bashrc
mkdir -p /rootfs/etc/systemd/system/serial-getty@hvc0.service.d
ln -sf /dev/null /rootfs/etc/systemd/system/serial-getty@hvc0.service
printf 'none /tmp tmpfs defaults 0 0\n' > /rootfs/etc/fstab
curl -fL --retry 3 -o /out/yazi.deb https://github.com/sxyazi/yazi/releases/download/v26.9.1/yazi-aarch64-unknown-linux-musl.deb
echo '39ae427eb0f0275c4302429b7a8fd48d1b862a2ee40d68d37b23f336be025164  /out/yazi.deb' | sha256sum -c -
dpkg-deb -x /out/yazi.deb /rootfs
rm -f /out/debian.img
truncate -s 20G /out/debian.img
mkfs.ext4 -q -F -L tos-debian -d /rootfs /out/debian.img
gzip -n -1 -c /out/debian.img > /out/debian.img.gz
sha256sum /out/debian.img > /out/debian.img.sha256
