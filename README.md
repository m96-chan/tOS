# tOS

**tOS is a terminal-native Linux environment where the terminal is the display system.**

The project does not aim to build a conventional desktop and then place a terminal on top of it.

tOS starts from the opposite direction:

> **The terminal is the desktop, compositor, session model, and primary application platform.**

There is no X11 requirement, no Wayland requirement, no desktop environment, and no dependency on an existing terminal emulator such as Kitty.

The target architecture is designed from day one around direct Linux display and input APIs.

---

## Goal

The long-term architecture is:

```text
┌──────────────────────────────────────────────┐
│                 TUI Applications             │
│                                              │
│  shell   neovim   yazi   lazygit   browser  │
└──────────────────────┬───────────────────────┘
                       │ PTY / terminal protocol
                       ▼
┌──────────────────────────────────────────────┐
│                 tOS Compositor               │
│                                              │
│  Terminal Cells        Session / Pane Model  │
│  ANSI / VT Parser      PTY Manager           │
│  Kitty Graphics        Input Routing         │
│  Font / Glyph Engine   Clipboard / IME       │
│  Image Surfaces        Notifications         │
└──────────────────────┬───────────────────────┘
                       │
              DRM/KMS / libinput / evdev
                       │
                       ▼
┌──────────────────────────────────────────────┐
│                 Linux Kernel                 │
│                                              │
│  DRM/KMS   Input   Audio   Network   USB     │
└──────────────────────────────────────────────┘
```

Booting tOS should eventually mean:

```text
Firmware / Bootloader
        ↓
Linux Kernel
        ↓
tOS Compositor
        ↓
$ _
```

No graphical desktop needs to exist underneath it.

---

## Core Idea

Traditional Linux desktops are usually structured like this:

```text
Linux Kernel
    ↓
Display Server / Compositor
    ↓
Desktop Environment
    ↓
GUI Toolkit
    ↓
Application
```

tOS instead makes the terminal abstraction itself the native display model:

```text
Linux Kernel
    ↓
tOS Terminal Compositor
    ↓
PTY / Terminal Protocol
    ↓
Application
```

The project is not trying to emulate a GUI inside a terminal window.

There is no terminal window.

**The terminal is the screen.**

---

## Design Principles

### 1. No X11 or Wayland dependency

The core compositor talks directly to Linux display and input facilities.

Initial targets:

- DRM/KMS for display output
- evdev or libinput for keyboard, pointer, and touch input
- PTYs for applications
- Linux virtual terminal / direct session ownership where necessary

Wayland support may exist later as a compatibility backend for development or nesting, but it is not part of the core architecture.

### 2. Terminal-native does not mean text-only

tOS should support rich graphical content without introducing a conventional widget toolkit.

```text
Text
  → terminal cells / glyphs

Color
  → true color

Images
  → Kitty Graphics compatible surfaces

Video
  → rapidly updated image surfaces

Interactive applications
  → PTY / TUI

Web
  → terminal-native browser
```

Images, video, diagrams, previews, and web content are welcome.

The constraint is not "no graphics".

The constraint is **no conventional GUI application model as the foundation of the system**.

### 3. Kitty is a protocol reference, not a dependency

tOS does not embed or launch the Kitty terminal emulator.

Instead, useful terminal extensions should be implemented directly by the tOS compositor.

Important compatibility targets include:

- ANSI / VT escape sequences
- true color
- OSC sequences
- Kitty Graphics Protocol
- Kitty keyboard protocol
- terminal mouse reporting
- hyperlinks
- clipboard integration

Existing terminal applications should work without knowing that they are running directly on the display server.

### 4. Panes are native objects

A conventional desktop manages graphical windows.

A terminal multiplexer manages PTYs.

tOS combines those concepts.

```text
┌─────────────────────┬─────────────────────┐
│                     │                     │
│       neovim        │        yazi         │
│                     │                     │
├─────────────────────┴─────────────────────┤
│                                           │
│                  shell                    │
│                                           │
└───────────────────────────────────────────┘
```

