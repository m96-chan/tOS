#!/usr/bin/env python3
"""Turn Chromium's screencast into kitty graphics commands.

This is the join that the whole proof of concept turns on. Chromium can already
produce a PNG per frame over CDP, and tOS can already decode a PNG that arrives
as a kitty graphics command -- `f=100` in `compositor/tos-term/src/graphics.rs`,
decoded by `compositor/tos-term/src/png.rs`. Neither end knows about the other,
and this file is the seventy lines in between.

Nothing here needs anything added to the compositor. The output is an ordinary
byte stream on stdout, of the kind any program in a pane may write:

    ESC _ G a=T,f=100,t=d,i=1,q=2,C=1,m=1 ; <base64> ESC \\
    ESC _ G m=0 ; <base64> ESC \\

`a=T` transmits and displays in one command, `i=1` keeps reusing one image id so
each frame replaces the last rather than filling the store, `C=1` stops the
cursor moving under the picture, and `q=2` suppresses the per-frame replies that
would otherwise come back up the PTY at sixty a second. Payloads are chunked at
4096 base64 bytes because the protocol says so.

    ./kitty_stream.py --frames 120            play into this terminal
    ./kitty_stream.py --save frames.kitty     write the bytes out instead
    ./kitty_stream.py --verify frames.kitty   read them back and check them

--save and --verify exist because the interesting property is not "it looked
right on my terminal": it is that the byte stream is well formed and that the
payload really is a PNG of the size claimed. That can be checked in CI, on a
machine with no terminal at all.

--shm sends the same pictures the other way, as `t=s`: the PNG is written to a
POSIX shared memory object and only its name travels down the PTY. That is not
an optimisation, it is the difference between the idea working and not working
-- see docs/design/browser.md for the measurement. tOS already reads `t=s`
through `ImageFiles` and unlinks the object afterwards, so nothing has to be
added to the compositor for this to arrive.
"""

import argparse
import base64
import os
import re
import sys

import cdp

ESC = b"\x1b"
APC_START = ESC + b"_G"
APC_END = ESC + b"\\"
CHUNK = 4096  # base64 bytes per command, per the protocol


def encode_frame(png_bytes, image_id=1):
    """Return the kitty graphics commands that display one PNG.

    The first command carries the keys and the first chunk; continuations carry
    `m=1` and nothing else, and the last one carries `m=0`. That shape is what
    tOS's parser expects -- it accumulates payload until a command says the
    picture is complete.
    """
    payload = base64.b64encode(png_bytes)
    chunks = [payload[i:i + CHUNK] for i in range(0, len(payload), CHUNK)] or [b""]
    out = []
    for n, chunk in enumerate(chunks):
        more = 1 if n < len(chunks) - 1 else 0
        if n == 0:
            keys = f"a=T,f=100,t=d,i={image_id},q=2,C=1,m={more}".encode()
        else:
            keys = f"m={more}".encode()
        out.append(APC_START + keys + b";" + chunk + APC_END)
    return b"".join(out)


def encode_frame_shm(png_bytes, image_id=1, counter=[0]):
    """Write one PNG to shared memory and return the command that names it.

    The command is about sixty bytes however large the picture is, which is the
    whole point: the PTY carries a name and the pixels travel beside it. tOS
    reads the object and unlinks it (`ImageFiles::read`), so this does not have
    to clean up after a frame that was delivered -- only after one that was not.
    """
    counter[0] += 1
    name = f"tos-browser-{os.getpid()}-{counter[0]}"
    path = f"/dev/shm/{name}"
    # Written whole and then named, so the terminal cannot read a partial file:
    # the rename is atomic within the same filesystem.
    tmp = path + ".partial"
    with open(tmp, "wb") as fh:
        fh.write(png_bytes)
    os.rename(tmp, path)
    payload = base64.b64encode(name.encode())
    keys = f"a=T,f=100,t=s,i={image_id},q=2,C=1".encode()
    return APC_START + keys + b";" + payload + APC_END, path


def verify(path):
    """Parse a saved stream back and check every frame is a real PNG.

    Deliberately written against the bytes rather than against the encoder, so
    that it would catch the encoder being wrong.
    """
    data = open(path, "rb").read()
    commands = re.findall(re.escape(APC_START) + rb"(.*?)" + re.escape(APC_END),
                          data, re.S)
    frames, payload, keys, bad = 0, b"", None, 0
    for cmd in commands:
        head, _, chunk = cmd.partition(b";")
        fields = dict(kv.split(b"=", 1) for kv in head.split(b",") if b"=" in kv)
        if b"a" in fields:
            keys = fields
        payload += chunk
        if fields.get(b"m", b"0") == b"0":
            raw = base64.b64decode(payload)
            # PNG signature, then the IHDR width and height as big-endian u32.
            if raw[:8] != b"\x89PNG\r\n\x1a\n":
                bad += 1
            else:
                w = int.from_bytes(raw[16:20], "big")
                h = int.from_bytes(raw[20:24], "big")
                if frames == 0:
                    print(f"first frame: {w}x{h}, {len(raw)} bytes, "
                          f"{len(payload)} base64, keys {keys}")
            frames += 1
            payload = b""
    print(f"{len(commands)} commands, {frames} frames, {bad} not PNG, "
          f"{len(data)} bytes total")
    return 1 if bad or not frames else 0


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=9222)
    ap.add_argument("--url", default="file:///srv/testpage.html")
    ap.add_argument("--width", type=int, default=640)
    ap.add_argument("--height", type=int, default=360)
    ap.add_argument("--frames", type=int, default=60)
    ap.add_argument("--save", metavar="PATH", help="write the bytes here, not to stdout")
    ap.add_argument("--verify", metavar="PATH", help="check a saved stream and exit")
    ap.add_argument("--shm", action="store_true",
                    help="send t=s shared memory names instead of inline base64")
    args = ap.parse_args()

    if args.verify:
        return verify(args.verify)

    conn, session = cdp.attach(cdp.endpoint(args.host, args.port))
    conn.call("Page.enable", {}, session)
    conn.call("Emulation.setDeviceMetricsOverride", {
        "width": args.width, "height": args.height,
        "deviceScaleFactor": 1, "mobile": False}, session)
    conn.call("Page.navigate", {"url": args.url}, session)
    conn.event("Page.loadEventFired")

    sink = open(args.save, "wb") if args.save else sys.stdout.buffer
    leaked = []  # shm objects a terminal that never read them would leave behind
    conn.call("Page.startScreencast", {
        "format": "png", "maxWidth": args.width, "maxHeight": args.height,
        "everyNthFrame": 1}, session)
    try:
        for _ in range(args.frames):
            ev = conn.event("Page.screencastFrame")
            png = base64.b64decode(ev["params"]["data"])
            if not args.save:
                # Home the cursor first: the image is placed where the cursor
                # is, and without this each frame would land a line lower until
                # the pane scrolled.
                sink.write(b"\x1b[H")
            if args.shm:
                command, path = encode_frame_shm(png)
                leaked.append(path)
                sink.write(command)
            else:
                sink.write(encode_frame(png))
            sink.flush()
            conn.call("Page.screencastFrameAck",
                      {"sessionId": ev["params"]["sessionId"]}, session)
    finally:
        conn.call("Page.stopScreencast", {}, session)
        conn.close()
        # Whatever the terminal did read, it unlinked; the rest is ours to drop.
        for path in leaked:
            try:
                os.unlink(path)
            except FileNotFoundError:
                pass
        if args.save:
            sink.close()
            print(f"wrote {args.frames} frames to {args.save}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
