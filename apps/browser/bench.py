#!/usr/bin/env python3
"""Measure what a tOS pane would have to keep up with.

The browser proof of concept (issue #147) puts Chromium's pixels on a tOS pane
by way of the kitty graphics protocol. Before writing that, it is worth knowing
what the source can produce: how fast a frame arrives, how big it is, and
whether PNG -- the one format tOS already decodes -- is affordable, or whether
the PoC has to grow a JPEG decoder it would rather not have.

So this measures three things against a headless Chromium over CDP:

  * page load, to know the fixed cost of getting somewhere
  * Page.captureScreenshot, the one-frame-on-demand path
  * Page.startScreencast in both jpeg and png, the streaming path

The test page repaints a full-width bar every animation frame, which is the
worst case on purpose: a page that idles lets Chromium skip encoding, and a
number measured on an idle page would flatter the design.

Run it through run.sh, which starts the browser first.
"""

import argparse
import base64
import json
import statistics
import sys
import time

import cdp


def _viewport(conn, session, width, height):
    """Pin the viewport, so a number is comparable across machines.

    Without this the size comes from --window-size and the platform's idea of
    a default, and two runs stop meaning the same thing.
    """
    conn.call("Emulation.setDeviceMetricsOverride", {
        "width": width, "height": height,
        "deviceScaleFactor": 1, "mobile": False,
    }, session)


def measure_load(conn, session, url, runs):
    """Time Page.navigate to the load event, from a blank page each time."""
    samples = []
    for _ in range(runs):
        conn.call("Page.navigate", {"url": "about:blank"}, session)
        conn.event("Page.loadEventFired")
        start = time.perf_counter()
        conn.call("Page.navigate", {"url": url}, session)
        conn.event("Page.loadEventFired")
        samples.append((time.perf_counter() - start) * 1000)
    return samples


def measure_screenshot(conn, session, runs, fmt="png", quality=None):
    """Time Page.captureScreenshot, and record how many bytes it returns."""
    ms, sizes = [], []
    params = {"format": fmt}
    if quality is not None:
        params["quality"] = quality
    for _ in range(runs):
        start = time.perf_counter()
        data = conn.call("Page.captureScreenshot", params, session)["data"]
        ms.append((time.perf_counter() - start) * 1000)
        sizes.append(len(base64.b64decode(data)))
    return ms, sizes


def measure_screencast(conn, session, seconds, fmt, quality, width, height):
    """Count frames and bytes over a fixed wall-clock window.

    Every frame has to be acknowledged or Chromium stops sending, so the ack is
    part of the measured loop -- which is honest, because a real consumer has
    to ack too.
    """
    params = {"format": fmt, "maxWidth": width, "maxHeight": height,
              "everyNthFrame": 1}
    if quality is not None:
        params["quality"] = quality
    conn.call("Page.startScreencast", params, session)
    frames, total, deadline = 0, 0, time.perf_counter() + seconds
    first = None
    try:
        while time.perf_counter() < deadline:
            ev = conn.event("Page.screencastFrame")
            now = time.perf_counter()
            if first is None:
                # Start the clock at the first frame: the time between the
                # command and the first frame is startup, not throughput.
                first, frames, total = now, 0, 0
            payload = ev["params"]["data"]
            total += len(base64.b64decode(payload))
            frames += 1
            conn.call("Page.screencastFrameAck",
                      {"sessionId": ev["params"]["sessionId"]}, session)
        elapsed = time.perf_counter() - first if first else seconds
    finally:
        conn.call("Page.stopScreencast", {}, session)
    return frames, total, elapsed


def _stat(samples):
    return {
        "n": len(samples),
        "min": round(min(samples), 2),
        "median": round(statistics.median(samples), 2),
        "max": round(max(samples), 2),
    }


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=9222)
    ap.add_argument("--url", default="file:///srv/testpage.html")
    ap.add_argument("--width", type=int, default=640)
    ap.add_argument("--height", type=int, default=360)
    ap.add_argument("--runs", type=int, default=20, help="load and screenshot samples")
    ap.add_argument("--seconds", type=float, default=5.0, help="per screencast format")
    ap.add_argument("--png", metavar="PATH", help="write one screenshot here, to look at")
    ap.add_argument("--json", metavar="PATH", help="write the numbers here")
    args = ap.parse_args()

    ws = cdp.endpoint(args.host, args.port)
    conn, session = cdp.attach(ws)
    version = json.loads(json.dumps(conn.call("Browser.getVersion")))
    conn.call("Page.enable", {}, session)
    _viewport(conn, session, args.width, args.height)

    load = measure_load(conn, session, args.url, args.runs)
    shot_ms, shot_sz = measure_screenshot(conn, session, args.runs, "png")

    if args.png:
        data = conn.call("Page.captureScreenshot", {"format": "png"}, session)["data"]
        with open(args.png, "wb") as fh:
            fh.write(base64.b64decode(data))

    casts = {}
    for name, fmt, q in (("jpeg-q70", "jpeg", 70), ("png", "png", None)):
        frames, total, elapsed = measure_screencast(
            conn, session, args.seconds, fmt, q, args.width, args.height)
        casts[name] = {
            "frames": frames,
            "seconds": round(elapsed, 3),
            "fps": round(frames / elapsed, 1) if elapsed else 0.0,
            "avg_bytes": round(total / frames) if frames else 0,
            "mbytes_per_s": round(total / elapsed / 1e6, 2) if elapsed else 0.0,
        }
    conn.close()

    out = {
        "browser": version.get("product"),
        "viewport": f"{args.width}x{args.height}",
        "url": args.url,
        "load_ms": _stat(load),
        "screenshot_png_ms": _stat(shot_ms),
        "screenshot_png_bytes": _stat(shot_sz),
        "screencast": casts,
    }
    print(json.dumps(out, indent=2))
    if args.json:
        with open(args.json, "w") as fh:
            json.dump(out, fh, indent=2)

    print(f"\n{out['browser']}  viewport {out['viewport']}", file=sys.stderr)
    print(f"  load                 {_stat(load)['median']} ms (median of {args.runs})",
          file=sys.stderr)
    print(f"  captureScreenshot    {_stat(shot_ms)['median']} ms, "
          f"{_stat(shot_sz)['median'] / 1024:.1f} KB", file=sys.stderr)
    for name, c in casts.items():
        print(f"  screencast {name:<9} {c['fps']} fps, "
              f"{c['avg_bytes'] / 1024:.1f} KB/frame, {c['mbytes_per_s']} MB/s",
              file=sys.stderr)


if __name__ == "__main__":
    main()