The compositor owns:

- PTY creation
- pane layout
- focus
- resizing
- sessions
- workspaces
- input routing

A separate tmux layer should not be required for the core desktop model.

### 5. Linux distribution != tOS

tOS is the display/session environment.

The initial userspace can be Debian without making tOS conceptually dependent on Debian.

```text
tOS
├── Linux kernel
├── tOS compositor
└── userspace/rootfs
    └── Debian initially
```

Debian provides a practical first root filesystem because it offers:

- amd64 and arm64 support
- glibc
- apt / .deb ecosystem
- broad hardware and software compatibility
- a large selection of terminal applications
- a convenient development environment

Later rootfs targets may include other distributions or a purpose-built image.

---

## Platform Architecture

The compositor and user experience should remain portable across machines.

Platform-specific hardware support lives below that boundary.

```text
tOS/
├── compositor/
│   ├── terminal/
│   ├── renderer/
│   ├── graphics/
│   ├── input/
│   ├── pty/
│   ├── session/
│   └── shell/
│
├── platform/
│   ├── generic-x86_64/
│   ├── generic-arm64/
│   └── android/
│
└── rootfs/
    └── debian/
```

### Generic PC / SBC

```text
Linux Kernel
    ↓
DRM/KMS + input
    ↓
tOS Compositor
    ↓
Debian userspace
```

### Android hardware

Android devices are primarily a hardware enablement problem, not a userspace problem.

The intended model is:

```text
Bootloader
    ↓
Linux / Android-derived Kernel
    ↓
Device-specific drivers / firmware / vendor compatibility
    ↓
DRM/KMS or platform display backend
    ↓
tOS Compositor
    ↓
Debian arm64 userspace
```

Some devices may eventually require Android compatibility layers for hardware support. Those should remain platform adapters rather than leaking into the terminal/session architecture.

The goal is for the same tOS compositor and TUI environment to run on:

- x86_64 PCs
- arm64 PCs
- SBCs
- repurposed Android phones and tablets

---

## Internal Compositor Model

A terminal cell is the fundamental text layout primitive, but graphical surfaces may coexist with cells.

A simplified conceptual cell model:

```text
Cell
├── glyph
├── foreground
├── background
├── attributes
└── graphics reference
```

The renderer may therefore combine:

```text
Terminal Grid
     +
Glyph Cache
     +
Image Surfaces
     ↓
GPU / CPU Renderer
     ↓
DRM/KMS framebuffer
```

The first renderer does not need to be sophisticated.

A CPU-rendered DRM dumb buffer is enough to prove the architecture.

GPU acceleration can be added later without changing the terminal/session model.

---

## Implementation Stack

The implementation language is **Rust**.

```text
Display      DRM/KMS via direct ioctls
Input        evdev via direct reads
PTY          POSIX pseudoterminals
Fonts        built-in bitmap face, plus TrueType
Shaping      not yet; per cell glyph placement
Rendering    CPU framebuffer
             GPU acceleration later
Audio        not yet
Rootfs       Debian
```

The dependency surface is deliberately small: `libc` for the kernel
interfaces, and `fontdue` for TrueType rasterization. DRM/KMS, evdev and the
virtual terminal are spoken to directly rather than through a wrapper crate,
so the first milestone has nothing between tOS and the kernel.

## Browser

A modern browser is one of the most important missing pieces in a terminal-native desktop.

Traditional text browsers intentionally simplify the web.

tOS eventually wants something different:

```text
HTML / CSS / JavaScript
        ↓
      WebKit
        ↓
 Layout / Paint
        ↓
tOS Presentation Backend
   ├── terminal cells
   ├── glyph runs
   ├── image surfaces
   └── input events
```

The long-term experiment is whether a real browser engine can treat the tOS compositor as a native presentation target.

A browser should be able to mix semantic terminal UI with graphical surfaces instead of flattening every webpage into plain text.

