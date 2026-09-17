# The picture

**Issue #132.** Decided: one picture, on the two screens that open a machine.

Above the password box on the login screen, compiled into the compositor,
drawn in pixels at whole multiples of its own, and only over a login screen.
And at the head of every pane, in place of the banner drawn in cells, for the
terminals that can be sent a picture — which on a tOS machine is the terminal
tOS runs the shell in.

**Since.** A second picture, `lock.png`, in the bottom right corner of a
locked screen. Same machinery, same whole-pixel rule, its own file and its own
place — see *And the lock gets one in the corner* below, which is decided out
of the argument that kept the frontispiece off a lock rather than against it.

## What there was

A box on an empty field. `LockScreen::draw` drew the box and
`Compositor::render_locked` cleared to `chrome.background` around it, and on a
machine with a password that is the first thing anybody sees of tOS — since
#112 a console comes up at `Purpose::Login` with no session behind it.

Everything tOS had to say about itself it said *after* the boundary.
`.motd_art` is printed by a shell profile, so it arrives once a pane is
running: on a gated machine that is after somebody has logged in, and on the
login screen it is never.

## Where it comes from

**Compiled in**, `include_bytes!` on `compositor/tos-compositor/assets/splash.png`.
The login screen is the first thing on the display, so a picture that lived
only in `/etc` would be missing from exactly the machines that are hardest to
look at: the initramfs rescue session, and any disk whose `/etc` did not come
from the installer.

**And overridable**, `/etc/tos/splash.png`, which is the door `/etc/tos/motd_art`
already opens for the banner and for the same reason — a machine should be
able to say it is somebody's without being rebuilt. Nothing at that path is
not an error, and neither is a file that will not decode: both are the picture
tOS ships, which is what the screen would have shown anyway. A compositor that
refused to start over a decoration would be a machine bricked by its own
frontispiece.

Neither half is new machinery. `tos_term::png::decode` is the decoder the
graphics protocol already needed, and `Surface::blit_rgba` is the compositing
path a placement already takes.

## Whole pixels, or none

A framebuffer is whatever the panel is and the picture is one fixed size, so
something has to give. `blit_rgba` samples nearest, and nearest sampling at a
fractional ratio is what makes scaled pixel art look melted: one source pixel
lands on two screen pixels and its neighbour on one, so an edge that was
straight comes out ragged — and ragged differently on every display.

So the picture is only ever drawn at a whole number of screen pixels per
picture pixel: `n` of them going up, one picture pixel in `n` coming down, and
nothing in between. `Splash::fit` is that rule and nothing else. It is given
the room the layout has — four fifths of the width, and whatever height the
box left — and answers with a size or with nothing.

Interpolating instead was the alternative, and it was turned down twice over:
it would need a resampler tOS does not have, and it would blur the one kind of
picture this is for.

## What a screen too small for it does

The box survives and the picture goes. It is the same order the installer's
banner already follows — the picture gives way to a smaller one and that to
nothing, rather than the welcome screen losing the words it exists to say
(`banner_lines`) — and here there is nothing to fall back to but the box.

The layout follows from it. The picture and the box are centred as one thing,
with a blank row between them and a row of margin above and below, so a
display that takes the picture puts the pair where the box alone used to be
and a display that does not is the box exactly where it has always been. A
quarter of the shipped picture is the smallest fraction drawn; below that it
would be a smudge rather than a picture.

## The frontispiece is only over a login

`Purpose::Lock` does not get *this* picture, in *this* place. A lock has a
session behind it and somebody in front of it who has already been told what
this machine is; what they want is their session back, and a picture over the
box would be decoration in the way of that. A login screen is the machine
opening, which is what a frontispiece is for.

## And the lock gets one in the corner

**Decided after the above, and out of it.** The argument against a frontispiece
over a lock is an argument about *where*, not about *whether*: what it says is
that nothing should come between somebody and the field they walked back to.
A picture in the bottom right corner does not, and a locked screen is the one
tOS leaves up for hours at a time.

So it gets one, on the terms the frontispiece already set and with all three
of them different:

| | login | lock |
|---|---|---|
| the file | `assets/splash.png`, `/etc/tos/splash.png` | `assets/lock.png`, `/etc/tos/lock.png` |
| the room | four fifths of the width, whatever height the box left | a third of the display each way (`room_in_corner`) |
| where | centred above the box, laid out with it as one stack | the corner, a row of margin in from each edge |

**Two files and not one.** The pictures are different shapes for different
places, so one file would mean one of the two screens showing a picture
composed for the other. It also means a machine can replace either without
touching the other — and `/etc/tos/splash.png` is already spoken for twice
over, since it is what a pane is sent at the head of every shell.

**Nothing lays out around the corner.** The box sits where it would on an
empty field; the picture is placed against the corner afterwards. On a display
short enough that the two would meet, the picture goes — the same order the
frontispiece follows, and for the same reason: the box is the part somebody
cannot do without. `Rect::intersect` is that check, and the corner is drawn
before the box so that a miss in it costs a corner of a picture rather than
the field.

