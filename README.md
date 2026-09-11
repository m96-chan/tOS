# tOS

**tOS is a terminal-native Linux environment where the terminal is the desktop.**

Instead of building a traditional GUI desktop around windows, panels, launchers, GTK, Qt, or Electron, tOS treats the terminal itself as the primary application platform.

The long-term goal is simple:

> **Linux, but the display model is a terminal.**

Text is rendered with ANSI/Unicode, rich media is exposed through terminal graphics protocols such as Kitty Graphics, and applications are primarily TUIs.

## Concept

Traditional Linux desktops look roughly like this:

```text
Linux Kernel
    ↓
Display Server / Compositor
    ↓
Desktop Environment
    ↓
GTK / Qt / Electron / ...
    ↓
Applications
```

tOS aims for this:

```text
Linux Kernel
    ↓
Terminal Display Layer
    ↓
TUI Applications
```

For the first implementation, the stack will intentionally be much more practical:

```text
Linux Kernel
    ↓
Debian minimal
    ↓
Wayland
    ↓
Cage
    ↓
Kitty
    ↓
tmux
    ↓
TUI applications
```

Boot the machine and land directly in a full-screen terminal workspace.

No desktop shell.
No panel.
No launcher.
No conventional window manager workflow.

Just the terminal.

## Why Debian?

The initial tOS prototype is based on **Debian minimal**.

Debian is not necessarily the smallest possible base, but it is a good development platform for the early stages of tOS because it provides:

- broad hardware support
- predictable package management
- easy access to Kitty, Wayland, PipeWire, networking tools, fonts, and development packages
- a large ecosystem for experimenting with TUI software
- fewer distractions from building and debugging the actual tOS layer

Once the architecture stabilizes, smaller bases such as Alpine, Buildroot, or a custom root filesystem may become interesting.

The base distribution is an implementation detail. The terminal-native UX is the project.

## Display Model

tOS treats terminal capabilities as a primitive display API.

```text
Text
  → ANSI / Unicode

Images
  → Kitty Graphics Protocol

Video
  → terminal graphics frames

Interactive UI
  → TUI

Web
  → terminal-native browser
```

A future tOS display layer may expose a hybrid model using:

- ANSI escape sequences
- Unicode and box drawing
- true color
- Kitty Graphics Protocol
- Sixel
- mouse reporting
- keyboard protocol extensions
- clipboard integration

The goal is not to recreate a conventional GUI inside a terminal.

The goal is to make the terminal itself sufficient.

## Browser

One major missing piece in a terminal-only desktop is a modern web browser.

A future tOS browser could use WebKit as the web engine while replacing or adapting the final presentation layer for terminal output.

Conceptually:

```text
HTML / CSS / JavaScript
        ↓
      WebKit
        ↓
 Layout / Paint
        ↓
Terminal Renderer
   ├─ ANSI cells
   ├─ Unicode
   ├─ Kitty Graphics
   └─ Sixel
```

Unlike traditional text browsers, the objective is to preserve modern browser behavior while targeting a terminal-native display.

## Candidate Applications

A usable tOS environment could initially be composed from existing software:

| Purpose | Candidate |
| --- | --- |
| Terminal | Kitty |
| Multiplexer | tmux / Zellij |
| Editor | Neovim / Helix |
| File manager | Yazi |
| Git UI | lazygit |
| Process monitor | btop |
| Network configuration | nmtui |
| Bluetooth | bluetuith |
| Shell | Bash / Zsh / Fish |
| Web browser | TBD |

The exact applications are not fixed. Applications should be replaceable as long as they fit the terminal-native model.

## Architecture Roadmap

### Phase 0 — Prototype

Build a bootable Debian-based image that launches directly into:

```text
Cage → Kitty → tmux
```

Goals:

- boot directly into the terminal workspace
- keyboard and mouse support
- networking
- audio
- SSH
- image preview through Kitty Graphics
- usable development environment

### Phase 1 — tOS Shell

Create a coherent terminal desktop experience around existing TUI software.

Possible components:

- session launcher
- application launcher
- status UI
- notification system
- power controls
- device controls
- configuration UI

### Phase 2 — Terminal-native Browser

Experiment with a modern browser engine targeting terminal output.

Possible paths:

- WebKit offscreen rendering → terminal quantization
- WebKitGTK/WPE integration
- Kitty Graphics backed rendering
- terminal-aware DOM presentation

### Phase 3 — Remove the Desktop Stack

Reduce dependence on a conventional Wayland environment.

```text
Linux
  ↓
DRM / KMS
  ↓
tOS terminal compositor
  ↓
PTY / TUI applications
```

The terminal compositor would eventually own responsibilities such as:

- DRM/KMS output
- font rendering
- ANSI parsing
- terminal cells
- Kitty Graphics Protocol
- keyboard input
- mouse input
- clipboard
- IME integration
- PTY/session management

At this point, tOS stops being "Linux that automatically starts Kitty" and becomes a genuinely terminal-native graphical environment.

## Non-goals

At least initially, tOS is **not** trying to:

- replace the Linux kernel
- invent a new package manager
- rebuild every Unix utility
- emulate a normal desktop environment pixel-for-pixel
- prohibit graphical content

Images and video are welcome. Conventional GUI application models are simply not the center of the system.

## Philosophy

The Unix terminal has spent decades acting as an abstraction over text streams.

Modern terminals can already display true color, images, rich keyboard events, hyperlinks, and increasingly sophisticated interactive interfaces.

So tOS asks a slightly dangerous question:

> What happens if the terminal stops being an application and becomes the desktop itself?

That is the experiment.

## Status

Very early experimental project.

Expect architectural changes, broken ideas, strange prototypes, and aggressive removal of unnecessary GUI layers.

## License

TBD