---

## First Milestone

The first milestone is deliberately small, but it must already use the final architecture.

### tOS v0.0.1

```text
Linux Kernel
    ↓
DRM/KMS
    ↓
tOS compositor
    ↓
PTY
    ↓
shell
```

Success means:

- no X11
- no Wayland
- no Cage
- no Kitty process
- obtain a DRM/KMS display
- render a monospace font
- create a PTY
- start a shell
- parse enough ANSI/VT sequences for interactive shell use
- route keyboard input into the PTY
- display `$ _` directly from the tOS compositor

That is the first real tOS.

Everything after that grows from the same architecture.

This milestone is implemented. See [Status](#status).

---

## Roadmap

Implemented items are ticked. Everything ticked is covered by tests in the
workspace; see [Status](#status) for what has and has not been run on real
hardware.

### 0.0.1 — Direct terminal

- [x] DRM/KMS initialization
- [x] framebuffer renderer
- [x] font loading
- [x] basic terminal grid
- [x] basic ANSI / VT parser
- [x] keyboard input
- [x] PTY
- [x] shell

### 0.0.2 — Usable terminal

- [x] UTF-8
- [x] true color
- [x] scrollback
- [x] resize
- [x] cursor styles
- [x] mouse support
- [x] clipboard
- [x] font fallback

Line reflow on width changes is still missing: narrowing a pane truncates
wrapped lines rather than re-wrapping them.

### 0.0.3 — Native compositor

- [x] multiple PTYs
- [x] pane splitting
- [x] focus management
- [x] workspaces
- [x] session persistence — resolved by decision: a restart starts clean with one terminal ([#5](https://github.com/m96-chan/tOS/issues/5)); shells surviving the compositor is a separate feature ([#6](https://github.com/m96-chan/tOS/issues/6))
- [x] compositor key bindings

### 0.0.4 — Graphics

- [x] Kitty Graphics Protocol
- [x] image surfaces
- [x] scaled texture cache
- [x] image previews
- [x] video experiments

Raw RGB and RGBA transmission work, including chunked transfers, placements
and deletion. PNG payloads (`f=100`) and zlib-compressed payloads (`o=z`) are
decoded in-tree by a hand-written DEFLATE and a PNG reader covering every
colour type, bit depths 1 through 16, all five scanline filters and Adam7
interlacing, which is the shape an image preview arrives in. A payload that
cannot be decoded is still answered with the protocol's error response rather
than being silently dropped, so applications can fall back instead of hanging.

Only inline transmission is read, though. `t=f` and `t=t`, where the
application hands over a path instead of the bytes, are still refused, and
that is the route some file managers take for a large image. No preview tool
has been run against tOS yet, so what is verified is the decoding, not the
integration.

Placements are scaled once and the result is kept, so a repeat frame costs a
blend instead of a resample. The cache is a plain CPU one, bounded in bytes
and evicted least-recently-used; there is no GPU pipeline under it to upload
textures to, which is why the roadmap item no longer says there is.

Moving pictures reach a terminal as animation frames, and those play. An
image can carry frames sent with `a=f`, each one a rectangle of new pixels
composed over an earlier frame or over a flat background colour, blended or
copied; `a=a` starts and stops the animation, sets the frame on screen, the
loop count and each frame's gap. The compositor steps every pane's
animations from its tick, wakes in time for the next frame rather than on
its idle timer, and repaints only the rows the moving image covers.

`cargo run --example graphics_animation -- /tmp/tos-animation` transmits a
thirty frame animation into a real pane and saves ten pictures of it
playing. Composing between two frames that already exist (`a=c`) is still
answered with an error, and frames carry the same raw formats images do.

### 0.0.5 — System UI

- [x] status interface
- [x] notifications
- [ ] launcher
- [x] power controls
- [x] network controls
- [ ] Bluetooth controls
- [ ] audio controls



`tos-system`'s `power` module reads `/sys/class/power_supply`: which supplies
are batteries and which are chargers, charge from `capacity` or worked out
from `energy_*` or `charge_*` when it is missing, charging state, and time to
empty or to full where a rate is reported — absent rather than invented where
it is not, which covers the idle battery whose `power_now` is zero. Two
batteries are weighted into one reading, and a machine with none says so.
Powering off and rebooting call `reboot(2)` directly, after `sync(2)`, because
tOS may be PID 1 with no init to ask; suspend writes `mem` to
`/sys/power/state`. All three sit behind a trait, so the tests assert what was
asked for without the machine acting on it. There is no UI on any of this yet,
and the syscall path itself is only exercised on a real Linux machine.

The network is read out of `/sys/class/net` and `/proc/net` with no
NetworkManager under it: every interface and what sort it is, link and carrier
state, MAC, MTU, speed, byte counters, IPv4 and IPv6 addresses from
`getifaddrs`, and which interface holds the default route. It can also bring a
link administratively up or down, which is one `SIOCSIFFLAGS` ioctl. Joining a
wireless network is not part of it: an associated interface's SSID and signal
are reported, but scanning, WPA and DHCP need nl80211 and a supplicant, and
those are a later item of their own.

Bluetooth is read out of `/sys/class/bluetooth` and acted on over an
`AF_BLUETOOTH` socket. `tos-system` lists the adapters, reads each one's
address, whether it is up and whether rfkill has it blocked, takes an adapter
up or down with `HCIDEVUP` and `HCIDEVDOWN`, sets and clears the soft block by
writing to `/dev/rfkill`, lists the links the kernel currently holds, and runs
an inquiry for devices in range. Pairing and connecting are not there, and the
box stays unticked for that reason: both are BlueZ, BlueZ is D-Bus, and doing
them here instead means implementing SMP and an agent to answer for the user,
which is a piece of work in its own right rather than a missing function.

### 0.1 — Portable tOS

- [x] generic x86_64 image
- [ ] generic arm64 image
- [ ] Debian rootfs tooling
- [x] install / boot tooling
- [ ] hardware abstraction cleanup

`iso/` builds a bootable x86_64 image, and `tos-install` puts it on a disk
from inside a pane. What lands on the disk is still the busybox initramfs
world rather than a Debian rootfs, so the two remaining userspace items are
the same piece of work.

### Later — Android devices

- Android boot image support
- device adaptation layer
- touch-first terminal interaction
- mobile power management
- vendor hardware integration where necessary

### Later — Web

- WebKit experiments
- terminal-native browser
- graphical web surfaces
- keyboard / pointer / touch web interaction

## Candidate Applications

Existing TUI applications can provide most of the initial userspace.

| Purpose | Candidate |
| --- | --- |
| Shell | Bash / Zsh / Fish |
| Editor | Neovim / Helix |
| File manager | Yazi |
| Git UI | lazygit |
| Process monitor | btop |
| Network configuration | nmtui |
| Bluetooth | bluetuith |
| Media | terminal-native / Kitty Graphics aware tools |
| Web browser | tOS browser, TBD |

Applications are replaceable.

The compositor and protocol model are the platform.

---

## Non-goals

tOS is not currently trying to:

- replace the Linux kernel
- invent a package manager
- replace the Unix process model
- rebuild every command-line utility
- emulate GNOME, KDE, Windows, or macOS
- forbid images, video, or graphical content
- make every existing GUI application run unchanged

Compatibility layers may appear later, but they should not dictate the core design.

---

## Philosophy

Terminal emulators traditionally live inside a graphical desktop.

Modern terminals already support rich color, images, hyperlinks, complex keyboard events, mouse input, and increasingly sophisticated interfaces.

The experiment behind tOS is to invert the relationship:

> **What if the terminal does not run inside the desktop?**
>
> **What if the terminal is the desktop?**

Not a desktop that launches Kitty.

Not a Wayland session with only one window.

Not a minimal Linux distribution with a terminal theme.

A terminal-native display and session system built directly on Linux.

That is tOS.

---

## Status

Early, but running. The first milestone is implemented: tOS obtains a DRM/KMS
display, renders a monospace font into a dumb buffer, creates PTYs, starts
shells, parses ANSI/VT, and routes evdev input, with no X11, no Wayland, no
Cage and no Kitty process anywhere in the stack.

What has been exercised, and how:

| Area | Verified by |
| --- | --- |
| Terminal model, parser, grid, graphics protocol | unit and behavioural tests |
| Glyph engine, box drawing, TTF rasterization | unit tests, rendered ASCII art |
| Renderer | pixel-level tests over a real framebuffer |
| PTYs, signals, window size, controlling terminal | tests that fork real processes |
| Input encoding, both legacy and Kitty | unit tests, plus a decode round trip |
| Layout, focus, workspaces, key bindings | unit tests |
| Whole compositor | tests that run shells in split panes and inspect pixels |
| DRM/KMS, evdev, VT ownership | compile for x86_64 and arm64 Linux; ioctl numbers and structure layouts are unit-tested against the kernel headers |
| Installer | the whole sequence against a recorded backend, plus the real binary driven on a pseudoterminal with its output read back through tOS's own terminal emulator |

Panes refuse to split once they are too small to divide, rather than creating
a pane with nowhere to go, and a virtual terminal is only taken over once the
VT switch signals have handlers, so switching away with Ctrl+Alt+F2 cannot
leave the console in graphics mode with no keyboard.

The last row is the honest gap: the kernel-facing backends have not yet been
run on hardware. Everything above them has, through the nested and headless
backends.

## Repository layout

```text
tOS/
├── installer/           tos-install: put tOS on a disk from the live session
├── iso/                 bootable image and its initramfs
└── compositor/
    ├── tos-term/        terminal model: cells, grid, VT parser, graphics
    ├── tos-font/        glyph engine: bitmap face, box drawing, TrueType
    ├── tos-render/      CPU renderer: surfaces, grid painting
    ├── tos-pty/         pseudoterminals
    ├── tos-input/       key and mouse model, encoders, evdev
    ├── tos-session/     pane tree, focus, workspaces, key bindings
    ├── tos-platform/    display backends: DRM/KMS, nested, headless
    └── tos-compositor/  the `tos` binary
```

Only `tos-platform` and `tos-input` contain Linux-specific code. Everything
else is portable, which is what makes the compositor testable away from the
target hardware.

## Building and running

```sh
cargo build --release
cargo test
```

On Linux hardware, from a virtual terminal with no display server running:

```sh
sudo ./target/release/tos
```

Access to `/dev/dri/card0` and `/dev/input/event*` is required; adding the
user to the `video` and `input` groups avoids needing root.

To develop on any Unix, run tOS nested inside another terminal. The pixel
framebuffer is encoded as half block characters, so the whole compositor,
renderer and font stack are exercised exactly as they would be on hardware:

```sh
./target/release/tos --backend nested
```

To render a frame without a display at all:

```sh
./target/release/tos --screenshot /tmp/tos.ppm --size 1280x720 \
    -e /bin/sh -c 'ls; sleep 1'
```

`tos --help` lists the options and the default key bindings. The leader key is
`ctrl+a`; on hardware the same bindings work directly with `super`. The two
bindings that grow a session are also where most people expect them:
`ctrl+shift+enter` splits the focused pane and `ctrl+shift+t` opens a new
workspace.

## Installing

`iso/build.sh` builds a bootable image. Booting it gives a live session whose
every shell prints the banner from `.motd_art` and the one line that matters:

```text
  Type tos-install to install tOS on this machine.
```

`tos-install` is a TUI running in a pane — installing tOS is the first real
use of the platform as a platform. It will not write to a disk until the
disk's own name has been typed, and it refuses the medium it booted from.
`tos-install --plan` prints every command it would run without running any.
See [`iso/README.md`](iso/README.md).

## License

TBD
