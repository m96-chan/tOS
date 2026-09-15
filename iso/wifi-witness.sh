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
#   join a wireless network  the overlay; hwsim-ap is in the list
#   hwsim-ap                 the passphrase prompt, drawn masked
#   correct horse            status: joining hwsim-ap, then joined hwsim-ap,
#                            then the lease: wlan0 hwsim-ap 10.99.0.x/24
#   ping 10.99.0.1           from a pane, which answers
#
# and then the three that are the actual gate:
#
#   the same join with a wrong passphrase, which has to say
#     "hwsim-ap: wrong passphrase" rather than time out;
#   a reboot, which has to rejoin with no keystroke at all
#     (SAVE_CONFIG wrote the network into /etc/wpa_supplicant/tos-wlan0.conf,
#     and on the live image's tmpfs /etc that only holds until the reboot —
#     so this one is a test for an installed machine);
#   this script's AP killed — `pkill -f 'wpa_supplicant.*wlan1'` — which has
#     to show wlan0 losing its carrier in the menu.
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

# Everything a person is meant to read goes to the screen and to the serial
# line both, the way iso/init's `say` does it: a pane is where this is typed
# and a serial log is where CI and anybody debugging a headless boot reads it
# back from. `tee` would need the file to exist; this does not.
say() {
    echo "$@"
    [ -c /dev/ttyS0 ] && echo "$@" >/dev/ttyS0 2>/dev/null
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
# supplicant on each of them as they do.
for _ in 1 2 3 4 5 6 7 8 9 10; do
    [ -d "/sys/class/net/$CLIENT_IF" ] && [ -d "/sys/class/net/$AP_IF" ] && break
    sleep 1
done
for interface in "$CLIENT_IF" "$AP_IF"; do
    [ -d "/sys/class/net/$interface" ] || {
        say "witness: $interface never appeared"
        exit 1
    }
done

# The AP side must not also be a client. udev's rule fires on every wlan, so
# wlan1 has a tos-supplicant of its own by now, and two supplicants on one
# interface is two programs fighting over one netlink socket.
say "witness: taking tos-supplicant@$AP_IF off $AP_IF"
systemctl stop "tos-supplicant@$AP_IF.service" 2>/dev/null || true
sleep 1

mkdir -p "$RUN"

# wpa_supplicant's own AP mode rather than hostapd, which is not on this image
# and would be a package carried for a test. mode=2 is the whole of it: the
# same daemon, the same config file, an access point instead of a station.
# frequency=2412 is channel 1, and proto/pairwise say WPA2-PSK-CCMP because
# that is what the client is meant to see in the scan's flags.
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
# run it again after each keystroke step below and keep the dumps at the end:
# a STATUS reading COMPLETED and a LIST_NETWORKS with [CURRENT] on it can only
# come from a run made after a join, and the [TEMP-DISABLED] line the wrong
# passphrase produces only from a run made after that. So a second run leaves
# the access point and the lease server exactly where they were.
if pgrep -f "wpa_supplicant.*$RUN/ap.conf" >/dev/null 2>&1; then
    say "witness: the access point on $AP_IF is already up"
else
    pkill -f "wpa_supplicant.*-i$AP_IF" 2>/dev/null || true
    say "witness: starting the access point on $AP_IF"
    wpa_supplicant -B -Dnl80211 -i"$AP_IF" -c"$RUN/ap.conf" ||
        { say "witness: the access point would not start"; exit 1; }
fi

# The AP's own address, and then a lease server behind it, because a join that
# ends with no address is a join tOS's status line cannot finish announcing.
sleep 2
ip addr flush dev "$AP_IF" 2>/dev/null || true
ip addr add 10.99.0.1/24 dev "$AP_IF"
ip link set "$AP_IF" up

# busybox's udhcpd, which is on this image already — the `busybox` package is
# in the rootfs for #109 and `busybox --list` has udhcpd in it, so a DHCP
# server for this test costs nothing and installs nothing. It wants its lease
# file's directory to exist and will not make one.
cat >"$RUN/udhcpd.conf" <<EOF
interface $AP_IF
start 10.99.0.10
end 10.99.0.50
option subnet 255.255.255.0
option router 10.99.0.1
option lease 3600
lease_file $RUN/udhcpd.leases
pidfile $RUN/udhcpd.pid
EOF
if pgrep -f "udhcpd.*$RUN/udhcpd.conf" >/dev/null 2>&1; then
    say "witness: udhcpd is already serving $AP_IF"
else
    [ -f "$RUN/udhcpd.leases" ] || : >"$RUN/udhcpd.leases"
    say "witness: starting udhcpd on $AP_IF (10.99.0.10-10.99.0.50)"
    busybox udhcpd "$RUN/udhcpd.conf" ||
        { say "witness: udhcpd would not start"; exit 1; }
fi

# And the client's supplicant, which udev started when wlan0 appeared. If it
# is not there the machine will say "no supplicant on wlan0" in the menu, and
# the dumps below would be empty for a reason that has nothing to do with the
# parsers.
if [ ! -S "/run/wpa_supplicant/$CLIENT_IF" ]; then
    say "witness: no control socket at /run/wpa_supplicant/$CLIENT_IF"
    say "witness: is tos-supplicant@$CLIENT_IF running? (systemctl status)"
    exit 1
fi

# Give the client one scan of its own before anything is read out of it: the
# supplicant scans on its own schedule and the first one may not have run.
wpa_cli -p /run/wpa_supplicant -i "$CLIENT_IF" scan >/dev/null 2>&1 || true
sleep 5

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
say "=== witness: $SSID is up on $AP_IF; join it from the menu ==="
