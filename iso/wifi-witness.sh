#!/bin/sh
# Make this machine a radio to talk to, and print the supplicant's own words
# about it. Run on the booted image, in a pane, as root:
#
#   /usr/share/tos/wifi-witness.sh
#
# which is where iso/mkiso.sh puts this file in the rootfs, so that the
# machine under test carries the test.
#
# Nothing in docs/design/wifi.md had met a radio when it was written. This is
# what a boot uses instead of one: mac80211_hwsim, a kernel module that makes
# radios out of nothing and lets them hear each other. Two of them — wlan0 is
# the machine's, the one tOS joins with, and wlan1 is turned into an access
# point here (wpa_supplicant's own AP mode: WPA2-PSK, ssid "hwsim-ap",
# passphrase "correct horse") with busybox udhcpd behind it on 10.99.0.1/24.
#
# The access point lives in a network namespace of its own, and that is not
# tidiness. The first run of this script kept both radios in one namespace
# and proved two things that were not true of any real network: the client's
# DISCOVER arrived carrying the *cable's* address as its source (the kernel
# picks one from any link when the sending link has none), and a packet whose
# source is one of the receiving host's own addresses is a martian, dropped
# before udhcpd could see it. Then the lease's gateway, 10.99.0.1, was an
# address of the same host, which no kernel will accept as a next hop. Both
# are what "the access point is this machine" means, and neither is what a
# laptop meets. A namespace makes wlan1 another host with its own routing
# table: the DISCOVER is a stranger's, the gateway is across the air, and
# `ping 10.99.0.1` is a round trip rather than loopback. Moving a radio is
# `iw phy ... set netns` — a mac80211 device cannot be moved by name, only by
# its phy — which is why `iw` is on the image.
#
# What it prints at the end is SCAN_RESULTS, STATUS and LIST_NETWORKS as
# wpa_cli read them off the control socket, labelled and with the supplicant's
# version beside them, on stdout and on /dev/ttyS0 where there is one. That
# text is what tos-system's three parsers are tested against, verbatim: copy
# it out of the serial log into the tests rather than typing what it ought to
# have said.
#
# wpa_cli is used here and nowhere else on this image. The compositor talks to
# the same socket itself, one datagram at a time, and that is the thing under
# test; a witness that drove tOS's own client would be asking the code whether
# it agrees with itself.
#
# Then the part no script can do, which is somebody at the keyboard:
#
#   super+shift+n            the network menu; wlan0 is in it
#   wlan0                    the link menu
#   join a wireless network  the overlay; hwsim-ap is in the list — give the
#                            first scan ten seconds on hwsim
#   hwsim-ap                 the passphrase prompt, drawn masked
#   correct horse            status: joining hwsim-ap, then joined hwsim-ap,
#                            then asking for an address on wlan0, then the
#                            lease: wlan0 10.99.0.x/24 via 10.99.0.1
#   ip route                 from a pane: a default route through each link,
#                            the cable's at metric 100 and the radio's at 600
#   ping 10.99.0.1           from a pane, which answers across the air
#
# and then the ones that are the actual gate:
#
#   the same join with a wrong passphrase, which has to say
#     "hwsim-ap: wrong passphrase" rather than time out — forget the network
#     from the link menu first;
#   the cable pulled — `VBoxManage controlvm <vm> setlinkstate1 off` from the
#     host — after which `ip route get 8.8.8.8` has to name wlan0 and the ping
#     has to keep answering, and back on, after which it names the cable again;
#   a reboot, which has to rejoin with no keystroke at all
#     (SAVE_CONFIG wrote the network into /etc/wpa_supplicant/tos-wlan0.conf,
#     and on the live image's tmpfs /etc that only holds until the reboot —
#     so this one is a test for an installed machine);
#   this script's AP killed — `pkill -f 'wpa_supplicant.*ap.conf'`, and by
#     that pattern rather than by name: pids are not namespaced the way the
#     network is, and a bare `pkill wpa_supplicant` takes wlan0's own
#     supplicant with it, which the first run of this found out — after which
#     the link menu has to show wlan0 without its network.
#
# Running it again after each of those is the point: it leaves the access
# point and the lease server where they are and re-prints the three dumps, so
# the STATUS at COMPLETED, the LIST_NETWORKS with [CURRENT] on it and the one
# with [TEMP-DISABLED] all come out of runs that had something to say.
set -eu
AP_IF=wlan1
CLIENT_IF=wlan0
SSID=hwsim-ap
PSK="correct horse"
RUN=/run/tos-witness
NS=tos-ap

# Everything a person is meant to read goes to the screen and to the serial
# line both, the way iso/init's `say` does it: a pane is where this is typed
# and a serial log is where CI and anybody debugging a headless boot reads it
# back from. Not twice, though, when stdout already *is* the serial line —
# the way this is usually run from a pane is `> /dev/ttyS0`, and a log with
# every line doubled is a log nobody copies fixtures out of.
say() {
    echo "$@"
    if [ -c /dev/ttyS0 ] && [ "$(readlink /proc/$$/fd/1 2>/dev/null)" != /dev/ttyS0 ]; then
        echo "$@" >/dev/ttyS0 2>/dev/null
    fi
    return 0
}

# Both radios, out of nothing. Already loaded is not a failure: the script is
# meant to be run twice on one boot while something is being worked out.
say "witness: loading mac80211_hwsim"
modprobe mac80211_hwsim radios=2 2>/dev/null ||
    lsmod | grep -q '^mac80211_hwsim' || {
    say "witness: mac80211_hwsim would not load — is /lib/modules on this root?"
    exit 1
}

