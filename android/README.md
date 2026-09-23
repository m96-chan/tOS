# tOS Android — Debian VM preview

Keep Android installed and run tOS in a standalone ARM64 APK. The app owns a
Debian VM through Android's Virtualization Framework (AVF), and displays its
shells in tOS's native compositor. No Termux or stock Terminal app is launched.
The character greeting, HackGen font, adjustable text size, Japanese IME,
touch controls and pane splits remain in the Android UI.

This preview requires an AVF-capable Android 15+ device and **two one-time
ADB permission grants**. It has been exercised on a non-rooted Pixel 10a with
Android 17, a locked bootloader and 4 KiB pages. Root is not requested.
Installation alone is not sufficient, and other devices/OS builds have not
been established as compatible. See [AVF.md](AVF.md) for the verified entry
point and its limitations.

## Debian environment

The current development APK is about 212 MiB.
The APK contains a Debian 12 (bookworm) ARM64 disk and Linux kernel. The guest
runs Bash, Git, curl, less, Neovim, ripgrep, fzf, OpenSSH client, rsync, unzip,
file, man-db, btop, Yazi, sudo, systemd and the usual Debian base utilities.
These are Linux programs inside Debian; they can use `apt` and install native
Linux dependencies. The package list is [vm/packages.txt](vm/packages.txt).
The desktop hardware/DRM session and Wi-Fi controls are supplied by Android
instead of the PC ISO's boot services.

First launch expands a 20 GiB disk and configures the bundled packages offline.
Allow at least 20.1 GiB free space **after** installing the APK. Subsequent
starts reuse this disk. The guest has 1 GiB RAM and one virtual CPU. Shells
start as guest root; this does not grant root access to Android.

```sh
apt update
apt install jq
```

IPv4 networking uses app-owned libslirp NAT, including DNS and HTTPS. It needs
Android's Internet permission, not root, a VPN, or a host TAP device. There is
no inbound port-forwarding UI, Android document sharing or direct access to
Android's files. Guest `localhost` refers to Debian. The Android host's
loopback services are not exposed by this NAT.

Files and installed packages survive VM shutdown and same-key APK updates.
Updates refresh the managed guest bridge and startup defaults, while keeping
`/root/.bashrc` and the disk. Existing Debian packages are updated using apt;
an APK update does not overwrite the disk with a new factory image. Data from
the previous Android-native preview remains in the app's old `files/home`
directory and is not automatically imported into Debian.
Uninstalling tOS or clearing its app data removes its disks.

## Build

On Linux x86_64, install Docker, Rust, JDK 17, Python 3.9+, curl, binutils,
zip, Android SDK platform 35, SDK build-tools and the Android NDK. Tested with
NDK 29.0.14206865 and build-tools 36.1.0. No Gradle setup is needed.

Which NDK is not a detail here. The guest's `init` and `agent` are statically
linked against its libc, and r27's gives them a TLS segment aligned to 8 bytes
where Bionic wants 64: the binaries build and sign cleanly, then the guest
kernel panics on PID 1 and the VM reboots instead of initialising, which is
how v0.0.8 shipped. `android/vm/prepare.py` now refuses to put such a binary
in the initramfs, so a machine whose default NDK is too old fails the build
rather than the phone.

```sh
rustup target add aarch64-linux-android
bash android/vm/build-image.sh
bash android/build-apk.sh
```

The SDK is found through `ANDROID_HOME`, `ANDROID_SDK_ROOT`, or
`~/Android/Sdk`; the NDK through `ANDROID_NDK_HOME`, `ANDROID_NDK_ROOT`, or the
newest installed SDK NDK directory. Docker must be usable by the build user.
The image builder downloads ARM64 Debian packages without executing them on
the host. Their maintainer scripts run on first boot inside the actual VM.
No QEMU binfmt registration or privileged build container is required.

The Debian base image, AVF kernel archive, Yazi and Android runtime packages
have pinned hashes. Debian package updates/dependencies are resolved from the
bookworm repositories when rebuilding the initial disk; the complete APK is
not a bit-reproducible Debian snapshot. Retain `dist/android` caches if
upstream package archives rotate. [vm/README.md](vm/README.md) records the
kernel source, protocol and build layout.

