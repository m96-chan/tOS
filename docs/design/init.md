# What is PID 1 on a tOS machine

**Issue #110.** Decided: **systemd**, on the live image and on every machine
installed from it.

## What it was

Three lines of `/etc/inittab`, written by the installer:

```text
::sysinit:/etc/rc
::respawn:/etc/tos-session
::ctrlaltdel:/sbin/reboot
```

and `/etc/rc`, a fixed script that mounted `proc`, `sysfs`, `devtmpfs`,
`devpts`, `shm` and two `tmpfs`, remounted `/` writable and set the hostname.
`/sbin/init` was a symlink to busybox. That is enough for exactly one program,
which is what tOS had.

The live image did not even have that: `/init` execed `/sbin/tos-session`, a
`while :; do /sbin/tos; done` loop in `/bin/sh` with no signal traps — which is
what made #104, a PID 1 the kernel delivers nothing to.

## Why it had to change

tOS is not an appliance. It runs several terminals, it wants background
execution that outlives the compositor (#6), and the thing it is trying to be
is a machine somebody works on. A machine somebody works on accumulates things
that have to be running.

The concrete cost was `apt install openssh-server`, which works now that the
machine has a network and a keyring: the package installs, its `postinst` calls
`invoke-rc.d` and finds nobody to talk to, and nothing starts it at the next
boot either. The same is true of every daemon in Debian.

## Why systemd rather than the alternatives

Measured in a bookworm container, `--no-install-recommends`, with the
`tos-minimal` dpkg excludes, on top of the package set the image already had:

| | packages | bytes unpacked |
|---|---|---|
| `sysvinit-core` | 5 | 369,958 |
| `systemd-sysv` | 8 | 13,498,993 |
| `openssh-server` | 11 | 8,488,625 |

For scale: `iproute2` and `procps` already cost 18 packages and 13,445,033
bytes, for `ip`, `ps` and `free`. **systemd costs about what those three
commands already cost**, which is worth saying plainly so nobody re-litigates
it from a guess. Whatever the argument against it is, it is not the size.

What is bought is that a Debian package which ships a unit works when it is
installed, with nothing written on tOS's side. `sysvinit-core` is five packages
and buys a shrinking fraction of that — newer packages increasingly ship a unit
and no init script — which is a bet against the direction Debian is going.
`runit` and `s6` are real supervision with no Debian integration at all: every
service gets a run script written here, which is tOS owning a service story it
has no reason to own. Keeping busybox init costs nothing and is honest while
there is one program to run; the moment there are two, `/etc/rc` starts growing,
and what it grows into is sysvinit reimplemented one symptom at a time.

The reasoning that settled it, from the maintainer: if going through busybox
makes the ordinary tools awkward to handle, systemd is the better trade.

## What that means on the image

`iso/mkiso.sh` puts `systemd-sysv` in the rootfs, which is what makes
`/sbin/init` systemd and what brings `reboot`, `halt` and `poweroff` with it.
The busybox symlinks that used to provide all four are gone.

The session is a unit the image carries:

```ini
# /etc/systemd/system/tos-session.service
[Unit]
Description=tOS session
Conflicts=getty@tty1.service
After=systemd-user-sessions.service getty@tty1.service

[Service]
ExecStart=/sbin/tos-session
Restart=always
RestartSec=1
StandardInput=tty
TTYPath=/dev/tty1
TTYReset=yes
TTYVHangup=yes
UtmpIdentifier=tty1

[Install]
WantedBy=multi-user.target
```

In `/etc` rather than `/lib/systemd/system` because tOS writes it by hand and
dpkg owns the other directory. Enabled by the symlink `systemctl enable` would
make, because a rootfs being assembled in a container has nothing to ask.
`default.target` is set to `multi-user.target`: Debian's own default is
`graphical.target`, which is a target tOS has nothing under.

`Restart=always` is the restart loop that used to be inside `iso/live-session`
and the `::respawn:` line that used to be in the inittab — one mechanism, in a
place where `systemctl status` can say how often it has fired. The script keeps
its loop for the one case with no init above it: the rescue session out of the
initramfs, where it really is PID 1. It tells the two apart by asking whether
it is.

### tty1 and the gettys

`getty@tty1` would draw over the compositor, and systemd's generator adds a
serial getty for every `console=` on the kernel command line — a login prompt
on a line anybody with the cable can reach. `getty.target`, `getty@.service`,
`serial-getty@.service` and `autovt@.service` are all masked in the rootfs.

That is `docs/design/lock-other-doors.md`'s Door 4 rule — *a getty is a login
prompt, and tOS has no login* — written somewhere a boot obeys it rather than
only somewhere a person can read it. It is also the thing #112 will revisit,
because what it is really saying is that the login boundary is the compositor's
to draw.

### The live image is the same machine

`/init` execs `/sbin/init` on both roots now, falling back to
`/sbin/tos-session` only for a rootfs built before this. The two images were
answering "what is PID 1 here" differently, and the live one's answer was the
`while :` loop that made #104; one answer is worth more than any particular
answer.

## What the installer stopped writing

`/etc/inittab`, `/etc/rc` and `/etc/tos-session` are gone — not carried
alongside an init that does all three jobs. systemd mounts `/proc`, `/sys`,
`/dev`, `/run` and `/tmp`, remounts the root from `/etc/fstab`, and sets the
hostname from `/etc/hostname`, which is every line `/etc/rc` had.

`/etc/fstab` lost three lines for the same reason: `proc`, `sysfs` and
`devpts` were in it because `/etc/rc` mounted them by hand and that file was
the only place saying so. As fstab entries they become mount units over
mountpoints systemd has already covered, which it carries out — a second mount
on each, and a line on the console about it. The root filesystem and the ESP
are what is left, which is what an fstab is for.

What is left is one drop-in:

```ini
# /etc/systemd/system/tos-session.service.d/user.conf
[Service]
Environment=TOS_USER=<the person>
```

The unit is the image's and this line is the machine's. A unit rewritten by the
installer would be a unit that stops improving the day a new image is installed
over it.

That also ends a duplication `installer/src/install.rs` had carried since the
rootfs arrived: the session environment was written out a second time there,
because busybox init could not run `iso/live-session`, and the two copies
drifted by a `TERM` and a `PATH` before a test was written to compare them.
There is one copy now.

## What this does not settle

- **DRM master and the seat.** tOS still takes the VT, DRM master and the evdev
  devices itself. `logind` is on the machine now and could arbitrate that, or
  `seatd` could without systemd; that is #22 and it is not decided here.
- **The login boundary.** A session is still entered without anybody being
  asked and can still be fallen out of. That is #112, which depends on this and
  on #111 but not on how either was done.
- **Shutting down cleanly.** Half of this is decided now; half is not.

  What is decided is the words a person types. `shutdown -h now` in a pane
  could not turn an installed machine off at all — a pane runs as the person
  since #119, `/sbin/shutdown` is `systemd-sysv`'s symlink to `systemctl`, and
  neither way to PID 1 is open to anybody but root on this image: no D-Bus, so
  no `logind` to ask and no polkit to ask it, and `/run/systemd/private` is
  `srwx------ root root`. The command said `Failed to connect to bus`, exited
  1, and the machine stayed up, while the same command on the live image
  worked because that session happens to be root. #158 answered it with two
  small things rather than a daemon: `/usr/local/sbin/shutdown`
  (`iso/shutdown`, with `poweroff`, `reboot` and `halt` beside it), which is
  first on the PATH `iso/live-session` exports and hands the real program to
  `sudo -n`; and a line in the installer's `/etc/sudoers.d` drop-in letting
  exactly those four through without a password, which gives away nothing the
  power menu and the power button do not already give. The rescue session has
  no init to reach at all and busybox has no `shutdown` applet, so the same
  file maps `-h` to `poweroff` and `-r` to `reboot` there. Shipping D-Bus and
  polkit — what every other systemd machine does, and what would make these
  programs behave as their manual pages say — was turned down for now: it
  needs a `logind` session tOS does not create, and the seat it would
  arbitrate is #22's question as much as this one.

  What is still open is the other half: the compositor's power actions still
  call `reboot(2)` directly, so a machine ended from the power menu never
  unmounts its ext4 root and `sync(2)` is all that stands in for a shutdown.
  The menu should ask the init that is now there, the way the command above
  does. That is a change to `tos_system::power` and it is deliberately not
  part of #158 — one is a program on `PATH`, the other is the compositor's own
  path to the kernel, and whatever replaces it still has to answer for the
  rescue session, where there is no init to ask.
- **busybox.** It is no longer PID 1 and nothing else in the rootfs needs it:
  the installer runs Debian's `sfdisk`, `mkfs.ext4`, `mount`, `unsquashfs` and
  `grub-install`, and a pane's shell is bash or dash. It stays on the image
  while #109 is open about what a tOS machine should be able to type; the note
  beside `ROOTFS_PACKAGES` no longer claims it is load bearing.
