# A browser in a pane

Design for [#147](https://github.com/m96-chan/tOS/issues/147).

The README has wanted this for a long time: HTML, CSS and JavaScript going
through a real engine and coming out in a tOS pane, instead of the web being
flattened into text. It draws the pipeline ending in a "tOS Presentation
Backend" with four outputs — terminal cells, glyph runs, image surfaces, input
events — and leaves open what builds it.

This document is what an afternoon of measuring says about that. The headline
is that the backend the README asks for is mostly already written, and the part
nobody had measured turns out to be the part that decides the design.

---

## The engine does not get a display server, and that is the easy half

tOS runs on DRM/KMS under PID 1. There is no X11 and no Wayland, and there
never will be — [Design Principle 1](../../README.md#1-no-x11-or-wayland-dependency).
A browser engine that insists on a display server is not a browser tOS can have.

Chromium does not insist, but it has to be told twice.

`--headless` alone is not enough. Ozone still selects the X11 platform and the
process dies:

```text
ERROR:ui/ozone/platform/x11/ozone_platform_x11.cc:257] Missing X server or $DISPLAY
```

**`--ozone-platform=headless` is the flag that matters.** With it, Chromium
renders with no display server of any kind, which was confirmed by rendering a
page containing Japanese text, a CSS gradient, a table and a script and looking
at the result rather than at an exit code.

The failure to know about is the quiet one. `--screenshot` *without*
`--ozone-platform=headless` does not print that error and does not exit: it
hangs, indefinitely, with no output. Anything that drives Chromium
unattended — a CI job, a test — has to put a `timeout` around it, because the
process will not do it for them.

Four flags come along for the ride, none of them interesting but all of them
required: `--no-sandbox` (the sandbox wants privileges a container and a root
session do not have; Chromium refuses to start as root without it, and refuses
even for `--version`), `--disable-dev-shm-usage` with `--shm-size=1g`
(`/dev/shm` defaults to 64 MB in a container and Chromium will fill it),
`--disable-gpu`, and `--remote-allow-origins=*` for a CDP client that is not
DevTools. Japanese pages need `fonts-noto-cjk` installed or every glyph is a
tofu box, which a screenshot shows and an exit code does not.

---

## The measurement that decides the architecture

The obvious design is the one the issue implies: Chromium renders, tOS
displays, and the pictures travel over the PTY as kitty graphics commands,
which tOS has spoken since #13 and #68. `Page.startScreencast` with
`format="png"` hands over a PNG per frame, and
`compositor/tos-term/src/png.rs` already decodes PNG, so no image codec has to
be written at all.

Against a headless Chromium at 640x360, on a page repainting every frame:

| | rate | per frame |
| --- | --- | --- |
| `Page.startScreencast` jpeg q=70 | 60.2 fps | 14.2 KB |
| `Page.startScreencast` png | 60.2 fps | 69.4 KB |
| `Page.captureScreenshot` png | ~15 fps | 40.4 KB |

Sixty is the cap, not the ceiling — Chromium was never the slow end. PNG costs
about five times what JPEG costs per frame, which is 4.14 MB/s against
0.88 MB/s, and PNG is the one tOS can already decode.

Then the same pictures were pushed through tOS itself —
`tos --backend headless --screenshot`, with the converter running in the
pane — and the first attempt rendered **nothing**. An empty pane, a status bar,
and no image.

The reason is not in the browser and not in the graphics code. It is
`Pane::pump` (`compositor/tos-compositor/src/pane.rs:177`), which reads the
PTY until a short read and then returns, and a PTY read returns at most what
the kernel's PTY buffer holds — a few kilobytes, whatever size buffer is handed
to it. `Compositor::pump_panes` calls it **once per pane per frame**. So a pane
absorbs roughly one PTY buffer per compositor frame, and no more.

Measured rather than assumed: one 95 KB frame, sent inline as `t=d` base64,
needed between 21 and 25 compositor frames to arrive — about **4 KB per
frame**. At a 60 Hz compositor that is ~240 KB/s.

**The PTY is the bottleneck, and it is an order of magnitude too narrow.** A
60 fps PNG stream wants 4.14 MB/s. Even JPEG at 0.88 MB/s — a codec tOS would
have to grow in order to use — is nearly four times what an inline transmission
can carry. Going faster is not a matter of encoding better. Inline
transmission cannot get there from here.

---

## So the pixels go beside the PTY, not through it

The protocol already has the answer, and tOS already implements it. `t=s` names
a POSIX shared memory object instead of carrying the picture: the compositor
opens it, reads it, and unlinks it, all through the `MediumReader` seam that
#68 put in for exactly this reason (`compositor/tos-term/src/medium.rs`,
implemented by `compositor/tos-compositor/src/imagefile.rs`).

What travels down the PTY becomes a command of about sixty bytes, whatever the
picture weighs:

```text
ESC _ G a=T,f=100,t=s,i=1,q=2,C=1 ; <base64 of the shm name> ESC \
```

At 60 fps that is ~3.6 KB/s against a budget of ~240 KB/s: under two percent,
instead of seventeen times over. The same converter run with `--shm` renders
through tOS, and the animated bar on the test page is visibly further along in
the same wall-clock time than the inline run managed — more frames arrived,
which is the whole claim.

**Decision: the browser transmits frames as `t=s`, in PNG, at `f=100`.** PNG
because tOS decodes it already and the byte cost stopped mattering the moment
the bytes left the PTY; `t=s` because nothing else fits through.

This is the conclusion worth carrying into every later decision: *the terminal
protocol is a control channel, and the pixels travel beside it.* A design that
forgets that will keep rediscovering the same 240 KB/s wall.

---

## Nothing in the compositor had to change

This was tested rather than argued. Every result above was produced against
`main` with no modification to any crate under `compositor/`. The pieces the
proof of concept leans on were all already there:

- `graphics.rs` — `f=100` PNG, `t=s` shared memory, `t=d` inline, and the
  `a=f` / `a=c` animation frames a differential update would later want
- `png.rs` — the decoder, from #13
- `medium.rs` and `imagefile.rs` — opening the shm object, and the policy about
  what the compositor will open on a program's say-so, from #68
- `tos-input/src/encode.rs` — `encode_mouse` (SGR) and `encode_kitty`, which is
  the input direction: a pane's mouse and key events are already encoded in
  forms that map onto CDP's `Input.dispatchMouseEvent` and
  `Input.dispatchKeyEvent`

**The browser is an ordinary program in a pane.** It is not a compositor
feature, it needs no new protocol, and it does not get privileges a pane does
not have. If a browser design starts requiring compositor changes, that is the
signal that something has gone wrong in it.

---

## Chromium or WPE

The issue proposed WebKit, and WPE WebKit is the WebKit built for exactly this
situation: no window system assumed, a pluggable backend, a third of Chromium's
install size. It is the better answer on paper, and the question nobody had
checked was whether its backend needs EGL or Wayland underneath.

It does. On bookworm, `libWPEBackend-fdo-1.0.so.1.9.4` links:

```text
libwayland-client.so.0
libwayland-egl.so.1
libwayland-server.so.0
```

Both halves of Wayland, because `wpebackend-fdo` runs a private Wayland
compositor inside the process to move buffers from the web process to the UI
process. `libWPEWebKit-1.1.so` itself pulls in EGL, GBM, DRM, X11 and XCB on
top. **`wpebackend-fdo` is not usable on a machine with no display stack, and
that is now measured rather than suspected.**

The interesting part is what does *not* link them. `libwpe-1.0.so.1.8.0` — the
backend interface itself — links neither Wayland nor EGL. WPE's architecture
puts a replaceable seam at precisely the layer that is the problem, which
Chromium does not have: a tOS backend for `libwpe`, handing out shm buffers
instead of Wayland ones, is a thing the design admits. Whether it is a thing
somebody wants to write is a different question, and it is not a question the
proof of concept has to answer.

**Decision: the proof of concept uses Chromium over CDP, and WPE stays open.**
Chromium works today with flags; WPE would work better eventually with a
backend somebody has to write. Doing the exploration with Chromium settles the
tOS-side questions — transport, rate, input mapping — and every one of those
answers is still true if the engine is later swapped underneath, because
`t=s` PNG frames do not care what drew them.

---

## Size, and why none of this goes on the ISO

Dependency closures on bookworm, `--no-install-recommends`, measured on
`debian:bookworm-slim`:

| | packages | installed |
| --- | --- | --- |
| `chromium` | 174 | — |
| `chromium-shell` | 115 | 547 MB |
| `libwpewebkit-1.1-0` | 131 | 387 MB |
| `fonts-noto-cjk`, on top of either | 4 | 88 MB |

These counts are higher than an earlier measurement against a fuller base, for
the ordinary reason that a smaller base needs more packages to reach the same
closure; the ratios are what matter and they hold. The current ISO is 241 MB.

`docs/design/applications.md` settled what is on every image: thirteen Debian
packages and one upstream binary, chosen because somebody needs them to get to
work on a machine that just booted. A browser is three times the image, and it
is not in that category.

**A browser is something the person installs.** The PoC being an ordinary
program in a pane is what makes that possible: `apt install chromium-shell` and
run it, on a machine that already speaks the protocol it needs.

---

## What the proof of concept is, and what it is not

`browser/` holds it: a Dockerfile pinning a headless Chromium, a CDP client in
the standard library, a benchmark, and the converter. `browser/README.md` says
how to run it.

It renders a page and puts it on the screen. It does **not** yet send input
back, and that is the honest boundary: `encode_mouse` and `encode_kitty` are
written and CDP's `Input.dispatch*` is waiting, but nothing has been wired
between them, so nothing here has clicked a link.

Actionable next pieces, in the order the measurements suggest:

1. **Input.** Read SGR mouse and Kitty keyboard from the pane's stdin, dispatch
   to CDP. This is the one that turns a picture into a browser, and it needs
   nothing new in the compositor.
2. **Resize.** A pane resize arrives as `SIGWINCH`; the response is
   `Emulation.setDeviceMetricsOverride` at the new cell-to-pixel size.
3. **Differential update.** `a=f` and `a=c` compose frames from pieces, and
   CDP's screencast metadata says what moved. Worth doing only after input,
   because at 3.6 KB/s the transport is no longer the thing that hurts.
4. **The shm object's lifetime.** The PoC writes one object per frame and lets
   tOS unlink it. Sixty allocations a second is not obviously right; a ring of
   reused objects is the alternative, and picking between them needs a
   measurement nobody has taken.

---

## Negative results, kept on purpose

This was an exploration, so the configurations that did not work are part of
the finding:

- **`--headless` without `--ozone-platform=headless`** — dies on a missing X
  server, or, with `--screenshot`, hangs silently and forever.
- **Inline `t=d` transmission at video rates** — ~240 KB/s through the PTY
  against 4.14 MB/s needed. Not slow; wrong by an order of magnitude.
- **One frame at a time through `Page.captureScreenshot`** — ~66 ms for a
  frame the screencast path delivers in 16.6 ms on the same machine and the
  same page. About 34 ms of that is fixed cost, paid even for `about:blank`,
  which is a round trip the screencast does not make. The streaming API is not
  a convenience over the screenshot one; it is four times faster.
- **`wpebackend-fdo` on a machine with no display stack** — links
  `libwayland-client`, `libwayland-server` and `libwayland-egl`. Ruled out.
- **A browser on the ISO** — three times the size of the image, against a
  decision already taken in `applications.md`.
