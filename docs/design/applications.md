# The applications a tOS machine comes with

**Issue #151.** Decided: bash is the shell, thirteen Debian packages and one
upstream binary are on every image, and everything else is the person's.

Until this, `iso/mkiso.sh` argued for every package on the image in terms of
booting, installing or reaching a network — and it was right to, because that
is what the image was for. What it produced was a machine that comes up,
partitions a disk, joins a wireless network, and then has no editor. The
README listed candidates. This is the list.

## The shell is bash, and Homebrew does not get a vote

Bash, and it already was: `apps/installer/src/install.rs` writes the account's line
with `/bin/bash` when the rootfs has one and `/bin/sh` when the fallback
busybox world is on the disk, and `iso/live-session` asks the same question on
the live side. Nothing changes here; it is written down because #151 asked for
it to be.

The reason it is bash and not something else is in that function already: ash
has no programmable completion, a weaker line editor, no arrays and no `[[`,
and somebody who brings their dotfiles brings bash ones.

The reason it is not **zsh** deserves its own line, because the question
arrived attached to Homebrew. Homebrew on Linux is a package manager that
happens to have come from macOS, and the shell macOS defaults to is not a fact
about Homebrew — it is a fact about Apple. Installing `brew` on a tOS machine
does not make that machine a Mac, and a default shell chosen to match one
would be tOS taking a position on a matter that belongs to the person. Anybody
who wants zsh, fish or nushell installs it and `chsh`es; that is a shell being
a choice, which is the only thing tOS has to say about it.

## What is on every image

Thirteen from Debian, and one that Debian does not have.

| | | why |
| --- | --- | --- |
| `git` | 2.39.5 | "must git." — the issue's words |
| `curl` | 7.88.1 | the other name the issue asked for, and what `curl \| sh` means |
| `less` | 590 | the pager git needs |
| `neovim` | 0.7.2 | the editor |
| `ripgrep` | 13.0.0 | search |
| `fzf` | 0.38.0 | choosing from what it found |
| `btop` | 1.2.13 | what is this machine doing |
| `openssh-client` | 9.2p1 | off this machine |
| `rsync` | 3.2.7 | and files with it |
| `unzip` | 6.0 | what a download usually is |
| `file` | 5.44 | what a download actually is |
| `man-db` | 2.11.2 | and the manual pages, which stopped being thrown away |
| `libatomic1` | 12.2.0 | see below; it is not an application |
| `yazi` | 26.9.1 | the file manager, from upstream |

And the face: **HackGen Console NF 2.10.0**, replacing `fonts-vlgothic`.

`libatomic1` is on the list for a reason that has nothing to do with
applications tOS ships and everything to do with the ones somebody installs.
Nothing in Debian's essential set pulls it, and Node links it from v25 — so
`nvm install node` on a tOS machine downloads, unpacks, reaches 100%, and then
dies with `node: error while loading shared libraries: libatomic.so.1`. It is
45 kB. It is here rather than left to `apt install` because the error names a
*file* and not a package: apt cannot tell somebody what to type, so the person
is left searching for a string. Reproduced in a bookworm container against
v22.14.0, v23.11.1, v24.9.0 (all fine) and v25.0.0, v26.8.2 (both broken), and
fixed there by the one package.

## The manual pages, which were excluded and are not any more

`iso/mkiso.sh` has always written a dpkg configuration that throws
`/usr/share/man` away, and #151's list proposes `man-db`. Only one of those can
be true, and the exclusion loses.

The line that created it said a squashfs "should not spend it on manual pages
it has no pager story for". That reason expired the moment `less` went on the
image, three rows up this page. What was left was the size, and the size was
assumed to be large. It is not: **7,876,608 bytes of the squashfs**, measured
by building the same rootfs with the exclusion and without it and compressing
both the way the image does.

The argument for paying that is not really about the image. A `path-exclude`
is not "this image has no manuals" — it is a line in the *installed machine's*
dpkg configuration, so it is "this machine can never have manuals". Install a
package on a tOS laptop a year from now and its manual page is discarded on the
way in. That is a property somebody meets the first time they type `man git`,
cannot explain from what they can see, and cannot easily undo. It is not worth
3% of the medium.

