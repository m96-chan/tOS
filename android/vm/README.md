# App-owned Debian VM

```text
Android activity / Rust compositor
  └─ one tos-vmclient per local PTY
      └─ same-UID abstract Unix socket → VmService (foreground service)
          ├─ AVF vm CLI → app-private ext4 disk → Debian/systemd
          │   └─ tOS guest agent → independent Debian PTYs
          └─ libslirp host helper ← framed Ethernet → guest TAP
```

`init.c` mounts the disk, refreshes managed bridge files from the initramfs,
and enters Debian. `boot.sh` installs cached Debian packages on first boot,
then starts systemd. The agent owns guest PTYs and `/dev/net/tun`. Android
never needs a TAP device. `client.c` forwards terminal input, output and
SIGWINCH; `net.c` provides app-owned IPv4 NAT/DNS.

Each frame has a one-byte type, a big-endian 32-bit session ID and a big-endian
32-bit payload length, at most 65536 bytes. Types are `O` (open), `R` (resize),
`D` (terminal data), `C` (close), `E` (exit), `N` (Ethernet), `Q` (poweroff).
Open/resize payloads contain four big-endian uint16 values: columns, rows,
width and height in pixels. Network/poweroff use ID zero; pane IDs are assigned
by the service after checking the local socket peer's Android UID. No shell
listener is exposed to the network.

The service waits for `TOS_VM_READY` before forwarding frames. Binary traffic
uses hvc0, kernel/system logs use hvc2 (`hypervisor.log`), and the VM CLI's own
stdout is redirected to `vm-cli.log`. CLI stderr goes to `vm-error.log`.
An early UART console retains boot failure diagnostics until hvc2 takes over.
The kernel command line follows the official image's `arm64.nompam` and
`8250.nr_uarts=4` settings. Initial package configuration explicitly writes to
hvc0 before READY and systemd inherits null standard streams afterward.
This separation is required: systemd status output and the CLI's shutdown
message must not be interpreted as binary frames.

## Image inputs

- Debian bookworm ARM64 base: Docker digest
  `sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171`.
- Kernel: Google's [AVF Debian image archive](https://dl.google.com/android/ferrochrome/latest/aarch64/images.tar.gz),
  build `ferrochrome/aarch64/hourly-6422-Thu Jan 22 00:01:39 UTC 2026`.
  Linux `6.12.60-android16-6-gf33ca06267f8-ab14749849-4k`;
  [kernel source](https://android.googlesource.com/kernel/common/+/f33ca06267f8),
  [GPL-2.0 license and exceptions](https://android.googlesource.com/kernel/common/+/f33ca06267f8/COPYING).
  `prepare-kernel.sh` verifies both archive and kernel SHA-256. The upstream
  URL can change; a changed hash fails the build and requires a reviewed pin
  update. This is a pinned January 2026 kernel, not a claim of latest security
  patches. Kernel updates require rebuilding the APK; guest apt updates do
  not replace the kernel selected by AVF.
- Yazi 26.9.1: official ARM64 musl Debian archive, SHA-256 in
  `prepare-rootfs.sh`, matching the PC image's version.
- Core tools/dependencies: bookworm, bookworm-updates and bookworm-security
  repositories, downloaded into the initial disk for offline configuration.
- Guest init/agent: statically linked NDK binaries; no Android process is
  called from inside Debian. APK updates refresh these files on next boot.
- Character image: `compositor/tos-compositor/assets/splash.png`.

The build uses an ordinary x86 Debian container for filesystem assembly and
APT's ARM64 package resolution. It never runs ARM64 maintainer scripts on the
host. Docker's exclusions of manuals and unsafe dpkg write setting are
removed from the interactive guest. The initial disk is a 3 GiB ext4 image;
its SHA-256 is checked while it is expanded on the phone. It is installed
atomically and is never overwritten on an ordinary APK update.

The Android host still contains the previous preview's pinned native runtime
bundle, including libslirp and its dependencies, plus HackGen. The guest
shell always runs Debian; reducing the redundant legacy host tools is a
separate packaging optimization. See [../userland/](../userland/README.md)
for their upstream packages, licenses and source recipes. Debian packages
retain their `/usr/share/doc/*/copyright` notices.
