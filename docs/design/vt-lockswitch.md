# `VT_LOCKSWITCH` and `vt_dont_switch`

The experiment for [#53](https://github.com/m96-chan/tOS/issues/53), and the
decision it settles for [`screen-lock.md`](screen-lock.md).

**The lock does not take `VT_LOCKSWITCH`. It refuses each switch with
`VT_RELDISP 0` and nothing else.**

---

## The question

`VT_LOCKSWITCH` sets one global kernel flag, `vt_dont_switch`. Nothing owns
it. tOS's release profile sets `panic = "abort"`, so no destructor runs on a
panic, and none ever runs on `SIGKILL`. If the kernel does not clear the flag
when the process that set it dies, a crash while the screen is locked leaves a
machine whose virtual terminals cannot be reached until it reboots.

Two things had to be measured, not reasoned about:

1. Does the flag outlive the process that set it?
2. If it does, what does taking it actually cost?

## What the source says

Read from the release tarballs on `cdn.kernel.org`, not from memory:
`linux-6.1.187` — which is the exact source of Debian bookworm's
`linux-image-amd64` 6.1.187-1, the kernel `iso/mkiso.sh` puts on the tOS ISO —
and `linux-7.2.4`.

`vt_dont_switch` appears in six places in each whole tree, and in no others:

```text
                                        6.1.187      7.2.4
include/linux/vt_kern.h   extern          :123        :130
drivers/tty/vt/vt_ioctl.c definition       :42         :42
drivers/tty/vt/vt_ioctl.c set, VT_LOCKSWITCH   :946    :930
drivers/tty/vt/vt_ioctl.c cleared, VT_UNLOCKSWITCH :951 :935
drivers/tty/vt/vt_ioctl.c tested, change_console() :1207 :1204
drivers/tty/vt/vt.c       tested, set_console()   :3017 :3387
```

Two ioctls write it and two functions read it. Nothing in process exit, in the
tty release path, in `vc_deallocate` or in `reset_vc` touches it. The two
trees are identical in this respect. That is the hypothesis: the flag is set
until something clears it, and the only things that can are `VT_UNLOCKSWITCH`
and a reboot.

Two details of the read matter later:

- **`VT_ACTIVATE` does not fail while the flag is set.** `vt_ioctl.c` calls
  `set_console(arg)` and throws the result away, and `set_console`'s own
  comment says so: "Existing `set_console()` users don't check the return
  value". The ioctl returns 0 and the console simply does not move.
- **`VT_WAITACTIVE` does not return while the flag is set.** `vt_waitactive`
  waits on a `VT_EVENT_SWITCH` with no timeout, interruptible only by a
  signal. `VirtualTerminal::activate` in `vt.rs` issues `VT_ACTIVATE` and then
  `VT_WAITACTIVE`, so calling it while the flag is set hangs the caller.

## What was measured

`compositor/tos-platform/examples/vt_lockswitch.rs` is the reproducer. It was
built static against musl, put in a busybox initramfs, and booted under qemu,
so the experiment touched only the VM's virtual terminals:

```sh
cargo +1.98.1 build --release --target x86_64-unknown-linux-musl \
    -p tos-platform --example vt_lockswitch
# initramfs: /bin/busybox, /init, the binary; boot it with
qemu-system-x86_64 -m 512 -display none -serial stdio -no-reboot \
    -kernel vmlinuz -initrd initrd.gz -append "console=ttyS0 panic=1"
```

It ran on Debian bookworm's `linux-image-amd64` 6.1.187-1 (6.1.0-53-amd64) and
on Arch's 7.2.4-arch1-2. **Both kernels gave the same transcript**, which on
6.1 reads:

```text
Part one: does vt_dont_switch outlive the process that set it?

1. baseline            VT_ACTIVATE returned 0, the console switched
2. child locked        VT_LOCKSWITCH taken by pid 84
3. while locked        VT_ACTIVATE returned 0, the console did not move
4. child SIGKILLed     pid 84 reaped
5. after the kill      VT_ACTIVATE returned 0, the console did not move
6. unlocked elsewhere  VT_ACTIVATE returned 0, the console switched
7. back home           VT_ACTIVATE returned 0, the console switched

Part two: after a compositor dies holding the terminal, can another
process still reach a console?

   without it             VT_ACTIVATE returned 0, the console switched
   with VT_LOCKSWITCH     VT_ACTIVATE returned 0, the console did not move
```

So:

- **The flag survives `SIGKILL`.** Line 5 is the answer to #53. The process
  that set it was killed and reaped, and switching stayed dead.
- **A different process clears it.** Line 6: `VT_UNLOCKSWITCH` from the parent,
  which never set it, gave switching back. Recovery needs
  `CAP_SYS_TTY_CONFIG` and nothing more.
- **`VT_ACTIVATE` returns 0 the whole time.** Every line above says so,
  including the ones where nothing happened. A caller that checks the return
  value of `VT_ACTIVATE` learns nothing; the only way to know whether a switch
  happened is to read `VT_GETSTATE` afterwards.

A second run drove the emulated keyboard from the qemu monitor, to check the
key a user would actually press rather than the ioctl:

```text
KEYTEST-BEFORE=1          # flag set, setting process already killed
                          # host: sendkey ctrl-alt-f2
KEYTEST-WHILE-LOCKED=1    # nothing happened
vt_dont_switch cleared
                          # host: sendkey ctrl-alt-f2
KEYTEST-AFTER-UNLOCK=2    # the same keystroke moved the console
```

Same on both kernels. Ctrl+Alt+F2 is dead for as long as the flag is set, and
the flag is set for as long as the machine is up.

## What it costs, which is the part that decides it

Part two of the transcript is the argument. When the process holding a VT
under `VT_PROCESS` dies, the kernel notices on the next switch attempt:
`change_console` calls `kill_pid` to send the release signal, sees it fail,
and calls `reset_vc`, which puts the terminal back to `KD_TEXT` and `VT_AUTO`
and then lets the switch through. The comment in `vt_ioctl.c` says exactly why
it is there: "it saves the agony when the X server dies and the screen remains
blanked due to `KD_GRAPHICS`".

That is tOS's crash recovery, and it is the only one. A killed `tos` leaves
the terminal in graphics mode with the keyboard in `K_OFF` — `Drop` does not
run — so the screen is dead and the console keyboard is dead with it. The
machine is recovered by one `VT_ACTIVATE` from somewhere else: over ssh, from
the serial console, from anything that can run `chvt`. The kernel then resets
the terminal and hands the machine back.

Taking `VT_LOCKSWITCH` removes that. `set_console` tests `vt_dont_switch`
before it reaches any of the dead-owner handling, so the rescue switch is
discarded and `reset_vc` never runs. Measured, both kernels: *without it the
console switched, with it the console did not move.*

So the trade is not "a stronger lock for a small risk". It is:

- **What it buys.** Nothing the refusal does not already buy. `VT_RELDISP 0`
  refuses a switch whoever asked for it — the kernel handshakes with the
  owning process for a keyboard switch and for another process's
  `VT_ACTIVATE` alike, so `chvt` from a second root shell is already refused
  while tOS is alive and locked. On hardware the keyboard path is gone twice
  over before either mechanism is reached: tOS `EVIOCGRAB`s the keyboards, so
  the kernel's input handler never sees a key, and `K_OFF` makes the VT
  keyboard drop everything but `KT_SPEC` and `KT_SHIFT` keysyms — Ctrl+Alt+Fn
  is `KT_CONS` (`keyboard.c`, the `VC_OFF` test before `k_handler`).
- **What it costs.** The one path that gets a machine back after tOS dies
  holding the screen.

A mechanism that adds no protection against a live attacker and removes the
recovery from a dead compositor is not worth taking.

## The decision

The lock refuses each switch with `VT_RELDISP 0`. `lock_switching`,
`unlock_switching` and `switching_locked` stay in `vt.rs`: `unlock_switching`
is the rescue, and the pair is what makes this decision checkable rather than
remembered. Nothing in the lock calls `lock_switching`.

Two things follow from the measurements and should be done when #45 is built:

- **`tos` should call `VT_UNLOCKSWITCH` once at startup, unconditionally.**
  It costs one ioctl, it needs no state, and the installed system respawns
  `tos` from `/etc/inittab`, so it turns any stuck flag — however it got set —
  into something a respawn clears.
- **`VirtualTerminal::activate` must not be called blind.** `VT_WAITACTIVE`
  does not return while the flag is set. Nothing calls `activate` today.

### What would have to change to revisit this

All three, not any one:

1. tOS gains a privileged process that is not tOS and could call
   `VT_ACTIVATE` — a session manager, a logind equivalent — so that blocking
   it means something.
2. The release profile stops aborting on panic, or the lock gets a supervisor
   that clears the flag when `tos` dies.
3. Some rescue path other than `VT_ACTIVATE` exists and is documented, so
   that taking the flag does not take the last one with it.

## What remains unverified

- **Bare metal.** Both runs were qemu guests. The flag is tested in
  `set_console` and `change_console`, above any console driver, so the
  hardware underneath should not matter — but it was not measured on a real
  tOS machine. `vt_lockswitch` is checked in so that it can be, and
  `--leave-locked` exists for exactly that: it leaves the flag set so a person
  can press Ctrl+Alt+F2 themselves, and `--unlock` puts it back.
- **aarch64.** Only x86_64 was run. Nothing in the code read is
  architecture-dependent.
- **`VT_WAITACTIVE` hanging.** Read from the source, not measured. The
  reproducer avoids it deliberately — it polls `VT_GETSTATE` instead — so a
  wrong reading here would hang the harness rather than mislead it.
- **Kernels other than these two.** 6.1.187 and 7.2.4 are what tOS boots and
  what it is developed on. The six sites are identical across both, which is
  weak evidence that nothing changed in between and none at all about what
  comes after.