Outputs:

```text
dist/android/tos-android-arm64.apk
dist/android/tos-android-arm64.apk.sha256
```

The APK targets and requires API 35. Android host libraries use 16 KiB ELF
alignment; a 16 KiB-page device still needs testing. The Linux guest uses its
own 4 KiB-page kernel independently of Android's page size.

Local builds retain a development signing key at
`dist/android/development.keystore`. Keep it for updates without data loss.
For distribution, supply `TOS_ANDROID_KEYSTORE`, `TOS_ANDROID_KEY_ALIAS`,
`TOS_ANDROID_STORE_PASSWORD` and optionally `TOS_ANDROID_KEY_PASSWORD`.
Do not distribute the development keystore as a release identity.

## Install

Enable USB debugging and select the intended device explicitly:

```sh
adb devices -l
adb -s DEVICE_SERIAL install --no-incremental -r dist/android/tos-android-arm64.apk
adb -s DEVICE_SERIAL shell pm grant io.github.m96chan.tos android.permission.MANAGE_VIRTUAL_MACHINE
adb -s DEVICE_SERIAL shell pm grant io.github.m96chan.tos android.permission.USE_CUSTOM_VIRTUAL_MACHINE
adb -s DEVICE_SERIAL shell am start -n io.github.m96chan.tos/.MainActivity
```

Its launcher icon is an adaptive icon whose foreground is the splash art's
own character, cut out of `compositor/tos-compositor/assets/splash.png`: the
character rests against the bottom of the mask, so the crop through its arms
falls outside the viewport every launcher shows, and its face stays inside
the 66dp safe circle. The foreground PNGs are scaled from the pixel art by
whole numbers before being resampled down, which is what keeps the pixels
square at every density.

A tag push builds this APK in CI and attaches it to the GitHub release, so
the usual way to get one is to download it rather than to build it. That
build signs with the identity in the `TOS_ANDROID_KEYSTORE_BASE64` and
`TOS_ANDROID_STORE_PASSWORD` repository secrets, which has to be the keystore
earlier releases were signed with: Android refuses an update signed by a key
it has not seen, and the uninstall that would make room for it takes the
guest Debian disk with it. Local builds keep their own identity in
`dist/android/development.keystore` for the same reason — the file is
ignored by git, so a second machine building the same commit produces an APK
that cannot update the first one's.

The launcher app is **tOS** (`io.github.m96chan.tos`). Missing permissions
produce a setup dialog. If the OS refuses these development permission
grants, this preview cannot launch its VM on that build. It does not change
SELinux, hidden-API settings, the bootloader or the stock Terminal's data.

## Phone controls and lifecycle

| Control | Behavior |
| --- | --- |
| **A− / A+** | Adjust text by 0.5 sp without restarting shells |
| Pinch | Change text size when the gesture ends |
| **⌨** | Show/hide the Android keyboard and resize the guest PTY |
| Tap a pane | Focus it |
| Swipe vertically | Terminal wheel events |
| Soft keyboard | ASCII types straight through; CJK keyboards compose above the extra keys |
| **Esc / Ctrl / Alt / Tab / arrows** | Terminal keys; modifiers apply to the next input |
| Long press and drag | Select text; lifting copies it to the Android clipboard |
| Long press and lift | Clipboard menu: paste, copy the selection, or **More…** |
| **⋮** | Panes, workspaces, selection, clipboard, Debian shutdown |

Text defaults to 10.5 sp and is adjustable from 8 to 24 sp. The top bar shows
columns and rows. Rendering uses the surface's pixels, without half-block
scaling. Hardware-keyboard `Ctrl+A` prefix shortcuts also work.

### Android desktop windowing (experimental)

The APK does not disable Android's resizable-window behavior. Its terminal
surface is wired to follow window size and display-density changes; the top
toolbar can scroll horizontally in a narrow window while **⋮** stays visible.
A physical keyboard sends terminal keys. `Ctrl+Shift+C` / `Ctrl+Shift+V` copy
the selection to / paste from Android's clipboard. Mouse and trackpad input
includes click-and-drag selection, middle/right buttons, hover and vertical or
horizontal wheel events. `Shift+right-click` opens tOS's Android clipboard
menu; an unmodified right-click is passed to the terminal application.

