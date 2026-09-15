# The picture

**Issue #132.** Decided: one picture, on the two screens that open a machine.

Above the password box on the login screen, compiled into the compositor,
drawn in pixels at whole multiples of its own, and only over a login screen.
And at the head of every pane, in place of the banner drawn in cells, for the
terminals that can be sent a picture — which on a tOS machine is the terminal
tOS runs the shell in.

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

## Only over a login

`Purpose::Lock` does not get it. A lock has a session behind it and somebody
in front of it who has already been told what this machine is; what they want
is their session back, and a picture over it would be decoration in the way of
that. A login screen is the machine opening, which is what a frontispiece is
for.

## Nothing in `lock.rs` reads a file

The rule at the top of that module — nothing there draws to a display it is
not given, reads a file it is not pointed at, or asks what time it is — is
what lets its tests drive the whole state machine with no display. A picture
is a file, so the loading is `splash.rs` and the compositor hands the result
in, the same shape the clock already has.

The compositor holds the picture only while the login screen is up: read in
`show_login`, dropped in `unlock`. A login happens once, and a megabyte of
pixels nobody is going to look at again is a megabyte a session could have
had.

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

## What this is not

Not a boot splash. The kernel messages before the compositor starts are still
on the screen, and hiding them is a different decision with a different cost —
`docs/design/init.md` is where booting quietly is argued. This is the first
screen the compositor draws, not the first screen the machine draws.

Not cached, either. A locked frame is painted when something changes rather
than on a timer, so the picture is re-blitted on a keystroke and on a pointer
that is moving, and not otherwise. `tos_render::Texture` is the cache to reach
for if a large display ever makes that felt.
