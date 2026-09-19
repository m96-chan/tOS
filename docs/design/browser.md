# A browser in a pane

**Issue #147.** Decided: the presentation backend the README imagined already
exists and is not a private API — it is the terminal protocols the compositor
already speaks into every pane, so a browser is an ordinary program in a pane
and the frame path needs no compositor change at all. The engine is Chromium's
headless shell, driven over the DevTools protocol, chosen over WPE WebKit for
the proof of concept because its no-display-server operation was proven here in
one run and WPE's was not. The engine is **not on the ISO** — it is twice the
image — so `tos-browser` ships and the engine is the person's. The one
compositor change the experiment needs is on the input side: SGR-Pixels mouse
reporting (mode 1016), because a click reported in 8×16 cells cannot hit a
link.

Everything measured below was measured on 2026-09-17 and 2026-09-18, on Debian
bookworm with `--no-install-recommends`, in a container with no X11 server, no
Wayland compositor and no `$DISPLAY`.

---

## What already existed, which is most of the answer

The README's box says the browser's output goes to a "tOS Presentation
Backend", and the shape of that drawing invites the assumption that somebody
has to write one: a library the engine links, a socket the compositor listens
on, a private surface API. That assumption is what made #147 look large.

There is no such thing to write, because it is already there and it is not an
API. It is the protocol tOS speaks into every pseudoterminal:

```text
pictures      Kitty graphics. `f=100` PNG, decoded by tos_term::png
              `t=s` a POSIX shared memory name, read out of /dev/shm by
              tos-compositor's ImageFiles, which reads it and unlinks it
              `a=f` animation frames, in every format a whole image takes,
              and `a=c` to compose one frame onto another
geometry      TIOCGWINSZ with the pixel fields filled: pane.rs::winsize_for
              multiplies the pane's cells by the cell size, so a program in a
              pane is told 640x384 as well as 80x24, and gets SIGWINCH when
              that changes
pointer       SGR mouse reporting, mode 1006, in cells
keyboard      the Kitty keyboard protocol, including key releases, event
              types and associated text
```

Every one of those already has a program on the far end of it that is not
tOS. `tos-preview` sends `f=100` because that is the path a file manager uses
(`apps/preview/src/lib.rs`), and yazi — which was never told what tOS is — asks the
terminal whether it can show a picture, is told yes, and previews a photograph
(`docs/design/applications.md`). The graphics file media were argued and built
for #68, and `a=f` taking compressed frames was #69.

**So the browser is a program in a pane.** It gets its size from the ioctl it
would get it from anywhere, it puts the page on the screen by writing escape
sequences to standard output, and the compositor does not know it is a
browser. That is the whole architecture, and the interesting consequence is
negative: the frame path requires zero changes to `tos-term`, `tos-render` or
the compositor. Nothing here is a special case for a browser, which is the
test of whether a protocol is a protocol.

The input path is where the gap is, and it is one mode wide.

### The gap: a click lands in a cell

`compositor.rs:991` takes a `PointerEvent` in display pixels and divides by the
cell size before anything else happens:

```rust
let x = pointer.x.max(0.0) as u32;
let y = pointer.y.max(0.0) as u32;
// ...
self.route_mouse(x / cw, y / ch, /* ... */)
```

From there the position is cells and only cells: `MouseEvent` has `col` and
`row` and no pixels, which `event.rs` says is deliberate — "a device knows
pixels and nothing about cells, while the encoders need cells". `encode_mouse`
formats those cells into `ESC [ < code ; col ; row M`.

On the 8×16 face that is 8 pixels of horizontal and 16 pixels of vertical
quantisation. A link in a sentence is smaller than that vertically and usually
not aligned to it horizontally, so a browser that only knows which cell was
clicked is a browser that clicks the wrong link, and the failure is not
occasional. Scrolling and dragging a selection have the same problem one step
down.

