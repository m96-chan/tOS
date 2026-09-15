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
Audio        ALSA control interface via direct ioctls
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

Everything before 0.1 is a proof of concept and is run at that pace: a
milestone lands when its idea has been demonstrated, and a version can be
skipped outright rather than held up. 0.0.6 is one — it was merged without a
tag of its own, and `v0.0.7` is the release that carries it.

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

Bytes need not arrive inline. `t=f` names a file, `t=t` names one to read and
then delete, and `t=s` names a shared memory object — the route a file manager
takes for a large image rather than base64-ing megabytes through the PTY. What
a compositor that owns the machine will open on a program's say-so is its own
question, answered in `docs/design/graphics-file-transmission.md`.

`tos-preview` is the program that uses all of this, and it ships on the ISO
next to `tos` and `tos-install`. It reads a PNG, sizes it to the pane from the
cell metrics the kernel is already holding in the pane's `winsize`, and hands
the file to the terminal as `f=100` rather than decoding it first — the same
route a file manager takes, so what is exercised is the integration and not
only the decoder. It has been run in a pane and the resulting frame counted
pixel by pixel against the picture that went in (`preview/tests/pane.rs`), and
it has been run on the booted ISO under QEMU with the picture read off a
second disc — the live image itself still carries no image file to point it
at, which is the last thing between this and a session that can demonstrate
itself.

