#!/bin/sh
# Start a headless browser with no display server and measure it.
#
# The proof of concept for issue #147 needs a browser that renders without X11
# or Wayland, because that is the one thing a tOS pane cannot offer it. This
# script starts one, waits for its CDP endpoint, runs bench.py against it, and
# stops it again -- so that "it worked on my machine" is a command someone else
# can run rather than a claim they have to take on faith.
#
#   ./run.sh                    use a browser found on this machine
#   ./run.sh --docker           use the pinned bookworm image instead
#   ./run.sh --png out/f.png    keep a frame to look at
#
# Everything after the recognised options is passed to bench.py.
set -eu

here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
port=9222
mode=host
png=""

while [ $# -gt 0 ]; do
    case $1 in
        --docker) mode=docker; shift ;;
        --port) port=$2; shift 2 ;;
        --png) png=$2; shift 2 ;;
        *) break ;;
    esac
done

# The flags. Every one of these is load-bearing:
#
#   --ozone-platform=headless  without it Ozone still picks the X11 platform and
#                              Chromium dies on "Missing X server or $DISPLAY".
#                              With --screenshot it does something worse: it
#                              hangs, silently, with no error and no timeout.
#   --no-sandbox               the sandbox needs privileges a CI container and a
#                              root session do not have. Chromium refuses to
#                              start as root without it -- even for --version.
#   --disable-dev-shm-usage    /dev/shm is 64 MB in a default container, and
#                              Chromium will fill it and crash.
#   --disable-gpu              there is no GPU to talk to and no compositor to
#                              ask; this keeps it from trying.
#   --remote-allow-origins=*   CDP rejects a WebSocket whose Origin it does not
#                              know, which is every client that is not DevTools.
flags="--headless --no-sandbox --disable-gpu --disable-dev-shm-usage \
--ozone-platform=headless --remote-debugging-port=$port --remote-allow-origins=*"

cleanup() { :; }
trap 'cleanup' EXIT INT TERM

if [ "$mode" = docker ]; then
    image=tos-browser-poc:bookworm
    docker build -q -t "$image" "$here" >/dev/null
    name=tos-browser-poc-$$
    # --shm-size=1g is the other half of --disable-dev-shm-usage: together they
    # keep a page from dying on a shared memory segment that is too small.
    docker run -d --rm --name "$name" --shm-size=1g \
        -p "127.0.0.1:$port:9222" "$image" about:blank >/dev/null
    cleanup() { docker rm -f "$name" >/dev/null 2>&1 || true; }
    url_default=file:///srv/testpage.html
else
    # $CHROME_HEADLESS_SHELL is set by the CI image; the rest is for a laptop.
    # Chrome for Testing's chrome-headless-shell and Debian's chromium-shell are
    # the same thing under two names: a Chromium with no browser UI compiled in.
    bin=""
    for cand in "${CHROME_HEADLESS_SHELL:-}" chrome-headless-shell chromium-shell chromium; do
        [ -n "$cand" ] || continue
        if command -v "$cand" >/dev/null 2>&1; then bin=$cand; break; fi
    done
    if [ -z "$bin" ]; then
        echo "no headless browser found; try --docker, or set CHROME_HEADLESS_SHELL" >&2
        exit 1
    fi
    profile=$(mktemp -d)
    # shellcheck disable=SC2086
    "$bin" $flags --user-data-dir="$profile" about:blank >"$profile/log" 2>&1 &
    pid=$!
    # Wait for the browser to actually be gone before removing its profile:
    # kill(1) returns as soon as the signal is delivered, and Chromium is still
    # writing into the directory when it does.
    cleanup() {
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
        rm -rf "$profile"
    }
    url_default=file://$here/testpage.html
fi

set -- --port "$port" --url "$url_default" ${png:+--png "$png"} "$@"
python3 "$here/bench.py" "$@"
