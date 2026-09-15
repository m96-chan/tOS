# Where a picture is, once the text under it has scrolled

Design for [#139](https://github.com/m96-chan/tOS/issues/139), and for
[#145](https://github.com/m96-chan/tOS/issues/145), which is the same subject
one screen over.

Put a picture on a pane and print past the bottom of the screen. The text
scrolls. The picture does not: it stays in the top left, on top of whatever is
passing behind it, and the rows it covers cannot be read at all.

```
150
151
...
155
      <- 156 to 160 are here, behind the picture, and unreadable
161
162
```

The immediate cause is three lines long. The reason a first attempt at fixing
it made things worse is not, and that is what this document is for.

---

## The immediate cause

`GraphicsStore::scroll` moves placements up and drops the ones that leave:

```rust
pub fn scroll(&mut self, n: u16) {
    self.placements.retain(|_, p| {
        let bottom = p.row as i32 + p.rows as i32;
        bottom - n as i32 > 0
    });
    for p in self.placements.values_mut() {
        p.row = p.row.saturating_sub(n);
    }
}
```

`Placement::row` is a `u16`. It saturates at zero, so a placement can reach the
top of the screen but never pass it — and the `retain` above recomputes
`bottom` from the stuck row on every later call, getting `0 + 11 - 1 = 10`
every time, which is greater than zero, so it is never dropped either.
**A placement taller than one row, scrolled one row at a time, is immortal.**

Measured on an eleven-row picture in a 42-row pane, fed two hundred newlines:

```
placed at [(row 0, rows 11)]
after 200 newlines: [(row 0, rows 11)]  history=169
```

169 line feeds reached `scroll(1)`. The wiring is not the problem. The
placement simply cannot move.

The one existing test, `scrolling_drops_offscreen_placements`, places an image
exactly one row tall and scrolls by one. `0 + 1 - 1 = 0`, not greater than
zero, so it is dropped and the test passes. That is the single shape of
placement the code gets right: one as tall as the scroll.

---

## Why `row: i32` on its own is not the fix

Making the row signed and letting it go negative does fix the symptom. It also
breaks `an_animation_scrolled_out_of_view_asks_for_no_repaint`, and that test
is not wrong to object. Two questions have to be settled first, and neither is
about the type of a field.

### What `display_offset` counts, and which way a screen row moves

The renderer, and `Terminal::damage_image` with it, computes a placement's
screen row as `placement.row - display_offset`:

```rust
// Images belong to screen rows, so scrolling back into history moves them
// up with the text rather than leaving them pinned to the display.
let offset = term.display_offset() as i64;
let row = placement.row as i64 - offset;
```

The comment is right and the arithmetic is backwards. `Grid::display_row`
says what the offset does:

```rust
pub fn display_row(&self, y: usize) -> &Row {
    let back = self.display_offset;
    if y >= back {
        &self.screen[y - back]
    } else {
        &self.scrollback[self.scrollback.len() - back + y]
    }
}
```

Screen row `y - back` is shown at display row `y`. So a screen row `r` appears
at `r + back`: **scrolling back pushes the active screen *down*,** making room
at the top for history. Measured on a three-row pane holding `three/four/five`
with `one/two` behind it:

```
offset 0 -> ["three", "four", "five"]
offset 1 -> ["two", "three", "four"]
screen row 0 is displayed at y=1 with offset 1
```

> **A placement's display row is `placement.row + display_offset`.**

Nobody noticed the sign because a row frozen at zero makes `0 - offset` and
`0 + offset` differ only in a sign nothing was checking: the picture was in the
wrong place either way, and it was in the wrong place for a much louder reason.

### When a placement should be dropped

`scroll` describes itself as "dropping those that leave the screen". With
scrollback, that is the wrong rule. A pane keeps ten thousand lines of history
by default; text that leaves the top of the screen is still there, and scrolling
back finds it. A picture that was on those lines has to be found too, or
scrolling back over the greeting shows a picture-shaped hole.

The existing animation test says so in as many words:

> A playing animation off the top of the viewport is still playing ... It has
> to be a tall image to get into that state at all: a placement one row high is
> dropped the moment it scrolls off, so it is gone before there is any history
> to look back through.

That parenthesis is a description of the bug being worked around. Under the
rule below there is nothing special about a tall image: every placement
survives leaving the viewport, because leaving the viewport is not what killing
a placement is for.

> **A placement lives until the last line it covers has been evicted from
> scrollback.**

---

## The rule

`Placement::row` becomes an `i32`, counted from the top of the *active screen*,
which is where `place` already puts it. Negative is history: `-1` is the newest
scrolled-off line, `-history` the oldest one still held.

```rust
pub fn shift_rows(&mut self, delta: i32, history: usize) {
    for p in self.placements.values_mut() {
        p.row = p.row.saturating_add(delta);
    }
    self.retain_in_history(history);
}

pub fn retain_in_history(&mut self, history: usize) {
    let floor = -(history as i64);
    self.placements
        .retain(|_, p| p.row as i64 + p.rows as i64 > floor);
}
```

A line feed calls `shift_rows(-1, grid.scrollback_len())`. While history is
growing, `row` and `floor` fall together and nothing is ever dropped — right,
because nothing has been lost. Once history is full at `max_scrollback` the
floor stops moving, `row` keeps falling, and each placement is dropped on the
line feed that evicts its last row.

A screen-relative row was chosen over an absolute line number on purpose.
`Grid` numbers selections absolutely, and renumbers nothing when the oldest
line is evicted — but it does not have to, because a selection is clamped and
a placement would have to be *moved*. Screen-relative rows need no renumbering
at eviction at all: the row is already relative to the thing that did not move.

Three consequences fall out, and all three are wanted:

- The renderer and `damage_image` use `row + display_offset`, and clip. An
  image whose rows are all in history is simply off the top at offset zero, and
  comes back down into view as the viewport scrolls back over it.
- `retain_rows`, which resize uses to drop placements below the new bottom,
  reads `p.row < rows as i32` and leaves negative rows alone. They are in
  history, not below anything.
- Resize itself has to move placements. `Grid::resize` pulls lines back out of
  history when the pane grows and pushes them in when it shrinks; the cursor
  already follows that shift, and now placements do too, through the same
  number. Without it, every window resize slides every picture off its text.

The same "by the same number" rule catches something the old code got away
with. `Grid::scroll_up` clamps to the height of the region, so `CSI 65535 S` on
a 24-row screen puts twenty-four lines into history and no more. A placement
that moved the whole 65535 would be dropped as out of reach while its text was
twenty-four lines back — invisible while the row was pinned at zero, fatal once
it is not. `Terminal::scroll_up_with_graphics` does the clamp once and hands the
same number to both, and is now the only way either line feed or `CSI S`
scrolls.

---

## Which rows a scroll moves

The old store took a single number and moved everything by it, which was
survivable only because the row could not pass zero. With a row that can, two
more things have to be got right, and both were found by measurement.

**Only the region moves.** `CSI 2;5r` puts the scroll region below the top of
the screen, and row 1 then sits still while rows 2 to 5 scroll under it. A
picture on row 1 that moved anyway slid into a history that had not grown, and
came back — drawn over lines it was never placed on — the moment anyone
scrolled back. Measured before the fix: a picture placed at row 0, ten line
feeds inside a `2;5` region, and the placement had walked to row −10 with
scrollback still empty.

**Where the lines went decides whether the picture survives them.**
`Grid::scroll_up` archives a line only when the region starts at the top of the
screen; out of any other region the line is destroyed. So a placement leaving
the top of such a region is dropped rather than going negative — there is no
text left for it to be scrolled back to.

One case is easy to get backwards: when the region *does* start at the top,
placements already in history have to move too. The line leaving the screen is
pushed into scrollback underneath them, which puts every one of them a row
further back.

```rust
let moved = (p.row as i64) < bottom
    && (top == 0 || p.row as i64 + p.rows as i64 > top);
```

## What `clear` may take with it

`ED 2` clears the screen and leaves scrollback alone — that is what makes
`clear` a thing you can scroll back through. It used to empty the graphics
store outright, which was true enough when no placement could be anywhere but
the screen, and wrong the moment one can be in history: a `clear` between the
greeting and now would take the greeting's picture with it, off lines it never
touched. `GraphicsStore::clear_screen` drops the placements with a row on the
screen and keeps the ones wholly behind it.

It no longer frees the image data along with them. That is the same rule any
other placement deletion follows — `a=d` frees pixels only when asked in
capitals — and the pixels are still bounded by the store's byte budget.

## And the cells

`place_at_cursor` tags every covered cell with a `GraphicsRef`, and those cells
travel into scrollback with their row. `clear_graphics_refs` sweeps only the
active screen, so a fix that dropped placements out from under tags in history
would leave them dangling.

It cannot, and it is worth being precise about why, because the alternative is
an O(scrollback) sweep on every line feed. A placement covers screen rows
`[row, row + rows)`. It is dropped when `row + rows <= -history`, so its last
row is at `-history - 1` or above — strictly older than `-history`, the oldest
line scrollback still holds. **Every row that carried one of its tags was
evicted before the placement was.** The tags go into the bin with the rows that
held them, and a `GraphicsRef` outliving its placement is not reachable from
any surviving line.

Placement ids are never reused (`next_placement` only counts up), so there is
no aliasing to worry about either: a stale ref could at worst find nothing.

`ED 3` — erase scrollback — is the one place history shrinks without any row
moving. It calls `retain_in_history(0)` afterwards, which drops everything
already above the screen.

---

## Not in this change

Two neighbouring gaps are real and not fixed by the work above. They are
written down so the next person does not read the code as claiming to have
handled them.

- **Scrolling down does not move placements.** `CSI T`, reverse index, `IL` and
  `DL` push text around the screen and leave pictures where they are. This is
  older than the issue and unchanged by it; the rule it needs is the mirror of
  `scroll_up` above, and it belongs with whoever writes it.
- **A placement now lives for the depth of scrollback, not a screenful**
  ([#146](https://github.com/m96-chan/tOS/issues/146)). Nothing bounds how many
  an application may stack on one row, so where the old code dropped them within
  a screen of scrolling, the store can now hold them for ten thousand lines.
  Each one is a few dozen bytes and one pass of a line feed, and the renderer
  only sorts the ones on view, so this is a cost rather than a leak — but the
  implicit cap is gone and nothing replaced it.

A third was on this list and has since been dealt with: the alternate screen
emptying the store, which is the section below.

---

## A store for each screen

**Issue [#145](https://github.com/m96-chan/tOS/issues/145).** Everything above
makes a placement outlive the screen it was placed on, which is the whole point:
scroll back over the greeting and the picture is there. Open `vim` and quit, and
it was not.

`swap_alt_screen` swapped the grid and emptied the graphics store:

```rust
std::mem::swap(&mut self.screen, &mut self.inactive);
...
self.graphics.clear();
```

The text and its history made the round trip; the pictures did not. That was
harmless while a placement could only ever be on the visible screen — entering
the alternate screen hides the visible screen anyway — and stopped being
harmless the moment placements started living in scrollback. Every full-screen
program, an editor, a pager, `top`, took them with it.

So the store is swapped too, and `Terminal` holds the other one in
`inactive_graphics` the way it already holds `inactive`.

### Two stores rather than one

The cheaper shape would be one image store shared between the screens with two
sets of placements, since the byte budget is charged against images and a
placement is a few dozen bytes. It is the wrong shape, and the reason is an id.

A program on the alternate screen may transmit `i=1`. The shell's greeting is
sitting at `i=1` in the primary screen's scrollback. Sharing the image map means
the second transmission overwrites the first, and scrolling back afterwards
finds the greeting's placement drawing the editor's pixels. Kitty splits the two
for the same reason — a `GraphicsManager` per screen buffer — and tOS follows
Kitty on the protocol it speaks.

### What that costs, and what stops it costing more

A second store is a second ceiling: `graphics_budget` is per store, so a pane's
peak becomes two of them rather than one.

What stops that being two budgets of pixels sitting idle is that the alternate
screen's store is emptied on the way out as well as on the way in. It has no
scrollback for a picture to be scrolled back to, and a program re-entering the
alternate screen is handed a cleared screen and has to retransmit regardless —
so nothing it held can ever be looked at again. The peak is therefore the
primary screen's budget plus whatever an alternate-screen program is showing
*right now*, and it falls back the moment that program exits. The budget is a
ceiling rather than a reservation, so an idle second store costs nothing at all.

### Resize moves both

`Grid::resize` is already called on the hidden grid — deliberately, so that a
resize taken inside an editor does not destroy the shell's newest output behind
it. Its placements have to travel the same distance, or the primary screen comes
back with its pictures sitting on the wrong lines. `Terminal::resize` now shifts
and prunes both stores against their own grid's shift and history.