Placements are scaled once and the result is kept, so a repeat frame costs a
blend instead of a resample. The cache is a plain CPU one, bounded in bytes
and evicted least-recently-used; there is no GPU pipeline under it to upload
textures to, which is why the roadmap item no longer says there is. Handing an
image to a DRM overlay plane, so the display engine scales and composites it
during scanout and the CPU stops touching those pixels, is the part that was
reaching for, and is tracked separately as
[#31](https://github.com/m96-chan/tOS/issues/31).

Moving pictures reach a terminal as animation frames, and those play. An
image can carry frames sent with `a=f`, each one a rectangle of new pixels
composed over an earlier frame or over a flat background colour, blended or
copied; `a=a` starts and stops the animation, sets the frame on screen, the
loop count and each frame's gap. The compositor steps every pane's
animations from its tick, wakes in time for the next frame rather than on
its idle timer, and repaints only the rows the moving image covers.

`cargo run --example graphics_animation -- /tmp/tos-animation` transmits a
thirty frame animation into a real pane and saves ten pictures of it
playing. Composing between two frames that already exist (`a=c`) works, and a
frame may arrive as a PNG or a zlib payload rather than only as raw pixels.

### 0.0.5 — System UI

- [x] status interface
- [x] notifications
- [x] launcher
- [x] power controls
- [x] network controls
- [ ] Bluetooth controls
- [x] audio controls
- [x] configuration file
- [x] screen lock

Sound is driven straight through the kernel's control interface. `tos-system`
opens `/dev/snd/controlC<N>` and issues `SNDRV_CTL_IOCTL_CARD_INFO`,
`ELEM_LIST`, `ELEM_INFO`, `ELEM_READ` and `ELEM_WRITE` against structures
declared by hand from `asound.h`, so there is no libasound and no sound
daemon. It takes the first card that can actually play, then whichever of
`Master`, `PCM`, `Speaker` or `Headphone` playback volume that hardware
happens to expose, converts between that control's own range — rarely
`0..=100` — and a percentage, steps up and down within it, and mutes with the
card's switch or, on a card that has none, by turning the level down and
remembering where it was. Every ioctl goes through a trait, so all of it is
tested against a card built out of structures in a test; none of it has been
run against real hardware yet.

The volume keys a keyboard already has — `KEY_VOLUMEUP`, `KEY_VOLUMEDOWN`,
`KEY_MUTE` — reach it directly, with no modifier and no leader, and
`super+>`, `super+<` and `super+shift+m` do the same on a keyboard that has
none. Each press re-reads the card and puts what it found on the status bar,
rather than the level that was asked for: a card whose whole range is four
steps cannot be at 55%, and a muted card does not get louder when it is
turned up. A machine with no sound card says so once and then stops saying
it. Whether an installed system should get PipeWire instead is decided, with
what was measured to decide it, in [`docs/design/audio.md`](docs/design/audio.md):
it stays ALSA-only, because tOS sets the knob and never plays a sound, and
because PipeWire's units decline to start for root, which is the only user a
tOS session has. The machine has systemd since #110, so a person who installs
PipeWire on their own machine now has an init that will start it.



`tos-system`'s `power` module reads `/sys/class/power_supply`: which supplies
are batteries and which are chargers, charge from `capacity` or worked out
from `energy_*` or `charge_*` when it is missing, charging state, and time to
empty or to full where a rate is reported — absent rather than invented where
it is not, which covers the idle battery whose `power_now` is zero. Two
batteries are weighted into one reading, and a machine with none says so.
Powering off and rebooting call `reboot(2)` directly, after `sync(2)`, because
tOS may be PID 1 with no init to ask — which since #110 is only the rescue
session out of the initramfs, and is the next thing this should learn to tell
apart; suspend writes `mem` to
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

`super+space`, or `ctrl+a` then space, opens the launcher: a bordered box over
the panes with a query line and a list of every executable on `$PATH`. Typing
filters it by subsequence, so "gi" finds `git` and `gifbuild`, with shorter and
earlier matches first; the arrows or `ctrl+p` / `ctrl+n` move, enter runs the
selected program in a new pane, and escape closes the box without touching
anything. While it is up it owns the keyboard, so neither the pane underneath
nor the other bindings see a key. `$PATH` is read once when it opens, and a
directory that is missing, unreadable or enormous costs the launcher nothing
worse than the names it would have contributed.

The mouse reaches the box as well. A pointer moving over a row highlights it, a
left press on one chooses it — the same answer enter gives — the wheel scrolls a
list too long to fit, and a press outside closes it the way escape does. A
prompt is the exception: a click away from it keeps what has been typed, because
a list can be reopened unchanged and a half-typed name cannot be got back. The
gaps between panes are draggable in the same spirit: a press on a divider takes
hold of that divider and no other, moving the pointer moves it, and letting go
drops it. What the mouse is holding is one thing at a time, so a drag along a
divider never leaves a selection highlighted in the pane it crossed.

The box itself is not the launcher. It is a list-and-filter surface that takes
a title, a list of labels and their details, and reports which one was chosen —
which is exactly the shape the four remaining items need. Power, network,
Bluetooth and audio are the same overlay over a different list, and none of
them exists yet: there is no system layer behind them to list.

Settings now come from a file as well as from flags. The format and the search
order are under [Configuration](#configuration); what matters to the rest of
this milestone is the shape. Every setting is a plain field on one `Config`
struct, the file is applied to that struct before the flags are, and adding a
setting is one arm of one match. The sections for key bindings and the font
fallback list are named but not answered yet, and land with the code behind
them, the way `[status]` has.

The status bar is a list of segments per side rather than a fixed strip. It
ships showing the workspaces, the arrangement and the focused pane on the left,
and the message slot, the link, the battery and a clock on the right; `[status] left` and
`[status] right` name segments in the order they read on screen, out of
`workspaces`, `panes`, `title`, `layout`, `message`, `clock`, `battery`,
`network`, `volume` and `bluetooth`. A segment whose machine cannot answer — a battery on
a desktop, an adapter on a machine with no Bluetooth — draws nothing at all
rather than a slot saying so, and the rule that would have gone beside it does
not appear either. One segment on the bar is elastic, and by default it is the
message: everything else is as wide as its words, the message takes what is
left and is clipped with an ellipsis rather than dropped. A layout that leaves
`message` out gets the banner `--no-status-bar` already gets, because
rearranging the bar must not be a way to make every failure disappear.

`panes` is the answer to an unfocused pane having no title anywhere: the
compositor has tracked each pane's title all along and only ever drawn the
focused one. It is not on by default, because on the one-pane session almost
every session starts as it says what `title` says at three times the width, and
a bar that grows with the pane count is one that eventually pushes the clock
off the end.

The clock is `%H:%M` in the machine's own zone unless told otherwise.
`[status] clock` takes a `strftime` subset — `%Y %y %m %d %e %H %I %M %S %p %P
%a %A %b %B %j %Z %z %F %T %R` and `%%`, with anything else copied out verbatim
so a typo is visible rather than silent. `[status] timezone` takes `local`,
`utc`, or a zone name such as `Asia/Tokyo`. `local` means `TZ` if it is set and
`/etc/localtime` otherwise, both read as TZif, including the POSIX rule in the
footer — `zic` has written files whose transition table stops a few years out
since 2020, so a reader that stopped at the table would have the wrong hour for
half of every year from about 2038. A zone that cannot be read falls back to UT
rather than refusing to start. No dependency was taken for any of it; the civil
arithmetic is fifteen lines of integer division and the rest is a file format.

The bar repaints when the minute turns over, which needs a trigger as well as a
source: nothing in a pane is damaged by time passing, so nothing would
otherwise ask for the frame. The machine poll already wakes the loop once a
second and its deadline is already folded into the wait, so the trigger is a
comparison of what the clock would draw against what it drew last — which
repaints once a minute for `%H:%M` and once a second for `%S` without the clock
having to be asked how precise it is. A bar that is hidden or dark keeps its
time current and asks for no frames for it.

Clicking a workspace on the bar switches to it, and clicking a pane on the
`panes` strip focuses it; the bar sits on the row `grid_area` takes away from
the layout, so until now a press there matched no pane and was dropped.
Clicking the message opens the notification history. `super+b`, or `ctrl+a`
then `b`, shows and hides the whole bar — `--no-status-bar` decides what a
session starts as, which is the wrong granularity for a row of the display you
want back while reading a long file — and the panes are resized and told so
either way.

Notifications are a queue rather than a slot. The status bar shows one at a
time — three seconds each, or one second while others are waiting, so a burst
drains at a pace that can be read instead of one that has to be waited out —
and `super+m` opens the list of what has been raised, newest first, saying what
each one said, which pane said it and how long ago. Choosing one goes to the
pane that raised it, wherever that pane has since ended up; the first row of
the list clears it. Applications raise them with `OSC 9;body` or
`OSC 777;notify;title;body`, and a bell becomes one too, because a display
server with no audio stack has nothing to ring. The compositor's own messages —
copied, no room to split, a split that failed and what the kernel said about it
— queue and are kept the same way, and the leader indicator is not one of them:
it is drawn from the keymap, so arming the leader no longer wipes whatever was
on the bar.

The body is whatever was on the other end of a pipe, so it is read only as far
as it could possibly matter, stripped of control characters and of combining
marks that have no base to attach to in a cell grid, collapsed at every run of
whitespace into a single space, and cut to two hundred cells with an ellipsis.
What is still too long for the bar is clipped there rather than dropped, which
is what used to happen: a message that did not fit was not drawn at all, and
the messages that do not fit are the ones carrying an `io::Error`. With
`--no-status-bar` there is no bar to clip into, so the same line is drawn over
the top right of the panes — a failure has to look like something, and it used
to look like a dead key. Kitty's OSC 99, with its ids, urgency and dismissal
from the application, is not implemented: it belongs on top of this queue
rather than beside it.
Take the list away and the same box is a prompt: a title and one line to type
into. That is what `super+,`, or `ctrl+a` then `,`, uses to name a workspace —
the comma is where tmux renames a window. The line opens holding the name the
workspace has now, so correcting one is a few keys rather than retyping it;
enter takes the line and the status bar says it in the next frame, and escape
leaves the old name alone. Accepting an empty line is how the number is asked
for back: the workspace forgets it was ever named, so renumbering moves it
along with the rest again when a workspace before it closes. A name belongs to
the workspace rather than to the position, which is why a named workspace keeps
its name while its neighbours are renumbered around it.

A machine with a password boots to a **login screen**, and ending a session
comes back to it (#112). It is the lock screen with nothing behind it: the
same masked field, the same wait after a wrong password, and a title that says
which of the two it is and whose password it wants. Nothing is started until
it is answered — the session is built when the password verifies, not before —
and what comes back after a log out is a new session with a fresh layout and
an empty clipboard, not the last person's. A machine with **no** password is
not gated, because a login screen with nothing to check against is a brick:
that is the live image, whose only account is Debian's root with `*`, and an
installed machine whose owner declined a password. `exit` in the last pane and
the `quit` binding are the same thing, and what they mean depends on that one
rule: log out where there is a login to come back to, and hand the machine
back to its init where there is not. `docs/design/login.md` has the rest.

That screen has a **picture** on it (#132). It is the one thing the compositor
draws in pixels rather than cells: the PNG decoder and the alpha blit the
graphics protocol already needed, pointed at a picture of its own instead of
at a program's. It is compiled into the compositor, because the first screen
on the display should not depend on a file having been installed, and a
machine that wants its own puts it at `/etc/tos/splash.png` — the door
`/etc/tos/motd_art` opens for the banner. Nothing there, or something that is
not a picture, is the one tOS ships rather than an error.

It is drawn at whole multiples of its own pixels, never at a fraction of one:
nearest sampling at a fractional ratio is what makes scaled pixel art look
melted, so a display gets `n` screen pixels per picture pixel, or one picture
pixel in `n`, or no picture at all. A screen too small for the smallest of
those keeps the box, in the place the box has always been — the picture is the
part that can give way, the same order the installer's banner already follows.
A lock does not get it: that screen has a session behind it and somebody in
front of it who knows what the machine is.

The **same picture is at the head of every pane**, where the banner has always
been. A shell asks the terminal what it is before it greets anybody: a tOS pane
is sent `/etc/tos/splash.png` over the graphics protocol and everything else —
a serial console, the kernel VT the rescue session lands on, somebody logged in
from another machine — goes on getting the banner drawn in cells. What decides
is whether `TOS` is in the environment and whether the terminal filled in the
pixel fields of its `winsize`, which tOS does for every pane it spawns and the
kernel's VT does not; querying the terminal and waiting for a reply is the
answer `tos-preview` already turned down, and paying for it at the top of every
shell would be worse. The picture goes as a path (`t=f`) rather than as a
payload, so the escape is a hundred bytes however large the picture is and
nothing travels through the pseudoterminal. `docs/design/splash.md` has the
rest, and `cargo run --example login_screenshot -- /tmp/tos-login.ppm` is how
the login screen gets looked at.

`super+shift+l`, or `ctrl+a` then `L`, locks the screen. The lock is a password
field, and it is its own type rather than another use of that box for exactly
that reason: the box echoes what is typed, refilters a list on every keystroke
and closes on escape, and a password field is the opposite of all three. This
one masks, it always submits, and escape clears the line because there is
nothing to close.

The password is checked against `/etc/shadow`: the machine's own file, the
account's `$6$` crypt line, hashed and verified by `tos-crypt` in this tree
rather than by `crypt(3)`, which the workspace cannot link. It was tOS's own
`/etc/tos/shadow` until #111, and moving it is what makes the password the
installer asks for a password `sshd`, `su` and `login` can use as well —
`docs/design/credentials.md`. Which account is asked for is whose session it
is: `TOS_USER`, set by `iso/live-session` — which every tOS session runs —
and overridden on an installed machine by the `tos-session.service` drop-in
the installer writes. The line is read when the lock engages rather than when a
password is offered, so a machine with no password does not lock — the binding
says there is nothing to unlock with and the session carries on. An account
with no password is `*` in that file, which is what the live image's root
carries and what an installed machine gets when its owner declines one, so
that one rule still makes the live ISO behave without the compositor ever
being told what live media is.

What the lock owns is the input, not merely the keyboard. The gate is at the top
of `handle_input` and not in `handle_key`, because mouse, pointer and paste
events never pass through `handle_key`: a lock one level further in would still
let a middle click paste the primary selection into a shell and a drag select
what is on the screen. What it draws is a frame with the panes, the dividers,
the status bar and any open menu *skipped* rather than painted over — a frame
that is not a full redraw only repaints the cells a pane marked as damaged, so
a box drawn on top of a session leaves the rest of that session exactly where it
was. The screen is cleared on every locked frame rather than the first, because
a display with two buffers hands out the other one next time.

Underneath it, everything goes on running. A pane's program is not told the
screen is locked; its output arrives, its terminal takes it, and none of it is
drawn until the password is accepted, at which point the whole screen is
repainted rather than the damage replayed. Notifications queue and none is
shown, so neither a bell nor a build finishing can put anything on a locked
screen, and nothing is lost by it: the queue stands still and says its piece
when the session comes back. A pane whose program exits cannot end the session
either, because exiting is a way out of a locked screen — the last pane dying is
remembered and acted on once somebody has said who they are. A wrong password
clears the field and waits a second, then two, four and eight; the wait is on
checking a guess rather than on typing one, since checking is what a guess
costs.

The screen also locks and goes dark on its own. Two deadlines run from the last
piece of input — `lock-after`, five minutes by default, and `blank-after`, ten —
and they are two deadlines over one state machine rather than one deadline with
two effects: a dark screen has not necessarily been locked, and a locked screen
goes dark later for the same reason an unlocked one does. Locking comes first on
purpose. Blanking first would leave five minutes in which a tap on the keyboard
shows the session to whoever is standing there; locking first means the screen a
passer-by wakes is the password prompt. When both come due on the same pass the
lock still goes up before the blank, because coming back from a blank shows the
last frame that was drawn, and that frame must never be the session of somebody
who is not there.

What counts as being there is input, and only input. A pane producing output is
not a person: a `tail -f` on a log that turns over all night would otherwise
hold the screen on and the lock off for as long as the machine kept running, and
an idle timer that any program can hold open is not one. Every kind of input
counts — a key, the mouse, a paste, the host terminal saying its window has been
switched to — because every kind of it is somebody doing something.

The event that wakes a dark screen is taken and given to nobody. Whoever sent it
could not see what they were aiming at, and a key let through would go to
whatever program has the focus, where `q`, `space` and `enter` each mean
something. It does not reach the lock either, so a password typed blind arrives
missing its first character — the field shows how much it is holding, and that
is the cheaper of the two mistakes. An open menu is left exactly as it was,
under the lock rather than closed by it: the person who gets it back is the
person who left it there.

A machine with no password goes dark and stays unlocked. The deadline reads the
credential the way the binding does, finds nothing and leaves the session alone;
unlike the binding it says nothing about it, because nobody asked and a live
session would otherwise find "cannot lock" waiting on the status bar every time
its user walked away from it. That is "no credential, no lock" arriving by the
other road, and it is exactly the failure the design refuses `VT_LOCKSWITCH`
for: a machine that blanks and then locks with nothing able to open it.

Both deadlines are folded into the poll timeout, beside the animation frame that
was already folded in there. A session a minute from locking waits the minute
out rather than waking ten times a second to find out that it is not a minute
yet, and once the screen is dark the wait stretches further: there is no blink
phase to flip behind a blank, no notification spending its time on a screen
nobody can see and no animation frame anyone would watch, so the only reason
left to come back is the next deadline. Nothing underneath stops. The panes go
on running and the compositor goes on painting them into a buffer the display is
not scanning out, which is why the screen comes back showing the session as it
is now rather than as it was when it went dark.

Only the DRM backend really goes dark, where blanking is disabling the CRTC and
the panel loses its signal. Nested and headless have no panel to put to sleep,
so they are given no blank deadline rather than a pretended one — a session that
believed it was dark when it was not would swallow the keystroke that woke a
screen its user could see all along. The lock deadline is untouched there:
locking means the same thing everywhere. A display that refuses to blank is
reported and then left alone; a screen that will not go out is not a reason to
end somebody's session.

One part of the design is not here yet, and the lock claims nothing it has not
got: refusing a VT switch with `VT_RELDISP 0`
([#47](https://github.com/m96-chan/tOS/issues/47)). Until that, the lock defends
the session rather than the machine — which on the nested and headless backends
is all there was ever going to be to defend.

### 0.0.6 — Japanese input

- [x] romaji to kana
- [x] an SKK dictionary, read and looked up
- [x] the dictionary on the ISO
- [x] a preedit drawn at the cursor
- [x] a candidate window
- [x] okurigana
- [x] learning
- [ ] the keys a JIS keyboard has left ([#62](https://github.com/m96-chan/tOS/issues/62))

Romaji becomes kana through one `const` table of a few hundred entries, binary
searched, and the lone `n` everybody states as a special case is not one here:
it falls out of the rule that an entry which is also the prefix of a longer
entry cannot commit until the next key arrives. Katakana is not a second
table — hiragana and katakana are the same list of kana 0x60 apart, so the
table is read differently rather than written twice, and halfwidth katakana is
a third reading of it.

Conversion goes through an SKK dictionary. Debian's `SKK-JISYO.L` is EUC-JP,
so it is converted at build time and the ISO ships one; it is read once into a
list of line offsets and binary searched there rather than held in memory as a
map. Okurigana is found by searching for the boundary instead of asking the
typist to mark it: on 52 everyday inflected words, 43 convert right on the
first candidate and 50 are in the first three. What was chosen is remembered
in an append-only journal replayed forwards, so the last choice wins and
nothing already written has to be rewritten.

The preedit is drawn at the cursor of the pane it belongs to and erased by
marking the rows it used rather than by redrawing the screen. The candidate
window is deliberately not an overlay, for reasons that accumulated as it was
written — among them that typing during a conversion extends the preedit,
which is the opposite of what typing into an overlay does. Turning the IME on
is `super+i` and not 半角/全角: what that key sends through the kernel's event
stream has not been established yet, which is the one item above still open.
`[ime] dictionary` under [Configuration](#configuration) says where the
dictionary is looked for.

### 0.0.7 — Kitty-like

- [x] window and tab bindings that follow Kitty
- [x] layout arrangements
- [x] a pointer that can be seen
- [x] menus that answer a click
- [x] dividers that can be dragged

`ctrl+shift` is a third binding table, bound directly rather than mirrored onto
the leader and super — those two go on mirroring each other, because that is
what a nested session needs. A binding matches on the whole modifier set, so
`ctrl+shift+l` and `super+shift+l` cannot collide, which is what lets the
layout key and the lock key share a letter. The keys are listed under
[Building and running](#building-and-running), including the two left unbound
on purpose: `ctrl+shift+c` and `ctrl+shift+v` stay with the programs in the
panes.

The mouse is the other half. There is a pointer on screen, the overlays answer
a click, and the gap between two panes can be taken hold of and dragged. A
grab is one thing at a time and is dropped by everything ordinary that
interrupts it — a lock, a menu, a workspace change, the pane underneath
closing — which is most of what the milestone's review was about.

Four defects found after the merge were filed rather than fixed, and are
carried into 0.0.8 below: nested mode reads a host terminal's cell numbers as
compositor cells, so most clicks in a nested session land nowhere
([#88](https://github.com/m96-chan/tOS/issues/88)); clicking a pane on the
status bar drops the zoom without resizing it
([#87](https://github.com/m96-chan/tOS/issues/87)); an expired notification
banner is never erased on a bar with no message segment
([#86](https://github.com/m96-chan/tOS/issues/86)); and a refused
`MovePaneToWorkspace` orphans the pane
([#85](https://github.com/m96-chan/tOS/issues/85)).

### 0.0.8 — More useful

- [ ] the four defects 0.0.7 left open ([#85](https://github.com/m96-chan/tOS/issues/85), [#86](https://github.com/m96-chan/tOS/issues/86), [#87](https://github.com/m96-chan/tOS/issues/87), [#88](https://github.com/m96-chan/tOS/issues/88))
- [ ] networking against a real interface ([#84](https://github.com/m96-chan/tOS/issues/84))
- [ ] bash as the shell an installed machine gives you ([#82](https://github.com/m96-chan/tOS/issues/82))
- [x] a way to add anything to an installed machine ([#83](https://github.com/m96-chan/tOS/issues/83))
- [ ] generic arm64 image ([#21](https://github.com/m96-chan/tOS/issues/21))
- [x] Debian rootfs tooling ([#20](https://github.com/m96-chan/tOS/issues/20))
- [ ] hardware abstraction cleanup ([#22](https://github.com/m96-chan/tOS/issues/22))
- [ ] images scanned out on DRM overlay planes ([#31](https://github.com/m96-chan/tOS/issues/31))
- [x] a picture on the login screen and at the head of every pane ([#132](https://github.com/m96-chan/tOS/issues/132))

The name is the test the round is held to: most of this list is about a
machine somebody installed being one they can actually use — a shell they
know, a way to add anything to it, a network that has run against real
hardware — and the defects above are on it because a mouse that clicks
nowhere is in the way of the same thing.

This is where the work is tracked now, including everything 0.1 needs: the
portability items below were moved here rather than waited for, because a
version before 0.1 lands when its idea has been demonstrated and there is no
reason to hold a round open for the name of the release it is aimed at.

### 0.1 — Portable tOS

- [x] generic x86_64 image
- [ ] generic arm64 image
- [x] Debian rootfs tooling
- [x] install / boot tooling
- [ ] hardware abstraction cleanup

`iso/` builds a bootable x86_64 image, and `tos-install` puts it on a disk
from inside a pane. What lands on the disk is a Debian bookworm rootfs with a
working `apt`, unpacked from a squashfs on the medium. The unticked boxes here
are the ones tracked under 0.0.8 above; this section is the release they add
up to, not a second pile of work.

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
| Configuration file | unit tests over the parser and the search order, plus a compositor built from a configuration and read back off the framebuffer |
| DRM/KMS, evdev, VT ownership | compile for x86_64 and arm64 Linux; ioctl numbers and structure layouts are unit-tested against the kernel headers |
| Installer | the whole sequence against a recorded backend, plus the real binary driven on a pseudoterminal with its output read back through tOS's own terminal emulator |

Panes refuse to split once they are too small to divide, rather than creating
a pane with nowhere to go, and a virtual terminal is only taken over once the
VT switch signals have handlers, so switching away with Ctrl+Alt+F2 cannot
leave the console in graphics mode with no keyboard.

Panes sit where the splits that made them put them, and a workspace can also
be read through one of three named arrangements: `tall` gives one full-height
pane the left and stacks the rest beside it, `fat` gives one full-width pane
the top and puts the rest side by side underneath, and `grid` makes as square
a grid as the pane count allows. `ctrl+shift+l` walks them and `ctrl+shift+b`
walks back. None of the three touches the split tree — each works out where
the panes go from the order they are in — so leaving `splits` and returning to
it gives back every manual split and every dragged divider exactly as it was,
where Kitty discards them the moment the layout is cycled past. Kitty's
`stack` is missing on purpose: zooming a pane with `super+z` already shows the
focused pane alone and full screen, and one behaviour does not need two keys.
While a derived arrangement is up there is no divider on screen to drag, so
the resize and balance keys refuse and say why rather than moving something
nobody can see. The status bar names the arrangement in force, and says
nothing at all while it is the tree, which is where every session starts.

The last row is the honest gap: the kernel-facing backends have not yet been
run on hardware. Everything above them has, through the nested and headless
backends.

## Repository layout

```text
tOS/
├── installer/           tos-install: put tOS on a disk from the live session
├── preview/             tos-preview: show an image in a pane
├── iso/                 bootable image and its initramfs
└── compositor/
    ├── tos-term/        terminal model: cells, grid, VT parser, graphics
    ├── tos-crypt/       SHA-512 and the $6$ crypt scheme, for passwords
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

`tos --help` lists the options and the default key bindings.

Panes and workspaces answer to the combinations Kitty uses, because a pane is
Kitty's window and a workspace is its tab, and tOS agrees with Kitty on every
other protocol it speaks. These need nothing pressed first:

| Key | Does |
| --- | --- |
| `ctrl+shift+enter` | split the focused pane |
| `ctrl+shift+w` | close it |
| `ctrl+shift+]` / `ctrl+shift+[` | focus the next or the previous pane |
| `ctrl+shift+t` | open a workspace |
| `ctrl+shift+right` / `ctrl+shift+left` | the next or the previous workspace |
| `ctrl+shift+1` … `ctrl+shift+9` | a workspace by number |
| `ctrl+shift+alt+t` | name the workspace |
| `ctrl+shift+up` / `ctrl+shift+down` | scroll a line |
| `ctrl+shift+page_up` / `ctrl+shift+page_down` | scroll a page |
| `ctrl+shift+end` | jump back to the live screen |

Kitty's `ctrl+shift+c` and `ctrl+shift+v` are deliberately left alone. tOS
sends the Kitty keyboard protocol *into* its panes, so a program running in one
can legitimately be handed `ctrl+shift+c`; claiming it at the compositor would
take the combination away from every program in tOS at once. `ctrl+shift+q`
closes a tab in Kitty and tOS has no close-workspace action to give it — only
`Quit`, which leaves the compositor entirely — so it is left alone too.

Everything else is on a leader key, `ctrl+a`, pressed and released before the
key it prefixes. On hardware the same table works directly with `super`, which
only a compositor that owns the keyboard can claim; the leader is what a nested
development session has instead. So `leader x` and `super+x` close a pane,
`leader h j k l` and the arrows move focus the way vim does, `super+space`
opens the launcher, `super+m` opens the notifications, `super+,` names the
workspace and `super+shift+l` locks the screen on a machine that has a password
to unlock with.

`super+[` takes the keyboard into copy mode, where vi's motions — `h j k l`,
`w b e`, `0 $`, `g G` and a screenful on `ctrl+f` and `ctrl+b` — move a copy
cursor through the pane and its history, `v` fixes one end of the selection and
`y` copies it and leaves; the arrow, home, end and page keys do the same for
anyone who does not think in vi. Copy and paste are `leader y` and `leader ]`.

From inside a session, `leader ?` puts the binding list over the panes; both it
and `--help` are generated from the keymap that is running, so neither can fall
behind it.

The mouse has a pointer to move. It is a small arrow drawn in software into the
composited frame, after everything else and over everything else, outlined so
that it stays findable against any background a pane or a menu can put behind
it — a cell inversion needs no geometry but is several characters wide and says
nothing about where the tip is. It appears the first time a pointing device is
heard from and never before, so a machine that has no mouse is never given one
to look for, and it goes away while somebody is typing and comes back on the
next motion. Moving it repaints the rows of the pane it was over rather than the
screen, the same way the Japanese preedit does, so a hand resting on a mouse
costs a few rows of one pane per report instead of a frame of the panel.

## Configuration

An installed machine starts the compositor from `/init`, so anything that can
only be said on the command line is fixed until the image is rebuilt. tOS
therefore reads a file, and looks for it in this order, stopping at the first
one that exists:

```text
$XDG_CONFIG_HOME/tos/tos.conf   or ~/.config/tos/tos.conf
$XDG_CONFIG_DIRS/tos/tos.conf   or /etc/xdg/tos/tos.conf
/etc/tos/tos.conf
```

The last of those is not XDG. It is there because `/init` has no home
directory and often no environment at all, and a machine that boots straight
into tOS still has to be configurable. `--config <path>` reads one named file
instead of searching, and `--no-config` skips the file entirely.

The format is `key = value` lines under `[section]` headers, with `#` starting
a comment on a line of its own. It is hand-parsed, like the command line, the
PNG decoder and the DEFLATE decoder before it: the compositor is what an
installed machine runs as PID 1, and a dependency in that path should earn its
place. A comment has to be a whole line because values begin with `#` all the
time — every colour does.

```ini
# General settings. The [general] heading is optional; this is the top of the
# file, which is the same place.
backend = auto
font = /usr/share/fonts/TTF/DejaVuSansMono.ttf
font-size = 16
# Used for the built-in face, when no font file is given.
bitmap-scale = 2
scrollback = 10000
# The program each pane runs; -e on the command line overrides it.
shell = /bin/sh -l
status-bar = true
# How far unfocused panes are dimmed, 0 to 255.
inactive-fade = 40
# The headless size, and the fallback when a backend cannot report one.
size = 1280x720

# What the session does when nobody touches it, in seconds. The screen locks
# first and goes dark afterwards; 0, or never, switches a deadline off. A
# machine with no password never locks, whatever this says.
[idle]
lock-after = 300
blank-after = 600

# Japanese input. The dictionary it converts through; kana still type without
# one and conversion simply finds nothing. Left out, it is searched for at
# $XDG_DATA_HOME/tos/SKK-JISYO and then /usr/share/tos/SKK-JISYO.L. Turning it
# on is a binding, super+i, and not the 半角/全角 key: on a JIS keyboard that
# key arrives as KEY_GRAVE and is indistinguishable from a backtick.
[ime]
dictionary = /usr/share/skk/SKK-JISYO.L

# What applications paint with. color0 to color255 set the palette itself.
[colors]
background = #101012
foreground = #d0d0d0
cursor = #87b7ff
cursor-text = #101012
color0 = #1c1c1c
color1 = #cc5757

# What the compositor paints its own dividers, status bar and menus with.
[chrome]
background = #18181c
foreground = #c8c8d0
dim = #70707c
accent = #5f87d7
accent-text = #101014
divider = #2c2c34
divider-focused = #5f87d7
# The block behind selected text. Follows accent unless it is set here, which
# is worth setting when the palette's own blue is close to the accent.
selection = #5f87d7

# What the status bar says, in the order it reads on screen, and what it says
# it in. Segments: workspaces, panes, title, layout, message, clock, battery,
# network, volume, bluetooth. A segment this machine cannot answer draws
# nothing, and layout says nothing while the panes are where the splits left
# them.
[status]
left = workspaces layout title
right = message network battery clock
# A strftime subset. %a %d %b %H:%M is the one with a date on it.
clock = %H:%M
# local, utc, or a zone name. local is $TZ, else /etc/localtime.
timezone = local
# The bar's own colours. Each falls back to the [chrome] colour above it, so
# setting accent once still moves the bar's highlight with everything else.
background = #18181c
foreground = #70707c
active = #5f87d7
active-text = #101014
divider = #2c2c34
```

Colours are hex, with or without the `#`, in the three digit shorthand or the
six digit form. Booleans take `true`, `yes`, `on` and `1` or their opposites.

A flag always wins over the file, so `tos --scrollback 0` means what it says
whatever the file asked for. A line the file gets wrong is reported by name and
line number and then skipped — the rest of the file still applies, and tOS
still boots into a usable terminal, the same way it degrades when a display
backend is unavailable rather than refusing to start.

Key bindings and the font fallback list each want a section of their own, and
will get one as the code behind them lands, the way `[status]` did. Until then
a key the compositor does not know is reported rather than silently ignored,
because a setting that quietly does nothing is indistinguishable from one that
is broken. A `left` or `right` naming a segment that does not exist is reported
the same way, with the list of the ones that do, and the side it names keeps
what it had.

## Installing

`iso/build.sh` builds a bootable image. Booting it gives a live session whose
every shell prints the picture from `/etc/tos/splash.png` — or, on a terminal
that cannot be sent one, the banner from `.motd_art` — and the one line that
matters:

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