**Sized to land at 1:1.** The shipped `lock.png` is 426x142, and 426 is a
third of 1280 exactly, so it is drawn pixel for pixel on the display tOS runs
headless at and on the one it runs on a laptop. That matters more here than it
does for the frontispiece: `splash.png` is real pixel art and a whole multiple
of it is still pixel art — the login screen draws it at 3x on a 1080p panel —
where `lock.png` is a render, which is soft at a whole fraction and blocky at a
whole multiple and looks like itself only at its own size. A unit test asserts
the 1:1, because a picture swapped for a wider one would still draw, at half
size and softly, and nothing else would notice.

## Nothing in `lock.rs` reads a file

The rule at the top of that module — nothing there draws to a display it is
not given, reads a file it is not pointed at, or asks what time it is — is
what lets its tests drive the whole state machine with no display. A picture
is a file, so the loading is `splash.rs` and the compositor hands the result
in, the same shape the clock already has. Which of the two files was loaded is
decided where the screen goes up; where the result is *drawn* is decided by the
screen's own `Purpose`, which `draw` already has.

The compositor holds one picture and not two, because the two screens are
never up at once: read in `show_login` or `engage_lock`, dropped in `unlock`.
A megabyte of pixels nobody is going to look at again is a megabyte a session
could have had.

## At the head of every pane

**Issue #132, the second half.** The same picture, at the top of every shell,
where `.motd_art` has always been.

A banner drawn in cells is what a shell can print anywhere, and it is what a
serial console, a kernel VT and somebody logged in from another machine will go
on getting. But the shell tOS actually runs is in a tOS pane, and a tOS pane
can be sent a picture. Turning this one into coloured blocks to show it there
would be handing the one terminal that can draw it the version made for the
ones that cannot.

So the greeting asks the terminal what it is, and there are exactly two
answers:

| the terminal | the banner |
|---|---|
| a tOS pane | `/etc/tos/splash.png`, over the graphics protocol |
| anything else | the banner drawn in cells, as before |

*Since #162 there is a third answer between them — the same picture drawn in
cells, for a terminal that cannot be sent it and has room to draw it. See "And
the picture, drawn in cells" below.*

**A path, not a payload.** The picture goes as `t=f` — the compositor is told
the file's name and opens it — so the escape is a hundred bytes whether the
picture is a hundred kilobytes or ten megabytes, and nothing travels through
the pseudoterminal. `tos-preview` takes the same route for the same reason,
and the sequence around the command is deliberately the one it builds: `a=T`
clips at the bottom of the screen rather than scrolling, so the room is
scrolled up first and the cursor walked back into it.

**How a pane is told from everything else.** Two questions, and both have to
answer yes:

- `TOS` is in the environment. `iso/live-session` exports it, and it is what
  starts every tOS session, so it is set for everything a tOS machine runs and
  for nothing somebody arrived with. Without it this could be a terminal at
  the far end of an `ssh`, and a graphics command such a terminal does not know
  is not ignored — it is printed, as the text of its own escape.
- The terminal filled in the pixel fields of its `winsize`. tOS does that for
  every pane it spawns (`pane::winsize_for`) and the kernel's own VT leaves
  them at zero, which is what tells a pane from the console the rescue session
  lands on — inside a tOS session, and unable to draw a thing. It is also the
  number the picture has to be sized against, so it had to be asked for
  anyway.

Querying the terminal instead — sending a graphics command and waiting for the
reply — is the answer that would work for any terminal, and it is the one
`tos-preview` already turned down in `fit.rs`: it means raw mode, a write, and
a timeout that becomes the common path exactly on the terminals that do not
answer. Paying that at the top of every shell, for a greeting, is worse than
being wrong about an unusual terminal.

**Sized in whole pixels here too.** The picture gets its own size in cells —
512x170 in an 8x16 cell is 64 by 11 — so one picture pixel is one screen
pixel. It narrows with a pane that is narrower and is never more than half the
pane tall, because the greeting has ten more lines to print underneath and a
picture that pushed them off the top would be a picture instead of a greeting.
Below sixteen cells by four there is no picture: the drawn banner says more at
that size, and it is what such a terminal gets.

**The words the picture does not have.** `.motd_art` ends with "the terminal is
the desktop"; the picture does not say it, so the greeting does, centred under
the picture in the colour the art gives that line. It is a copy of a sentence
that exists in two places, so a test asserts the two still match.

**One file, both screens.** The picture is at `/etc/tos/splash.png` because
`t=f` needs a path, and it is the *same* path the login screen prefers over
its compiled-in copy. `iso/mkiso.sh` puts it there beside `motd_art`, and the
installer carries it onto the disk with the rest of `/etc`. A machine that
replaces it has replaced both screens at once, which is the point of there
being one file rather than two.

## And the picture, drawn in cells

**Issue #162.** Decided: one more rung on the ladder, `.motd_ascii`, between
the picture and the banner.

