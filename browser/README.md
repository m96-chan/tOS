# Browser proof of concept

A page rendered by a headless Chromium, arriving in a tOS pane as kitty
graphics. The reasoning, the measurements and the decisions are in
[`docs/design/browser.md`](../docs/design/browser.md); this file is how to run
it.

Nothing here requires a change to the compositor. The converter is an ordinary
program writing escape sequences to stdout, which is what a pane is for.

## What is in here

| | |
| --- | --- |
| `Dockerfile` | headless Chromium on bookworm, pinned, with the CJK fonts |
| `testpage.html` | a page whose every element makes one kind of failure visible |
| `cdp.py` | a Chrome DevTools Protocol client, standard library only |
| `bench.py` | how fast frames come out, and how big they are |
| `kitty_stream.py` | CDP screencast → kitty graphics commands |
| `run.sh` | start a browser, measure it, stop it |

`cdp.py` speaks enough WebSocket to talk to Chromium and nothing more. That is
deliberate: a browser experiment for a system that ships thirteen packages
should not open by asking for a `pip install`.

## Running it

With Docker, which needs nothing installed:

```sh
./run.sh --docker --png frame.png
```

With a browser already on the machine — `chrome-headless-shell`,
`chromium-shell`, or `$CHROME_HEADLESS_SHELL` as the CI image sets it:

```sh
./run.sh --png frame.png
```

Either prints the numbers and leaves a PNG to look at. Look at it: a wrong
frame and a right frame both exit zero, and missing CJK fonts turn every
Japanese glyph into a box that only a human notices.

## Putting it in a pane

`kitty_stream.py` converts the screencast into kitty graphics commands. The
default sends each frame inline as base64, which works in any terminal that
speaks the protocol and is too slow to stream — the PTY carries about 240 KB/s
into a tOS pane, and a 60 fps PNG stream wants seventeen times that.

`--shm` sends the pixels through a POSIX shared memory object and puts only its
name on the PTY, which is the transport the design settles on:

```sh
# start a browser first, e.g. ./run.sh --docker in another pane
python3 kitty_stream.py --shm --frames 300
```

Through tOS, end to end, with no display server anywhere:

```sh
cargo build --release
./target/release/tos --backend headless --warmup 25 --screenshot /tmp/tos.ppm \
    -e /bin/sh -c 'cd browser && python3 kitty_stream.py --shm --frames 40'
```

`--warmup` matters. A screenshot renders after that many frames, and the pane
needs a few of them to read the picture in; the default of one will photograph
an empty pane and tell you nothing is working when something is.

## Checking it without a terminal

The byte stream can be saved and checked on a machine with no terminal at all,
which is what CI is:

```sh
python3 kitty_stream.py --frames 60 --save frames.kitty
python3 kitty_stream.py --verify frames.kitty
```

`--verify` parses the commands back out, reassembles the base64 and checks that
each frame really is a PNG of the size it claims. It reads the bytes rather
than calling the encoder, so it would catch the encoder being wrong.
