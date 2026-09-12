# Video in a terminal

Design for [#14](https://github.com/m96-chan/tOS/issues/14).

The issue is called "video experiments" on purpose. It does not ask for a
player; it asks what the cell and surface model can carry, and it says the
output is a written conclusion plus whatever protocol gaps the attempt exposes.
This is the conclusion.

The short version, because the rest of this document is the argument for it:
**tOS can play 720p at 30fps today, it cannot play 720p at 60fps or 1080p at
anything, and the reason is not the cell model, not the renderer and not the
missing GPU. It is base64.** Fifty-five per cent of a 720p frame is spent
turning pixels into text and back again. Everything the renderer does — the
scale, the blend, the blit into the framebuffer — is twelve per cent.

Every number below came from
[`compositor/tos-compositor/examples/video_throughput.rs`](../../compositor/tos-compositor/examples/video_throughput.rs),
which is in the tree so that the next person can disagree with the conclusion
by re-running the measurement rather than by arguing with the prose.

---

## What was measured, and on what

```text
cargo run --release --example video_throughput
```

**`--release`, and the harness refuses to be quiet about it.** Every leg of
this path is a single-threaded loop over a few megabytes. At `opt-level = 0`
they are not merely slower, they are slower by *different* factors, so a debug
run does not even preserve the ordering that the conclusions below rest on. A
benchmark run in a debug build would be a wrong answer sitting in the repo
forever, so the harness prints a warning in place of the build line when
`debug_assertions` is on.

The machine:

| | |
| --- | --- |
| CPU | AMD Ryzen 9 9950X3D, 8 cores visible to the build container |
| Memory | 121 GiB |
| Kernel | Linux 7.2.4-arch1-2 |
| Toolchain | rustc 1.98.1, `release` profile (`opt-level = 3`, thin LTO) |
| Load average | 4.4 at the time of the run, on a shared machine |

That last row matters and is printed by the harness on every run, from
`/proc/loadavg`. These are single-threaded loops, and a contended machine
measures its own run queue rather than the work: under a load average of 20 the
mean cost of the 720p run moved by a factor of two between consecutive
invocations. So the tables report the **lowest** of the sixty per-frame samples
— the frame that got an uncontended core, which is the cost of the work itself
— and print the median beside it. When the two agree the machine was quiet and
the numbers can be quoted, which on this run they do, to within three per cent.

### What the harness drives, and why not the whole compositor

The measurement calls `tos_render::render` against a `Terminal` directly rather
than driving a whole `Compositor`. That is not a shortcut past the real path.
`Compositor::render_frame` (`compositor.rs:2268`) ends in exactly that call,
with exactly those arguments, once per pane. What driving it directly removes
is a shell writing a prompt, a status bar, and whichever font the machine
happens to have installed — three sources of variance that would make the
numbers unreproducible without changing what is being measured. It also pins
the cell size to the bitmap font's 6x11, so "how many rows does a 720p picture
cover" is the same number on every machine that runs this.

The last section of the harness then runs a real `Compositor` anyway, shell and
status bar included, so the gap between the isolated path and the whole program
is a measurement rather than a hope. At 640x360 the whole compositor spends
3.22ms per frame on ingest and render; the isolated path's corresponding legs
add up to 3.56ms. They agree, and the isolated numbers can be read as numbers
about tOS rather than about a test harness.

---

## What the CPU path sustains

Whole-image retransmission (`a=T`), 60 frames, milliseconds per frame:

| Picture | wire/frame | encode | decode | store | present | render | total | fps |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 320x180 | 300 KiB | 0.184 | 0.400 | 0.381 | 0.000 | 0.153 | **1.12** | 895 |
| 640x360 | 1202 KiB | 0.740 | 1.539 | 1.506 | 0.000 | 0.511 | **4.30** | 233 |
| 960x540 | 2705 KiB | 1.654 | 3.258 | 2.575 | 0.000 | 1.106 | **8.59** | 116 |
| 1280x720 | 4810 KiB | 2.935 | 5.625 | 4.992 | 0.000 | 1.916 | **15.47** | 65 |

The columns are the legs the issue asked for them to be split into. `encode` is
base64 and the escape-sequence framing on the sender's side. `decode` is base64
back to bytes, measured on its own so that it can be subtracted. `store` is
everything else `Terminal::advance` does with the frame — scanning the APC
sequence, reassembling the 4 KiB chunks, converting to RGBA, replacing the
image. `present` is stepping the animation clock. `render` is the scale and the
blit into the framebuffer.

These are not the whole cost. The wire is on top, and it is real: a separate
measurement drives base64 through an actual pseudoterminal from an actual child
process, and gets **887 MiB/s**, which is 5.28ms for a 720p frame. A player and
a compositor are different processes and can overlap, but they are competing
for the same machine, so the honest budget for one 720p frame is about 20.7ms.

**That is 48fps at 720p, and it is the headline number.** Rounded into the
shapes anyone actually asks for:

- **960x540 at 30fps**: 11.6ms of a 33.3ms budget. Comfortable, two thirds
  spare.
- **1280x720 at 30fps**: 20.7ms of 33.3ms. It works, with a third spare, and
  the third is what the rest of the compositor has to live in.
- **1280x720 at 60fps**: 20.7ms of 16.7ms. No.
- **1920x1080 at anything**: the cost is linear in pixels — 19.4, 18.6, 16.6
  and 16.8 milliseconds per megapixel across a sixteenfold range of picture
  sizes, converging as the fixed overheads amortise. 1080p is 2.07 megapixels,
  so about 35ms of CPU plus 11.9ms of wire. That is 21fps. No.

The linearity is worth stating plainly because it is the thing that says the
cell model is not involved. Nothing in these numbers is about cells. The grid,
the damage rows, the placement arithmetic, the escape-sequence parsing — all of
it is noise next to the per-pixel work, and the per-pixel work scales with
pixels exactly as arithmetic says it must.

---

## Where the time actually goes

Take the 720p row apart:

| Leg | ms | share |
| --- | --- | --- |
| base64 decode (`graphics.rs:1149`) | 5.63 | 36% |
| store: convert, allocate, replace (`graphics.rs:1072`) | 4.99 | 32% |
| base64 encode (`graphics.rs:1184`) | 2.94 | 19% |
| render: scale and blit (`terminal.rs:432`) | 1.92 | **12%** |
| present: step the animation | 0.00 | 0% |

**Base64 is 55% of a 720p frame.** It is also 33% of the bytes on the wire,
which is why the transmission leg costs 5.28ms instead of 3.96ms. It is paid
twice, once by the player and once by the compositor, and it is paid on every
single pixel of every single frame.

The renderer — the part of this system everyone assumes is the problem, the
part #12 was opened to accelerate — is 12%.

The fix is not a faster base64. A hand-tuned SIMD decoder might halve that leg
and would buy 18% overall, which is one sixth of the way from 48fps to 60fps at
720p. The fix is not to encode pixels as text at all. The protocol already has
the vocabulary for this: `Medium` in `graphics.rs:56` parses `t=f` (a file),
`t=t` (a temporary file) and `t=s` (POSIX shared memory), and `store` rejects
all three with `EINVAL:only direct transmission supported` (`graphics.rs:598`,
and again for frames at `:813`). A shared-memory transmission would delete the
encode leg, the decode leg and the wire leg together — 55% of the CPU frame and
all 5.28ms of the transport — and leave a 720p frame costing about 6.9ms, which
is 145fps, which is 1080p at 60 with room to spare.

That is the single most valuable thing anyone could do to this path, and it is
a protocol decision rather than an architecture one.

> **Caveat.** `t=f` file transmission is being implemented in parallel with
> this measurement and does not exist in the tree this was measured against.
> When it lands, the encode and decode legs of this table should be re-measured
> rather than assumed; a file still costs a `read`, and if the data on the far
> side of it is still base64 then nothing in the 55% has moved.

---

## Which route the compositor should favour

**Decision: for video, favour whole-image retransmission — `a=T`, or `a=t`
followed by `a=p`. Keep `a=f` animation frames for what they were designed for,
which is a short loop where a small rectangle moves.**

The surprise is that this is not a decision about speed, because on speed the
two routes are a tie. Same pixels, same sizes, sixty frames each:

| Picture | `a=T` whole image | `a=f`, whole-image rect | `a=f`, one-ninth rect |
| --- | --- | --- | --- |
| 320x180 | 1.12ms | 1.08ms | 0.29ms |
| 640x360 | 4.30ms | 4.25ms | 1.01ms |
| 960x540 | 8.59ms | 8.12ms | 1.98ms |
| 1280x720 | 15.47ms | 15.09ms | 3.58ms |

`a=f` with a whole-image rectangle comes in between one and six per cent ahead,
which is inside the run-to-run spread — on other runs of the same harness `a=T`
was ahead at two of the four sizes. There is no size at which the answer flips.
Call it a tie and decide on something else.

The something else is memory, and it is not close. An `a=f` frame is stored at
**full image size whatever rectangle it carried** — `Image::frame_bytes`,
`graphics.rs:384`, with a comment explaining that a partial update must never
leave a short buffer for the renderer to read. That is the right call for
correctness and it is fatal for streaming, because it means an animation's
memory is frames times the whole picture and has nothing whatever to do with
how much of the picture moved:

| Picture | bytes per frame | frames in the 256 MiB store | seconds at 25fps |
| --- | --- | --- | --- |
| 320x180 | 230,400 | 1165 | 46.6s |
| 640x360 | 921,600 | 291 | 11.6s |
| 960x540 | 2,073,600 | 129 | 5.2s |
| 1280x720 | 3,686,400 | 72 | **2.9s** |

Two point nine seconds of 720p, and then `store_frame` returns
`EINVAL:animation exceeds graphics budget` (`graphics.rs:868`). There is a
second ceiling behind that one at `MAX_FRAMES = 1024` (`graphics.rs:275`).

A determined player could stay under both by rewriting frames in place with
`r=N` instead of appending, turning the animation into a ring buffer. It would
work — `store_frame` handles the rewrite case, and when the rewritten frame is
the visible one it replaces `data` and bumps the generation. But the protocol
describes no such thing, the playback clock would be walking the ring in an
order the sender does not control, and a stall in the sender would show the
viewer a frame from two seconds ago rather than the last one it managed. That
is a lot of machinery to buy a tie.

Retransmission has none of it. One image, one placement, no budget arithmetic,
no frame ceiling, and a sender that falls behind simply shows the last frame it
managed to send. It is also the only one of the two that can survive a `q=2`
quiet stream without the compositor accumulating state it will have to evict.

### What `a=f` is genuinely good at

The third column of that table is not a footnote. When only a ninth of the
picture moves, `a=f` costs 3.58ms against retransmission's 15.47ms at 720p — a
**4.3x** win, because every leg except the render scales with the bytes sent
rather than with the size of the picture. The Kitty animation extensions are
well designed for the thing they are for: a spinner, a progress animation, a
small sprite over a fixed background, a loop short enough to fit in memory.

Video is simply not that thing. A video's "what moved" is everything, which is
why the whole-image-rect column exists in the first place, and with `rect =
whole image` every advantage `a=f` has evaporates and only its memory ceiling
is left.

### The route that did not work at all, and now does

The obvious way to stream by retransmission is to place the image once and then
send `a=t` for each frame after it, since the placement already says where the
picture goes. When this document was first drafted it did not repaint, and the
harness caught it:

```text
a=t under a live placement — new pixels, no repaint
  damage after the retransmission: false
  pixels the retained render changed: 0
```

The cause was one missing call. `Terminal::handle_graphics` damaged the rows an
image covers after `a=f` and after `a=a`, but the arm that handled a completed
`a=t` stored the new pixels and returned. Nothing marked the rows the existing
placement covered, so the old frame stayed on screen until something unrelated
happened to repaint it: the new pixels were in the store, the generation had
moved, and the screen did not know.

`a=T` hid it, because transmit-and-display re-places the image and placing
damages rows — which is why the bug survived as long as it did, the recommended
route being the one that never tripped over it.

It is fixed, and the harness now asserts the fix rather than reporting the bug.
Both halves of the recommendation above therefore work on their own merits, and
`a=t` followed by `a=p` is no longer carried by the `a=p`.

---

## What damage does, and what it does not

A key claim of this architecture is that only damaged rows repaint. For text
the claim holds and holds well. For an image it is not true at all, and the
difference is measurable.

At 720p, with a picture covering 66 of the pane's 68 rows and the texture cache
warm so that nothing in the number is a rescale:

| What was marked dirty | rows | render |
| --- | --- | --- |
| every row the image covers | 66 | 0.599ms |
| **one row of the image** | **1** | **0.457ms** |
| the whole pane | 68 | 0.611ms |
| nothing | 0 | 0.000ms |
| one row *below* the image | 1 | 0.002ms |

Read the arithmetic rather than the rows. Sixty-six dirty rows cost 0.142ms
more than one dirty row; spread over the sixty-five extra rows that is
0.0022ms each, which is exactly what the control line measures for a single row
of text outside the picture. In other words the *text* part of the repaint is
perfectly proportional to the damage, and the **image blit is 0.455ms whether
one row is dirty or all sixty-six are**. The 320x180 case says the same thing
at a tenth of the scale: 0.069ms for seventeen rows, 0.033ms for one, and the
same 0.0022ms per text row in between.

The reason is in `draw_graphics` (`tos-render/src/terminal.rs:469`). For each
placement it asks whether *any* row the placement covers is dirty, and if one
is it blits the whole placement. There is no clipping of the blit to the
damaged rows. Damage is row-granular right up to the edge of an image and then
becomes all-or-nothing.

Two things follow, and they pull in opposite directions.

For video this costs nothing, because a video frame dirties every row it covers
anyway. The damage path is doing exactly what it claims for the case that
matters here, and the "nothing" row of that table — 0.000ms, a genuine
zero — is worth noticing: a pane with a still image in it and nothing happening
costs the renderer literally nothing per frame. That is the retained model
working.

For everything else it is a real inefficiency. Any single dirty row inside a
large picture's rows costs a full 0.46ms repaint of the picture. Whether that
matters in practice depends on what else damages those rows, and this document
is not the place to guess; it is listed below as a gap with a number attached
so that somebody can decide whether the number is worth the clipping code.

### Playback itself is free

The `present` column is 0.000ms at every size, and that is not a measurement
artefact. `Image::show_frame` (`graphics.rs:505`) moves the visible frame with
a `mem::take` and a `mem::replace` — it swaps ownership of two buffers and
copies no pixels, keeping the invariant that the current frame's pixels live in
`Image::data`. Advancing an animation is a pointer swap and a row-range mark.

This is a genuinely good piece of design and it deserves recording: the entire
cost of the animation route is paid at transmission, and none of it at
playback. It is also why the `a=f` small-rect column is as fast as it is.

---

## What #12 changes, and what it does not

#12 asked for a GPU-backed texture cache and closed without one. Its closing
comment is unambiguous — nothing GPU landed, there is no GBM, EGL, GL or Vulkan
code in the tree, and the issue was superseded by
[#31](https://github.com/m96-chan/tOS/issues/31), which ruled an EGL/GBM
dependency out of scope and chose DRM overlay planes instead. So the issue's
third bullet, "note where the GPU texture cache changes the answer", has a
shorter answer than it expected: **there is no GPU texture cache, and what
landed in its place makes video slightly worse.**

What landed is `tos-render/src/texture.rs`, a CPU cache of already-*scaled*
image regions, LRU with a 64 MiB budget. Its own module doc says plainly that
it is a CPU cache with no texture upload and nothing to hand a driver. Its key
(`texture.rs:103-117`) carries the image's generation and its frame number,
which is correct and careful — a re-transmission must not serve the old
picture, and a looping animation must be able to reuse a frame it comes back
to.

Video moves both of those fields on every single frame. A retransmission bumps
the generation by definition; an animation step changes the frame by
definition. So the cache cannot hit, and not as a matter of tuning — as a
matter of construction. The harness reports the cache's own counters for every
run in the tables above, and they are the same everywhere:

```text
hit/miss:  0/60
```

Zero hits. Four picture sizes, three routes, every combination.

A cache that never hits is not free. On a miss the renderer scales into a
`Texture` and then blits that `Texture` into the surface; declining the lookup
scales straight into the surface in a single pass (`terminal.rs:499`), for
pixels that a test already proves are identical either way
(`tos-render/tests/render.rs:589`). The harness measures the difference by
running the same route twice, once with the default budget and once with a
budget of zero so that every lookup is declined:

| Picture | cached | bypassed | saved by bypassing |
| --- | --- | --- | --- |
| 320x180 | 0.152ms | 0.138ms | 9% |
| 640x360 | 0.507ms | 0.454ms | 10% |
| 960x540 | 1.113ms | 0.975ms | 12% |
| 1280x720 | 1.918ms | 1.674ms | **13%** |

Thirteen per cent of the render leg, which is 1.6% of a 720p frame. Small, but
it is a cost with no offsetting benefit, and it comes with an allocation and an
eviction on every frame.

### And what a GPU would have bought

This is the part worth being blunt about, because it changes what #31 is for.

Suppose the DRM overlay plane work lands and is perfect: the video surface
becomes a plane the display hardware scans out directly, the scale is free, the
blit is free, the render leg goes to zero. A 720p frame then costs 13.55ms
instead of 15.47ms. Sixty-five frames per second becomes seventy-four.

**A perfect GPU path is worth fourteen per cent.** It cannot be worth more,
because the renderer is only twelve per cent of the frame and the other
eighty-eight is base64 and memory traffic on the CPU, where it will stay
whatever the display hardware does.

So #12 being abandoned costs video essentially nothing, and #31 should be
justified on the things it is actually good for — tearing, power, scanning out
a full-screen surface without a copy — rather than on throughput. If somebody
wants video to go faster, the GPU is the wrong end of the pipe to look at.

---

## So can the cell and surface model carry video?

Yes, and the model was never the question.

The honest finding of this experiment is that almost nothing in these numbers
is about cells, surfaces, grids or damage. The cost is linear in pixels across
a sixteenfold range. The placement arithmetic does not appear. The damage
bookkeeping does not appear. The retained-rendering model works: a pane showing
a still picture costs a measured zero per frame, and the compositor's per-pane
render path handed a 720p video frame spends 1.9ms on it, of which 0.46ms is
the blit.

What the terminal-shaped part of the system imposes is not the grid. It is
that the pipe between a program and its terminal is a byte stream that has to
stay escape-safe, and so pixels travel as base64 inside an APC sequence. That
is the constraint, that is where 55% of the frame goes, and it is a constraint
of the Kitty graphics protocol rather than of tOS's architecture. The cell and
surface model is carrying 720p at 30fps while spending 12% of its time on the
part it is responsible for.

Which sets the ceiling on what is worth doing next, in order:

1. **A binary transmission medium** (`t=s` shared memory, or `t=f` done
   properly). Removes 55% of the CPU frame and all of the wire. 720p at 60fps
   and 1080p at 30fps both become reachable. Everything else on this list is a
   rounding error beside it.
2. **Stop putting video through the texture cache.** 13% of the render leg,
   free to take.
3. **Clip an image blit to the damaged rows.** Does nothing for video; helps
   every other use of a large image.
4. **DRM overlay planes** (#31). Worth 14% here, and worth doing for reasons
   that are not this document's.

And the thing not to do: do not build a video player. The issue was right to
call itself an experiment. What tOS has is a path that will carry a 720p
preview, a camera feed, a scrubbing thumbnail strip — and that is worth
knowing, and worth the four items above, and is not worth a decoder.

---

## What this changes, and what it only writes down

Changed:

| Where | Change |
| --- | --- |
| `compositor/tos-compositor/examples/video_throughput.rs` | new; the harness every number here came from |
| `docs/design/video.md` | this document |

Nothing in `tos-term`, `tos-render` or the compositor was touched. This issue
asked for a conclusion and for the gaps the attempt exposed, and changing the
code under the measurement would have made the measurement worth less.

Written down and deliberately not done:

- **No binary transmission medium**, although it is the one change that would
  matter. `t=f` is being implemented in parallel with this and `a=c` frame
  composition likewise; neither existed in the tree these numbers came from,
  and both should be re-measured rather than assumed to have moved them.
- **No change to the texture cache**, although it measurably costs video 13% of
  its render leg and can never repay it.
- **No clipping of an image blit to its damaged rows**, although the blit is
  all-or-nothing and that is now a number rather than a suspicion.
- **No GPU work**, and #12's abandonment is confirmed as costing video almost
  nothing.
