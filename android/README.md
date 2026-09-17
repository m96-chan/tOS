# tOS standalone Android APK

Keep Android installed and run tOS as an ordinary app. This ARM64 preview
embeds the Rust compositor and its PTY sessions in a native Android window.
It requires neither Termux, root nor an unlocked bootloader.

The preview starts Bash in the app's private home directory, with the desktop
character greeting and HackGenConsoleNF font. Tools are included in the APK;
first launch needs no package download. It does **not** include Debian, `apt`,
Termux's `pkg`, or `sudo`.
Files in the home directory survive app updates signed with the same key;
uninstalling the app removes them.

## Included tools

The aim is the PC edition's terminal toolset, using Android ARM64 builds:

| PC tools | APK |
| --- | --- |
| Bash, Git, curl, less, Neovim | Included |
| ripgrep, fzf, OpenSSH, rsync, unzip, file, Yazi | Included |
| GNU coreutils, findutils, grep, sed, gawk, diffutils; tar/compression tools | Included |
| `man-db` | `mandoc` supplies `man` and bundled manuals |
| `btop` | `htop`; Android restricts visibility into other apps' processes |
| HackGenConsoleNF | Same 2.10.0 font as the PC image, including Japanese and icons |

The pinned set contains 84 packages including dependencies. The current APK
is about 78 MiB. Programs run as the app UID on both rooted and non-rooted
devices; root privileges are not requested. They are Android builds, not the
PC image's Linux binaries, and their versions can differ from the ISO.

The binaries come from the official Termux package repository. The Termux
**application** is not required. Package versions, hashes, source locations
and build details are recorded in [userland/](userland/README.md).
The bundle is updated by installing a new APK. An on-device package manager,
arbitrary downloaded native programs and native editor plugins are not yet
supported. PC boot/system services and hardware controls are not part of this
app environment.

## Build

On Linux x86_64, install Rust, JDK 17, Python 3.9+, `curl`, `ar` (binutils),
`zip`, Android SDK platform 35,
recent SDK build-tools (including `aapt2`, `d8`, `zipalign`, `apksigner`),
and the Android NDK. The build has been exercised with NDK 29.0.14206865
and SDK build-tools 36.1.0.

```sh
rustup target add aarch64-linux-android
bash android/build-apk.sh
```

The script finds the SDK at `ANDROID_HOME`, `ANDROID_SDK_ROOT`, or
`~/Android/Sdk`, and the NDK at `ANDROID_NDK_HOME`, `ANDROID_NDK_ROOT`, or
the newest installed SDK NDK directory. No Gradle setup is needed.
The first build downloads the pinned packages and font, verifies their
SHA-256 hashes, and caches them under `dist/android/package-cache`.

Outputs:

```text
dist/android/tos-android-arm64.apk
dist/android/tos-android-arm64.apk.sha256
```

The APK targets API 35 and requires API 24 or newer on ARM64. Native libraries
and APK packaging use 16 KiB alignment; operation on a 16 KiB-page device
still needs testing.

Local builds use a development signing key retained at
`dist/android/development.keystore`. Keep that file to install future builds
over the existing app without losing its private data. For distribution,
provide your own key using `TOS_ANDROID_KEYSTORE`, `TOS_ANDROID_KEY_ALIAS`,
`TOS_ANDROID_STORE_PASSWORD` and, optionally, `TOS_ANDROID_KEY_PASSWORD`.
Do not distribute the development keystore as a release identity.

## Install

Copy the APK to the phone and open it with Android's package installer, or
use USB debugging with the intended device selected explicitly:

```sh
adb devices -l
adb -s DEVICE_SERIAL install -r dist/android/tos-android-arm64.apk
adb -s DEVICE_SERIAL shell am start -n io.github.m96chan.tos/.MainActivity
```

The launcher app is **tOS**, package `io.github.m96chan.tos`. It requests
network access for shell programs, with no storage or root permission.
No other terminal app needs to be installed or running.

## Phone controls