**Mode 1016, SGR-Pixels, is the fix**, and it is exactly the mode xterm and
kitty added for this. It reports the same SGR frame with pixel coordinates in
place of cell ones. It does not exist on `main`: `MouseEncoding` has `X10`,
`Utf8`, `Sgr` and `Urxvt` and no pixel variant, and `report_private_mode`
(`term.rs:1283`) has no `1016` arm. It is being added on `mouse-sgr-pixels`,
and what it needs is the pixel offset surviving the division at
`compositor.rs:991` rather than being thrown away there.

**The browser detects it with DECRQM and falls back.** `CSI ? 1016 $ p` is
already routed to `report_private_mode`, which answers `CSI ? 1016 ; s $ y`
with `s` of 1 for set, 2 for reset and — because of its `_ => 0` arm — **0 for
a mode it has never heard of**, which is what DECRQM defines 0 to mean. So the
probe already works today and already gives the right answer on a compositor
without the mode: `tos-browser` asks, gets `0`, and runs in cells. That is not
a degraded mode worth hiding — link-clicking on a cell grid is usable for a
page of prose and unusable for a dense one, and the browser should say which
it is in.

---

## The engine

### Chromium, headless, over CDP

`chromium-shell` is Debian's headless shell — Blink and V8 with no browser UI,
no X11 link at run time, and the DevTools protocol as its only interface. In
the container described above it rendered a page with Japanese text, CSS
gradients, a table and executed JavaScript to a PNG, with nothing resembling a
display server present. That is the proof #147 asked for, and it is the whole
reason Chromium is here rather than WPE.

The flags that work:

```text
--headless --no-sandbox --disable-gpu --disable-dev-shm-usage
--ozone-platform=headless --remote-debugging-port=9222 --remote-allow-origins=*
```

Two of those cost real time to find, and both are written down because the
next person will otherwise spend it again.

**`--headless` alone is not headless.** Ozone still selects the X11 platform
and the process dies with

```text
ERROR:ui/ozone/platform/x11/ozone_platform_x11.cc:257] Missing X server or $DISPLAY
```

`--ozone-platform=headless` is required and is the thing the flag's name
promises.

**Without it, `--screenshot` does not fail — it hangs.** Thirty-five minutes
observed, no output, no error, exit status never arrived. That is worse than
the crash above because it looks like slowness, and it is the reason for a
rule that belongs in the client rather than in a flag list: **every wait on
the engine carries a deadline.** A CDP call that has not answered, a screencast
that has stopped producing frames, and the initial handshake all get one, and
the timeout is reported to the person rather than retried silently.

`fonts-noto-cjk` is needed or every Japanese glyph is a box — the engine has
its own font stack and does not see the compositor's HackGen. In a container
`--shm-size=1g` is needed; the default 64 MB is not enough for Blink even with
`--disable-dev-shm-usage`.

### What it does, in numbers

At 640×360, which is roughly an 80×23 pane on the 8×16 face:

| | |
| --- | --- |
| page load | 12–14 ms |
| `Page.captureScreenshot`, png | 21–32 ms, 32.5 KB |
| `Page.startScreencast`, sustained | 60.2 fps |
| screencast frame, jpeg q70 | ≈ 14.5 KB |
| screencast frame, png | ≈ 57.8 KB |

The screencast number is the one that decides the architecture. 60.2 fps is
the cap, not the engine's limit — the test page repainted its whole area every
frame and the rate stayed pinned there, so the engine is not the bottleneck in
this experiment and something else will be.

**The frames go through as PNG, unchanged.** JPEG is 4× smaller per frame and
it is not used, because `tos-term` cannot decode it and writing a decoder to
get this is the trade `apps/preview/src/lib.rs` already refused for previews: a
baseline JPEG decoder is 600–900 lines before progressive JPEG is considered,
in a repository whose one dependency is `libc`, and nothing else in the tree
wants it. PNG is what the graphics protocol names as its own payload format
and what the compositor already decodes, so the client's frame path is a
base64 decode and a write — it does not touch a pixel.

### Why not WPE WebKit, honestly