So `man-db` is installed, the exclusion is gone, and the by-hand `rm -rf` that
matched it drops `/usr/share/man` — otherwise the essential set's manuals, the
ones somebody is most likely to want, would be the only ones missing.

**`/usr/share/locale` stays excluded**, and that reason is intact: the rootfs
has no `locales` package, so no locale is generated and every program falls
back to C. Those 31.8 MB are translations nothing on this machine can display.

## What is not, and why

**`tmux`.** #6 decided this already: a session that survives a detach is what
tOS's own panes are for, and tmux inside tOS is two multiplexers deep. It is
an `apt install` away for somebody who wants `ssh` into a long-running job.

**`lazygit`.** A nicer way to do what `git` already does, which makes it taste
rather than capability — and unlike yazi it has no .deb, only a tarball, so it
would cost the update story below for something that is not the difference
between working and not working. `go install` or a release tarball, by the
person.

**Homebrew.** Not preinstalled, and not endorsed as *the* way to get software
onto a tOS machine — apt is, because apt is what the rootfs is made of and
what gets security updates from `bookworm-security`. But it is not blocked
either, and `curl` is on the image partly so that the `brew` install line
works. Where it earns its place is exactly where apt cannot help: a version of
something newer than bookworm's. The cost is the usual one and is the person's
to accept — Homebrew does not know about Debian's packages, so anything it
builds against a library it also installed is a second copy.

## Neovim 0.7.2, which is old, shipped anyway

bookworm has neovim 0.7.2, released in 2022. That is four years behind, it is
before `vim.lsp` became pleasant, and a good share of the plugins somebody
would reach for now declare 0.9 or 0.10 as their minimum.

The alternative was the upstream tarball, the way yazi below arrives. It was
turned down, and the reason is the difference between the two programs rather
than a rule about tarballs:

- An editor is the program somebody points at their own configuration, and a
  configuration outlives an image. Debian's neovim gets bookworm's security
  updates for as long as bookworm has them; a tarball gets whatever the next
  person to edit `mkiso.sh` remembers to bump. Handing somebody a *newer*
  editor that then silently stops being updated is the worse of the two.
- Somebody who wants current neovim can have it in one line, from Homebrew or
  from the same tarball, and it will be *their* copy — updated when they
  update it, which is the arrangement that actually works.

So the image ships the one the distribution maintains, and says plainly that
it is old. This is the decision most likely to be revisited, and the thing
that would change it is tOS moving off bookworm — trixie has 0.9.5.

## Yazi, which does not come from Debian

The file manager, and the one name here that Debian has no package for at any
version.

It is on the image rather than left out because of what it does in a *tOS*
pane specifically. Everything that has drawn a picture through
`docs/design/graphics-file-transmission.md` so far was written by tOS —
`tos-preview`, the greeting at the head of a shell, the tests. Yazi was never
told what tOS is. It asks the terminal whether it can show a picture, tOS
answers, and it previews a photograph in a pane. That is the protocol being a
protocol rather than a feature, and it is worth 12 MB.

**Where it comes from.** Upstream's own `.deb`, `musl` build, fetched over
https and checked against a sha256 written beside the URL in `mkiso.sh`. The
musl build is statically linked, so it does not care what libc the rootfs has.
It carries no maintainer scripts, so `dpkg-deb -x` puts exactly what `dpkg -i`
would and needs no chroot.

**Who owns its updates.** tOS does, and only at image-build time. It has no
line in the package database, so `apt upgrade` will never touch it, and a
CVE in it is a commit here — a version and a hash changed together — and a new
image. That is a genuinely worse arrangement than every other name on this
page, and it is the reason the list of things arriving this way is two long
and not ten. The same sentence governs the face.

**Its previewers.** Yazi's `Recommends` names ffmpeg, 7zip, poppler-utils,
imagemagick, zoxide and others; none are installed. Image previews are decoded
by yazi itself and sent over the graphics protocol, so the previewers that
exist for terminals *without* one are previewers this terminal does not need.
Its one hard `Depends` is `file`, which is on the list above for its own sake.

**Verified on the userspace it ships on**, rather than on a developer's
machine: the static `tos` out of the built squashfs, in a bookworm container
with this exact package set, previewing a PNG. 379 kitty graphics escapes and
a clean picture.

