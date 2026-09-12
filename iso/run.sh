#!/usr/bin/env bash
# Boot the built ISO in VirtualBox.
#
#   iso/run.sh              # headless; the serial console comes to this terminal
#   iso/run.sh --gui        # ... in a window as well
#   VM=name iso/run.sh      # under a different VM name
#
# VirtualBox rather than QEMU, which this script used to drive. The compositor
# is the thing being tested and it takes the screen through DRM, so what a
# developer needs from a local boot is a machine whose display a person can
# actually look at and whose keyboard they can drive — and on the machines this
# is run from, VirtualBox is what is installed. CI still boots the image under
# QEMU (.github/workflows/iso.yml and release.yml, inline), so the image is
# proved against both and neither is the only witness.
#
# The VM is disposable: it is created here, destroyed on the way out, and
# nothing on it survives. `tos-install` wants a disk to write to, which this
# does not give it — install testing means a VM you keep, not this.
set -euo pipefail
cd "$(dirname "$0")/.."

ARCH=${ARCH:-x86_64}
VM=${VM:-tos-run}
ISO="dist/tos-$ARCH.iso"
front=headless
for argument in "$@"; do
    case "$argument" in
    --gui) front=gui ;;
    *)
        echo "iso/run.sh: unknown option: $argument" >&2
        exit 2
        ;;
    esac
done

if [[ $ARCH != x86_64 ]]; then
    echo "iso/run.sh: VirtualBox here is x86_64 only; got ARCH=$ARCH" >&2
    exit 1
fi
if [[ ! -f $ISO ]]; then
    echo "iso/run.sh: $ISO not found; run iso/build.sh first" >&2
    exit 1
fi
if ! command -v VBoxManage >/dev/null; then
    echo "iso/run.sh: VBoxManage not found (Debian/Ubuntu: virtualbox, Arch: virtualbox)" >&2
    exit 1
fi

serial=$(mktemp -t "tos-serial-XXXXXX.log")
# Torn down however this exits, including the Ctrl-C that is the usual way out:
# a VM left registered would make the next run fail on the name.
cleanup() {
    VBoxManage controlvm "$VM" poweroff >/dev/null 2>&1 || true
    VBoxManage unregistervm "$VM" --delete >/dev/null 2>&1 || true
    rm -f "$serial"
}
trap cleanup EXIT INT TERM

VBoxManage unregistervm "$VM" --delete >/dev/null 2>&1 || true
VBoxManage createvm --name "$VM" --ostype Linux_64 --register >/dev/null
# vmsvga is what a Linux guest gets by default and is one of the drivers the
# initramfs carries; the others are there for the hosts that hand out something
# else. COM1 to a file is the same serial log CI greps, which is the only way
# to read a boot that has not reached the compositor yet.
#
# The adapter is named rather than left to the default, because the default is
# whatever VirtualBox picked for the guest type and a boot that gets an
# adapter the initramfs has no driver for looks exactly like a boot with no
# adapter at all: `lo` and nothing else. virtio is the one to ask for — it is
# the fastest of the emulations and virtio_net is packed — and NAT is the mode
# that needs nothing of the host: VirtualBox's own DHCP server answers on the
# 10.0.2.0/24 it invents, which is enough to take the network menu all the way
# to a lease. See docs/design/network.md for what that does and does not
# prove.
VBoxManage modifyvm "$VM" \
    --memory 1024 --vram 128 --cpus 2 \
    --graphicscontroller vmsvga --firmware bios \
    --boot1 dvd --boot2 none --boot3 none --boot4 none \
    --audio-driver none \
    --nic1 nat --nictype1 virtio \
    --uart1 0x3F8 4 --uartmode1 file "$serial" >/dev/null
VBoxManage storagectl "$VM" --name IDE --add ide --controller PIIX4 >/dev/null
VBoxManage storageattach "$VM" --storagectl IDE --port 1 --device 0 \
    --type dvddrive --medium "$PWD/$ISO" >/dev/null

echo "iso/run.sh: booting $ISO as $VM ($front); ctrl-c to stop" >&2
VBoxManage startvm "$VM" --type "$front" >/dev/null

# The serial log is the terminal's view of the boot, the way -serial mon:stdio
# was. tail follows the file VirtualBox is writing; the VM going away ends it.
tail -n +1 -f "$serial" &
follow=$!
while VBoxManage showvminfo "$VM" --machinereadable 2>/dev/null |
    grep -q '^VMState="running"'; do
    sleep 2
done
kill "$follow" 2>/dev/null || true
wait "$follow" 2>/dev/null || true