| Control | Behavior |
| --- | --- |
| **A− / A+** | Adjust text by 0.5 sp, retaining the shell and scrollback |
| Pinch | Change text size when the gesture ends |
| **⌨** | Show or hide the Android keyboard; resize the PTY to available space |
| Tap a pane | Focus it |
| Swipe vertically | Send wheel events for terminal scrollback or mouse-aware programs |
| **Esc / Ctrl / Alt / Tab / arrows** | Terminal keys; Ctrl and Alt apply to the next input |
| **⋮**, or long press | Split, close or zoom panes; workspaces; text selection and clipboard |

Text defaults to 10.5 sp and can be adjusted from 8 to 24 sp. The top bar
shows terminal columns and rows. The terminal renders at the surface's pixel
resolution, avoiding the nested backend's half-block scaling. IME composing
text appears above the extra keys and is sent to the shell when committed.
The existing `Ctrl+A` prefix shortcuts also work with a hardware keyboard.
In selection mode, use the existing keyboard selection controls.

## Implementation and limits

- `app/`: Android activity, keyboard/IME connection and native surface bridge.
- `native/`: Rust static library wrapping the existing compositor.
- `userland/`: pinned Android packages, Bash defaults and asset assembler.
- `build-apk.sh`: compile, package, align, sign and verify the APK.

All native session calls run on one worker thread. Window size changes resize
the existing PTYs; changing fonts does not restart programs. Activity
orientation changes retain the session. Going into the background pauses
rendering, but there is no foreground service yet: Android may terminate
the process, and closing the activity ends its sessions. Files persist;
running processes do not survive a process restart.

Session persistence, Android document sharing and dedicated touch
text-selection handles remain future work. Programs run under the app UID
and Android's normal sandbox, not as the ADB shell user. Native code is
installed by Android from the signed APK; only data and interpreted scripts
are extracted into writable app storage. `~/.bashrc` is created once and
preserved on updates; it sources the managed defaults in `$PREFIX/etc/tos/bashrc`.

## Validation

The signed APK was built and installed on a Pixel 10a running Android 17,
with a locked bootloader and 4096-byte pages. The app and its child shell
were observed running under the app UID. On-device checks verified shell
commands, Gboard text input and Enter, Japanese composition and committed
text, top/bottom pane splitting, and keyboard-driven surface resizing.
With HackGen at 8 sp, the portrait terminal has 90 columns. Changing font
size preserved the shell PID and updated `stty size`.
Returning from the Android home screen also retained the same shell PID and
accepted another command. This verifies an ordinary background/foreground
transition, not survival after Android kills the app process.

Pixel's CFF2 Noto fallback font initially produced invisible Japanese glyphs;
enabling the parser's
variable-font support fixed this, verified with `あア漢` on the device and a
small synthetic CFF2 regression font.

On-device instrumentation verified 14 checks under the actual app UID:
Bash, a local Git commit, curl and certificates, headless Neovim, ripgrep,
fzf filtering, SSH key generation, rsync file copying, unzip/file, manual
pages, Yazi, htop, script interpreter relocation, and HTTPS certificate
verification. The character greeting, interactive Neovim and Yazi were also
checked on screen. These checks do not establish compatibility with every
subcommand, plugin or remote server.

After installing and opening the app, reproduce the device checks with:

```sh
bash android/test-device.sh DEVICE_SERIAL
```

This builds a test-only APK signed with the same key, runs isolated fixtures
in the app cache and removes the test APK afterward. It requires Android 8
or newer, `rg` on the host, and network access for the HTTPS check. Running
instrumentation restarts the tOS process, so finish work in active sessions
first; reopen tOS afterward. The user's home directory is not used for tests.

Package assembly boundary tests run without network or the SDK:

```sh
python3 -m unittest discover -s android/userland -p 'test_*.py' -v
```

Native unit tests cover pixel
channel order, key translation and surface bounds. A compositor test checks
that a font change preserves the PTY and updates its reported column count.
All 534 library tests across the Android bridge, compositor and font crates
passed. The Android crate and compositor also pass Android-target Clippy
checks. APK signature and 16 KiB ELF alignment checks passed.

The previous nested-backend experiment is documented in
[TERMUX.md](TERMUX.md) and built separately with `android/build.sh`.

References: [Android native APIs](https://developer.android.com/ndk/guides/stable_apis),
[IME input connection](https://developer.android.com/reference/android/view/inputmethod/BaseInputConnection),
[16 KiB page sizes](https://developer.android.com/guide/practices/page-sizes).