WPE is the WebKit port built for exactly this: no display server, no toolkit,
render into a buffer somebody else owns. On paper it is the better final
engine for tOS and this document should not pretend otherwise:

- **It is 40% smaller** than the headless shell — 313 MB installed against
  482 MB.
- **It would hand over raw buffers with damage rectangles**, not encoded
  frames. That deletes the PNG encode on one side and the PNG decode on the
  other, and it turns a full-frame repaint into the rectangle that actually
  changed. Every cost in the section below is a cost WPE does not have.

It is not the PoC engine for one reason, and it is a reason about evidence
rather than about design: **its no-display-server claim is unverified here.**
The backend Debian bookworm ships is `wpebackend-fdo` 1.14, which runs a
private Wayland compositor *inside the process* and exports buffers through
EGL. Whether that comes up with no GPU device node and no display was not
tested. The newer WPE Platform headless backend, which is the part that would
make the claim straightforwardly true, is not in WebKitGTK/WPE 2.38.

So: one engine was proven in a single run, and one was reasoned about. The PoC
takes the proven one, and the experiment that would change this is named at the
bottom of this page rather than left as a feeling that WPE is probably nicer.

### The engine is not on the image

| | packages | installed | download |
| --- | --- | --- | --- |
| `chromium` | 112 | 607 MB | 167 MB |
| `chromium-shell` | 76 | 482 MB | 136 MB |
| `libwpewebkit-1.1-0` (WPE WebKit 2.38.6) | 86 | 313 MB | 75 MB |

The tOS ISO is 241 MB.

The smallest of those three is 1.3× the whole image and the one this document
chooses is 2×. That is not a close call, and it is not a new decision either:
`docs/design/applications.md` put thirteen Debian packages and one upstream
binary on every image and said everything else is the person's, and it left
the browser to this issue by name. A browser engine is the clearest case that
rule will ever get.

**So `tos-browser` ships and the engine does not.** The client is a small Rust
program — a CDP client over a local websocket, a PNG passthrough, an input
translator — and it belongs on the image for the same reason `tos-preview`
does: it is the program that proves the protocol from the other end. It finds
the engine by looking for `$TOS_BROWSER_ENGINE` and then for
`chromium-shell`, `chromium` and `chrome` on `$PATH`, and when it finds none
it says which names it looked for and what to `apt install`. A browser that is
absent until somebody installs 482 MB is honest; a 723 MB ISO is not.

---

## The architecture

```text
                    a page
                       │
        ┌──────────────▼───────────────────────────────┐
        │  chromium-shell --headless                   │
        │  --ozone-platform=headless                   │   the person's
        │  Blink, V8, layout, paint                    │   482 MB
        └──────────────┬───────────────────────────────┘
                       │ CDP over a websocket on 127.0.0.1
                       │ Page.screencastFrame — base64 PNG, ~57.8 KB
                       ▼
        ┌──────────────────────────────────────────────┐
        │  tos-browser                                 │
        │  an ordinary program in a pane. No compositor│   on the image
        │  API, no socket to the compositor, no plugin │
        └──────────────┬───────────────────────────────┘
                       │ write the PNG to /dev/shm/<name>
                       │ ESC_G a=T,f=100,t=s,q=2;<base64 name> ESC\
                       │ PTY
                       ▼
        ┌──────────────────────────────────────────────┐
        │  tos-term                                    │
        │  ImageFiles reads /dev/shm and unlinks it    │
        │  png.rs decodes to RGBA8, GraphicsStore holds│
        └──────────────┬───────────────────────────────┘
                       ▼
                   tos-render  ──►  DRM/KMS
```

and the other way, which is where the compositor changes:

```text
        evdev
          │ PointerEvent { x, y } in display pixels
          ▼
        tos-compositor — compositor.rs:991 divides by the cell size
          │ MouseEvent { col, row }        ← and, with 1016, the pixel offset
          ▼
        tos-input encode_mouse
          │ 1006:  ESC [ < 0 ; col ; row M      cells
          │ 1016:  ESC [ < 0 ; x   ; y   M      pixels
          │ PTY
          ▼
        tos-browser  subtracts the placement's origin
          │ CDP Input.dispatchMouseEvent
          ▼
        chromium-shell


        evdev ─► Keymap::resolve ─► Passthrough ─► encode_key
          │ the Kitty keyboard protocol: ESC [ 97 ; 1 : 1 ; 97 u
          │ PTY
          ▼
        tos-browser
          │ CDP Input.dispatchKeyEvent, and insertText for committed text
          ▼
        chromium-shell
```

Three details in that are decisions rather than drawing.

**`t=s` and not `t=d`.** A 57.8 KB frame base64s to 77 KB of escape sequence
sixty times a second, which is 4.6 MB/s of PTY. A shared memory name is about
thirty bytes. The compositor already reads `/dev/shm` for `t=s` and already
unlinks the object afterwards, which means the client writes a fresh name per
frame and never cleans up — the consuming read is the cleanup. That is the
protocol working as designed, and it is also the thing to measure first,
because it is an object created and destroyed sixty times a second.

**The page is a picture; the browser's own chrome is cells.** The URL line,
the back and forward indicators and any error message are drawn as text in the
pane, with terminal attributes, above or below the placement. They are the
part of a browser that *is* a terminal UI, they cost nothing, and they resize
by themselves. What is deliberately not attempted is the page's text as cells;
that is the next section.

**Resizing is SIGWINCH and nothing else.** The pane changes size, the
compositor calls `winsize_for` and `Pty::resize`, the client wakes on
SIGWINCH, reads the new pixel fields out of `TIOCGWINSZ`, and sends
`Emulation.setDeviceMetricsOverride` and a fresh `Page.startScreencast`
bounded to the new size. The engine then lays the page out at the pane's real
pixel dimensions rather than being scaled into them, which is what makes a
resize a reflow rather than a blur.

---

## What the proof of concept is

Included, and this is the checklist #147's "Done when" is measured against:

- `tos-browser https://example.org` in a pane shows the page, at the engine's
  frame rate, with a URL line above it.
- Scrolling with the wheel and with the keyboard.
- Clicking links: pixel-precise where the compositor has 1016, cell-precise
  where it does not, and the client says which one it got.
- Typing into a form, through the Kitty keyboard protocol into
  `Input.dispatchKeyEvent` and `Input.insertText`.
- Back, forward and reload.
- A resize reflows the page rather than scaling the last frame.
- The test page is the one from the measurement above — HTML, CSS with
  gradients, a table, JavaScript that changes the DOM, and Japanese text, so
  that a missing `fonts-noto-cjk` is visible rather than silent.

Excluded, with reasons, because the scope of a PoC is mostly what it refuses:

**The README's "terminal cells" and "glyph runs" for page text.** Blink and
WebKit paint pixels. Neither has a text output the way a terminal means text,
and neither is going to grow one. Getting a page out as cells means one of two
things: reading the accessibility tree, which gives the text and the structure
but not the layout, and would produce a document rather than the page; or
writing a paint backend that intercepts glyph runs before rasterisation, which
means building the engine rather than installing it and undoes the entire
argument above. Neither is in the PoC.

What would have to be true for it to be worth doing later: a real page's text
would have to land close enough to a cell grid that snapping it does not
destroy the layout, and the win over a picture would have to be something a
picture cannot give — selectable text, a screen reader, reflow at the terminal
line width, a page that is legible over ssh on a slow link. Those are real
motivations and they are not this issue's. It is a research item after the
pixel path works, and it should be measured on a page somebody actually reads
rather than argued about.

**Audio and video in the page.** `docs/design/audio.md` says tOS is ALSA-only
and starts no sound server; `docs/design/video.md` says the cell-and-surface
path carries 720p at 30fps and not 720p at 60. A browser playing video would be
that document's numbers with a PNG round trip added. Out.

