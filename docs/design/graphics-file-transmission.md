# Opening a file because a program asked

Design for [#68](https://github.com/m96-chan/tOS/issues/68).

The kitty graphics protocol has four transmission media. One of them, `t=d`,
puts the picture in the escape sequence as base64. The other three hand over a
name instead: `t=f` a path to read, `t=t` a path to read and then delete, and
`t=s` a POSIX shared memory object. tOS parsed all four and stored only the
first, so the PNG decoder that went in for #13 bought nothing for the programs
it was written for — a file manager showing a preview sends a path, because
base64-ing four megabytes of photograph through a PTY is not a thing anyone
does twice.

Making the other three work is a short piece of code. Deciding what it is
allowed to do is not, and that is what this document is about. A program in a
pane sending `ESC_Gt=f,f=100,i=1;L2V0Yy9zaGFkb3c=` is asking the compositor to
open a path of the program's choosing, and `t=t` is asking it to unlink one.
tOS is not a terminal emulator running as somebody's desktop application: it
is PID 1's child, it holds the machine, and in a booted session it is root. So
the question is not what a picture is. It is what a process that owns the
machine will do on the say-so of a process that does not.

---

## The fact that decides half of this

`iso/init` mounts the filesystems, exports `HOME=/root`, and runs `/sbin/tos`
in a loop. There is no login, no `su`, and nothing anywhere that changes a
credential: `child_setup` (`compositor/tos-pty/src/lib.rs:352`), which is
everything a pane's child does between `fork` and `execve`, opens a session,
takes a controlling terminal, redirects three descriptors, chdirs, and resets
signals. It does not call `setuid`.

**So today every program in a pane already runs as the same user as the
compositor, which is root.**

That is not an accident to be worked around; it is what tOS currently is, and
it settles a question that would otherwise be the hard one. A rule that stops
the compositor reading `/etc/shadow` on a program's behalf protects nothing
while the program can read `/etc/shadow` itself, in one line of shell, in the
pane it is already sitting in. Confinement of *reads* would be security
theatre with a real cost: it would break the one use the feature has.

Deleting is not symmetrical, and that asymmetry runs through everything below.
`/tmp` is a tmpfs (`iso/init:26`) and tmpfs mounts `1777` — sticky, so an
unprivileged user cannot unlink another's file there. tOS as root ignores the
sticky bit. A compositor that unlinks whatever it is told to unlink is a
deputy that performs deletions its caller could not perform itself, and that
is true no matter who the caller is.

**The rule that falls out: limits on reading exist to stop the session
hanging or running out of memory. Limits on deleting exist because deleting
is something tOS can do that the asker cannot.**

---

## Where the policy lives, and why not in the terminal

`tos-term` says of itself that it "has no dependencies and no knowledge of
display hardware, PTYs or input devices, which is what makes it testable on
any host while the rest of tOS targets Linux directly"
(`compositor/tos-term/src/lib.rs:5`). It has no `libc`, and the whole
workspace's `std::fs` calls live in other crates. `GraphicsStore` is the
purest thing in it: it is handed byte slices, it reads no clock —
`advance_animations` is *told* what time it is, which is what lets a test walk
an animation frame by frame — and a test drives every path through it with a
literal.

Putting an `open(2)` in `GraphicsStore::store` would end all of that for the
sake of two call sites.

**Decision: the terminal declares a seam, and the compositor — the part of
tOS that owns the machine — implements it.**

- `compositor/tos-term/src/medium.rs` defines `trait MediumReader`, one
  method: given a medium, a name and a ceiling, produce bytes or a protocol
  error. It knows nothing about files.
- `compositor/tos-compositor/src/imagefile.rs` implements it as `ImageFiles`,
  which is where every rule in this document is.
- `Pane::spawn` installs it (`compositor/tos-compositor/src/pane.rs:86`). A
  `Terminal` built anywhere else — the chrome, a test, the installer's session
  harness — gets the default, `NoMedia`, which refuses with
  `ENOSUP:this terminal cannot read files`. A terminal model that opened
  whatever path arrived down a pipe would be a surprise to every test that
  drives one.

This is the shape the rest of the tree already uses for the same reason:
`exec::Backend` in the installer, `net::Kernel` and `power::PowerBackend` in
`tos-system`. Within `ImageFiles` the directories are injected as roots, which
is the `Sysfs::new(root)` idiom — a test needs somewhere it can make a fifo
and a dangling symlink that is not the developer's `/tmp`.

Two alternatives were rejected. Passing a resolver as a new argument to
`store` and `store_frame` would put a filesystem in the signature of a pure
store, for two callers. Letting the store take the path and calling back would
be the same thing with more steps.

One consequence is worth stating because it looks like sleight of hand.
`Terminal::handle_graphics` resolves a named transfer *after* reassembling its
chunks and then records `medium = Direct` on the command before handing it to
the store. That is not hiding where the bytes came from; it is saying that a
transfer whose bytes are in hand is a direct one. It is also what lets the
store's own guards (`graphics.rs:608` and `:823`) go on refusing an
unresolved medium, which is now an internal invariant — a payload that still
names a medium is a path, and storing it as pixels would be a bug.

---

## Decision 1 — a read is not confined to a list of directories

**Decision: `t=f` opens any absolute path. The limits on it are about
blocking and about memory, not about secrecy.**

The argument is the one above: a pane's program can already open anything the
compositor can. What a path list would buy is the difference between a program
reading a file and a program asking tOS to read it, and today there is none.

What it would cost is the feature. The paths a file manager hands over are the
paths it is showing — `/root/photos/`, a mounted stick under `/run`, anywhere.
A list that covered those is a list that covers everything, and a list that
did not is a preview pane that stays blank.

A relative path is refused (`EINVAL:the path must be absolute`). It would
resolve against the compositor's working directory, which is wherever `/init`
happened to start it. No program means that.

### What would change this

The day tOS gives panes their own uid, this decision is wrong, and the fix is
*not* a list of directories. It is to open the file as the user who asked —
`setfsuid(2)` around the open, or handing the work to a process that already
has those credentials — so that the answer to "may this be read" is the
kernel's, computed against the asker, rather than a table in the compositor
that has to be kept in step with the filesystem. That is a follow-up issue,
and it is blocked on there being a second user to have.

---

## Decision 2 — the open cannot block, and the type is checked on the descriptor

**Decision: every open is `O_RDONLY | O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC`,
followed by `fstat` on the descriptor, and anything that is not `S_IFREG` is
refused.**

`O_NONBLOCK` is the one that matters, and it is not an optimisation. Opening a
fifo for reading waits for a writer. A program that creates a fifo, names it
in a `t=f`, and never writes to it has stopped the compositor inside
`open(2)` — not the pane, the compositor: graphics commands are handled
synchronously from the parser, in the same call that is feeding every other
pane's output. That is a denial of service on the whole session, from one
escape sequence, with no memory and no CPU spent. With `O_NONBLOCK` the open
returns immediately and `fstat` gets to say no.

The type check is on the descriptor and not on the path, because a check made
against a name and then acted on through a second lookup of the same name is a
race the sender gets to win by renaming in between. `fstat` on the descriptor
describes the file that is actually open.

Regular files only. A character device would be `/dev/tty0` or the session's
own input; a block device would be the disk, where "the image budget" means
reading 256 MiB of somebody's root filesystem into a texture. Neither is a
picture, and the protocol has a way to say so.

`O_CLOEXEC` is the standing rule in this tree, for the reason written at
`compositor/tos-input/src/evdev.rs:107`: without it, every process started
afterwards inherits the descriptor.

---

## Decision 3 — an empty file is refused, and that is how `/proc` is refused

**Decision: `st_size` must be greater than zero.**

The type check is not enough, and the file that proves it is `/proc/kmsg`. It
is a regular file. `fstat` says `S_IFREG`. Reading it blocks until the kernel
logs something, which is the fifo attack again wearing a different hat, and
`O_NONBLOCK` does not save the *read* the way it saved the open.

Files under `/proc` report a length of zero, because their contents do not
exist until they are read:

```text
$ stat -c '%s %n' /proc/kmsg /proc/cpuinfo /proc/self/maps /proc/kcore
0 /proc/kmsg
0 /proc/cpuinfo
0 /proc/self/maps
140737471594496 /proc/kcore
```

And no image is zero bytes long. One check disposes of the whole tree, and it
is honest on its own terms rather than being a special case for `/proc`: a
file with nothing in it cannot be a picture. The one file there that does
claim a length claims 128 TiB of it, and Decision 4 refuses that without
reading a byte.

### The honest limit

This is not a general defence against a slow or hanging read. `/sys`
attributes report 4096 bytes and can block in a driver; a file on a network
filesystem that has gone away will block for as long as the mount's timeout;
and tOS reads all of it inside the parse loop. tOS has no network filesystems
and no `/sys` file that is also a PNG, so the exposure today is theoretical.
The general answer is a deadline on the read, or doing it off the loop
entirely, and that is a follow-up rather than something to half-do here.

---

## Decision 4 — the size cap is the store's budget, applied before the read

**Decision: `st_size` is compared against the graphics byte budget on the
descriptor, before a byte is copied. Over it, the transfer is refused with
`EINVAL:file exceeds the image budget`.**

The budget is the right ceiling because it is already the ceiling everywhere
else. `decode_payload` passes it to the inflater and to the PNG decoder so
that a decompression bomb is refused while it is being decoded rather than
after it has been materialised (`graphics.rs:1082`), and an image that would
not fit in the store could never have been kept anyway.

Doing it from `st_size` rather than while reading is the same argument one
level up: a file that cannot be kept is not worth the time it takes to copy
in.

It is worth noticing what this changes. An inline payload is capped at 16 MiB
by the APC buffer (`compositor/tos-term/src/term.rs:1739`), so before this
change no single command could ever ask the terminal to hold more than that at
once. A file can, up to the whole budget, which defaults to 256 MiB. That is
intended — the point of the feature is the picture too big to send inline —
but it means a `t=f` is also the largest synchronous read the compositor
does. On tmpfs, where temporary files and shared memory objects live, that is
a memcpy. On a spinning disk it is not, and the cap is as much a latency bound
as a memory one. Reading off the parse loop would fix that properly; it is
listed at the end.

---

## Decision 5 — the named component is never a symbolic link

**Decision: `O_NOFOLLOW`, and `ELOOP` is reported as
`EBADF:the path is a symbolic link` rather than folded into "cannot open".**

Being honest about what this is worth: while reads are unconfined, a symbolic
link buys a sender nothing it could not have by naming the target directly. It
is load-bearing in two other places. It is load-bearing for `t=t`, where it is
the difference between unlinking a name in a temporary directory and unlinking
whatever that name points at. And it is load-bearing on the day Decision 1 is
revisited, when a link would otherwise be the way around whatever replaces it.

The specific error exists because a client can do something about it: a file
manager that handed over a symlink can hand over the target instead, which it
cannot work out from `EBADF:cannot open the file`.

Intermediate components are a different matter. For `t=f` they are not
checked, and while reads are unconfined that cannot matter — a link in the
middle of a path reaches a file the sender could have named. For `t=t` there
are no intermediate components at all, by construction, which is the next
decision.

---

## Decision 6 — a delete is one name, in a directory tOS chose

**Decision: `t=t` accepts exactly `<root>/<one component>`, where `<root>` is
`/tmp`, `/var/tmp` or `/dev/shm`. The directory is opened once; the file is
opened, read and unlinked through that descriptor; and the unlink happens only
if the name still refers to the file that was read. Anything else is refused
whole, before anything is opened.**

Four things in that, each of which had an alternative.

**Why a root list at all.** Everything a program is entitled to have tOS
delete is a temporary file, and these are the three places on this system
where temporary files live. `$TMPDIR` is deliberately not consulted: it comes
from the environment of the program doing the asking, and the list of
directories the compositor is willing to unlink inside is exactly the wrong
thing to let the asker choose. kitty confines `t=t` for the same reason,
though it is not running as root when it does.

**Why one component and not a subtree.** A path like
`/tmp/preview/../../etc/passwd` is dealt with by spelling alone — its parent
directory is not spelled like any root — but `/tmp/preview/x` where `preview`
is a symlink to `/etc` is not, and `O_NOFOLLOW` on the last component does
nothing about a link in the middle. The choices were a component-by-component
`openat` walk, `openat2(RESOLVE_BENEATH)`, or requiring that there be no
middle. The last is the smallest and the easiest to state, and it costs
nothing real: `mkstemp` puts its file directly in the temporary directory,
which is what every client that uses `t=t` does.

**Why the directory descriptor.** Holding `/tmp` open and doing both the open
and the unlink through it is what makes "one name inside a directory tOS
chose" true rather than merely spelled. The name that was read and the name
that is unlinked are the same name in the same directory, whatever anyone
renames the directory to in between.

**Why the inode check.** Between the open and the unlink, the sender can
replace the name. tOS is root, so the sticky bit that stops one program
deleting another's file in `/tmp` does not stop tOS from doing it for them:
that is the deputy problem in its sharpest form, and it is a real escalation,
not a theoretical one. Before unlinking, `fstatat(dirfd, name,
AT_SYMLINK_NOFOLLOW)` is compared against the `fstat` of the descriptor that
was actually read. Different device or inode, and the name is somebody else's
file, and it stays where it is.

When that happens the transfer still succeeds. The picture arrived; the only
thing lost is a cleanup that was no longer tOS's to do, and failing the
command would throw away a good image to report a file the sender has already
lost track of.

**Why a refusal and not a quiet non-delete.** kitty, faced with a `t=t`
outside a temporary directory, reads the file and declines to delete it. tOS
refuses the command. The sender of a `t=t` believes the file is gone
afterwards; letting it believe that wrongly is how a temporary file holding a
photograph stays on the disk for ever, and the sender has no way to find out.
An error it can see is better than a leak it cannot.

---

## Decision 7 — `t=s` is implemented, and it is implemented as a file

**Decision: `t=s` works. The name is a single POSIX shared memory name, it is
read from `/dev/shm`, and the object is unlinked afterwards. There is no
`shm_open` and no `mmap`.**

The issue allowed a documented refusal. There was nothing to refuse: on Linux
POSIX shared memory *is* a tmpfs mounted at `/dev/shm`, and `shm_open(3)` is
`open(2)` on a name under it. That is not an implementation detail glibc
happens to have chosen; it is the ABI. `iso/init:25` mounts it, so the
directory exists in every tOS session. Treating `t=s` as "one name in a
directory tOS chose" makes it the same code as `t=t`, with the same type
check, the same size cap, the same `O_NOFOLLOW` and the same inode-verified
unlink — and one policy that covers three media is worth more than a fourth
code path.

Not using `mmap` is a decision and not laziness. A mapping of a file another
process can still `ftruncate` delivers `SIGBUS` when the compositor touches a
page past the new end. A signal with no handler is the session gone. `read(2)`
on the same shrinking file returns fewer bytes, which is a decode error, which
is an error message. There is no version of the speed argument that is worth
that trade: the data is on tmpfs and the copy is a memcpy.

The object is unlinked because `t=s`, like `t=t`, is a transfer that consumes
what it read; a client that sends it and then has to clean up after the
terminal is a client leaking pages of tmpfs on every preview.

Names are validated as POSIX spells them — one optional leading slash, no
other slash, and not `.` or `..` — which is the same single component the
directory holds.

**`O=` and `S=`.** kitty lets `t=f`, `t=t` and `t=s` carry `O=` (an offset into
the file) and `S=` (how many bytes to read), which is how a client packs more
than one picture into one shared memory object, or points at a picture inside
an object rounded up to a page. Both are honoured, in
`GraphicsCommand::slice_named_payload`, and honouring them is not a nicety: a
terminal that ignored `O=` would answer `OK` over the wrong pixels rather than
refusing anything, because raw pixel formats take the first `s*v*stride` bytes
of whatever they are handed and would find them at the wrong place.

They are applied after the read rather than by seeking, because the whole
object was going to be read anyway — it is bounded by the store's budget
either way — and because it keeps `MediumReader` a plain "read what this
name names" rather than a file API with an offset in it. An `O=` past the end
of the file is refused with `EINVAL:the offset is past the end of the file`:
the sender named bytes that are not there, and it is owed the error rather
than a success over an empty picture.

---

## When it is not an image

Nothing new happens, which is the point. The bytes go to `decode_payload` like
any other payload and come back as `EINVAL:not a PNG`,
`EINVAL:corrupt PNG`, `EINVAL:truncated payload` or
`EINVAL:unsupported PNG variant`. The module has said since it was written
that anything undecodable is answered with the protocol's error response
rather than silently dropped, so applications can fall back instead of
hanging, and a file is not an exception to that.

---

## Animation frames get this for free

`a=f` is handled by the same arm of `handle_graphics` as `a=t` and `a=T`: the
chunks are reassembled, the medium is resolved, and only then does the command
go to `store_frame` rather than `store`. So a frame can name a file under
exactly the rules above, with the same cap and the same refusals, and there is
no second copy of this policy to keep in step. What a frame may *contain* is a
separate question — frames still take raw pixels only — and that belongs to
[#69](https://github.com/m96-chan/tOS/issues/69).

## The errors, and why they are not all the same

A response body is the only thing tOS can tell the program that asked; there
is no log it reads and no dialogue box. So the distinctions that a client can
act on are kept, and the ones it cannot are not.

| Response | Means | What a client can do |
| --- | --- | --- |
| `ENOSUP:this terminal cannot read files` | no reader is installed | send the bytes inline instead |
| `EBADF:no such file` | `ENOENT` on the open | check the path it built |
| `EBADF:the path is a symbolic link` | `O_NOFOLLOW` refused it | send the target instead |
| `EBADF:cannot open the file` | anything else from `open` | give up on this path |
| `EINVAL:the path must be absolute` | a relative path | make it absolute |
| `EINVAL:not a regular file` | fifo, device, directory | it was never a picture |
| `EINVAL:the file is empty` | zero length, or truncated away | retry after writing it |
| `EINVAL:file exceeds the image budget` | bigger than the whole store | send a smaller one |
| `EINVAL:a temporary file must be one name in a temporary directory` | `t=t` outside the roots | move it to `/tmp` |
| `EINVAL:a shared memory name is one component with no slashes in it` | a malformed `t=s` name | spell it `/name` |

`ENOSUP` against `EBADF` against `EINVAL` is the distinction that matters:
nothing is wrong with the file in the first, the file is the problem in the
second, and the request is the problem in the third.

## A query does not open anything

`a=q` still answers `OK` without touching the filesystem, including when it
carries `t=t`. A query is defined as the command that stores nothing and only
proves support; one that deleted a temporary file would be the single command
in the protocol with a side effect nobody asked for. A client that wants to
know whether a `t=f` will work can send one and read the answer.

---

## What this does not defend against

In the same voice the lock documents use, because a limit nobody wrote down is
a limit nobody has.

- **A program in a pane doing any of it directly.** It is root. It can read
  the file, delete the file, and mount the disk. Nothing here is a boundary
  against that, and nothing here should be read as one.
- **Learning whether a path exists.** A program can tell `EBADF:no such file`
  from `EINVAL:not a regular file` from a picture, so it can probe the
  filesystem through the compositor. It can also just call `stat`.
- **A slow read.** Decision 4's cap bounds the bytes, not the seconds.
- **The last of the delete race.** `fstatat` and `unlinkat` are still two
  syscalls, and a rename between them still wins. Closing it completely would
  need an unlink-by-inode that Linux does not have. What the check does
  guarantee is that tOS never unlinks a name it did not just read
  successfully — the attack goes from "swap in any path at any time" to
  "swap in, inside a directory tOS chose, in the window between two adjacent
  syscalls".
- **A file that is a picture and is also secret.** If a program names
  `/root/secret.png`, tOS displays it. It would also have displayed it if the
  program had read it and sent it inline.

---

## What landed

- `compositor/tos-term/src/medium.rs` — `trait MediumReader`, and `NoMedia`,
  the reader a terminal has until it is given one.
- `compositor/tos-term/src/term.rs` — a named transfer is resolved after its
  chunks are reassembled and before the store sees it.
- `compositor/tos-compositor/src/imagefile.rs` — `ImageFiles`, every rule
  above, and the tests for them: a fifo refused on another thread so that
  losing `O_NONBLOCK` is a failed test rather than a hung one, a symlink
  refused both ways, a temporary file read and deleted, one outside the roots
  refused with the file still there, `/dev/null` refused, an empty file
  refused, an oversized file refused, a shared memory object read and
  unlinked, and a text file answered with `EINVAL:not a PNG` through a whole
  `Terminal`.
- `compositor/tos-compositor/src/pane.rs` — the one place a terminal is given
  a reader.

## Worth their own issues

1. **Read off the parse loop.** Everything in Decisions 3 and 4 that is a
   latency argument rather than a memory one wants this, and it is the only
   real answer to a file on a filesystem that has stopped answering.
2. **Open as the asker.** Blocked on panes having a uid of their own; the
   right shape for Decision 1 the moment they do.