The two answers above were the right two while "anything else" meant a serial
console and a rescue VT. It stopped being: an installed machine runs an init
that starts `openssh-server` for itself (#110) and gets an address without
anybody sitting down at it (#124), so the ordinary way into a tOS machine is
now the one path that was written for the terminals tOS has no say over. What
such a terminal saw of tOS was a 46-column rectangle.

It cannot be sent the picture — that is exactly what the `TOS` question above
refuses, and refuses correctly. It can draw one. Half blocks carrying a true
colour foreground and a true colour background are two rows of picture per row
of cells, which is what `chafa` produces, what every emulator anybody `ssh`s
from has drawn for years, and what `motd::art_runs` was already written to
read — its test fixture has been chafa output since the installer learned to
put a banner on its welcome screen.

**A file, not a build step.** `.motd_ascii` is checked in beside `.motd_art`,
compiled into the installer with `include_str!`, copied to
`/etc/tos/motd_ascii` by `iso/mkiso.sh`, and read from there in preference to
the compiled copy — the same door `/etc/tos/motd_art` and `/etc/tos/splash.png`
open, for the same reason. It is made from `splash.png` the way this makes
one:

```sh
chafa --format symbols --symbols vhalf --size 120x20 --colors full \
    compositor/tos-compositor/assets/splash.png > .motd_ascii
```

The file is the artefact and the command is only how it was got: a different
`chafa` renders the same picture differently, so a build step would make the
greeting depend on which one the builder happened to have. Rendering it is
also the one thing here that wants a tool nobody needs otherwise, and a banner
is not worth a build dependency. **The cost is that it is a copy**: change
`splash.png` and this does not follow. That is the same bargain as the tagline
being a copy of the last line of `.motd_art`, and it is written down here
because nothing enforces it.

**What decides.** The whole greeting has to fit the terminal, in cells, as it
was drawn — `motd::fits`, against `TIOCGWINSZ`. Width is the half that
matters: one cell too wide and every line wraps, and a picture whose every
other row begins a column further along is not a picture, where the small
banner at least arrives as what it was meant to be. Height is asked for a
softer reason — text scrolls, and nothing is lost — but the rows above the
prompt are all anybody sees without reaching for the scrollback, and a
greeting that opens on three rows of somebody's hair says less than the banner
that fits whole. It is the rule `fit` already follows for the picture: the
banner is the part that gives way, because the lines under it are the part
being read.

For the file in the tree that comes to **120 by 31**: a maximized terminal
window, and not an 80-column console. An 80-column console gets what it always
got. A second, narrower render would reach it, at the price of a second copy
to keep in step with `splash.png` by hand, and one copy is enough to be going
on with.

**Asking every terminal, not only tOS's own.** `Screen::probe` returns nothing
without `TOS` in the environment, so before this the greeting knew nothing at
all about a terminal it had not started. `motd::cells` is the question every
terminal answers — an `ssh`, a serial line and a kernel VT all fill in
`ws_col` and `ws_row` where they leave the pixel fields at zero — and it is
asked separately, so that "can be sent a picture" stays the one thing
`Screen` means. Nothing is a terminal for a pipe into `less`, and that gets
the banner.

**The words this picture does not have either.** The tagline goes under it,
centred, by the same function that puts it under the picture. The render has
the tOS in the artwork and no sentence anywhere.

## The colours the session is drawn in

The picture is not only on two screens; it is where the session's colours come
from. tOS was blue — `#5f87d7`, an accent nothing else on the machine used —
and the picture has two colours of its own that are better answers.

| | | where it is in the picture |
|---|---|---|
| accent | `#92f980` | the `tOS`, and the `$ _` under it |
| attention | `#cd3f73` | the streak in the hair beside them |

**The green is everything tOS highlights on its own chrome**: the focused
pane's divider, the workspace block on the status bar, the row under the
cursor in a menu, the login screen's box and its field, the installer's frame
titles and its ticks, and the block cursor in a pane. One colour, so the
brightest thing on the screen is always the thing the session is pointing at,
and it is the colour the machine introduced itself in thirty seconds earlier.

**The red is the one highlight drawn over somebody else's output**: selected
text. `Chrome::selection` existed for this and had always fallen back to the
accent; now it is the one field the default fills in, because a light green
block behind a program's own output hides what it is highlighting. The rule
that decides which colour a highlight gets is whose pixels are underneath it.

What did *not* change is the sixteen palette entries. Those are what
applications ask for by name — `\x1b[34m` is blue because the program that
wrote it means blue — and they are not tOS's to theme. The block under the
cursor is tOS drawing, which is why that one moved.

The banner in `.motd_art` moved with them: the `tOS` is drawn in a green
gradient from the picture's green down, in a box the colour of the streak. It
is the same two colours reaching the terminals that never see the picture.

## What this is not

Not a boot splash. The kernel messages before the compositor starts are still
on the screen, and hiding them is a different decision with a different cost —
`docs/design/init.md` is where booting quietly is argued. This is the first
screen the compositor draws, not the first screen the machine draws.

Not cached, either. A locked frame is painted when something changes rather
than on a timer, so the picture is re-blitted on a keystroke and on a pointer
that is moving, and not otherwise. `tos_render::Texture` is the cache to reach
for if a large display ever makes that felt.