**Tabs beyond a history list, downloads, bookmarks, cookies that survive an
exit, and any profile on disk.** The engine gets a temporary profile that is
removed when the client exits. Persistence is a question about where a tOS
machine keeps a person's data, and it is not this experiment's to answer.

---

## What it costs, and what to measure

Nothing here is a reason not to do it. They are the things that will be found
by somebody with a profiler if they are not written down now.

**3.5 MB/s of PNG through `/dev/shm`.** 60.2 fps × 57.8 KB. The engine
compresses it and the compositor decompresses it, sixty times a second, for
pixels that were raw on one side and are raw again on the other. It is the
price of the frame path being a protocol both ends already speak, and it is
the first number WPE would delete.

**One shared memory object created and unlinked per frame.** Sixty
`open`/`write`/`close` on the client's side and sixty `open`/`read`/`unlink`
on the compositor's, in the parse loop. `/dev/shm` is tmpfs so the bytes are a
memcpy, but the directory operations are not free and nothing in the tree has
asked this of them before.

**PNG decode on the CPU, in the compositor, per frame, on the parse loop.**
`docs/design/graphics-file-transmission.md` already lists reading off the parse
loop as a follow-up for latency reasons; a browser is the first program that
makes it a throughput question too. This is the first thing to measure once it
runs on real hardware, and it is the number most likely to say that 60 fps is
not what tOS actually gets.

**Seven engine processes behind one pane.** The browser process, two zygotes,
a GPU process that has no GPU, a network service, a storage service and one
renderer per page. That is what a pane's process tree looks like when the pane
is a browser, and it is worth knowing before somebody opens `btop` and files a
bug.

**Frame pacing is unmeasured end to end.** Everything above is the engine's
side. What has never been measured is the whole path — paint to
`screencastFrame` to shm to decode to blit to page flip — on a real machine
with a real display. The engine sustaining 60.2 fps into a socket says nothing
about what arrives on the screen.

---

## Open questions

1. **Does WPE come up with no display server and no GPU?** Everything about
   whether Chromium is the right *final* engine turns on this one run.
2. **Is `a=f` with damage rectangles better than a full frame?** A screencast
   frame is the whole viewport, but CDP reports the dirty region and the
   graphics protocol already takes a frame rectangle. Sending only what
   changed would cut both the PNG work and the blit, and whether it helps
   depends on how much of a real page repaints — a scroll changes everything,
   a cursor blink changes forty pixels.
3. **Does tOS's 1016 agree with kitty's?** The mode is implemented from the
   specification here; the client talking to it in six months may be somebody
   else's, and a difference of one in the origin is a bug nobody finds by
   reading.
4. **Where does the clipboard join?** The compositor has a clipboard and the
   page has one, and today nothing connects them. Copying a URL out of a page
   into a shell is the first thing anybody will try.
5. **What is a browser's process tree allowed to be?** Seven processes per
   pane is fine for one pane. It is a different conversation at four.

---

## Where the crate lives, and what would move it out

**Decided: `apps/browser`, in this tree, beside `apps/preview` and
`apps/installer`.** Not its own repository yet — and the conditions under
which it becomes one are written here so that the question is answered by
events rather than reopened by mood.

The case for a separate repository is real and mostly correct. A browser is a
program in a pane and not the compositor, which is the same line
`applications.md` drew for what goes on the image. What it tracks is
Chromium's DevTools protocol, on Chromium's calendar and not tOS's. Its
engine tests cannot run in this repository's CI, which has no Chromium and
must not borrow the runner's; a repository of its own could install
`chromium-shell` and run them on every push. And 1,739 lines of it — the
WebSocket client, the JSON reader, the HTTP GET, base64, SHA-1 — exist for no
reason except that this workspace depends on `libc` and nothing else. In a
repository with its own policy they are two lines of `Cargo.toml`.

