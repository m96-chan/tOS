# Screen lock

Design for [#45](https://github.com/m96-chan/tOS/issues/45).

A tOS session can be left and cannot be locked. Nothing in the compositor or
in `tos-platform` mentions lock, idle or blanking. This document decides the
three things the issue says have to be decided before code is worth writing,
and then says which part of the work is unblocked by those decisions and which
part is not.

---

## What exists today

```text
live ISO            busybox initramfs, no /etc/passwd, no /etc/shadow
                    /init execs /sbin/tos as root; no login, no getty
installed disk      tos-install writes /etc/passwd and /etc/inittab
                    ::respawn:/sbin/tos; still no login, still no /etc/shadow
compositor          owns the VT in VT_PROCESS mode, holds DRM master,
                    EVIOCGRABs every input device
```

The pieces a lock would be built out of are all there, and none of them is
pointed at a lock.

`main.rs:211-222` takes the virtual terminal, after installing handlers for
the switch signals. `main.rs:238-262` is the loop that acts on a switch: on
`SIGUSR1` it drops DRM master with `display.release()` and agrees to the
switch with `vt.allow_switch_away()`; on `SIGUSR2` it acknowledges and takes
master back. `vt.rs` can therefore already say yes to a switch. It has no way
to say no.

`main.rs:226` grabs the input devices, so on hardware the keyboard is already
tOS's alone — the kernel console does not see a keystroke. That is the single
most important precondition for a lock and it is already met.

`compositor.rs:328-334` is how the launcher owns the keyboard: `handle_key`
checks for an open overlay before the keymap and before the focused pane. The
check is in `handle_key` only. `handle_input` (`compositor.rs:283-315`) routes
`Mouse`, `Pointer` and `Paste` events without passing through it, so an
overlay does not in fact own the *input*, only the keys. A lock has to gate
`handle_input`.

`tos-system`'s `power` module reads batteries and calls `reboot(2)` and
`/sys/power/state`. It has no notion of idle. The compositor has no
`last_activity`, no blanking, and no DPMS.

The installer asks for a hostname and a username (`plan.rs:40-44`). It does
not ask for a password, and it writes `/etc/passwd` with `x` in the password
field — a promise that the hash is in `/etc/shadow`, which the installer never
creates. The installed system has no credential for anybody.

---

## What a lock has to be

A lock is not a screensaver and it is not a blank screen. It is a prompt that
owns the keyboard, refuses to give the session back, and cannot be walked
around. Three properties, and all three have to hold at once:

1. Input reaches the lock and nothing else. No pane sees a key, a mouse
   event or a paste; no binding fires except the ones the lock defines.
2. The screen shows nothing of the session.
3. There is no other way into the session while it is locked — not another
   virtual terminal, not another DRM master, not the process tree.

Property 3 is the one that makes this hard, and it is the one a lock built
only inside the compositor gets wrong.

---

## Where the password comes from

**Decision: from a credential file tOS owns, `/etc/tos/shadow`, written by
`tos-install`, hashed with SHA-512 crypt (`$6$`) implemented in tree. Not from
`/etc/shadow`, and not from `crypt(3)`.**

### There is nothing to read, and #20 does not change that

The live ISO has no user database at all. `iso/mkiso.sh` creates `/etc/tos`
and `/etc/profile` in the initramfs and nothing else under `/etc`; there is no
`/etc/passwd` and no `/etc/shadow` to be thin about. The installed system has
an `/etc/passwd` with two entries and no shadow file behind either of them.

The Debian rootfs of [#20](https://github.com/m96-chan/tOS/issues/20) does not
supply the missing piece. `mmdebstrap` produces a root account that is locked,
and the user the tOS installer creates is not a Debian user at all — the
installer writes that line itself. After #20 lands there is still no password
on the machine, because nobody has ever been asked for one.

So the credential has to be *created*, not *found*. The only moment tOS asks
the user anything is the installer's configuration screen, which already
collects the username. That is where the password comes from.

### `/etc/shadow` is the wrong file even when it exists

Two reasons, and the second one is fatal.

Writing a tOS-set hash into `/etc/shadow` silently makes it the machine's
login password as well, because Debian's PAM reads that file. Nobody asked for
that coupling, and it breaks in an ordinary way: a user who runs `passwd` gets
a new hash in that file and the tOS lock either starts accepting the new
password by accident or stops working, depending on the scheme.

And the scheme is the fatal part. Debian has hashed with **yescrypt** by
default since bullseye; a bookworm `/etc/shadow` contains `$y$` lines.
Verifying a yescrypt hash means implementing yescrypt — scrypt's memory-hard
core plus yescrypt's own additions — with no dependencies. That is a far
larger and far more error-prone piece of work than the DEFLATE decoder in
`tos-term`, and getting it subtly wrong produces a lock that rejects the
correct password or accepts an incorrect one.

`/etc/tos/shadow`, mode 0600, owned by root, holding one line, says what it
is: the credential that unlocks the tOS session. When a real PAM story exists
it can become a second source. It should not be the first one.

### `crypt(3)` is not available and would not be enough

The `libc` crate does not declare `crypt` — there is no `fn crypt` anywhere in
libc 0.2.189, on any target. tOS would have to declare it itself, and then:

```text
ISO build     x86_64-unknown-linux-musl, fully static
              musl's crypt does DES, MD5, SHA-256, SHA-512, blowfish
              it does not do yescrypt
dev host      glibc, where crypt lives in libxcrypt and needs -lcrypt,
              which means a build script and a library on the build machine
```

So `crypt(3)` costs a link-time dependency that the workspace does not have,
on the one build where it costs anything at all, and it still cannot read the
file it exists to read. It is not a shortcut.

### SHA-512 crypt is the right amount of work

`$6$` is fully specified, deterministic, has published test vectors, and needs
only SHA-512 underneath it — which is itself about a hundred lines and is
verified against FIPS vectors. Both halves are pure functions over bytes with
no I/O, which is exactly the shape the repo already tests well: the DEFLATE
decoder, the PNG reader and the ALSA structures are all this.

It also means the hash tOS writes is a hash any Linux tool understands, so the
file is inspectable and the choice is reversible.

```text
tos-install  asks for a password (twice), generates 16 bytes of salt from
             /dev/urandom, writes $6$<salt>$<hash> to /etc/tos/shadow

tos          reads /etc/tos/shadow, hashes what was typed with the salt and
             round count from that line, compares in constant time
```

The compositor reads the file directly. There is no setuid helper, no agent
and no PAM, because there is nothing to gain: `tos` already runs as root, owns
the framebuffer and holds an exclusive grab on every keyboard. A helper would
be a boundary between the process and itself.

### An empty password is allowed, and means no file at all

This was left open here and settled when the installer screen was written
([#48](https://github.com/m96-chan/tOS/issues/48)): the password field may be
left empty, and when it is, `tos-install` writes **no** `/etc/tos/shadow`.

Three reasons, in order of how much they matter.

The empty string hashes perfectly well. A `$6$` line for it is a valid
credential that a lock would engage on and then open for a bare Enter, which
is worse than no lock — it looks like one. Declining a password therefore has
to mean *no credential*, not *a credential of nothing*, and the rule below
already gives that case a defined, honest behaviour.

Requiring one would not buy what it looks like it buys. Nothing on the machine
gates a login — the console starts the compositor directly — so this password
protects the screen lock and nothing else. An installer that refuses to finish
without one teaches the person in front of it to type `a` and press Enter
twice, which is a credential in name only and a worse outcome than the empty
field they chose on purpose.

And it is not a silent choice. The configuration screen says "With no
password, the screen will never lock" under the empty field, and the
confirmation screen — the one nobody gets past without typing the disk's name
— carries `Password   none, so the screen will not lock` in the warning
colour.

`/etc/passwd` was fixed in the same change. It said `x` in the password field,
which means "the hash is in `/etc/shadow`" — a file tOS does not write. It now
says `*`: nothing logs in through that file, which is both true today and the
safe thing for Debian's PAM to read after [#20](https://github.com/m96-chan/tOS/issues/20).

---

## VT switching and DRM master

**Decision: the lock refuses the switch with `VT_RELDISP 0`. It does not take
`VT_LOCKSWITCH`, at least not yet.**

A lock that Ctrl+Alt+F2 walks around is not a lock. There are exactly two
mechanisms in the kernel for stopping it, `vt.rs` can reach both, and they
differ in one way that decides between them.

### Refusing the switch: `VT_RELDISP 0`

tOS already runs the VT in `VT_PROCESS` mode (`vt.rs:130-152`). That mode is
not advisory. When the user presses Ctrl+Alt+F2 the kernel sends the release
signal and **suspends the switch** until the owning process answers with
`VT_RELDISP`. The argument decides what happens:

```text
VT_RELDISP 1   allow the switch      vt.rs: allow_switch_away()
VT_RELDISP 2   acknowledge coming back   vt.rs: acknowledge_switch_back()
VT_RELDISP 0   refuse; the kernel abandons the switch    — missing
```

So the ability to refuse is already three lines away from the code that
exists, and it has the right properties:

- It is per-process. Nothing global is set. If tOS dies, its `VT_PROCESS`
  mode dies with it and the console is switchable again — which is the
  failure everyone wants, because a machine nobody can reach is worse than a
  session that stopped being locked.
- It composes with DRM master for free. The existing loop drops master
  (`display.release()`) *and then* agrees to the switch. A locked session does
  neither, so master is never released and no other process can set a mode.
  The ordering matters: `main.rs:241-247` must consult the lock before
  `display.release()`, not after.

It has one real cost. The kernel has no timeout on `VT_RELDISP`: a process
that never answers leaves the VT switch pending forever. X had exactly this
hang. That is why refusal must be a branch on lock state rather than a
standing policy — an unlocked session answers immediately, a locked one
refuses immediately, and neither is silent.

### `VT_LOCKSWITCH`, and why not yet

`VT_LOCKSWITCH` (`0x560B`) and `VT_UNLOCKSWITCH` (`0x560C`) set and clear the
kernel's `vt_dont_switch`, which makes `set_console()` — the function behind
the Ctrl+Alt+Fn keysym — refuse, and makes `VT_ACTIVATE` fail. It needs
`CAP_SYS_TTY_CONFIG`, which tOS has. This is what an X server's
`DontVTSwitch` uses, and it is a strictly stronger statement than refusing a
switch one at a time.

It is also a single global kernel flag with no owner. Nothing clears it when
the process that set it exits. If tOS is killed while locked, that machine can
never switch virtual terminals again until it reboots.

`vt.rs` is already careful about this exact class of failure — `Drop` restores
the terminal because "leaving a VT in graphics mode with the keyboard off would
make the machine look dead" — but `Drop` is not a guarantee here. The release
profile sets `panic = "abort"`, so a panic does not unwind and no destructor
runs, and `SIGKILL` never ran one to begin with. Today that exposure is
"this VT looks dead". Taking `VT_LOCKSWITCH` widens it to "no VT can be
reached".

That trade is probably still worth making, because the window is exactly the
locked interval and the whole point of a locked interval is that the machine
should be hard to get into. But it should be made after someone has answered
the empirical question on real hardware — does the kernel clear
`vt_dont_switch` when the process that set it dies, on the kernels tOS boots?
— rather than by reasoning from the source. That is its own task, below.

### What the lock does not stop

Stating these is part of the design, not an omission from it.

- **SysRq.** `Alt+SysRq+r` takes the keyboard out of raw mode and undoes the
  grab; `Alt+SysRq+k` kills everything on the VT. SysRq is above every
  mechanism here. The ISO's kernel command line sets nothing, so whatever the
  Debian kernel's default mask is, is the policy. On a locked machine that is
  a hole; on a project at this stage SysRq is also the debugging lifeline.
  It needs a decision, not a default.
- **The serial console.** `iso/mkiso.sh` boots with `console=ttyS0
  console=tty0`, and `/init` execs a shell on `/dev/console` if `tos` exits.
  A serial line is an unauthenticated root shell. On the live ISO that is the
  point. On an installed system it is a decision nobody has made.
- **The disk.** This is a screen lock. Anyone who can reboot the machine reads
  everything on it. Disk encryption is a different feature and is not implied
  by this one.
- **The nested and headless backends.** There is no VT and no DRM master to
  defend. `tos --backend nested` is a process in someone else's terminal;
  its lock is a lock on the session's contents, not on the machine. The lock
  should work there — that is how it gets tested — and it should not pretend
  to be more than it is.

---

## Whether the live session locks

**Decision: the lock is conditional on a credential existing, not on the
session being live. The live ISO therefore does not lock, and says why.**

The live ISO has no password and cannot be given one:

- A published password is not a lock. Shipping a known credential on a
  downloadable image is worse than shipping none, because it looks like a lock
  to the person relying on it.
- A password the live session generates and shows on screen is a screensaver
  with extra steps.
- A password the live session prompts for at boot is a login screen, which is
  a different feature and one nobody has asked for.

So the live session does not lock. But "am I live?" is the wrong question for
the compositor to ask, because the answer is a property of the medium and the
compositor has no business knowing about media. The rule is:

```text
/etc/tos/shadow exists and parses   ->  Lock works
otherwise                           ->  Lock refuses, with a message saying
                                        there is no password to unlock with
```

One rule, three configurations:

| Configuration | Credential | Result |
| --- | --- | --- |
| Live ISO | none | `Lock` says there is nothing to unlock with |
| Installed, password set | `/etc/tos/shadow` | locks |
| Installed, password declined | none | same as live |

And it is testable everywhere. The credential path goes behind a seam the way
`tos-system` and `installer/src/exec.rs` already do theirs, so a test points
the compositor at a file it wrote itself and drives the whole state machine —
lock, wrong password, right password, refusal with no credential — under the
headless backend on a machine with no VT and no DRM. The one configuration CI
can actually boot is the one where the lock does nothing, which is exactly why
the lock must not depend on booting to be tested.

---

## Blanking

Blanking the display while locked and blanking it on idle are the same
mechanism and it belongs to the platform layer, not to the lock. This overlaps
[#16](https://github.com/m96-chan/tOS/issues/16), so it is specified here once
and built there.

The mechanism is to **disable the CRTC**: a `DRM_IOCTL_MODE_SETCRTC` with
`fb_id = 0` and `mode_valid = 0`. Scanout stops, the panel loses its signal
and sleeps. `drm.rs` already has `set_crtc` and already saves the console's
CRTC to restore on exit (`drm.rs:814-831`), so this is a variation on code
that exists, and it needs no new ioctl. Coming back is `mode_set = false`,
which the existing `restore()` already does, so the next frame reconfigures
the CRTC on its own.

The alternative is the connector's `DPMS` property, set with
`DRM_IOCTL_MODE_OBJ_SETPROPERTY`. `query_connector` already reads the property
ids and values (`drm.rs:400-414`) and discards them, so it is reachable — but
it means a second ioctl to look up each property's *name*, and on an atomic
driver the legacy DPMS property is emulated anyway. Disabling the CRTC gets
the same result through the path `drm.rs` already speaks.

The seam is a new `Display` method next to `release` and `restore`:

```rust
/// Stop putting anything on the screen, without giving up the display.
fn blank(&mut self, blank: bool) -> io::Result<()> { let _ = blank; Ok(()) }
```

Defaulted, so the nested and headless backends are unaffected and no existing
implementation changes. Blanking is not releasing: tOS keeps DRM master and
keeps the VT, which is what makes a blanked *and locked* screen different from
a switched-away one.

Note that blanking does not imply locking and locking does not imply blanking
immediately. The lock draws its prompt; the display blanks after a further
idle interval; a key un-blanks and shows the prompt again. Those are two
timers over one state machine.

---

## The state machine

```text
                 Lock binding, or idle deadline
    Unlocked  ─────────────────────────────────►  Locked
       ▲                                          │    ▲
       │                                          │    │ wrong password,
       │          password verifies               │    │ or a key while
       └──────────────────────────────────────────┘    │ blanked
                                                       │
                          idle while locked            ▼
                    Locked ──────────────────►  Locked, blanked
```

Where it lives, against the code that exists:

- `Action::Lock` in `keys.rs:17-54`, one row in the table at `keys.rs:117-148`
  — which registers it as both `super+<key>` and leader-then-key for free —
  and one arm in `perform` (`compositor.rs:549`). The binding list in
  `config.rs:97-108` is updated in the same change, because `tos --help` is
  where the bindings are documented.
- A `lock: Option<LockScreen>` field on the compositor, checked at the top of
  `handle_input` (`compositor.rs:283`) and not only in `handle_key`, so mouse
  and paste are gated too.
- `LockScreen` is its own type, not an `Overlay`. `Overlay` echoes its query
  verbatim (`overlay.rs:281`), filters an item list on every keystroke, and
  has no way to submit when the list is empty — `Enter` returns `Consumed`.
  A password field masks, submits, and has no list. Reusing `Overlay` means
  three special cases in a type whose whole value is that it has none.
- Rendering skips the pane loop (`compositor.rs:889-925`) and the status bar
  (`966-968`) rather than painting over them. Painting over is not enough: on
  a frame where `force` is false only damaged cells are repainted, and the
  status bar shows pane titles.
- The idle deadline is the same shape as the transient message that already
  expires in `tick` (`compositor.rs:859-864`), and the same shape as the
  animation deadline that already pulls the poll timeout in
  (`frame_timeout_ms`, `compositor.rs:1042`). A `last_activity: Instant`
  bumped in `handle_input` is the only new state.
- On the DRM backend, `main.rs:238-262` asks the compositor whether it is
  locked before releasing the display and before acknowledging the switch.

Two details that are easy to get wrong and should be in the tests:

- Locking must not depend on the compositor being able to render. If the
  password is wrong the lock stays; if the display is in any state at all the
  lock stays. There is no error path out of `Locked` except a correct
  password.
- A pane whose program exits while the session is locked must not be able to
  end the session. `Action::Quit` and `ClosePane` are not reachable while
  locked, but `is_running()` going false because the last pane died is a
  different route to the same place. The lock has to survive it.

---

## What lands now, and what waits

**Landing with this document:** the missing half of `vt.rs` — the ability to
refuse a VT switch, and the `VT_LOCKSWITCH` / `VT_UNLOCKSWITCH` pair with the
bookkeeping that clears the flag wherever the terminal is restored, including
`Drop`. It is additive, it is confined to one file in `tos-platform`, nothing
calls it yet, and it turns the central claim of this design — that a tOS lock
can actually stop a VT switch — from an assertion into something the next
person can check.

**Not landing, deliberately:**

- *The credential.* SHA-512 crypt, the file format and the installer field all
  presuppose the answer to the first question above. If the owner prefers
  `/etc/shadow`, or PAM after #20, the whole of it is thrown away.
- *The lock state machine and its screen.* The shape is settled but the code
  sits in `compositor.rs`, `keys.rs`, `config.rs` and a new module beside
  `overlay.rs` — the four busiest files in the tree, with the config work of
  [#38](https://github.com/m96-chan/tOS/issues/38), the IME design of
  [#42](https://github.com/m96-chan/tOS/issues/42), the binding help of
  [#44](https://github.com/m96-chan/tOS/issues/44) and the input work of
  [#41](https://github.com/m96-chan/tOS/issues/41) all in flight. Landing a
  half-decided lock there buys a day and costs a week of conflicts.
- *Blanking.* It is decided, but it is #16's code. Building it here would
  mean building it twice.

---

## Proposed task issues

These are on the issue tracker as #47–#54.

### #47 — Lock: refuse the VT switch while the session is locked

`vt.rs` can now refuse a switch with `VT_RELDISP 0`, and nothing calls it.
`main.rs:238-262` releases DRM master and agrees to every switch the kernel
asks about, which is correct for an unlocked session and is the hole in a
locked one. Wire the DRM loop to ask the compositor whether it is locked
before it does either: a locked session refuses the switch and keeps master,
an unlocked one behaves exactly as it does today. The ordering matters —
`display.release()` happens before the VT is acked today, so a locked session
must skip both rather than skipping the second. Labels: `enhancement`,
`area:platform`.

### #48 — Installer: a password for the user it creates

`tos-install` collects a hostname and a username (`plan.rs:40-44`) and writes
`/etc/passwd` with `x` in the password field, promising a hash in a file it
never creates. So every installed tOS machine has a user with no credential,
which is why there is nothing for a screen lock to check against. Add a
password field to the configuration screen, confirmed twice, and write
`/etc/tos/shadow` with mode 0600 — the file tOS's own lock reads, not
Debian's. Decide there whether an empty password is allowed; the lock works
either way, because it refuses to engage when there is no credential. The
existing recorded-backend tests cover the write; what the screen looks like
when the two entries disagree is the part worth testing by hand. Labels:
`enhancement`, `area:iso`, `security`.

### #49 — SHA-512 crypt in tree, so tOS can check a password

tOS has no way to verify a password and no library it is willing to take one
from: the `libc` crate does not declare `crypt(3)`, musl's `crypt` cannot read
the yescrypt hashes Debian writes, and glibc's lives in a library the
workspace does not link. Implement SHA-512 and the `$6$` crypt scheme on top
of it as pure functions over bytes, the way the DEFLATE and PNG decoders in
`tos-term` are, with the published test vectors for both. Salt comes from
`/dev/urandom`; the comparison is constant time. This is the piece both the
installer and the lock depend on, and it is testable entirely on its own.
Labels: `enhancement`, `area:system-ui`, `security`.

### #50 — The lock screen: the state machine and the keyboard it owns

With a credential to check against, the lock itself is compositor work:
`Action::Lock` and a row in the binding table at `keys.rs:117-148`, a
`lock: Option<LockScreen>` field gated at the top of `handle_input` rather
than `handle_key` — mouse, pointer and paste events bypass `handle_key`
today, so an overlay owns the keys but not the input — and a render path that
*skips* the panes and the status bar rather than painting over them, because a
frame that is not a full redraw only repaints damaged cells. `LockScreen` is
its own type beside `overlay.rs`: `Overlay` echoes its query, refilters a list
on every keystroke, and cannot submit when the list is empty, and a password
field is the opposite of all three. Refuse to engage, with a message, when
there is no credential — which is what makes the live ISO behave. Labels:
`enhancement`, `area:system-ui`.

### #51 — Blank the display by disabling the CRTC

Nothing in tOS ever stops putting pixels on the screen, so a laptop left in a
tOS session burns its panel and its battery, and a locked session would show a
lit rectangle all night. Add `Display::blank(bool)` next to `release` and
`restore`, defaulted so the nested and headless backends are unaffected, and
implement it on DRM as a `SETCRTC` with `fb_id = 0` and `mode_valid = 0`:
scanout stops and the panel sleeps, using the ioctl `drm.rs` already speaks
rather than looking up the connector's DPMS property. Blanking is not
releasing — tOS keeps DRM master and keeps the VT, which is what makes a
blanked locked screen different from a switched-away one. This is the display
half of #16 and the screen half of #45. Labels: `enhancement`,
`area:platform`.

### #52 — Lock and blank on idle

The compositor has no idea whether anyone is there. It has two deadlines
already — the blink interval and the animation frame, both folded into
`frame_timeout_ms` (`compositor.rs:1042`) — and a transient status message
that expires in `tick`, which is the exact shape an idle timer takes. Add a
`last_activity: Instant` bumped in `handle_input`, a blank-after interval and
a lock-after interval, and fold both deadlines into the poll timeout so an
idle session wakes when it is due rather than a hundred times a second. Both
intervals belong in the configuration of #38; until that exists they are
constants with a flag. A session with no credential blanks and does not lock.
Labels: `enhancement`, `area:system-ui`.

### #53 — Decide whether the lock takes VT_LOCKSWITCH

Refusing each switch with `VT_RELDISP 0` stops Ctrl+Alt+F2 and dies with the
process, which is the right failure. `VT_LOCKSWITCH` is stronger — it makes
`set_console()` refuse outright, and it also blocks `VT_ACTIVATE` from any
other process — but it sets a single global kernel flag with no owner, and the
release profile builds with `panic = "abort"`, so no destructor runs on a
panic and none ever runs on `SIGKILL`. If the kernel does not clear
`vt_dont_switch` when the setting process dies, a crash while locked leaves a
machine whose virtual terminals cannot be reached until it reboots. `vt.rs`
has the ioctls and the bookkeeping; what it does not have is the answer, and
the answer is an experiment on the kernels tOS actually boots, not a reading
of the source. Labels: `experiment`, `area:platform`, `security`.

### #54 — The unauthenticated ways past a locked screen

A screen lock is only as good as the other doors. `Alt+SysRq+r` takes the
keyboard out of raw mode and undoes tOS's grab, and `Alt+SysRq+k` kills
everything on the VT; the ISO's kernel command line sets no `sysrq` policy, so
the kernel default is the policy by accident. `iso/mkiso.sh` boots with
`console=ttyS0`, and `/init` execs a shell on `/dev/console` when `tos` exits,
which is an unauthenticated root shell on a serial line — the point of a live
image, and a decision nobody has made for an installed one. Neither is a bug
in the lock; both are the reason a lock needs its limits written down. Decide
each for the installed system, leave the live image as it is, and record the
result next to the design. Labels: `design`, `security`, `area:iso`.
