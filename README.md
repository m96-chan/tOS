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

## Proposed Implementation Stack

The current preferred implementation language is **Rust**.

Possible low-level building blocks:

```text
Display      DRM/KMS
Input        evdev / libinput
PTY          Linux PTY
Fonts        FreeType or Rust font stack
Shaping      HarfBuzz where required
Rendering    CPU framebuffer first
             GPU acceleration later
Audio        PipeWire / ALSA userspace
Rootfs       Debian
```

The exact dependencies are intentionally not frozen yet.

The architecture matters more than any individual library.

---

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

---

## Roadmap

### 0.0.1 — Direct terminal

- DRM/KMS initialization
- framebuffer renderer
- font loading
- basic terminal grid
- basic ANSI / VT parser
- keyboard input
- PTY
- shell

### 0.0.2 — Usable terminal

- UTF-8
- true color
- scrollback
- resize
- cursor styles
- mouse support
- clipboard
- font fallback

### 0.0.3 — Native compositor

- multiple PTYs
- pane splitting
- focus management
- workspaces
- session persistence
- compositor key bindings

### 0.0.4 — Graphics

- Kitty Graphics Protocol
- image surfaces
- GPU-backed texture cache
- image previews
- video experiments

### 0.0.5 — System UI

- launcher
- status interface
- notifications
- power controls
- network controls
- Bluetooth controls
- audio controls

### 0.1 — Portable tOS

- generic x86_64 image
- generic arm64 image
- Debian rootfs tooling
- install / boot tooling
- hardware abstraction cleanup

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

---

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

Very early experimental project.

The first target is direct DRM/KMS output with a PTY-backed shell.

## License

TBD