That check was worth running. The same test on the host, from a shell that
happened to be inside kitty, came out differently — `TERM=xterm-kitty` and
`KITTY_WINDOW_ID` were inherited by the compositor and then by the pane, yazi
concluded it was talking to kitty behind something, and switched to the
protocol's Unicode placeholders: 3 graphics escapes and 590 `U+10EEEE` cells,
which tOS does not implement and drew as 590 hollow boxes over the picture. No
tOS session has those variables — `tos-pty` sets `TERM=xterm-256color` and
`TERM_PROGRAM=tOS` — so it is a thing only `--backend nested` under kitty can
produce. Worth writing down because the conclusion from the wrong environment
was a bug report, and the environment was the bug.

tOS not implementing Unicode placeholders is nonetheless true, and is a gap
anything that chooses that path would fall into. Nothing on this image does.

## The face: HackGen Console NF

Asked for by name in #151, and it replaces `fonts-vlgothic` rather than
joining it.

**Why it is better than what it replaces**, beyond taste: it is a Nerd Font.
Every file-type icon in yazi, every powerline separator in a prompt somebody
brings with them, and the glyphs a modern TUI decorates itself with live in
the private-use area, and vlgothic has none of them — they were hollow boxes.
Rendered both faces in a pane against the same probe to check rather than
assume: kana, kanji, powerline and Nerd icons all present in HackGen, the
icons missing in vlgothic.

**`HackGen`, not `HackGen35`.** tOS's width table says a wide character is
exactly two cells, and only the 1:2 cut is. HackGen35 is 3:5 and would put
every kana half a cell wrong.

**`Console`**, which is the cut without programming ligatures — a terminal
compositing its own cells cannot use them.

**`Regular` alone.** `tos-font` synthesizes bold from the regular face when a
cut is missing, which is what it already did for vlgothic; the Bold file is
another 13,464,288 bytes for something nothing would load.

**One face, not two.** Carrying vlgothic as well would be 2,318,336 bytes of
squashfs insurance against a case that cannot happen: a fetch that does not
produce the expected bytes stops the build, so there is no image that quietly
shipped without a face. The vlgothic *paths* stay in `tos-font`'s search
lists, because those are for the machines tOS merely runs on.

**No braille**, which is the one thing it costs and which vlgothic did not
have either. btop draws its graphs out of braille by default, so the image
ships `iso/btop.conf` setting `graph_symbol = "block"`. That file is the whole
of the "minimal defaults" this issue asked about: neovim, bash and the launcher
are left at their own defaults, because every one of those is a thing somebody
replaces with their own and a tOS opinion in the way is an obstacle.

## What it costs

Two ISOs, built on the same machine from `origin/main` and from this change:

| | before | after | |
| --- | --- | --- | --- |
| ISO | 194,025,472 | 240,781,312 | +46,755,840 |
| rootfs, squashed (zstd-19) | 155,623,424 | 202,375,168 | +46,751,744 |
| rootfs, unpacked | 486,614,158 | 623,082,598 | +136,468,440 |
| initramfs (gzip) | 9,893,959 | 9,893,964 | +5, gzip noise |

**About 24% more image.** Where it goes, each measured on its own by building
the rootfs both ways and squashing it the same way — the parts add up to
slightly more than the whole because compression finds less to share when a
tree is measured alone:

| | unpacked | squashed |
| --- | --- | --- |
| the Debian applications | 84,098,952 | 23,908,352 |
| yazi | 33,929,663 | 12,378,112 |
| the manual pages | 7,938,894 | 7,876,608 |
| HackGen Console NF, less the vlgothic it replaces | 8,834,072 | 3,428,352 |

The manual pages are the row that does not compress: they are already gzipped,
so the squashfs gains nothing on them. The initramfs is untouched, which is
the point of it — none of this is on the path that finds the medium.

## What this does not settle

- The **browser** is [blinkterm](https://github.com/m96-chan/blinkterm), in
  its own repository; #147 settled it, and its engine is the clearest case of
  the rule above that everything past the base set is the person's.
- **Network and Bluetooth** stay tOS's own menus, as the issue asks; no `nmtui`
  and no `bluetuith`, because those would be a second answer to a question the
  compositor already answers.
- A **launcher** entry per application: `leader` already runs anything on
  `PATH`, so nothing here needs registering. If that changes it is its own
  issue.