This is not yet validated on an external monitor or in freeform desktop
windowing. The current APK has no Android document import/export, and no
multi-instance window contract: use one tOS window until those paths have
device tests. Android still owns the desktop, display and input devices; the
two initial AVF ADB permission grants above are unchanged. Track the remaining
work in [issue #181](https://github.com/m96-chan/tOS/issues/181).

Before claiming desktop support, check on an AVF-capable device with an
external display, keyboard and mouse/trackpad: resize and maximize repeatedly,
move the window between displays of different densities, unplug and reconnect
the display, switch focus with another app, and verify `stty size`, pointer
selection, copy/paste and the guest disk after each transition. Record the
device and Android build because desktop-window and AVF behavior vary by OS.

ASCII from the soft keyboard reaches the pane as it is typed rather than
waiting for the keyboard to commit a word: a shell answers a character at a
time, and completion, history search and `^C` all happen before a word is
finished. What the keyboard later changes its mind about — an autocorrection,
a glide typing correction — is reconciled with backspaces, so the pane ends
up with what the keyboard finally says. A keyboard whose current language
composes Japanese, Chinese or Korean is the exception and keeps the
composition bar above the extra keys: its romaji are on their way to becoming
something else, and typing them through would put a `k` in the pane that has
to be taken back a keystroke later. Switching that keyboard to its Latin
language (Gboard's globe key) switches the pane back to direct input.

Long press is the clipboard rather than the session menu: a phone has no
`Ctrl+Shift+V`, and a `PATH` typed by hand on a soft keyboard is a typo
waiting to happen. Holding and dragging selects text and copies it on
release; holding without dragging opens paste, copy and **More…** at the
finger. Pasted text arrives bracketed, so a shell sees it as one line.

A foreground service owns the VM while tOS is in the background. Use
**⋮ → Shut down Debian** for a clean shutdown. The service also defines a
notification shutdown action; its visibility follows Android notification
settings. Reopen the activity to start another session. Closing an activity
ends its panes; the VM service can continue. Android may still kill the app
under resource pressure, and app updates/instrumentation stop the process.
Files persist, but live programs do not survive process/VM termination.
The guest filesystem is journaled; abrupt termination can lose unsaved work.

## Validation

On the Pixel 10a, all 18 device checks passed, including an actual first
installation and a clean VM reboot. After aligning the kernel boot options
with the official AVF image, an eight-reboot run passed all 32 checks. The test drives the same app-private
bridge used by visible panes:
Debian/glibc startup, Git commits, headless Neovim, ripgrep/fzf, SSH key
creation, rsync, manuals, btop/Yazi, resizing, independent pane sessions,
HTTPS, apt update/install and persistence across a clean VM reboot.

```sh
bash android/test-device.sh DEVICE_SERIAL
```

The on-screen check verified the character image, Japanese glyphs, command
input, and top/bottom splitting into separate guest shells. Changing from
8.5 to 9 sp retained the guest shell PID while `stty size` changed from
38×90 to 36×83 (rows×columns); 8.5 sp was restored afterward.

Set `TOS_VM_REBOOTS=8` to repeat the shutdown/persistence cycle; set
`TOS_VM_DIAGNOSTICS=yes` to include logs from the previous launch.
The test APK uses the same signing key and is removed afterward. Tests restart
the app, create temporary guest fixtures and install jq. Reopen tOS afterward.
For an explicit first-boot test, add `--fresh`: it renames the previous disk to
`rootfs.img.backup-TIMESTAMP` before expanding a new disk. This needs another
20.1 GiB of free space; backups are retained, not automatically deleted.

Host checks:

```sh
python3 -m unittest discover -s android/userland -p 'test_*.py' -v
bash android/vm/test-protocol.sh
cargo test -p tos-android -p tos-compositor -p tos-font --lib
```

The 534 Android/compositor/font library tests, five packaging boundary tests,
VM stream protocol tests, Android-target Clippy, Rust formatting and shell
lint checks passed.

The previous nested-backend experiment is documented in
[TERMUX.md](TERMUX.md). Android-only runtime provenance is in
[userland/README.md](userland/README.md); these libraries are an implementation
detail of the host bridge, not Debian's package manager.
