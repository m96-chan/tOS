# Headless Chromium for the browser proof of concept (issue #147).
#
# Debian bookworm, because that is what a tOS rootfs is: iso/mkiso.sh
# debootstraps bookworm, so a dependency closure measured here is the closure a
# tOS machine would actually pay for. The host this builds on does not have to
# be Debian, and the machine this was developed on is not.
#
# chromium-shell rather than chromium: the shell is the headless binary without
# the desktop browser UI, 76 packages against 112, and the PoC never opens a
# window. Nothing here is proposed for the ISO; see docs/design/browser.md.
FROM debian:bookworm-slim

# --no-install-recommends throughout, so that the package counts this image
# reports are the closure a tOS image would install and not Debian's
# suggestions on top of it.
RUN apt-get update && apt-get install -y --no-install-recommends \
        chromium-shell \
        fonts-noto-cjk \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# The PoC drives Chromium over CDP from outside the container, so the debugging
# socket has to leave localhost. This is a development image and nothing else:
# an open CDP port is remote code execution by design.
EXPOSE 9222

COPY testpage.html /srv/testpage.html

# --ozone-platform=headless is the flag that matters. With --headless alone the
# Ozone layer still selects the X11 platform, and Chromium dies on "Missing X
# server or $DISPLAY" -- or, for --screenshot, hangs without printing anything.
ENTRYPOINT ["/usr/bin/chromium-shell", \
    "--headless", \
    "--no-sandbox", \
    "--disable-gpu", \
    "--disable-dev-shm-usage", \
    "--ozone-platform=headless", \
    "--remote-debugging-address=0.0.0.0", \
    "--remote-debugging-port=9222", \
    "--remote-allow-origins=*"]
CMD ["about:blank"]
