# Japanese input

Design for [#42](https://github.com/m96-chan/tOS/issues/42).

The README's architecture box lists "Clipboard / IME" as a compositor
responsibility. The clipboard half exists. The IME half is not partly built or
badly built; there is nothing, and no place for anything, because a key goes
from the keymap to the PTY with no stage in between where uncommitted text
could sit.

Two of the four things this issue says have to be decided turn out to have
answers in the tree rather than in anyone's preference, and the other two have
answers in numbers that can be measured. This document decides all four, says
which parts of the work each decision unblocks, and says why none of the code
lands with it.

---

## What exists today

```text
tos-input       Keymap::lookup maps the five scancodes a JIS keyboard adds
                KeyCode::Ime(Convert | NonConvert | KanaMode) reaches the
                compositor; encode_key gives all three no bytes at all
tos-compositor  handle_key: overlay, then Keymap::resolve, then encode_key
                and pane.write. Nothing between resolve and the PTY
                Overlay is the one modal surface, and it is centred
tos-render      render() draws grid cells, and only rows the terminal damaged
tos-font        VL Gothic on the ISO; 漢字 draws, exactly two cells wide
dictionary      none, anywhere
```

The two prerequisites the issue names are both done and both did what they
promised. [#34](https://github.com/m96-chan/tOS/issues/34) put VL Gothic on the
ISO, so Japanese renders rather than showing hollow boxes, and it renders at
exactly the 2:1 ratio `tos-term/src/width.rs` already assumed.
[#41](https://github.com/m96-chan/tOS/issues/41) stopped the keymap dropping
henkan, muhenkan and katakana-hiragana, and gave them `KeyCode::Ime(ImeKey)`
with W3C UI Events names, so the compositor can match on what a key means
rather than on which scancode carried it.

What #41 built is precisely an input port with nothing plugged into it.
`encode_key` returns an empty slice for every `KeyCode::Ime`, and `handle_key`
treats an empty slice as a key that did nothing (`compositor.rs:340-358`). So
the three keys already travel all the way to the one function that would route
them, and already stop there. This design consumes that port and, with one
caveat set out below, does not change it.

---

## What an IME has to be

Not a keyboard layout. A layout is a function from a key to a character and
tOS has one; `keymap.rs` turns scancode 89 into `\` and 124 into `¥` and that
is the whole of it. An input method is four things a layout is not:

1. A place to hold text the program has not been given yet, and may never be
   given, because the user may abandon it.
2. A way to show that text where it is going to land, without putting it there.
3. A conversion from a reading to the word it stands for, which needs a
   dictionary and is ambiguous, so it needs a way to offer a choice.
4. A way to stay out of the way. Every key the compositor and the running
   program already have a meaning for has to keep it.

Property 4 is the one that decides where the code goes, and it is the one an
IME bolted on in front of the keymap gets wrong.

---

## Where the IME sits

**Decision: one engine, owned by the compositor; one input context per pane.
The interception point is the `Resolution::Passthrough` arm of
`Compositor::handle_key`, not anything ahead of `Keymap::resolve`. A pane sees
nothing at all while a preedit is open, and on commit sees the committed text
as UTF-8 — the same bytes typing it would have produced.**

### The interception point

`handle_key` (`compositor.rs:328`) has three stages and the IME belongs in the
third:

```text
compositor.rs:329   an open overlay owns the keyboard  -> overlay_key
compositor.rs:333   Keymap::resolve                    -> Action | Pending
compositor.rs:340   Resolution::Passthrough            -> encode_key, pane.write
```

An IME consulted before `Keymap::resolve` would break every binding the moment
kana mode was on: `super+h` would become `へ`, the leader would become a
character, and the fix would be a list of exceptions maintained in two places.
`Keymap::resolve` (`keys.rs:229`) already computes the set the IME wants and
nothing else does — it returns `Passthrough` for a key that is not a binding,
not the leader, not a modifier or lock key, and for every release. That set is
exactly "keys that belong to the focused pane", and "keys that belong to the
focused pane" is exactly the IME's input, because the IME is standing in front
of that pane and nothing else.

So the change is one arm:

```rust
Resolution::Passthrough => {
    // The IME answers first, and only for keys the bindings did not want.
    // Returning None means it is in Direct mode or the key meant nothing
    // to it, and the existing path below runs untouched.
    if let Some(changed) = self.ime_key(&key) {
        return changed;
    }
    // ... encode_key and pane.write, exactly as today
}
```

`KeyCode::Ime(_)` arrives here already. No binding claims it, so `resolve`
hands it back; `encode_key` gives it no bytes, so `handle_key` returns false
and nothing happens. The slot the IME needs is reachable and empty, and the
test in `encode.rs` that asserts the conversion keys send an application
nothing is the test that keeps it empty.

One detail that will otherwise be a bug: `resolve` returns `Passthrough` for
releases too (`keys.rs:230-232`). `ime_key` has to ignore anything that is not
a press, the way `Overlay::handle_key` does at `overlay.rs:137-140`, or every
character will be typed twice.

### One engine, many contexts

Split it, because the two halves have different lifetimes and different owners.

There is **one engine**, on the compositor, next to `fonts` and `keymap`. It
holds the romaji table and the dictionary. A dictionary is megabytes and
loading one per pane is absurd; a word the user teaches it in one pane should
be known in the next, because the user taught *the machine*, not the shell.

There is **one input context per pane**, on `Pane`, next to `selection` and
`textures`. It holds the mode, the preedit and the rectangle last painted. The
state a user perceives — is kana on, and what have I half-typed — belongs to
the program being typed at. Someone with vim in one pane and a chat client in
another wants kana off in the first and on in the second, and that is not a
preference to be configured; it is what the two panes are for. And a preedit
is text destined for one specific PTY. With per-pane contexts, `super+l` moves
focus and the preedit stays where it was going, which needs no rule. With one
global context, focus movement during a preedit needs a rule, and every rule
available is wrong: committing text the user did not commit, discarding text
they did not discard, or carrying it to a program they were not typing to.

`Pane` already carries `selection` and `textures` for exactly this reason —
state that is per-pane because the user thinks of it as the pane's. This is a
third field of the same kind.

This is also the arrangement every input-method framework converged on: XIM
has one input method and an input context per client, Wayland has one
`text_input_manager` and a `text_input` per surface. That is not a reason to
copy them. It is evidence that the constraint is real, and the constraint is
the one above.

### What a pane sees

Nothing. From the first romaji key to the commit, zero bytes reach the PTY.

The alternative is to write the preedit into the pane and take it back with
backspaces when it changes, which is what an IME does when it has no way to
draw. It is wrong here, and not for reasons of taste:

- **The bytes are real.** `cat > file` would record every intermediate state of
  a word the user was still deciding on.
- **There is no way to take them back.** Backspace is a byte the program
  interprets, not an undo. `vim` in normal mode reads `h` as a movement, and a
  preedit of `hiragana` would move the cursor, enter insert mode and delete a
  line before the user pressed 変換.
- **The program redraws.** readline, vim and tmux repaint the line they own
  whenever they please. Text the compositor put there is text they will move,
  wrap or overwrite, and the compositor has no way to know when.
- **Nothing has agreed to it.** There is no terminal sequence that means
  "this text is tentative" — not in the DEC set, not in xterm's, not in the
  Kitty keyboard protocol, which is the newest work in this area and does not
  have one. This is #41's reasoning one layer up: it refused to invent bytes
  for the conversion keys because "the Kitty private use range is the
  protocol's to hand out", and inventing a preedit protocol would be the same
  move with more surface.

On commit the pane is written the UTF-8 bytes of the committed string, through
`pane.write`, which is byte for byte what `encode_key` produces for a
`KeyCode::Char` of each character in turn. Deliberately **not** `encode_paste`:
bracketed paste would wrap the text in `ESC[200~`/`ESC[201~` and put the
program into paste mode for text the user typed one character at a time, which
in vim means no autoindent and in a shell means no history expansion — a
difference the user did not ask for and cannot see the cause of. Control
characters are filtered on the way out, for the reason `encode_paste` filters
them: a conversion candidate is data and should not be able to be an escape
sequence.

### The modes

Two, and the preedit is not one of them:

```text
Direct     what tOS does today, unchanged: handle_key reaches encode_key
           with nothing in between
Kana       romaji keys build a preedit; everything else behaves as in Direct
```

Katakana is not a third mode. It is a conversion applied to the preedit, which
is what 無変換 does on the hardware and what F7 does everywhere else, and
making it a mode means a mode the user can get stuck in. Halfwidth katakana is
the same again and is worth having for the same key.

Direct mode must cost nothing and must be provably byte-identical to today.
That is a test: type ASCII through a compositor with an IME and through one
without, and compare what reached the PTY.

### The one change the input port needs — and it is not a variant

The key a Japanese user actually presses to turn an IME on is 半角/全角, at
the top left. It is not among the five scancodes #41 mapped, and adding
`KEY_ZENKAKUHANKAKU` would not catch it. On a USB JIS keyboard that key is HID
usage 0x35 — the position HID names "Keyboard Grave Accent and Tilde" — and
`hid-input` maps 0x35 to `KEY_GRAVE` (41) without consulting what layout the
keyboard claims to be. `keymap.rs` maps 41 to `` ` ``/`~`, so **pressing
半角/全角 on a JIS keyboard under tOS types a backtick today**, and no event
exists that an IME could match on.

`KEY_ZENKAKUHANKAKU` (85), `KEY_KATAKANA` (90) and `KEY_HIRAGANA` (91) are
real — they are what HID's LANG5, LANG3 and LANG4 map to, and keyboards with
separate keys for those do send them. The 106/109 in front of most users does
not.

So the consequence is not "add three variants". It is that **the IME toggle
must be a binding, configurable, with a default that is not a JIS key**,
because on the hardware that most needs the toggle the toggle is
indistinguishable from an ordinary character. The three keys should still get
names eventually, because a named key that is dropped is the bug #41 fixed —
but what `KEY_KATAKANA` ought to *do* is a question about how many modes tOS
has, and a variant named for a mode that does not exist is a guess inside an
enum built specifically to carry meaning. It is a task below, with the
hardware check it needs.

---

## The conversion engine

**Decision: embedded, in tree. There is no external engine tOS can reach
without implementing D-Bus, and the one external engine whose transport tOS
could implement exists to serve a file that tOS can simply read.**

### What fcitx and ibus actually require

- **ibus is D-Bus and has no second interface.** `ibus-daemon` writes its
  address into `~/.config/ibus/bus/<machine-id>-unix-<display>`; that address
  names a socket, and the protocol spoken on the socket is D-Bus — the SASL
  EXTERNAL handshake first, then messages with D-Bus type signatures and
  headers. "Connect to the socket instead of the session bus" is not an escape
  from D-Bus; it is D-Bus on a private bus. Debian's `ibus` also depends on
  `libgtk-3-0`, `python3`, `libatk1.0-0`, `libcairo2`, `libpango-1.0-0`,
  `libgdk-pixbuf-2.0-0`, `libnotify4` and `libdconf1`.
- **fcitx5 is the same answer.** Its interface is `org.fcitx.Fcitx5` on D-Bus,
  and when there is no session bus it starts a private one and writes that
  address to a file — D-Bus again. `fcitx5` pulls in `fcitx5-modules`
  (3,905,536 bytes installed), which pulls `libcairo2`, `libglib2.0-0`,
  `libpangocairo`, `libgdk-pixbuf-2.0-0` and `libenchant-2-2`, and
  `libfcitx5utils2` depends on `libsystemd0`, which tOS does not have.
- **Both are only frontends.** The conversion is `mozc-server`: 19,374,080
  bytes installed, seven times the font, and it depends on `libgtk2.0-0`,
  `libpango-1.0-0` and `libcairo2` in a process with no window. Its own
  transport is length-prefixed protobuf on a UNIX socket, so reaching it
  directly means implementing protobuf against a schema that is not a stable
  interface between versions.
- **anthy is a C library, not a daemon**, which looks like the exception. It is
  not: linking it means linking a C library, and this workspace links exactly
  one, which is libc. Its dictionary, `anthy-common`, is 28,660,736 bytes
  installed — six times the font, for the data alone.

All sizes are `Installed-Size` from Debian bookworm's `main/binary-amd64`
index, the same archive `iso/mkiso.sh` installs `fonts-vlgothic` from.

### And none of it would save the drawing

fcitx5 and ibus put their candidate windows on screen with GTK or Qt, through
X11 or Wayland. tOS is a DRM/KMS compositor with no client protocol: there is
no surface for an external candidate window to appear on, and there is not
going to be one, because the premise of the project is that nothing sits
between the terminal and the kernel.

So the external option costs a D-Bus implementation, a 19 MB server and a GTK
stack, and **tOS still has to draw the candidate window itself**. It buys the
dictionary and the conversion algorithm and nothing else. That is the fact
that settles this question, and it holds no matter what anyone thinks about
dependencies.

### The house style, and why it fits better here than anywhere it has been used

`tos-system`'s answer to "the desktop reaches this through a daemon" is to
find what the daemon is reading and read it. `audio.rs` writes out
`asound.h` and drives `/dev/snd/controlC0` by ioctl rather than linking
libasound. `bluetooth.rs` opens an `AF_BLUETOOTH` socket and reads
`/sys/class/bluetooth` rather than asking BlueZ, and says so in its header:
"tOS has no D-Bus, so none of that is available here and none of it is
pretended at." [#18](https://github.com/m96-chan/tOS/issues/18) asks the same
question this issue asks, about the same bus, and that is the answer it got.

The IME's version of the move is easier than either of those, because what
sits behind the daemon here is not an ioctl interface — it is a text file. The
one external engine whose transport tOS could plausibly implement is
`skkserv`, which is a line protocol: send `1かんじ ` and read back
`1/漢字/幹事/`. It would be a few dozen lines. It is also pointless, because
skkserv exists to hold `SKK-JISYO.L` in memory and answer lookups against it,
and `SKK-JISYO.L` is a sorted text file tOS can open. Running a daemon to read
a file on tOS's behalf is the thing `tos-system` already declined to do twice.

### What "embedded" means here

Four pieces, three of them pure functions over bytes with no I/O — the shape
this repo tests best, and the shape the DEFLATE decoder, the PNG reader and the
ALSA structure layouts already have:

```text
romaji table    'k','a' -> か, with the small-tsu and n rules
                a table and a short state machine; no dictionary, no I/O
dictionary      a sorted text file, read once, binary searched
conversion      a reading -> a list of candidates
candidates      a cursor over that list, and what each key does to it
```

**Single-segment conversion**, not multi-segment. 変換 converts the whole kana
run; pressing it again walks the candidate list; the arrow keys shrink and
grow the range being converted, which is what they do in every Japanese IME
and which needs no model at all. Splitting a sentence into bunsetsu
automatically needs a cost model and a corpus, and is the part of mozc that
justifies its 19 MB.

State the limit plainly, because it is the thing a user notices first:
「かんじ」→「漢字」 works. 「かきます」→「書きます」 does not, because the
inflection lives in the dictionary's okuri-ari half, whose entries carry an
okurigana marker (`わたしm /私/`) that means nothing to a converter which does
not know where the stem ended. Okurigana is a task after this one, and
bunsetsu splitting a task after that, and they are where the interesting work
is.

---

## Drawing the preedit and the candidates

**Decision: the compositor draws both, with `chrome::draw_text` onto the
`Surface`, over the pane's cells. Neither is an `Overlay`. Neither writes into
the grid. The rows they covered are marked on the pane's own damage with
`Terminal::damage_mut` before the next frame, so a repaint costs the rows the
preedit touched rather than the screen.**

### Why the candidate window is not an `Overlay`

It is list-shaped, it is modal-looking, and `workspace-rename` has just shown
that the overlay generalises well. It is still the wrong type, for three
reasons, of which the first is decisive:

1. **An overlay owns the keyboard and a candidate window must not.**
   `OverlayOutcome`'s own documentation says it: "Every variant means the key
   was taken: while an overlay is open it owns the keyboard, so nothing here
   ever hands a key back to the pane underneath" (`overlay.rs:52-54`), and
   `handle_key` enforces it at `compositor.rs:329` by checking for an overlay
   before the keymap. A candidate window that took every key would be an input
   method that stops you typing. Keys must keep flowing — through the IME, into
   the preedit — while the list is up.
2. **An overlay is centred; a candidate list belongs at the cursor.** The list
   offers replacements for the text under the cursor, and reading it means
   looking at two places on the screen at once if it is anywhere else.
3. **An overlay filters; a candidate list is chosen from.** Typing another
   character into an overlay narrows the list. Typing another character during
   a conversion abandons the conversion and extends the preedit, which is the
   opposite operation.

`Overlay::prompt` was right for a rename, because a rename really is the same
box with the list taken out, and everything about *typing* — which keys are
text, which are bindings to swallow, where the cursor is drawn — has one answer
in both. Here the typing is not the overlay's; the keys are the IME's. What
should be shared is the **drawing**: the borders, the clipping, the padding
that `Overlay::draw` does by hand with `chrome::draw_text`. Lifting that into
`chrome.rs` as a box helper is the right shape and is a task below —
deliberately scheduled after `notification-queue`, `workspace-rename` and
`keys-cheatsheet`, all of which are in flight against `overlay.rs` right now.

### Where they are drawn

`render_frame` (`compositor.rs:878`) already ends by painting the overlay
"last, and over everything". The preedit and the candidate box go in the same
place, after the pane loop. They are never open at the same time as an
overlay, so their order relative to it does not matter.

The position is arithmetic the compositor already does. The pane's rect comes
from `self.session.active().geometry(area)`, which `render_frame` has in
`geometry`; the cursor within the pane is `pane.terminal.cursor()`; the cell
size is `self.cell_size()`. The preedit is drawn from that cell rightward on
the cursor's row. The candidate box is placed on the row below, and above the
cursor row instead when there is no room below — the rule every IME uses, and
the only one that does not cover the text being converted.

Two consequences worth writing down rather than discovering:

- The preedit hides whatever the program has in those cells. That is correct:
  the text is going to land there.
- A preedit is clipped to its pane, not to the screen. A pane is a rectangle
  someone chose the size of, and a preedit spilling into the neighbour would
  be drawn over a program that has no idea.

### The damage problem, which is the real one

`render()` skips a row entirely when `!options.force &&
!term.damage().is_row_dirty(y)` (`terminal.rs:196`). A preedit lives on rows
the pane has no reason to think are damaged, because from the pane's point of
view nothing happened. So:

- while the preedit **grows**, new glyphs are drawn on top of old ones and it
  happens to look right;
- when it **shrinks**, when the candidate box **closes**, and when the whole
  preedit is **committed and disappears**, nothing repaints the cells it was
  covering, and the stale glyphs stay on screen until something else damages
  those rows.

That last case is every commit, which is to say every word. It is not an edge.

Setting `needs_full_redraw = true` on every preedit change would fix it, and
it is what the overlay does when it opens and closes (`compositor.rs:699,
708`). It is the wrong fix here. An overlay opens once; a preedit changes on
every keystroke, and a full-screen repaint per keystroke on the DRM backend is
the exact cost the `retained` path exists to avoid.

The right fix is already public API and needs no new code in `tos-term`:
`Terminal::damage_mut()` returns `&mut Damage` (`term.rs:332`) and
`Damage::mark_range(from, to)` marks a row span (`term.rs:131-135`). The IME
remembers the cell rectangle it painted last frame; on any change it marks
those rows on the pane it covered, and then paints the new rectangle. The pane
repaints those rows from its grid, the IME paints over them again, and nothing
else on the screen is touched.

This is a second user of an established pattern, not a new one. The graphics
work already does exactly this for a moving image — the README says it
"repaints only the rows the moving image covers" — and `terminal.rs:454-458`
is that code.

One thing to get right: the rectangle must be recomputed from the cursor, not
stored as pixels. A pane resized under an open preedit has moved its own
cursor, and a stored pixel rectangle would damage the wrong rows.

---

## The dictionary

**Decision: `SKK-JISYO.L`, whole, in SKK's format, converted to UTF-8 by the
ISO build. It costs less than the font that makes it legible. After
[#20](https://github.com/m96-chan/tOS/issues/20) it comes from Debian's
`skkdic` package and the initramfs copy goes away.**

### Measured, not estimated

`SKK-JISYO` from `skk-dev/dict`, converted from its published EUC-JP to UTF-8
and compressed the way the initramfs compresses:

| | entries | UTF-8 bytes | gzip -9 |
| --- | --- | --- | --- |
| SKK-JISYO.S | 3,410 | 73,810 | 31,923 |
| SKK-JISYO.M | 8,377 | 194,881 | 74,197 |
| SKK-JISYO.L | 175,851 | 6,157,073 | 1,997,518 |
| SKK-JISYO.L, okuri-nasi only, annotations stripped | 159,795 | 5,240,300 | 1,594,363 |

The calibration point is the font, because the owner has just accepted it and
said what it cost. #34 measured VL Gothic at 4,088,728 bytes on disk and
2,544,069 inside the gzipped initramfs, on an image of 51,904,512 bytes. The
initramfs is built with `cpio -H newc | gzip -9`, so the gzip column above is
the like-for-like number:

```text
VL Gothic, accepted             2,544,069    4.90% of the image
SKK-JISYO.L, whole              1,997,518    3.85%
SKK-JISYO.L, okuri-nasi only    1,594,363    3.07%
SKK-JISYO.M                        74,197    0.14%
```

**The whole dictionary is smaller than the font.** There is therefore no size
argument for shipping a cut-down one, and there is a strong argument against:
SKK-JISYO.M's 8,377 entries make a demonstration, not an input method, and a
user whose own surname is missing concludes the feature does not work rather
than that the dictionary is small. Ship L.

For scale in the other direction, the alternatives from the section above cost
`mozc-server` at 19,374,080 bytes installed and `anthy-common` at 28,660,736,
and Debian ships the same SKK data as `skkdic` at 1,641,284 bytes as a `.deb`
— against `fonts-vlgothic` at 2,238,576. The dictionary is the cheap part of
Japanese input. The font was the expensive part, and it is already paid for.

### Two things that follow from the format, not the size

**It is read once and held.** 6 MB resident on a 50 MB image is not nothing,
and it is also not a reason to build a database. The file is sorted by reading,
so the whole index is a `Vec<u32>` of line start offsets — 159,795 entries,
about 640 KB — and a lookup is a binary search over it. Reading and parsing a
multi-megabyte file at startup is what `tos-font` already does for the 4 MB
VL Gothic face, with `fs::read` into a `Vec<u8>`.

**It is published in EUC-JP, and tOS is UTF-8 throughout.** Converting it is
the image build's job, done once with `iconv` in `mkiso.sh` beside the font
copy that is already there — not tOS's job on every boot, and not a reason for
`tos-term` to learn a second encoding. Which encoding Debian's `skkdic` copy of
`/usr/share/skk/SKK-JISYO.L` is in should be checked rather than assumed; it is
a line in the task below.

### Where it lives

```text
now, on the ISO    /usr/share/tos/SKK-JISYO.L, UTF-8, written by mkiso.sh
                   beside the VL Gothic copy it already makes
after #20          /usr/share/skk/SKK-JISYO.L from the skkdic package;
                   the initramfs copy goes away
the user's own     $XDG_DATA_HOME/tos/SKK-JISYO, searched first
```

Searched in that order, first hit wins, with the same reasoning
`config_file.rs` gives for its own order: an installed machine starts the
compositor from `/init`, where there is no home directory and often no
environment, so the system path has to be the one that always works. And
configurable, because a user with a dictionary they have curated for years
should point tOS at it:

```text
[ime]
dictionary = /usr/share/skk/SKK-JISYO.L
```

`[ime]` becomes a new reserved section in [#38](https://github.com/m96-chan/tOS/issues/38)'s
`config_file.rs`, alongside the `[keys]`, `[status]` and `[fonts]` it already
reserves, and adding a setting there is one arm of `fn set` — which that file's
author says is the point of its shape.

**Learning is deferred, not forgotten.** A converter that reorders candidates
by what was chosen before is most of the difference between an IME that is
usable and one that is a demo, and it is a second file
(`$XDG_DATA_HOME/tos/ime-history`) written on commit. It changes no interface
in this document, so it is a task rather than a decision.

---

## The state machine

```text
   Direct ───── toggle ─────► Kana ───── a romaji key ─────► Preedit
      ▲                        ▲                                │
      │                        │  Escape, or a commit           │ 変換
      └──────── toggle ────────┴────────────────────────────────┤
                                                                ▼
                                                           Converting
                                          変換 again : the next candidate
                                          1-9        : that candidate
                                          Enter      : commit the candidate
                                          無変換     : commit the kana
                                          Escape     : back to Preedit
```

Four states and there is no fifth. Direct is what tOS does today and must stay
byte-for-byte what it does today.

Where each piece lives, against the code that exists:

- `Ime` on `Compositor`, beside `fonts` and `keymap`: the romaji table, the
  dictionary, the candidate cursor.
- `ImeContext` on `Pane`, beside `selection` and `textures`: the mode, the
  preedit, the rectangle last painted.
- One arm in the `Resolution::Passthrough` branch of `handle_key`
  (`compositor.rs:340`), and nothing else in that function.
- `Action::ImeToggle` in `keys.rs`, one row in the binding table — which
  registers it as `super+<key>` and as leader-then-key for free — one arm in
  `perform`, and one line in `config.rs`'s binding list, because `tos --help`
  is where bindings are documented.
- `[ime]` in `config_file.rs`.
- Two draw calls at the end of `render_frame`, and a `damage_mut().mark_range`
  ahead of them.

Five things that are easy to get wrong and belong in tests:

- **A pane that dies with a preedit open.** The context dies with the pane and
  nothing is committed. The bytes were never sent, so there is nothing to flush
  and nothing to lose.
- **Focus moving with a preedit open.** The preedit stays with its pane and is
  drawn only when that pane is drawn. It is not carried and it is not
  committed, because the user asked for neither.
- **A pane resized under an open preedit.** The damage rectangle is stale.
  Recompute from the cursor.
- **Direct mode is free.** Type ASCII with the IME present and without it, and
  compare the bytes that reached the PTY.
- **A commit wider than the pane.** The bytes go to the program and the program
  wraps them. The IME does not wrap anything it has already handed over.

All of it is testable under the headless backend, on a machine with no
keyboard, no font and no dictionary, provided the dictionary is opened through
a path the test supplies — the seam `tos-system` and `installer/src/exec.rs`
already use for the same reason.

---

## What lands now, and what waits

**Landing with this document: nothing but the document.**

That is not caution, and it is not the same answer #45 gave. #45 landed the
missing half of `vt.rs` because its central claim — that a tOS lock can stop a
VT switch — was an assertion until the ioctl existed. The equivalent claim here
is that the conversion keys reach the compositor's routing layer, and #41 has
already landed the code that proves it, together with the test that keeps it
true. There is nothing left to demonstrate.

Everything else is downstream of a ruling this document does not have:

- **The romaji table and the kana state machine** are the least contentious
  code here and they are still the *embedded* answer to the second question. If
  the owner prefers an external engine, the table belongs to that engine and
  the whole module is thrown away.
- **The candidate window's drawing** is the one piece that survives every
  ruling, because no external engine can draw on a compositor with no client
  protocol. It is also the piece that cannot land today: it belongs in
  `overlay.rs` and `chrome.rs`, and `notification-queue`, `workspace-rename`
  and `keys-cheatsheet` are all changing `overlay.rs` right now. #45 declined
  to touch that file for this reason and was right to.
- **`ImeKey::ZenkakuHankaku`, `Katakana` and `Hiragana`** look additive and
  safe. They are not decided. What `KEY_KATAKANA` should *do* is a question
  about how many modes tOS has, which this document answers as two, and a
  variant named for a mode that does not exist is a guess inside an enum #41
  built specifically so that nothing would have to guess.
- **`[ime]` in the configuration file** cannot be written at all, because
  `config_file.rs` is #38's and does not exist on this branch.

The dictionary numbers above are the other thing this document contributes that
did not exist before, and they are measurements rather than estimates, so they
can be checked by anyone who wants to disagree with the conclusion drawn from
them.

---

## Proposed task issues

These are proposals for the issue tracker, not issues that have been created.

### Romaji to kana, as a table and a state machine

There is no way to type a Japanese character on a Japanese keyboard in tOS, and
the first missing piece is the smallest one: `ka` is か, `kya` is きゃ, `kka`
is っか, and a lone `n` is ん only once the next key proves it was not the
start of `na`. That is a table of a few hundred entries and a state machine
with a one or two character carry, it has no I/O, no dictionary and no screen,
and it is decidable entirely by its tests — which is what makes it the right
thing to build first and the right thing to build in tree, the way the DEFLATE
decoder and the PNG reader are. Cover halfwidth and fullwidth punctuation and
the katakana table too, since they are the same table read differently. Nothing
calls it yet. Labels: `enhancement`, `area:input`.

### Read an SKK dictionary and look a reading up in it

With kana to type, the next thing is somewhere to look them up. SKK's
dictionary format is a sorted text file — `かんれい /慣例/寒冷/管領/艦齢/` —
which is the whole reason tOS needs no conversion daemon: skkserv exists to
hold this file in memory and answer questions about it, and tOS can open the
file. Read it once into a `Vec<u8>`, build a `Vec<u32>` of line start offsets
(about 640 KB for the 159,795 okuri-nasi entries), and binary search it; parse
the candidate list, strip the `;` annotations, and handle the okuri-ari half by
ignoring it for now. The path comes from a seam a test can point at its own
small file, the way `tos-system` does, so none of this needs a dictionary on
the machine running the tests. Labels: `enhancement`, `area:input`.

### Ship a Japanese dictionary on the ISO

`iso/mkiso.sh` learned to copy a CJK font out of the build container for #34,
and the dictionary is the same move for a quarter less space: `SKK-JISYO.L`
costs 1,997,518 bytes inside the gzipped initramfs against the font's
2,544,069, on a 51,904,512-byte image, so the data that makes Japanese input
possible is cheaper than the font that makes it visible. Install Debian's
`skkdic`, convert the dictionary to UTF-8 with `iconv` at build time rather
than teaching tOS a second encoding at run time, and write it to
`/usr/share/tos/SKK-JISYO.L`. Check what encoding the `skkdic` copy is actually
in rather than assuming EUC-JP. Record the new image size in the comment block
beside the font's numbers, and note that after #20 this copy goes away and the
rootfs's own `/usr/share/skk` takes over. Labels: `enhancement`, `area:iso`.

### A box that is not an overlay

`Overlay::draw` builds its own border, clips its own text and pads its own rows
with `chrome::draw_text`, and it is the only thing in the tree that knows how.
The IME's candidate window needs the same box and must not be an `Overlay` —
an overlay owns the keyboard by its own definition (`overlay.rs:52-54`) and a
candidate window that took every key would be an input method that stops you
typing — so the drawing has to come out where both can reach it. Lift the
border, the clipping and the padding into `chrome.rs` as a box helper that
takes a cell rectangle and a list of lines, and make `Overlay::draw` the first
caller, with the existing pixel tests unchanged as the proof it still draws the
same thing. Do this after `notification-queue`, `workspace-rename` and
`keys-cheatsheet` have landed, since all three are changing `overlay.rs`.
Labels: `enhancement`, `area:system-ui`.

### Draw a preedit at the cursor, and damage what it uncovers

A preedit is text the program has not been given and must not be given, so it
cannot go in the grid; the compositor draws it over the pane's cursor row at
the end of `render_frame`, where the overlay is already drawn last and over
everything. The part that is not obvious is erasing it. `render()` skips any
row where `!options.force && !term.damage().is_row_dirty(y)`, and a pane has no
idea the preedit is there, so the frame after a commit repaints nothing and the
committed glyphs stay on screen twice. `needs_full_redraw` would fix it and is
what the overlay does, but an overlay opens once and a preedit changes on every
keystroke. Use `Terminal::damage_mut().mark_range` on the rows the preedit
covered last frame instead — the same trick the graphics code already uses to
repaint only the rows a moving image covers. Labels: `enhancement`,
`area:system-ui`.

### The candidate window

Conversion is ambiguous — かんれい is four words — so there has to be somewhere
to show the choice. It is a list with a cursor, drawn at the cursor rather than
centred, because it offers replacements for the text under it and putting it
anywhere else means reading two parts of the screen at once; it goes above the
cursor row when there is no room below. It is not an `Overlay` and must not
become one: it does not own the keyboard, it does not filter on what is typed,
and typing during a conversion abandons the conversion rather than narrowing
the list. 変換 walks it, the number keys pick from it, Enter commits, Escape
goes back to the unconverted kana. Labels: `enhancement`, `area:system-ui`.

### Wire the IME into handle_key, and give it a toggle

With a table, a dictionary and somewhere to draw, the IME becomes one arm of
`Resolution::Passthrough` in `handle_key` (`compositor.rs:340`) and nothing
else in that function — deliberately after `Keymap::resolve` and not before,
because `resolve` already computes exactly the set of keys that belong to the
focused pane, and an IME consulted ahead of it would turn `super+h` into へ. On
commit the pane is written the UTF-8 bytes directly, not through
`encode_paste`, because a commit is typing and bracketing it would put the
program into paste mode for text typed one character at a time. The state lives
on `Pane` beside `selection` and `textures`, so two panes can be in different
modes and moving focus mid-preedit needs no rule. The toggle has to be a
binding rather than a JIS key: on a USB JIS keyboard 半角/全角 is HID usage
0x35, which `hid-input` maps to `KEY_GRAVE`, so it is indistinguishable from a
backtick. Labels: `enhancement`, `area:input`, `area:system-ui`.

### Find out what 半角/全角 actually sends, and name the keys that are left

#41 mapped the five scancodes a JIS keyboard adds, and the key a Japanese user
presses to turn an IME on is not among them. On a USB JIS keyboard that key
sits where HID puts usage 0x35, "Grave Accent and Tilde", and `hid-input` maps
0x35 to `KEY_GRAVE` without consulting the layout — so pressing 半角/全角 in
tOS types a backtick today, and no event exists for an IME to match. That is a
reading of the kernel source and it needs confirming with `evtest` on a real
JIS keyboard, over USB and over PS/2, which behave differently:
`KEY_ZENKAKUHANKAKU` (85), `KEY_KATAKANA` (90) and `KEY_HIRAGANA` (91) exist
and are what HID's LANG5, LANG3 and LANG4 map to, and the question is which
hardware actually emits them. Record what the keyboards in the room really
send, then give the ones that turn up `ImeKey` variants. Labels: `experiment`,
`area:input`.

### Okurigana, so an inflected word can be converted

Single-segment conversion over the okuri-nasi half of the dictionary converts
「かんじ」to「漢字」and cannot convert「かきます」to「書きます」, which is most
of the Japanese anyone writes. The inflected forms are in the okuri-ari half —
15,996 entries whose readings carry a trailing marker, as in `わたしm /私/` —
and using them means knowing where the stem ends, which SKK gets by having the
user shift-key the okurigana and which other IMEs get from a grammar. Decide
which of those tOS does, implement it, and say in the commit which Japanese it
still cannot write. This is the largest single piece of the IME and the one
that decides whether it is usable. Labels: `enhancement`, `area:input`.

### Remember which candidate was chosen

A converter that offers 貴社 before 記者 every time, to someone who has picked
記者 forty times, is a converter people work around rather than use. Reordering
candidates by what was chosen before is most of the difference between an input
method and a demonstration of one, and it is a small amount of code: a second
file at `$XDG_DATA_HOME/tos/ime-history` in the same SKK format, written on
commit, searched ahead of the system dictionary and merged over it. Write it on
commit rather than on exit, because a compositor that can be PID 1 does not
reliably get an exit. Labels: `enhancement`, `area:input`.
