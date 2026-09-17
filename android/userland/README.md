# Bundled Android tools

`packages.lock.json` pins the ARM64 packages from the official
[Termux package repository](https://packages.termux.dev/apt/termux-main/)
and HackGenConsoleNF 2.10.0 from
[HackGen](https://github.com/yuru7/HackGen/releases/tag/v2.10.0).
The APK uses these Android builds without installing or launching Termux.
The requested tools are listed in `prepare.py`; dependencies are included
except `termux-tools`, whose app setup is replaced by tOS's installer.
No Debian maintainer script is executed during packaging or installation.

## Build and updates

`bash android/build-apk.sh` invokes `prepare.py`. Downloads are verified
against the checked-in SHA-256 hashes and cached in
`dist/android/package-cache`. A missing pinned upstream archive causes a
build failure; the build never silently chooses another version.
Keep these archives when reproducible builds across repository rotations
are needed.

To intentionally update packages, obtain the official AArch64 `Packages`
index and inspect the matching source recipes, then run:

```sh
python3 android/userland/prepare.py --pin /path/to/Packages \
  --recipe-commit FULL_TERMUX_PACKAGES_COMMIT_HASH
```

Review the changed versions, dependencies and checksums before building and
running the device tests. Pinning resolves package names and alternatives
from a consistent repository snapshot; it is not a general Debian dependency
solver. The font pin is retained and updated separately. Source recipes live
in [termux-packages](https://github.com/termux/termux-packages); the lock stores
the source revision used for provenance.

## APK layout and startup

Android targets API 35. Android 10 and later
[restrict execution from writable app storage](https://developer.android.com/about/versions/10/behavior-changes-10#execute-permission).
All package ELF programs and libraries therefore go into the APK's
`lib/arm64-v8a` directory under stable, path-derived names. Android installs
them into `nativeLibraryDir` as part of installing the signed APK. The
assembler checks ARM64 little-endian ELF headers and 16 KiB load alignment.

Only data, documentation, fonts and interpreted scripts enter
`assets/userland.zip`. `Userland.java` extracts this into a generation named
by its SHA-256 digest, creates symlinks from original program paths to the
installed native libraries, and points `$PREFIX` (`files/usr`) at that
generation. Links are refreshed after every APK update because Android can
change `nativeLibraryDir`. Old data generations are currently retained;
they may consume additional storage after updates. User files live
separately in `files/home`, with a `.bashrc` that sources the managed defaults.

`paths.c`, preloaded only into shell children, relocates the packages'
compiled Termux prefix to `$PREFIX`, and `/tmp` to `$TMPDIR`. It also launches
scripts through their interpreter. It does not make writable ELF files
executable. Neovim's small launcher sets its LuaJIT preload relative to
`$PREFIX`. This compatibility layer is intentionally bounded: downloaded
native plugins/programs and an on-device package manager need further work.

`motd.c` displays the existing desktop splash through the compositor's Kitty
graphics support, scales it to the pane and follows it with a short tool
greeting. `motd_ascii` is also available under `$PREFIX/share/tos`.

## Attribution

Package binaries retain the license/copyright files present in their data
archives. The installed `$PREFIX/share/tos/packages.json` records versions,
download hashes and homepages; `$PREFIX/share/doc/tos/PACKAGE-SOURCES.txt`
lists upstream homepages and pinned Termux build recipe links. The bundled
font license is included at `$PREFIX/share/doc/hackgen/LICENSE`, sourced
from [HackGen's license](https://github.com/yuru7/HackGen/blob/v2.10.0/LICENSE).
Downloaded packages, the font archive and signing keys are build artifacts
and are not checked into the tOS repository.
