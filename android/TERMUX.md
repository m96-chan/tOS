# tOS on Android, through Termux

This is the first Android bring-up path: keep Android installed and run the
tOS compositor inside Termux, without root or an unlocked bootloader. The
executable targets Android's Bionic libc directly. No Debian rootfs, PRoot,
GRUB, or `tos-install` is involved; programs in the panes are Termux programs.

The current display is the existing `nested` development backend. It encodes
the framebuffer as truecolor half-block characters. This establishes a way to
install and exercise the compositor; it is not yet a native Android screen or
a replacement for the PC installation.

## Build on a Linux x86_64 host

Install Rust and the Android NDK (r28 or newer recommended), then:

```sh
rustup target add aarch64-linux-android
bash android/build.sh
```

The script uses `ANDROID_NDK_HOME` or `ANDROID_NDK_ROOT` when set. Otherwise it
selects the newest NDK in `ANDROID_HOME/ndk`, `ANDROID_SDK_ROOT/ndk`, or
`~/Android/Sdk/ndk`. It builds for Android API 24 and ARM64, with 16 KiB ELF
segment alignment. Output:

```text
dist/android/tos-termux-aarch64.tar.gz
dist/android/tos-termux-aarch64.tar.gz.sha256
```

## Install on the phone

1. Install the official `com.termux` app using the
   [Termux installation instructions](https://github.com/termux/termux-app#installation).
   The tested installation used F-Droid's stable 0.118.3 release. Open it once
   and let its initial setup finish.
2. Copy the archive and its checksum into the phone's Download folder. For
   USB debugging, choose the intended device explicitly when more than one
   is attached:

   ```sh
   adb devices -l
   adb -s DEVICE_SERIAL push dist/android/tos-termux-aarch64.tar.gz /sdcard/Download/
   adb -s DEVICE_SERIAL push dist/android/tos-termux-aarch64.tar.gz.sha256 /sdcard/Download/
   ```

3. In **Termux**, allow shared-storage access, copy into its private home,
   verify, and install:

   ```sh
   termux-setup-storage
   # Grant the Android storage permission before the next commands.
   mkdir -p ~/tos-install
   cp ~/storage/downloads/tos-termux-aarch64.tar.gz* ~/tos-install/
   cd ~/tos-install
   sha256sum -c tos-termux-aarch64.tar.gz.sha256
   tar -xzf tos-termux-aarch64.tar.gz
   sh tos-termux-aarch64/install.sh
   tos
   ```

   Extract into Termux's private home as shown: shared storage is not an
   executable filesystem. The installer checks the ARM64 executable before
   installing it at `$PREFIX/libexec/tos/tos` and its launcher at
   `$PREFIX/bin/tos`. Repeating the install updates those two files.

The launcher selects the nested backend and smallest built-in font. Android
continues to handle screen locking and blanking. Extra tOS options can be
passed normally, for example `tos --no-status-bar` or `tos -e bash`.

### USB transfer without shared-storage access

This is the transfer used for the Pixel validation. Serve only the bundle
directory on the host's loopback interface, in a separate terminal:

```sh
python3 -m http.server 18086 --bind 127.0.0.1 --directory dist/android
```

On the host, forward that port through the selected USB device:

```sh
adb -s DEVICE_SERIAL reverse tcp:18086 tcp:18086
```

In Termux:

```sh
mkdir -p ~/tos-install
cd ~/tos-install
curl -fS http://127.0.0.1:18086/tos-termux-aarch64.tar.gz -o tos-termux-aarch64.tar.gz
curl -fS http://127.0.0.1:18086/tos-termux-aarch64.tar.gz.sha256 -o tos-termux-aarch64.tar.gz.sha256
sha256sum -c tos-termux-aarch64.tar.gz.sha256
tar -xzf tos-termux-aarch64.tar.gz
sh tos-termux-aarch64/install.sh
tos
```

After downloading, stop the host server with Ctrl+C and remove the forwarding
with `adb -s DEVICE_SERIAL reverse --remove tcp:18086`. tOS needs neither
connection to run.

## Using this first version

Pinch to reduce **Termux's** font size before starting tOS, and use landscape
orientation when possible. One host terminal cell represents only 1 x 2
framebuffer pixels, while one tOS bitmap character needs 6 x 11 pixels. An
ordinary 120-column Termux window therefore fits only about 20 tOS columns.
This limitation is in the display backend, not the ARM64 build.

Use Termux's CTRL extra key (or a hardware keyboard) for the `Ctrl+A` prefix:

| Keys | Action |
| --- | --- |
| `Ctrl+A`, then `d` | Split into columns |
| `Ctrl+A`, then `s` | Split into rows |
| `Ctrl+A`, then `?` | Show bindings |
| `Ctrl+A`, then `q` | Quit tOS and return to Termux |

Networking for shell applications is Android/Termux networking. The PC
hardware controls for Wi-Fi, Bluetooth, audio and power are not Android
integrations; their kernel interfaces may be unavailable or denied. This
bundle does not include the Debian packages and session setup from the ISO.

## Validation

On 2026-09-17, an ARM64 Android build ran on a Pixel 10a (`stallion`), Android
17, with a locked bootloader and 4096-byte pages. Running as the ordinary ADB
shell user, it created a PTY, executed `/system/bin/sh`, and rendered the
shell's output into a 720 x 400 headless screenshot. This verifies the native
executable, PTY and renderer.

The archive was then downloaded over USB into F-Droid Termux 0.118.3, its
checksum verified, and `install.sh` run as the app user. `tos --no-config`
opened an interactive shell; typing `echo TOS_ANDROID_OK` produced the expected
output, and `Ctrl+A d` created a second pane with another shell. The GitHub
APK was rejected by Android's install verification on this device; the
F-Droid-signed APK installed successfully without changing verification settings.

The host build used Rust 1.98.1 and Android NDK 29.0.14206865. ELF load segments
were checked for 16 KiB alignment; a 16 KiB-page device has not been tested.

Reproduce that check with a built bundle:

```sh
adb -s DEVICE_SERIAL push dist/android/tos-termux-aarch64/tos-bin /data/local/tmp/tos-android-probe
adb -s DEVICE_SERIAL shell chmod 755 /data/local/tmp/tos-android-probe
adb -s DEVICE_SERIAL shell /data/local/tmp/tos-android-probe --version
adb -s DEVICE_SERIAL shell '/data/local/tmp/tos-android-probe --no-config --backend headless --size 720x400 --bitmap-scale 2 --idle-lock 0 --screenshot /data/local/tmp/tos-android-probe.ppm -e /system/bin/sh -c "printf \"tOS on Android\\nPTY shell OK\\n\"; sleep 1"'
adb -s DEVICE_SERIAL pull /data/local/tmp/tos-android-probe.ppm .
```

CI checks the compositor and its dependencies against `aarch64-linux-android`.
The PC disk installer is intentionally outside this Android target.

## Next step after bring-up

A usable phone interface needs a display backend that presents the existing
framebuffer at screen resolution, plus Android keyboard/IME, touch, resize,
and lifecycle handling. The nested backend remains useful for installation
and regression checks, but increasing its resolution by shrinking Termux's
font is only a development workaround.

References: [Termux](https://github.com/termux/termux-app),
[Android 16 KiB page sizes](https://developer.android.com/guide/practices/page-sizes).
