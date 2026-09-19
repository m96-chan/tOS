"""A Chrome DevTools Protocol client small enough to read in one sitting.

The proof of concept talks to Chromium over CDP, which is JSON over a
WebSocket. The obvious way to do that in Python is the `websockets` package,
and this file exists because tOS should not need it: a tOS machine ships
thirteen Debian packages and a browser experiment that starts by asking for a
pip install has already lost the argument it is trying to make. Everything
here is the standard library.

Only the parts of WebSocket that CDP uses are implemented -- a client-side
handshake, masked text frames out, unmasked frames in, continuation frames
because a 60 KB PNG does not always arrive in one piece. No extensions, no
compression, no ping/pong beyond answering one.
"""

import base64
import json
import os
import socket
import struct
import urllib.request
from urllib.parse import urlparse

_OP_CONT, _OP_TEXT, _OP_BIN, _OP_CLOSE, _OP_PING, _OP_PONG = 0x0, 0x1, 0x2, 0x8, 0x9, 0xA


class CdpError(RuntimeError):
    """A command came back with an `error` member."""


def endpoint(host, port, timeout=30.0):
    """Return the browser-level WebSocket URL, waiting for Chromium to listen.

    Chromium writes its HTTP endpoint only once it is ready, so a connection
    refused here means "not up yet" far more often than it means "broken".
    """
    import time
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        try:
            with urllib.request.urlopen(f"http://{host}:{port}/json/version", timeout=2) as r:
                return json.load(r)["webSocketDebuggerUrl"]
        except Exception as exc:  # connection refused, or a half-written reply
            last = exc
            time.sleep(0.1)
    raise TimeoutError(f"no CDP endpoint at {host}:{port} after {timeout}s: {last}")


class Connection:
    """One WebSocket to one CDP target."""

    def __init__(self, url, timeout=30.0):
        parsed = urlparse(url)
        self.sock = socket.create_connection(
            (parsed.hostname, parsed.port or 80), timeout=timeout)
        self.sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self._buf = b""
        self._next_id = 0
        self._events = []
        self._handshake(parsed)

    def _handshake(self, parsed):
        key = base64.b64encode(os.urandom(16)).decode()
        path = parsed.path + ("?" + parsed.query if parsed.query else "")
        # Origin matters: Chromium rejects a non-null Origin unless it was
        # started with --remote-allow-origins. Sending none avoids the question
        # when the browser happens to be strict, and costs nothing when it is
        # not.
        req = (
            f"GET {path} HTTP/1.1\r\n"
            f"Host: {parsed.hostname}:{parsed.port}\r\n"
            "Upgrade: websocket\r\nConnection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        )
        self.sock.sendall(req.encode())
        while b"\r\n\r\n" not in self._buf:
            chunk = self.sock.recv(4096)
            if not chunk:
                raise ConnectionError("server closed during WebSocket handshake")
            self._buf += chunk
        head, self._buf = self._buf.split(b"\r\n\r\n", 1)
        status = head.split(b"\r\n", 1)[0]
        if b"101" not in status:
            raise ConnectionError(f"WebSocket upgrade refused: {status!r}")

    # -- framing ---------------------------------------------------------

    def _recv_exactly(self, n):
        while len(self._buf) < n:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise ConnectionError("server closed the WebSocket")
            self._buf += chunk
        out, self._buf = self._buf[:n], self._buf[n:]
        return out

    def _send_frame(self, payload):
        # Client frames must be masked; the mask may be anything, including
        # four zero bytes, but a real one keeps intermediaries honest.
        mask = os.urandom(4)
        n = len(payload)
        if n < 126:
            header = struct.pack("!BB", 0x80 | _OP_TEXT, 0x80 | n)
        elif n < 65536:
            header = struct.pack("!BBH", 0x80 | _OP_TEXT, 0x80 | 126, n)
        else:
            header = struct.pack("!BBQ", 0x80 | _OP_TEXT, 0x80 | 127, n)
        masked = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
        self.sock.sendall(header + mask + masked)

    def _recv_message(self):
        """Read one complete application message, reassembling continuations."""
        parts, opcode = [], None
        while True:
            b0, b1 = struct.unpack("!BB", self._recv_exactly(2))
            fin, op, masked, length = b0 & 0x80, b0 & 0x0F, b1 & 0x80, b1 & 0x7F
            if length == 126:
                (length,) = struct.unpack("!H", self._recv_exactly(2))
            elif length == 127:
                (length,) = struct.unpack("!Q", self._recv_exactly(8))
            if masked:  # a server must not mask; if one does, honour it anyway
                key = self._recv_exactly(4)
                data = bytes(b ^ key[i % 4]
                             for i, b in enumerate(self._recv_exactly(length)))
            else:
                data = self._recv_exactly(length)
            if op == _OP_PING:
                self.sock.sendall(struct.pack("!BB", 0x80 | _OP_PONG, 0x80 | len(data))
                                  + b"\x00\x00\x00\x00" + data)
                continue
            if op == _OP_CLOSE:
                raise ConnectionError("server closed the WebSocket")
            if op == _OP_PONG:
                continue
            if op != _OP_CONT:
                opcode = op
            parts.append(data)
            if fin:
                break
        payload = b"".join(parts)
        return payload.decode() if opcode == _OP_TEXT else payload

    # -- CDP -------------------------------------------------------------

    def send(self, method, params=None, session=None):
        """Send a command and return its id without waiting for the reply."""
        self._next_id += 1
        msg = {"id": self._next_id, "method": method, "params": params or {}}
        if session:
            msg["sessionId"] = session
        self._send_frame(json.dumps(msg).encode())
        return self._next_id

    def call(self, method, params=None, session=None):
        """Send a command and return its result, queueing any events that race it."""
        want = self.send(method, params, session)
        while True:
            msg = json.loads(self._recv_message())
            if msg.get("id") == want:
                if "error" in msg:
                    raise CdpError(f"{method}: {msg['error']}")
                return msg.get("result", {})
            if "method" in msg:
                self._events.append(msg)

    def event(self, method=None):
        """Return the next event, optionally the next one named `method`.

        Events that arrived while a command was in flight are returned first,
        which is what makes `call` safe to use in the middle of a screencast.
        """
        for i, ev in enumerate(self._events):
            if method is None or ev["method"] == method:
                return self._events.pop(i)
        while True:
            msg = json.loads(self._recv_message())
            if "method" not in msg:
                continue  # a reply nobody is waiting for; drop it
            if method is None or msg["method"] == method:
                return msg
            self._events.append(msg)

    def close(self):
        try:
            self.sock.close()
        except OSError:
            pass

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()


def attach(ws_url, timeout=30.0):
    """Open a browser connection and return (connection, page session id).

    CDP's browser endpoint cannot drive a page directly; a target has to be
    attached to first. `flatten` puts that session on the same socket, which
    keeps this file to one connection.
    """
    conn = Connection(ws_url, timeout=timeout)
    targets = conn.call("Target.getTargets")["targetInfos"]
    pages = [t for t in targets if t["type"] == "page"]
    target = pages[0]["targetId"] if pages else conn.call(
        "Target.createTarget", {"url": "about:blank"})["targetId"]
    session = conn.call("Target.attachToTarget",
                        {"targetId": target, "flatten": True})["sessionId"]
    return conn, session
