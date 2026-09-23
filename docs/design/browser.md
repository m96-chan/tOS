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

**The frames went through as PNG, unchanged, and that is no longer true.**
It was the right call at 640×360 and the wrong one at a pane's real size; the
section [The format is the frame rate](#the-format-is-the-frame-rate) below
has the measurement that changed it, and the short version is that the
engine's PNG encoder is the bottleneck and swapping it for JPEG buys 24 frames
a second. The client now decodes every frame itself and hands the compositor
raw pixels.

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
                       │ Page.screencastFrame — base64 JPEG q85, ~185 KB
                       │   while the page moves; one Page.captureScreenshot
                       │   in PNG when it stops
                       ▼
        ┌──────────────────────────────────────────────┐
        │  tos-browser                                 │
        │  an ordinary program in a pane. No compositor│   on the image
        │  API, no socket to the compositor, no plugin │
        │  jpeg.rs / png.rs decode the frame here, 8 ms│
        └──────────────┬───────────────────────────────┘
                       │ write the RGB to /dev/shm/<name>, 2.9 MB
                       │ ESC_G a=T,f=24,s=W,v=H,t=s,q=2;<base64 name> ESC\
                       │ PTY
                       ▼
        ┌──────────────────────────────────────────────┐
        │  tos-term                                    │
        │  ImageFiles reads /dev/shm and unlinks it    │
        │  no decode: f=24 is pixels. GraphicsStore    │
        │  holds them                                  │
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

**`t=s` and not `t=d`.** A 2.9 MB frame of raw pixels base64s to 3.9 MB of
escape sequence, and at 58 frames a second that is 226 MB/s of PTY. A shared
memory name is about thirty bytes. The compositor already reads `/dev/shm` for
`t=s` and already unlinks the object afterwards, which means the client writes
a fresh name per frame and never cleans up — the consuming read is the
cleanup. That is the protocol working as designed, and it is also the thing to
measure first, because it is an object created and destroyed sixty times a
second.

The inline fallback still exists and still works, and it is now a slideshow
rather than a slower picture. It sends the same raw pixels, because the
alternatives are to send a JPEG the terminal cannot read or to write a PNG
*encoder* in `apps/browser` — a third codec from a specification, to make
faster a path that exists only for terminals which are not tOS. Correct and
slow was the right side of that.

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

Tabs came in anyway, and the reason is worth recording because it is not scope
creep. A link with `target=_blank` makes the engine open a page target whatever
the client does, and a target nothing attaches to is a click that does nothing
at all — the first thing anybody meets on a real site. So `tos-browser` keeps a
list of page targets, one WebSocket each, on the row it already owns, with a
screencast on the one in front and none on the rest; `apps/browser/src/tabs.rs`
has the model and `lib.rs` has why they are tabs rather than panes. Still out:
everything else in that paragraph.

---

## What it costs, and what to measure

Nothing here is a reason not to do it. They are the things that will be found
by somebody with a profiler if they are not written down now.

**170 MB/s of raw pixels through `/dev/shm`.** 58 fps × 2.9 MB. This used to
be 3.5 MB/s of PNG, and the trade was made knowingly: tmpfs is memory, so a
write and a read are a memcpy at memory speed and cost about a third of a
millisecond each, while the PNG decode they replaced cost fifteen to twenty on
the compositor's parse loop. What is left is one large copy in each direction,
which is the thing WPE would delete by handing over a buffer instead.

**One shared memory object created and unlinked per frame.** Sixty
`open`/`write`/`close` on the client's side and sixty `open`/`read`/`unlink`
on the compositor's, in the parse loop. `/dev/shm` is tmpfs so the bytes are a
memcpy, but the directory operations are not free and nothing in the tree has
asked this of them before.

**~~PNG decode on the CPU, in the compositor, per frame, on the parse
loop.~~** Gone, and this is how. The decode is now in the pane's own process,
where it is 8 ms for a 1280×770 JPEG frame and costs the compositor nothing;
`f=24` is pixels and the terminal copies them. What remains on the parse loop
is the read out of `/dev/shm` and the copy into the store.
`docs/design/graphics-file-transmission.md` still lists reading off the parse
loop as a follow-up, and it is still worth doing — it is a 2.9 MB read now
rather than a 320 KB one — but it is no longer the number that decides the
frame rate.

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

## The format is the frame rate

Everything above was measured at 640×360. At the size a pane actually is —
1280×770, two vCPUs, a scrolling ja.wikipedia page, `chromium-shell` 153 — the
engine does not sustain 60 frames a second, and what stops it is not the
network, the PTY or the terminal. It is the engine's own single-threaded
encode of each frame:

| format | fps | KB/frame | frame gap p50 / p95 / max |
| --- | --- | --- | --- |
| png | 33.8 | 320 | 28 / 37 / 39 ms |
| jpeg q70 | 60.0 | 139 | 17 / 18 / 20 ms |
| **jpeg q85** | **57.8** | **185** | ≈ 17 ms |
| jpeg q95 | 40.0 | 268 | |
| jpeg q100 | 27.8 | 387 | |

**The rate does not change from two vCPUs to eight**, which is what says it is
one thread's encode rather than contention. `Page.captureScreenshot` in a loop
is 10–12 fps in every format CDP offers — `png`, `png` with
`optimizeForSpeed`, and lossless `webp` — so there is no fast lossless path to
prefer: it is JPEG or it is half the frame rate.

**Chromium's screencast JPEG is 4:2:0 at every quality.** The encoder
hard-codes a sampling factor of `2x2,1x1,1x1`, so chroma is half resolution in
both axes whatever quality is asked for and coloured text smears a little at
100 as well as at 70. Quality only decides how much of the luma underneath it
survives. At q70 the halo around blue link text is visible at 1:1. At q85 the
difference from the PNG of the same frame needs 3× zoom to find. **The person
looked at the comparison and chose 85.**

### What reproduced when the branch was built, and what did not

The table above is the measurement that decided this, and it is kept because
it is the one that was argued from. What a later run on different hardware
found is also kept, because the two disagree about the headline number and
pretending otherwise would leave the next person confused.

On 2026-09-23, in the same container with `--cpus=2`, at 1280×768, against a
page of prose with a picture in it, scrolling every frame:

| | frames a second | KB/frame | in the terminal, per frame |
| --- | --- | --- | --- |
| png, decoded by `tos-term` | 59.7–60.0 | 267 | 6.2–7.2 ms |
| jpeg q85, decoded in the pane | 59.7–59.8 | 204 | 0.89–0.91 ms |

The JPEG's 6.7 ms of decoding is not in that last column because it is not in
the compositor: it happens in the pane's own process, where it competes with
the engine for the same two vCPUs and still leaves the frame rate where it
was.

**The engine's PNG encoder kept up on that host.** The 33.8 fps in the table
is a slower machine against a real ja.wikipedia page — more glyphs, more
colour, a heavier encode — and it is a real number about a real machine tOS
will run on. It is simply not a number every machine produces, which means
*the format is the frame rate only where the encoder is the bottleneck*, and
whether it is depends on the page and the CPU.

What is a property of this code rather than of the host is the last column,
and it is the same several-fold difference everywhere: **the compositor's
per-frame cost falls seven- or eightfold**, because a PNG decode on the parse
loop became a copy out of tmpfs. That is what
`apps/browser/tests/engine.rs` asserts; the frame rates it prints and does not
assert, for exactly this reason.

So the decision stands on two legs rather than one. On a machine where the
encoder binds, JPEG is 24 frames a second. On every machine, the decode leaves
the thread that has to keep every other pane on the screen as well.

### JPEG while it moves, PNG when it stops

So the policy, which is what VNC and RDP do and for the same reason:

- The screencast runs at `format=jpeg, quality=85`.
- When no screencast frame has arrived for **250 ms** *and* no wheel notch or
  key has arrived for **400 ms**, the tab in front is asked for one
  `Page.captureScreenshot` in PNG, without waiting for it.
- The reply is painted only if at most **one** screencast frame arrived while
  it was being drawn — the one the screenshot takes of itself, below. Two or
  more and it is thrown away and the tab stays in motion.
- A painted still is credited with the moment its **reply** arrived, and a
  frame older than what is on screen is dropped and is not counted as motion.
- The next screencast frame that is newer than what is on screen resumes
  motion.

Text that somebody is reading is therefore always lossless. The lossy frames
are only ever the ones scrolling past, which nobody reads. A page that never
moves costs one still and then nothing at all — no frames, no polling, no
repainting.

#### What a slow engine did to the first version of this

The first version had one interval (150 ms of frame quiet), credited the still
with the moment it was asked for, and took it with a blocking call. On the
host the format was chosen on that was invisible. On an installed tOS in
VirtualBox — 2 vCPUs, no GPU, a 1280×770 pane — scrolling flashed. Measured
there, through an ssh tunnel, against the VM's own engine:

| | on the VM |
| --- | --- |
| `Page.captureScreenshot`, png, pane size | 66–98 ms |
| `Page.captureScreenshot`, jpeg, pane size | 42–51 ms |
| jpeg q85 screencast while scrolling | ≈ 42 fps, 24–27 ms gaps |

`metadata.timestamp` was checked against `SystemTime` on that machine and the
two are the same clock, so none of what follows is a clock bug.

Three separate things were wrong, and all three are the same number being too
small:

1. **A still after every notch.** A wheel notch makes the engine animate for
   about 100 ms and then stop; a hand on a wheel produces notches 150–300 ms
   apart. 150 ms of frame quiet therefore fits in the gap *between two
   notches*, so every notch ended in a PNG. The screen went JPEG frames, PNG,
   JPEG frames, PNG, several times a second, and because Chromium's screencast
   JPEG is 4:2:0 at every quality, on anything with colour in it — a gradient,
   a picture, coloured text — that difference is visible at 1:1. **No interval
   on the frames alone can fix this**: nothing about a gap in the frames says
   whether it is the end of a scroll or the moment before the next notch. The
   wheel says. So a still now waits for input quiet as well, and 400 ms is the
   300 ms a hand leaves with room.
2. **The loop blocked for 66–98 ms per still.** `Page.captureScreenshot` went
   out with `Client::call`, which sits on the mailbox's condition variable
   until the reply comes, so for the whole of the screenshot the program was
   not reading the terminal. That is the "sometimes a key needs pressing
   twice" report: the key was not lost, it was a tenth of a second late.
   `Client::send` and `Client::take_reply` were added for this — the same
   mailbox, the same wake pipe, the reply collected on whichever pass it has
   arrived on — and the loop keeps polling, handling input and painting frames
   while the still is in flight.
3. **Request-time crediting fed the still back into itself.** This is the one
   that was not guessed, and it turned up when the new rule — "any frame
   between the request and the reply means the page moved" — was driven
   against a real engine and *never produced a still at all*: 67 asked for, 67
   thrown away, on a page nothing was happening to. Probed on
   `chromium-shell`, at 1280×768, on an idle page:

   ```text
   idle, no screenshots          0 frames in 3 s
   8 screenshots in a row        exactly 1 screencast frame each
   that frame's timestamp        +4 ms from the request, 35–48 ms before the reply
   fromSurface=false             the same
   ```

   **`Page.captureScreenshot` forces a capture of the page's surface, and the
   screencast is watching that same surface, so every still photographs itself
   into the screencast.** Credit the still with the instant it was *asked for*
   and that shutter frame — stamped 4 ms later — counts as newer, so a JPEG of
   the page was painted straight over the PNG that had just replaced it, which
   cleared the tab's rest, which asked for another still 150 ms later, which
   produced another shutter frame. **A loop, about four times a second, on
   every page including a completely static one.** That is the flashing; the
   wheel only made it more frequent.

   So: one frame in the window is free (`motion::SHUTTER_FRAMES`), two or more
   are the page moving; a painted still is credited with the moment its
   **reply** arrived, the latest instant it could depict, which puts the
   shutter frame on the stale side; and **a frame older than what is on screen
   is not motion** — it shows a moment already drawn, so it does not clear the
   rest and does not restart the rest timer. With all three, the loop has
   nothing to stand on.

**What the timestamps are for.** Ordering a frame that was in the mailbox
before a still that *was* painted, arriving after it — the shutter frame is
the common case of exactly that. `Page.screencastFrame` carries
`metadata.timestamp` in seconds since the epoch and the engine is a child
process on this machine, so it is the same clock this program reads.

`apps/browser/src/motion.rs` is the policy and its tests, away from the engine,
the terminal and the pane. `apps/browser/tests/engine.rs` pins the three
claims against a real engine: that a still provokes exactly one screencast
frame and where in its window that frame lands; that ten wheel notches 200 ms
apart produce no still until they stop and exactly one afterwards; and that a
key sent while a still is in flight reaches the page before the still's reply
is collected.

### The decoder, and a decision overturned

None of this is available without a JPEG decoder, and `apps/preview/src/lib.rs`
had refused to write one: 600–900 lines for a second format, in a repository
whose one dependency is `libc`, with nothing in the tree wanting it. Every
clause of that was true and the estimate was accurate — `tos_term::jpeg` is a
little over nine hundred lines of code, written from T.81 the way `png.rs` and
`inflate.rs` were, baseline only and refusing progressive by name.

What changed is the other side of the ledger, and the point worth keeping is
that it changed because somebody measured it. "Nothing else in the tree wants
it" stopped being true the moment a pane-sized screencast was the first thing
that did, and 24 frames a second is not an argument anybody was going to win
with a line count. The preview program still does not show JPEGs — that is a
follow-up with its own tests — but the reason it gave for never showing them
is gone.

The decoder is 8.1 ms for a 1280×770 4:2:0 frame at quality 85, in release,
on the host the branch was built on, and 5.7 ms for a lighter page in the
container: inside the 17 ms between two frames either way, on one core, in the
pane's own process. `compositor/tos-term/examples/jpeg_decode.rs` is how that
is measured and `compositor/tos-term/tests/jpeg.rs` is what says it is right —
fixtures from ImageMagick with their pixels from Pillow, and a check against a
real Chromium screencast frame that came out within three counts a channel of
what libjpeg makes of the same file, at 37 dB against the PNG.

### And the frames stopped being files

The second half of the change, and the one that cost the compositor nothing:
the client decodes and sends **raw pixels**, `f=24`, over the same `t=s` it
used for the PNG. The terminal cannot read JPEG on the graphics path at all,
so something had to decode; doing it in the pane rather than in the compositor
deletes the per-frame PNG decode from the parse loop instead of moving it, and
`f=24` is the protocol's own format, so nothing in `tos-term`, `tos-render` or
the compositor changed. A still goes over as `f=32`, because that is what the
PNG decoder produces and a still is one frame every 150 ms.

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

**~~Measure the compositor's PNG decode per frame on real hardware.~~**
Answered by deleting it: the decode is in the pane now and the compositor
takes pixels. What is still untimed is the rest of that path — the read out of
`/dev/shm`, the copy into the store and the blit — on the machine tOS boots
on, with a 2.9 MB frame rather than a 320 KB one. Labels: `experiment`,
`area:graphics`.

**Show JPEGs in `tos-preview`.** The decoder exists and the program that
exists to prove the graphics path from the other end still refuses the format.
It is the `--rgba` path with a decoder in front of it, plus a decision about
what a progressive file the person double-clicked should say. Labels:
`enhancement`, `area:applications`.

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
