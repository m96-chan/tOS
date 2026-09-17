# tOS standalone Android APK

Keep Android installed and run tOS as an ordinary app. This ARM64 preview
embeds the Rust compositor and its PTY sessions in a native Android window.
It requires neither Termux, root nor an unlocked bootloader.

The preview starts `/system/bin/sh` in the app's private home directory. It
does **not** yet include Debian, `apt`, Termux's `pkg`, or the ISO's programs.
Files in the home directory survive app updates signed with the same key;
uninstalling the app removes them.

## Build

On Linux x86_64, install Rust, JDK 17, `zip`, Android SDK platform 35,
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
- `build-apk.sh`: compile, package, align, sign and verify the APK.

All native session calls run on one worker thread. Window size changes resize
the existing PTYs; changing fonts does not restart programs. Activity
orientation changes retain the session. Going into the background pauses
rendering, but there is no foreground service yet: Android may terminate
the process, and closing the activity ends its sessions. Files persist;
running processes do not survive a process restart.

This is an initial app shell. A bundled Linux userland, session persistence,
Android document sharing and dedicated touch text-selection handles remain
future work. PC hardware controls are not Android integrations. Programs run
under the app UID and Android's normal sandbox, not as the ADB shell user.

## Validation

The signed APK was built and installed on a Pixel 10a running Android 17,
with a locked bootloader and 4096-byte pages. The app and its child shell
were observed running under the app UID. On-device checks verified shell
commands, Gboard text input and Enter, Japanese composition and committed
text, top/bottom pane splitting, and keyboard-driven surface resizing.
At 8.5 sp the portrait terminal has 77 columns; at 9 sp it has 72. Changing
font size preserved the shell PID and updated `stty size`.
Returning from the Android home screen also retained the same shell PID and
accepted another command. This verifies an ordinary background/foreground
transition, not survival after Android kills the app process.

Android's shell startup file initially replaced the short prompt with a long
path; the launcher now sets `ENV=/dev/null` to retain `$ `. Pixel's CFF2 Noto
font initially produced invisible Japanese glyphs; enabling the parser's
variable-font support fixed this, verified with `あア漢` on the device and a
small synthetic CFF2 regression font.

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