# The interfaces appear a moment after the module does, and udev starts a
# supplicant on each of them as they do. On a second run wlan1 is already in
# the namespace and not here, which is fine.
for _ in 1 2 3 4 5 6 7 8 9 10; do
    [ -d "/sys/class/net/$CLIENT_IF" ] && break
    sleep 1
done
[ -d "/sys/class/net/$CLIENT_IF" ] || {
    say "witness: $CLIENT_IF never appeared"
    exit 1
}

in_ap() { ip netns exec "$NS" "$@"; }

if ip netns list 2>/dev/null | grep -q "^$NS\b"; then
    say "witness: the $NS namespace is already there"
else
    # The AP side must not also be a client. udev's rule fires on every
    # wlan, so wlan1 has a tos-supplicant of its own by now, and a supplicant
    # holding a radio cannot be moved out from under it.
    say "witness: taking tos-supplicant@$AP_IF off $AP_IF"
    systemctl stop "tos-supplicant@$AP_IF.service" 2>/dev/null || true
    sleep 1
    [ -d "/sys/class/net/$AP_IF" ] || {
        say "witness: $AP_IF never appeared"
        exit 1
    }
    say "witness: moving $AP_IF into the $NS namespace"
    ip netns add "$NS"
    iw phy "$(cat "/sys/class/net/$AP_IF/phy80211/name")" set netns name "$NS" || {
        say "witness: could not move $AP_IF's phy into $NS"
        exit 1
    }
    in_ap ip link set lo up
fi

mkdir -p "$RUN"

# wpa_supplicant's own AP mode rather than hostapd, which is not on this image
# and would be a package carried for a test. mode=2 is the whole of it: the
# same daemon, the same config file, an access point instead of a station.
# frequency=2412 is channel 1, and proto/pairwise say WPA2-PSK-CCMP because
# that is what the client is meant to see in the scan's flags — and because
# CCMP is the cipher whose kernel module the first run of this found missing.
cat >"$RUN/ap.conf" <<EOF
ctrl_interface=$RUN/ap-ctrl
update_config=0
network={
	ssid="$SSID"
	mode=2
	frequency=2412
	key_mgmt=WPA-PSK
	proto=RSN
	pairwise=CCMP
	psk="$PSK"
}
EOF

# Idempotent from here on, because the useful thing to do with this script is
# run it again after each keystroke step above and keep the dumps at the end.
if in_ap pgrep -f "wpa_supplicant.*$RUN/ap.conf" >/dev/null 2>&1; then
    say "witness: the access point on $AP_IF is already up"
else
    say "witness: starting the access point on $AP_IF"
    in_ap wpa_supplicant -B -Dnl80211 -i"$AP_IF" -c"$RUN/ap.conf" ||
        { say "witness: the access point would not start"; exit 1; }
    sleep 2
fi

# The AP's own address, and then a lease server behind it, because a join that
# ends with no address is a join tOS's status line cannot finish announcing.
# The router and the resolver in the lease are both the AP, so that the route
# tOS installs and the resolv.conf it writes are both things a pane can check.
in_ap ip addr flush dev "$AP_IF" 2>/dev/null || true
in_ap ip addr add 10.99.0.1/24 dev "$AP_IF"
in_ap ip link set "$AP_IF" up
cat >"$RUN/udhcpd.conf" <<EOF
interface $AP_IF
start 10.99.0.10
end 10.99.0.50
option subnet 255.255.255.0
option router 10.99.0.1
option dns 10.99.0.1
option lease 3600
lease_file $RUN/udhcpd.leases
pidfile $RUN/udhcpd.pid
EOF
if in_ap pgrep -f "udhcpd.*$RUN/udhcpd.conf" >/dev/null 2>&1; then
    say "witness: udhcpd is already serving $AP_IF"
else
    say "witness: starting udhcpd on $AP_IF (10.99.0.10-10.99.0.50)"
    # busybox's udhcpd, which is on this image already: the `busybox` package
    # is in the rootfs and `busybox --list` has udhcpd in it, so a DHCP server
    # for this test costs nothing and installs nothing.
    : >"$RUN/udhcpd.leases"
    in_ap busybox udhcpd -S "$RUN/udhcpd.conf" ||
        { say "witness: udhcpd would not start"; exit 1; }
fi

if [ ! -S "/run/wpa_supplicant/$CLIENT_IF" ]; then
    say "witness: no control socket at /run/wpa_supplicant/$CLIENT_IF"
    say "witness: is tos-supplicant@$CLIENT_IF running? (systemctl status)"
    exit 1
fi

# Give the client one scan of its own before anything is read out of it: a
# supplicant with no network configured is INACTIVE and does not scan by
# itself, and on hwsim a scan takes a few seconds to come back.
wpa_cli -p /run/wpa_supplicant -i "$CLIENT_IF" scan >/dev/null 2>&1 || true
sleep 6

# What all of this was for. Labelled, because the three parsers are three
# different formats and the tests want to know which is which, and with the
# supplicant's version beside them, because the format is that program's and
# a later one may print another.
dump() {
    say ""
    say "--- $1 ($CLIENT_IF, $(wpa_supplicant -v | head -1)) ---"
    say "$(wpa_cli -p /run/wpa_supplicant -i "$CLIENT_IF" "$1" 2>&1)"
}
say ""
say "=== witness: the text below goes into tos-system's tests verbatim ==="
dump scan_results
dump status
dump list_networks
say ""
say "=== witness: $SSID is up on $AP_IF in the $NS namespace; join it from the menu ==="