What keeps it here for now is what it is coupled to, measured rather than
assumed. At runtime the crate uses two things from the tree:
`tos_platform::tty` for raw mode and non-blocking reads, and
`tos_preview::fit` for `TIOCGWINSZ` and the cell arithmetic — perhaps a
hundred and fifty lines between them, and copyable. The coupling that matters
is in the tests. `graphics.rs` feeds the escape sequences it emits to a real
`tos_term::Terminal` and asserts what the store holds; the integration test
installs the compositor's own `ImageFiles` so that `t=s` is read and unlinked
by the code that does it in a session; the 1016 test asks the real
`report_private_mode`. Those are the tests that fail when the browser and the
compositor disagree about the protocol between them, and they can exist only
because both ends are in one tree behind one gate — mode 1016 landed with its
first consumer in the same afternoon for that reason. Outside the tree they
would pin a commit of tOS and test the past. None of `tos-term`, `tos-input`,
`tos-platform` or `tos-preview` is published, so a separate repository today
would depend on this one by git revision, which is that pinning made
permanent. And before 0.1 the protocols change weekly; a two-repository dance
for each change is a cost with nothing yet on the other side of the ledger.

It moves out when the first of these happens:

- **The pane-program crates are published.** Any program written for tOS by
  somebody else needs what `tos_preview::fit` and the graphics encoder know,
  and the moment that is on crates.io the browser can depend on it the way a
  third party would. This is the real prerequisite, and it is a question about
  a tOS SDK rather than about the browser.
- **0.1 ships and the browser's releases stop lining up with the image's.** A
  crate that is not on the ISO has no reason to be versioned with it.
- **The browser wants a real dependency** — a JPEG decoder, a WebSocket
  library, `serde` — and the workspace's one-dependency rule is the only thing
  in the way. That is the workspace's policy to change or the browser's tree
  to leave, and either is a decision for the person, not a drift.

Until then `apps/` is the separation: three programs that are not the
compositor, in one place, under one gate.

## Follow-ups

Written as issue candidates, in the order they are worth doing.

**Run WPE headless in the same container and see whether a frame comes out.**
`cog` or a minimal WPE host, `wpebackend-fdo` 1.14, no display server, no
`/dev/dri`. If a buffer arrives, WPE is 40% smaller, has no PNG round trip and
gives damage rectangles, and this document's engine choice is provisional
until somebody has tried it. If it does not, that is worth knowing once and
citing thereafter. Labels: `experiment`, `area:browser`.

**Measure the compositor's PNG decode per frame on real hardware.** The
engine's 60.2 fps is one end of a path nobody has timed. Decode, store and
blit, on the machine tOS boots on, against a page that repaints fully — and
against one that does not. This is the number that says whether the PoC is
60 fps or 20. Labels: `experiment`, `area:graphics`.

**Send `a=f` frames bounded to the damaged rectangle.** Depends on the
measurement above being taken first, because "only send what changed" is worth
whatever the full-frame path costs and not a byte more. Frames already take
PNG and already take a rectangle, so this is client work over a protocol that
is finished. Labels: `enhancement`, `area:browser`.

**Check tOS's mode 1016 against kitty's, byte for byte.** Same page, same
click, both terminals, diff the reports — origin, the release final byte, what
a drag outside the window says. A mode implemented from a specification and
never compared against the implementation that defined it is a mode that is
subtly wrong for years. Labels: `experiment`, `area:input`.

**Clipboard between the page and the pane.** The compositor owns a clipboard
and CDP can read and write the page's; joining them is a small amount of code
and the difference between a demonstration and something somebody uses. Decide
first whether a page may *read* the tOS clipboard without being asked, which is
a question about a program in a pane and not about a browser. Labels:
`enhancement`, `area:browser`.

**Page text as cells, as a research item.** Named here so it is not lost, and
deliberately last. The accessibility tree or a custom paint backend; what it
would buy is selectable text, reflow at the terminal's width and a page that is
legible over a slow link; what it costs is either a document that is not the
page or building the engine instead of installing it. Do not open this until
the pixel path has been used for something real. Labels: `experiment`,
`area:browser`.
