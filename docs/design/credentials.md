# Where a tOS machine's password lives

**Issue #111.** Decided: `/etc/shadow` is the credential, and the screen lock
reads it.

This supersedes the decision in `docs/design/screen-lock.md`, which is still
the place to read for what the lock is and how it behaves; only the file it
reads has changed.

## What it was

`tos-install` asked for a password, hashed it with `tos-crypt`, and wrote the
`$6$` line to `/etc/tos/shadow`, mode 0600. The account it created in
`/etc/passwd` got `*` in the password field, deliberately: `x` means "the hash
is in /etc/shadow", and there was no such entry, so `x` would have been a lie
about an account with no credential at all.

Every part of that was right for a machine whose only reader was its own
screen lock. It was also, as #111 put it, one password, one reader, and every
standard tool locked out by construction:

- **`sshd`** authenticates through PAM, PAM reads `/etc/shadow`, and there was
  no line. `apt install openssh-server` could work, the daemon could be
  started, and password authentication still could not succeed.
- **`su`, `sudo`, `login`** — the same, for the same reason.

## The decision

tOS is a purpose-built OS, but re-developing what an OS already has is not the
purpose. `/etc/shadow` is what every program that authenticates anybody reads;
writing a PAM module so the rest of the world could be taught about
`/etc/tos/shadow` is work spent arriving where the default already was.

So:

- `tos-install` writes the person's hash into `/etc/shadow`, and their
  `/etc/passwd` field becomes `x` — which is now true rather than a promise
  about a file that is not there.
- The screen lock reads that file instead of a private one.
- `/etc/tos/shadow` is gone. It is not written, not read, and not kept in step
  with anything: two files that must agree with nothing enforcing it is how a
  password change through one of them silently diverges from the other.

`/etc/tos` itself stays — it holds the message-of-the-day art and the marker
that says this disk was installed — but nothing secret is in it any more.

## What the installer writes

A Debian root arrives with an `/etc/shadow` of its own, one line per system
account, all of them `*`. The person's line is appended to it, the same way
their `/etc/passwd` and `/etc/group` lines are, and for the same reason:
writing over Debian's file would take apart the accounts its own packages run
as. Where no Debian rootfs was unpacked — the busybox fallback — tOS writes
the file itself, mode 0640, with a `root:*` line above the person's.

The line is:

```text
<user>:<$6$ hash or *>:::::::
```

Two things about that shape are deliberate.

**`*` when no password was given.** An empty password hashes perfectly well,
and a machine that accepted a bare Enter at a lock — or at an `sshd` — would
be worse off than one nothing authenticates as. `*` is not the output of any
hash function, so no password produces it; it is how that file says "not an
account anybody logs in as", and it is what every Debian system account in it
already says. The installer says so on the screen where the password is asked
for, and again on the last screen before the disk is erased.

**The aging fields are empty.** tOS runs no time synchronisation of any kind.
The day number this would otherwise write is whatever the RTC claims, and a
machine that comes up in 1970 would write `0` — which every login program
reads as "this password must be changed before you may come in", on a machine
with no `passwd` command to change it with. Empty means the aging features are
off, which is the truth about a machine with no clock to age anything against.

## What the lock reads

`lock::read_credential` takes a path and an account name. It finds the line
whose first field is that name and takes the second, and then:

| the field | what it means | what the lock does |
|---|---|---|
| `$6$…` | a hash `tos-crypt` can check | lock |
| `*`, `!`, `!$6$…`, empty | no password, or disabled | do not lock, say so |
| `$y$…` and anything else | a password this cannot check | do not lock, say why |

The middle row is the "no credential, no lock" rule from the screen-lock
design, arriving through the new file: the live image is root's session,
Debian's root line is `*`, and so a live session still never locks without the
compositor being told anything about live media. An installed machine whose
owner declined a password lands in the same row.

The last row is not a wrong password and must never be treated as one, because
a lock that went up over a hash it cannot verify is a lock nobody can open.
`$y$` yescrypt is what Debian's own `passwd(1)` writes, so it is the line that
will actually turn up on a machine somebody changed their password on — the
lock refuses to engage and says what it found.

## Whose password: `TOS_USER`

`/etc/shadow` has a line per account, so reading it means naming one. The
compositor runs as root on both images, so its own uid is not the answer: the
live image is root's session and an installed machine is the session of the
person the installer was told about.

The session says which, in `TOS_USER`:

- `iso/live-session` exports `TOS_USER=root`.
- The `/etc/tos-session` the installer writes exports the person's name.
- Nothing set falls back to `root`, whose Debian line is `*` — the safe way
  round, since the other would be a lock over somebody else's password.

It is `TOS_USER` rather than `USER` because the session runs with root's
privileges and the panes in it are root's shells: a `USER` that said otherwise
would be a lie told to every program in every pane, and `git` is only the
first one that would believe it. What has not been decided here is whether a
session should go on being root's at all — that belongs with the login screen
(#112) and with `logind`/`seatd` (#22, #110).

## What this does not settle

- **Nothing is started that could use it.** An installed machine has no
  service manager, so there is no `sshd` running to authenticate anybody
  against the file this now writes. That is #110, and its answer — systemd —
  is independent of this one.
- **There is still no login.** The console is not gated at either end: the
  machine boots into a session without asking, and ending the session drops
  out of it. That is #112, which this is a prerequisite for.
- **Changing a password.** There is no `passwd` on the machine and no tOS
  command for it. Installing Debian's would work from the day this lands, and
  would write a `$y$` line the lock cannot read — so the lock's answer for
  that case is a message, not a refusal to boot.
