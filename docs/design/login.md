# The login boundary

**Issue #112.** Decided: a login screen, drawn by the compositor, which is the
lock screen with nothing behind it.

## What there was

No boundary at either end.

**Nothing had to be entered.** The machine booted straight into a session. The
installer asked for a password and the only thing that ever consulted it was
the screen lock, and only once somebody pressed the key for it.

**It could be fallen out of.** `Compositor::close_pane` ended the session when
the last pane went, and what that landed on depended on the image:

- On the **live image** both GRUB entries carried `tos.rescue`, so typing
  `exit` in the last pane put an unauthenticated root shell on tty0 — the
  default way to boot the image, on a machine anybody could be standing at.
- On an **installed machine** init started a new session: everything in the
  old one gone, a flash of the bare VT on the way through, and nobody asked
  who they were.

So `exit` meant "destroy the session", and the reward was a raw root shell or
a blank new one. The compositor is the whole point of tOS and it was the one
thing you could fall out of by typing a word.

## The screen

**One type, two reasons for it being up.** `LockScreen` carries a `Purpose`:
`Lock`, where a session is waiting behind it, and `Login`, where there is
none. Everything else — the masked field, the rate limit, the doubling wait
after a wrong password, the way it owns every kind of input and not just the
keyboard — is the same code, because it is the same screen asking the same
account for the same password. Two types would have been two places for all of
that to drift apart, which the issue said out loud before either existed.

What differs is the title (`┌─ log in ─ tos ─┐`) and what answering it does.
The account name is on both: a prompt that does not say whose password it
wants is one people type the wrong password into, and on a login screen it is
the only thing on the panel that says anything about the machine at all.

## The rule: no password, no boundary

A login screen with nothing to check against is a brick, so the gate is the
credential:

| the account's `/etc/shadow` field | what happens at boot |
|---|---|
| a `$6$` hash | the login screen, and no session until it is answered |
| `*`, `!`, empty | a session starts, and nobody is asked |

That is the same rule the lock already obeyed, arriving at the other end of
the session — and it means the compositor still never learns what live media
is. The live image's only account is Debian's root, which carries `*`; an
installed machine whose owner declined a password is in the same row, and the
installer says so on the screen where the password is asked for.

This is the "explicit flag" the issue asked for, and it is explicit in a
better place than a kernel command line: the thing that decides is the machine
having a password, which is a thing a person chose during the install.

## Nothing runs behind the screen

The session is not built until somebody logs in. `Compositor::new` puts up
either a session or the screen that has to be answered before there is one,
and `begin_session` — one workspace, one pane, one shell — is called from
exactly two places: that, and the moment a password verifies.

A shell spawned before anybody said who they were would make this a boundary
in the drawing only. It is still true that the session runs as root and that
the compositor was root before it, so what this boundary is *not* is a
privilege separation; that waits on a session having a uid of its own, which
is `docs/design/init.md`'s open question about `logind` and #22.

## Ending a session

`exit` in the last pane, and the `quit` binding, are the same thing now:

```text
a machine with a password      log out  -> the login screen, machine still up
a machine without one          quit     -> the init starts another session
```

The second row is the live image, where leaving has always meant that, and
where `tos.rescue` asks for something else. The first row is the point of the
issue: `exit` means "log out" rather than "fall through the floor".

What comes back is a **new** session: a fresh `Session`, one pane, and the
clipboard cleared. A workspace layout is as much the last person's as the
shell history in it was, and the thing after a login boundary should not be
handed either.

A pane that dies while the screen is *locked* takes the same road. It used to
set `session_ended_while_locked` and quit the moment the password verified;
now the password verifying starts a session, because that is what answering
this screen does when there is nothing behind it.

## `tos.rescue` is a menu entry now

It used to be on both live GRUB entries, which made the emergency shell the
default. There are three entries now — `tOS`, `tOS (verbose)` and
`tOS (rescue shell)` — and only the last carries the flag. A person who wants
the shell chooses it, and can see in the menu that they are choosing it.

## What a display is

The boundary belongs to a machine's console and not to every window the
compositor can draw in. `Config::gated` is set by the DRM backend in `main`
and by nothing else: that session is the screen, the keyboard and the virtual
terminal taken from the kernel, where the compositor is the only thing between
a person and a root shell. The nested and headless backends are a window on a
desktop that has already asked, and gating them would mean every `cargo test`
on a machine whose root has a password starting at a prompt nobody typed into.

## What this does not settle

- **The session is still root's.** Logging in authenticates; it does not drop
  privilege, set a uid, or start anything as the person named in `TOS_USER`.
  Until it does, `login` here means "this console has been opened by somebody
  who knows the machine's password", which is a real boundary and not the
  whole of one.
- **DRM master and the seat.** The login screen draws on the panel, so it
  takes exactly what the compositor takes. Handing that over to `logind` or
  `seatd` is #22, and the handover is easier to reason about now that there is
  a moment in the boot where a session begins.
- **Switching user.** There is one account and one session. A second would
  need the privilege question answered first.
- **A getty.** Still masked, still for the reason in
  `docs/design/lock-other-doors.md` — but the rule's premise has changed: the
  machine has a login now, and what Door 4 is really refusing is a *second*
  one on a VT nothing here is drawing.
