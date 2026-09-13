# The unauthenticated ways past a locked screen

Design for [#54](https://github.com/m96-chan/tOS/issues/54).

[`screen-lock.md`](screen-lock.md) decides what a tOS lock is and lists, under
"What the lock does not stop", the things it knows it does not cover. This
document decides those, finds the ones that list missed, and says plainly what
a tOS lock is not for. The rule throughout is the one the issue sets: decide
each door for the installed system, leave the live image as it is.

Everything below was measured on the kernel the ISO actually ships rather than
reasoned from a man page, and every claim names the file it came from so the
next person can check it instead of trusting it.

---

## What the ISO ships

`iso/build.sh` runs `iso/mkiso.sh` inside `rust:1-bookworm` and installs
`linux-image-amd64`. Today that resolves to:

```text
linux-image-amd64          6.1.187-1
linux-image-6.1.0-53-amd64 6.1.187-1
```

and its `/boot/config-6.1.0-53-amd64` says:

```text
CONFIG_MAGIC_SYSRQ=y
CONFIG_MAGIC_SYSRQ_DEFAULT_ENABLE=0x01b6
CONFIG_MAGIC_SYSRQ_SERIAL=y
CONFIG_MAGIC_SYSRQ_SERIAL_SEQUENCE=""
CONFIG_VT=y
CONFIG_SECURITY_LOCKDOWN_LSM=y
CONFIG_LOCK_DOWN_KERNEL_FORCE_NONE=y
```

`0x1b6` against the bits in `include/linux/sysrq.h`:

| Bit | Name | In `0x1b6` | What it carries |
| --- | --- | --- | --- |
| `0x002` | `LOG` | yes | console loglevel, `0`–`9` |
| `0x004` | `KEYBOARD` | **yes** | **`k` (SAK) and `r` (unraw)** |
| `0x008` | `DUMP` | no | `t`, `m`, `w`, `p`, `l`, `c`, `d`, `g`, `z` |
| `0x010` | `SYNC` | yes | `s` |
| `0x020` | `REMOUNT` | yes | `u` |
| `0x040` | `SIGNAL` | no | `e`, `i`, `f` |
| `0x080` | `BOOT` | yes | `b`, `o` |
| `0x100` | `RTNICE` | yes | `n` |

So the accidental policy is not "everything". Debian already withholds the
debugging dumps and the process-killing keys — `Alt+SysRq+e` and `Alt+SysRq+i`
do nothing on this kernel today. What it does allow is exactly the bit that
carries the two keys #54 names. The accident landed on the wrong side of the
one distinction that matters to a lock.

It also means **REISUB is already not a thing on this kernel**: `e` and `i`
need `0x40`, which Debian does not set. The rescue sequence that actually
works here is S-U-B, and it lives in bits this document does not touch.

### The parameter is not spelled `sysrq=`

There is no `sysrq=` boot parameter. `drivers/tty/sysrq.c` registers exactly
one, `sysrq_always_enabled`, which ignores the mask entirely. The mask is the
`kernel.sysrq` sysctl (`kernel/sysctl.c`, `procname = "sysrq"`, handler
`sysrq_sysctl_handler`), and since 5.8 any sysctl can be set from the command
line with the generic `sysctl.*=` form, applied "right before loading the init
process" — which is before `/init` runs, and so before `tos` starts. The token
is therefore:

```text
sysctl.kernel.sysrq=<decimal>
```

Decimal rather than hex. The sysctl parser accepts `0x` and the kernel's own
documentation says so, but decimal is what `/proc/sys/kernel/sysrq` reads back
and it removes a parser detail from the boot path of every tOS machine.

---

## Door 1 — SysRq from the keyboard

**Decision: the installed system boots with `sysctl.kernel.sysrq=434`. The
live image boots with `sysctl.kernel.sysrq=438`, which is the value it has
today, written down.**

`438` is `0x1b6`, Debian's default. `434` is `0x1b2` — the same mask with
`SYSRQ_ENABLE_KEYBOARD` (`0x4`) removed.

### The grab already closes most of this, and that was not obvious

`main.rs:226` grabs every input device with `EVIOCGRAB`. What that does to
SysRq is decided in `drivers/input/input.c`:

```c
/* input_pass_values() */
handle = rcu_dereference(dev->grab);
if (handle) {
        count = input_to_handler(handle, vals, count);
} else {
        list_for_each_entry_rcu(handle, &dev->h_list, d_node)
                if (handle->open) { ... }
}
```

A grabbed device passes its events to the grabbing handle **and to nothing
else**. SysRq is not special-cased there: `drivers/tty/sysrq.c` registers an
ordinary input handler that happens to have a `.filter`, and
`input_register_handle()` puts filters at the head of `dev->h_list` — the list
the grab skips.

So `Alt+SysRq+r` and `Alt+SysRq+k` typed on a keyboard tOS grabbed have never
reached the kernel, on any mask. The issue's first door is narrower than it
looks, and the narrowing is checkable.

### What it does not close

Two keyboards escape the grab, and they are the real door:

- **A keyboard plugged in after `tos` started.** `InputBackend::open_all`
  (`compositor/tos-input/src/evdev.rs:258`) reads `/dev/input` once, at
  startup. There is no inotify watch and no netlink uevent socket, so a USB
  keyboard attached to a locked machine is not grabbed and its SysRq reaches
  `sysrq_filter`.
- **A keyboard `Device::open` or `grab()` failed on.** Both failures are
  non-fatal by design, which is right — one bad device should not cost the
  session its other keyboards — and it means "every device is grabbed" is a
  best effort, not an invariant.

From such a keyboard, ordinary keys are still harmless: `vt.rs:153` puts the
console in `K_OFF`, and `drivers/tty/vt/keyboard.c` drops everything that is
not `KT_SPEC` or `KT_SHIFT` in that mode (line 1521), while `k_spec()` drops
every `KT_SPEC` action except SAK (line 656). Ctrl+Alt+F2 and Ctrl+Alt+Del
from an ungrabbed keyboard do nothing at all. SysRq is the exception, because
the filter runs before the console keyboard handler ever sees the key.

That makes the door exactly one bit wide, and `434` closes it.

### What `434` costs

Kept: loglevel (`0x2`), sync (`0x10`), remount read-only (`0x20`),
reboot and poweroff (`0x80`), RT nice (`0x100`). **S-U-B still works**, which
is the sequence that gets a wedged machine down without losing the filesystem,
and this project runs on real hardware where that matters.

Lost: `k` and `r` — from an ungrabbed keyboard, which is the only place they
ever worked. Concretely, the rescue that gets worse is: the compositor has
wedged, the machine's own keyboard is grabbed by the wedged process and so
cannot send SysRq at all, and you plug in a second keyboard. Before `434` that
keyboard could SAK the console and free the machine. After `434` it can sync,
remount read-only and reboot, and you lose the session instead of rescuing it.

That is the price, it is one scenario, and it is paid only on installed
machines. The live image — the thing you boot when you are rescuing something
— keeps `438`.

### Why `438` and not "leave it alone"

The value is identical to the kernel's own default, so the live image's
behaviour does not change by a single key. Writing it down changes something
else: the next kernel bump cannot move tOS's SysRq policy without somebody
editing a line that says what the policy is.

Someone may later decide the live image should have *more* SysRq than Debian's
default, since it is the rescue image — `0x1fe` would add the debugging dumps
and the process-killing keys. That is a live-image-only change, it is a real
behaviour change, and #54 is not the issue that makes it.

---

## Door 2 — SysRq from the serial line

**Decision: the same mask covers it, and the installed system has no serial
console anyway. `434` is the whole of the fix.**

The grab is irrelevant here: a UART is not an input device. With
`CONFIG_MAGIC_SYSRQ_SERIAL=y` and an empty
`CONFIG_MAGIC_SYSRQ_SERIAL_SEQUENCE`, a BREAK on the console port followed by
a key within five seconds is a SysRq, and `uart_prepare_sysrq_char()` in
`include/linux/serial_core.h` gates it on `sysrq_mask()` — the same number.
So `434` removes `k` and `r` from the serial path too, and does it without a
second mechanism.

The live image is the only tOS configuration with `console=ttyS0` in the first
place. See the next door for why that stays true.

---

## Door 3 — the serial console and `/init`'s emergency shell

**Decision: the installed system boots `console=tty0` with no `console=ttyS0`,
and `/init` execs the emergency shell only when `tos.rescue` is on the kernel
command line. The live image's GRUB entries carry `tos.rescue`; the
installer's do not.**

### The installed system runs the live `/init`

This is the fact the decision turns on, and it is easy to miss. There is no
`switch_root` anywhere in the tree. `tos-install` writes `/etc/inittab` with
`::respawn:/sbin/tos` and an `/etc/rc` beside it, but nothing ever pivots to
the installed root, so those files are written for the Debian rootfs of
[#20](https://github.com/m96-chan/tOS/issues/20) and are not what runs. What
runs on an installed machine is `iso/init`, copied verbatim onto the disk with
the initramfs.

So the live image and the installed system execute the same script, and the
only thing that can tell them apart at runtime is the kernel command line —
which is the one thing they genuinely do not share, because `iso/mkiso.sh`
writes the live `grub.cfg` and `Installer::grub_config` writes the installed
one.

### The two halves of the door

The serial half is already shut on installed machines: `grub_config` emits
`console=tty0` and nothing else. That was an accident of the same kind as the
SysRq default, so it is now a comment, a named constant and a test that fails
if `ttyS0` ever appears there.

It is worth being precise about what that buys. `console=` is a kernel
parameter, and anyone at the GRUB prompt can add one — along with
`init=/bin/sh`, which is faster. Closing this is a statement about the default
configuration, not a boundary against a person standing at the machine.

The other half was open on every machine. `/init` ended with:

```sh
say "tOS: compositor exited ($?), dropping to emergency shell"
exec sh
```

On an installed machine that shell is on `/dev/console`, which is `tty0` —
the screen you were locked out of. Anything that ends the compositor, from a
panic to a SAK on an ungrabbed keyboard, ends with a root prompt on it. That
is a genuine way past a locked screen and it has nothing to do with serial
lines.

`/init` now asks for the shell by name. The live image's command line says
`tos.rescue`; the installed one does not, and `/init` restarts the compositor
instead.

### The honest limit of this one

**Restarting the compositor comes back unlocked.** Door 3 stops a crash from
handing out a root shell. It does not stop a crash from ending the lock,
because a fresh `tos` has no memory of having been locked.

Fixing that belongs to [#50](https://github.com/m96-chan/tOS/issues/50) and is
not decided here. The shape it would take is a marker the compositor writes
when it locks and removes when it unlocks, under `/run` so that it is on a
tmpfs and a reboot clears it — a reboot is not worth defending against here,
because the disk is not encrypted and a reboot reads everything anyway. What
it must not be is a marker that survives a reboot, which is the same
"machine nobody can reach" failure `screen-lock.md` refuses `VT_LOCKSWITCH`
for.

### How an installed machine is rescued now

Press `e` at the GRUB menu and add `tos.rescue`. That path is open, and Door 7
explains why leaving it open is deliberate: an installed tOS machine that will
not start its compositor is rescued through the same prompt that is already
the shortest way into an unencrypted disk, so the rescue costs nothing that
was not already spent.

The infinite restart is not free either. A machine whose compositor exits
immediately — no DRM device, say — now prints a line and retries once a second
forever instead of stopping at a shell. It says on its first restart that
`tos.rescue` is what it wants, which is as much as a script with no keyboard
of its own can do.

---

## The doors #54 did not name

### Door 4 — a second VT with a getty

**Decision: none is added, and none should be added before tOS has a login.**

There is no `getty` anywhere in the tree, on either configuration. The
installed `/etc/inittab` starts the compositor and nothing else — and, per
Door 3, is not even read yet.

A getty is worth naming because it is the obvious next thing somebody adds
when an installed machine is hard to debug, and it would be a complete bypass:
Ctrl+Alt+F2, a login prompt, and tOS has no credential for a login to check.
`/etc/passwd` had `*` in the password field and there was no `/etc/shadow`
behind it, so depending on the `login` implementation that was either "no
password accepted" or "any password accepted", and neither is a door you want
beside a lock. **#111 answered the credential half of this**: the password is
in `/etc/shadow` now, so a `login` on another VT would have something real to
check. The rule below stands anyway — what a getty needs is a decision about
logins, which is #112, and not merely a file to read.

The rule: **a getty is a login prompt, and tOS has no login.** Adding one
means answering the credential question for logins, not only for the lock,
and it makes [#47](https://github.com/m96-chan/tOS/issues/47) load-bearing
rather than tidy — the lock's `VT_RELDISP 0` refusal becomes the only thing
between a getty and a locked session, and the `VT_LOCKSWITCH` question of
[#53](https://github.com/m96-chan/tOS/issues/53) stops being optional.

### Door 5 — who can read `/dev/tty0` and the input devices

**Decision: nothing to change, because there is nobody to keep out. This
changes the day tOS has a second uid, and that day is #20.**

The nodes are created by devtmpfs, which makes everything root-owned and mode
0600 unless the subsystem widens it (`drivers/base/devtmpfs.c`: `if (req.mode
== 0) req.mode = 0600`). `input_devnode()` sets a name and no mode, so
`/dev/input/event*` are 0600 root:root. `tty_devnode()` widens only `/dev/tty`
and `/dev/ptmx` to 0666, so `/dev/tty0` is 0600 root:root. There is no udev in
the initramfs to relax any of it.

But the modes are not what is holding this door, because tOS has exactly one
uid. Every pane, every program a user starts and the compositor itself are
root. A keylogger on `/dev/input/event0` is available to anything already
running in the session no matter what the mode says.

What does exist today is narrower and worth keeping: `Device::open` passes
`O_CLOEXEC` (`evdev.rs:108`, with a test that reads its own source to make
sure the flag stays), so a program started in a pane does not *inherit* a
keyboard descriptor. It has to go and open one, which it is permitted to do.

When #20 gives the installed system a real Debian userspace and the user stops
being root, these modes become the mechanism rather than a formality, and
`/dev/input` will need a policy at that point. That is #20's decision to make
with udev in front of it, not this one's to guess at.

### Door 6 — `VT_ACTIVATE` from another process

**Decision: already answered by #47 and #53; nothing new here.**

`VT_ACTIVATE` goes through `set_console()`, which with the VT in
`VT_PROCESS` mode asks the owning process first — so a locked `tos` that
refuses with `VT_RELDISP 0` refuses this exactly as it refuses Ctrl+Alt+F2.
`VT_LOCKSWITCH` would make the ioctl fail outright, which is the stronger
option #53 exists to measure.

Either way the caller needs `CAP_SYS_TTY_CONFIG`, which on tOS means it is
root, which means it is already inside. This is not a way past a lock for
somebody standing at the machine; it is one more thing that code you already
ran can do.

### Door 7 — editing the kernel command line at the GRUB prompt

**Decision: no GRUB password, on either configuration. The condition under
which that flips is written down here so it does not have to be rediscovered.**

Press `e` at the menu, append `init=/bin/sh`, boot. Root on the installed
system, no password asked, the lock never loaded. The installed menu's
`timeout=2` is the only friction and it is not friction.

GRUB can close this with `set superusers` and `password_pbkdf2`. It should not
yet, for two reasons that have to hold together:

- **It buys nothing while the disk is unencrypted.** The same person, at the
  same machine, boots a USB stick and reads every file. A GRUB password moves
  the cost of that attack from twenty seconds to two minutes and changes
  nothing about the outcome. It looks like a boundary and is not one, which is
  worse than an admitted hole.
- **It is a lockout waiting to happen.** A forgotten GRUB password on a
  machine whose compositor will not start is a machine that needs its disk
  taken out. Per Door 3 the GRUB prompt is now the documented rescue path, so
  a password on it takes away the rescue and the attack together.

The condition to revisit: **when tOS encrypts the disk and the firmware can be
told to refuse other boot media.** At that point a GRUB password is the last
gap rather than a decorative one, and it is worth its cost. Until then it is
not.

And on the live image a GRUB password would be absurd: the image is
downloadable, so the password is published with it.

### Door 8 — physical access generally

**Decision: out of scope, and that has to be said in the same voice as the
decisions rather than buried.**

Reboot, removable media, a screwdriver, DMA over a port that allows it — all
of them read everything on the machine, and nothing in this document or in
`screen-lock.md` makes any of them harder. The disk is plain ext4 with no
encryption anywhere in the tree.

Disk encryption is a different feature with a different shape (a passphrase at
boot, a key in the initramfs, a rescue story of its own). It is not implied by
a screen lock and a screen lock is not a down payment on it.

### Door 9 — `/proc/sysrq-trigger`, and everything else that is already root

**Decision: nothing to change, and the mask must not be mistaken for a
sandbox.**

`Documentation/admin-guide/sysrq.rst` is explicit: the `kernel.sysrq` value
"influences only the invocation via a keyboard. Invocation of any operation
via `/proc/sysrq-trigger` is always allowed (by a user with admin
privileges)." So `echo k > /proc/sysrq-trigger` from a pane SAKs the console
whatever `434` says.

That is not a door past the lock. It is a program that is already inside the
session doing something a program inside the session can do — as it could by
sending `tos` a signal, or reading `/dev/fb0`, or opening a keyboard. A lock
is a boundary in time against somebody who walks up to a running machine. It
is not a boundary against code that ran while the session was open.

---

## Why the live image is different

One sentence, and it is checkable: **the live image has no credential, so by
`screen-lock.md`'s "no credential, no lock" rule it never locks — there is no
locked session on it for any of these doors to lead into.**

Everything else follows. A serial root shell on an image that cannot lock is a
feature of a rescue and install medium, not a bypass. SysRq on an image whose
whole purpose is to be booted at a machine that is already broken is a tool.
The live image is not a machine somebody leaves unattended; it is a machine
somebody is standing at on purpose.

---

## What a tOS lock does not defend against

Stated once, plainly, so that nobody has to infer it:

1. **Anyone who can reboot the machine.** The disk is not encrypted and the
   GRUB prompt takes `init=/bin/sh`. This is the big one and no amount of
   compositor work touches it.
2. **Anyone with removable media or a screwdriver.**
3. **Anything already running in the session.** Every process on a tOS machine
   is root. The lock keeps out a person at the keyboard, not a program that
   the person at the keyboard started an hour ago.
4. **A compositor crash.** It no longer ends in a root shell on an installed
   machine, but it still ends the lock, and that is #50's to fix.
5. **The nested and headless backends**, which have no VT and no DRM master to
   defend — as `screen-lock.md` already says.

What it does defend against, and what all of the above is in service of: a
person who walks up to a running, unattended tOS session and does not want to
reboot it.

---

## What changed and what only got written down

Changed:

| Where | Change |
| --- | --- |
| `iso/mkiso.sh` | live command line gains `sysctl.kernel.sysrq=438` and `tos.rescue` — the same behaviour it has today, said out loud |
| `iso/init` | the emergency shell needs `tos.rescue` on the command line; without it the compositor restarts |
| `installer/src/install.rs` | installed command line gains `sysctl.kernel.sysrq=434`; `console=tty0` with no serial console is now a named constant with a test |

Written down and deliberately not done:

- No GRUB password, on either configuration (Door 7), with the condition that
  would change that.
- No `VT_LOCKSWITCH`, which is still #53's experiment (Door 6).
- No getty, and a rule for anyone who wants one (Door 4).
- No change to device node modes, until there is a second uid (Door 5).
- No disk encryption, which is a different feature (Door 8).
- No lock state that survives a compositor restart, which is #50's (Door 3).
- No wider SysRq mask on the live image, which would be a real behaviour
  change and is not this issue's to make (Door 1).
