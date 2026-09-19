//! A WebSocket client, because CDP does not come any other way.
//!
//! The DevTools protocol has exactly one transport for the commands that
//! matter — an HTTP upgrade to a WebSocket — so a browser in a pane needs a
//! client. This is the client: the upgrade request, the accept check, and the
//! framing in both directions.
//!
//! It is a *client*, and the asymmetries of RFC 6455 are taken as written
//! rather than as suggestions. Every frame this program sends is masked, with
//! a key that is not a constant, because a server is entitled to close the
//! connection over an unmasked frame and Chromium does. Every frame it
//! receives may be fragmented, may be a ping in the middle of a fragmented
//! message, and may be a close it has to answer. Text frames only in the
//! direction that matters: CDP is JSON, and a binary frame from the engine is
//! a protocol error rather than something to guess about.
//!
//! What is deliberately missing: no `permessage-deflate` (the engine is a
//! child process on loopback, so the compression would cost CPU to save a
//! copy between two processes on the same machine), no server role, no
//! continuation frames on the sending side (a CDP command is a few hundred
//! bytes and fits any frame).

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use crate::base64;
use crate::sha1::sha1;

/// The largest message that will be assembled.
///
/// A screencast frame at a pane's size is tens of kilobytes; a megabyte is a
/// very large page screenshot. Sixteen is room for anything CDP sends and a
/// ceiling on what a confused peer can make this program allocate.
pub const MAX_MESSAGE: usize = 16 << 20;

/// The constant RFC 6455 has both ends hash with the key.
const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// What a frame is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opcode {
    Continuation,
    Text,
    Binary,
    Close,
    Ping,
    Pong,
}

impl Opcode {
    fn from_bits(bits: u8) -> Option<Opcode> {
        Some(match bits {
            0x0 => Opcode::Continuation,
            0x1 => Opcode::Text,
            0x2 => Opcode::Binary,
            0x8 => Opcode::Close,
            0x9 => Opcode::Ping,
            0xa => Opcode::Pong,
            _ => return None,
        })
    }

    fn bits(self) -> u8 {
        match self {
            Opcode::Continuation => 0x0,
            Opcode::Text => 0x1,
            Opcode::Binary => 0x2,
            Opcode::Close => 0x8,
            Opcode::Ping => 0x9,
            Opcode::Pong => 0xa,
        }
    }

    fn is_control(self) -> bool {
        matches!(self, Opcode::Close | Opcode::Ping | Opcode::Pong)
    }
}

/// One frame off the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub fin: bool,
    pub opcode: Opcode,
    pub payload: Vec<u8>,
}

/// Everything that can go wrong between here and the engine.
#[derive(Debug)]
pub enum WsError {
    /// The peer said something the protocol does not allow.
    Protocol(String),
    /// The socket did.
    Io(std::io::Error),
    /// The peer closed, with the code and reason it gave.
    Closed(String),
}

impl std::fmt::Display for WsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WsError::Protocol(what) => write!(f, "{what}"),
            WsError::Io(err) => write!(f, "{err}"),
            WsError::Closed(why) => write!(f, "the engine closed the connection: {why}"),
        }
    }
}

impl From<std::io::Error> for WsError {
    fn from(err: std::io::Error) -> WsError {
        WsError::Io(err)
    }
}

/// Frame a payload the way a client must: masked, unfragmented.
pub fn encode_frame(opcode: Opcode, payload: &[u8], mask: [u8; 4]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 14);
    out.push(0x80 | opcode.bits());
    let len = payload.len();
    if len < 126 {
        out.push(0x80 | len as u8);
    } else if len <= u16::MAX as usize {
        out.push(0x80 | 126);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(0x80 | 127);
        out.extend_from_slice(&(len as u64).to_be_bytes());
    }
    out.extend_from_slice(&mask);
    out.extend(payload.iter().enumerate().map(|(i, &b)| b ^ mask[i % 4]));
    out
}

/// Read one frame out of a buffer.
///
/// `Ok(None)` means the buffer holds the start of a frame and not all of it,
/// which is the ordinary case on a stream socket; the count returned with a
/// frame is how many bytes of the buffer it consumed.
pub fn decode_frame(buf: &[u8]) -> Result<Option<(Frame, usize)>, WsError> {
    if buf.len() < 2 {
        return Ok(None);
    }
    let first = buf[0];
    if first & 0x70 != 0 {
        return Err(WsError::Protocol(
            "a reserved bit is set, which means an extension that was not negotiated".into(),
        ));
    }
    let opcode = Opcode::from_bits(first & 0x0f)
        .ok_or_else(|| WsError::Protocol(format!("unknown opcode {}", first & 0x0f)))?;
    let fin = first & 0x80 != 0;
    let masked = buf[1] & 0x80 != 0;
    let short = (buf[1] & 0x7f) as usize;

    let (len, mut at) = match short {
        126 => {
            if buf.len() < 4 {
                return Ok(None);
            }
            (u16::from_be_bytes([buf[2], buf[3]]) as usize, 4)
        }
        127 => {
            if buf.len() < 10 {
                return Ok(None);
            }
            let mut eight = [0u8; 8];
            eight.copy_from_slice(&buf[2..10]);
            let len = u64::from_be_bytes(eight);
            if len > MAX_MESSAGE as u64 {
                return Err(WsError::Protocol(format!("a frame of {len} bytes")));
            }
            (len as usize, 10)
        }
        other => (other, 2),
    };
    if opcode.is_control() && (len > 125 || !fin) {
        return Err(WsError::Protocol(
            "a control frame that is fragmented or longer than 125 bytes".into(),
        ));
    }
    if len > MAX_MESSAGE {
        return Err(WsError::Protocol(format!("a frame of {len} bytes")));
    }

    let mask = if masked {
        if buf.len() < at + 4 {
            return Ok(None);
        }
        let mask = [buf[at], buf[at + 1], buf[at + 2], buf[at + 3]];
        at += 4;
        Some(mask)
    } else {
        None
    };
    if buf.len() < at + len {
        return Ok(None);
    }

    let mut payload = buf[at..at + len].to_vec();
    if let Some(mask) = mask {
        for (i, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[i % 4];
        }
    }
    Ok(Some((
        Frame {
            fin,
            opcode,
            payload,
        },
        at + len,
    )))
}

/// The upgrade request, as one string.
pub fn upgrade_request(host: &str, path: &str, key: &str) -> String {
    format!(
        "GET {path} HTTP/1.1\r\n\
         Host: {host}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: {key}\r\n\
         Sec-WebSocket-Version: 13\r\n\
         \r\n"
    )
}

/// What the server must echo for a given key.
pub fn accept_for(key: &str) -> String {
    base64::encode(&sha1(format!("{key}{GUID}").as_bytes()))
}

/// Check the response head against the key that was sent.
pub fn check_upgrade(head: &str, key: &str) -> Result<(), String> {
    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap_or_default();
    if !status.contains(" 101") {
        return Err(format!("the engine answered {status:?} instead of 101"));
    }
    let mut accept = None;
    let mut upgraded = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.to_ascii_lowercase().as_str() {
            "sec-websocket-accept" => accept = Some(value.to_string()),
            "upgrade" => upgraded = value.eq_ignore_ascii_case("websocket"),
            _ => {}
        }
    }
    if !upgraded {
        return Err("the engine did not upgrade the connection".to_string());
    }
    match accept {
        Some(accept) if accept == accept_for(key) => Ok(()),
        Some(accept) => Err(format!(
            "the engine's key answer {accept:?} is not the one for the key it was sent"
        )),
        None => Err("the engine sent no Sec-WebSocket-Accept".to_string()),
    }
}

/// Numbers that are not guessable enough to matter and not a dependency.
///
/// Masking keys have to be unpredictable to a *third* party, which on a
/// loopback socket to a child process is nobody. Seeding xorshift from
/// `/dev/urandom` costs one open at startup and settles the question anyway,
/// and falling back to the clock when there is no `/dev/urandom` keeps the
/// program working on a system that is too early in its boot to have one.
pub struct Keys {
    state: u64,
}

impl Default for Keys {
    fn default() -> Self {
        Keys::new()
    }
}

impl Keys {
    pub fn new() -> Keys {
        let mut seed = [0u8; 8];
        let seeded = std::fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut seed))
            .is_ok();
        let state = if seeded {
            u64::from_ne_bytes(seed)
        } else {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0x9e3779b97f4a7c15);
            now ^ ((std::process::id() as u64) << 32)
        };
        Keys { state: state | 1 }
    }

    fn next(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    /// A masking key.
    pub fn mask(&mut self) -> [u8; 4] {
        let bytes = self.next().to_ne_bytes();
        [bytes[0], bytes[1], bytes[2], bytes[3]]
    }

    /// The sixteen bytes of a `Sec-WebSocket-Key`, base64'd.
    pub fn handshake_key(&mut self) -> String {
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&self.next().to_ne_bytes());
        bytes[8..].copy_from_slice(&self.next().to_ne_bytes());
        base64::encode(&bytes)
    }
}

/// Split `ws://host:port/path` into its three parts.
pub fn split_url(url: &str) -> Result<(String, u16, String), String> {
    let rest = url
        .strip_prefix("ws://")
        .ok_or_else(|| format!("not a ws:// url: {url}"))?;
    let (authority, path) = match rest.find('/') {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, "/"),
    };
    let (host, port) = authority
        .rsplit_once(':')
        .ok_or_else(|| format!("no port in {url}"))?;
    let port: u16 = port
        .parse()
        .map_err(|_| format!("not a port number: {port}"))?;
    Ok((host.to_string(), port, path.to_string()))
}

/// The sending half. Held behind a mutex, because the reader thread answers
/// pings on it while the main thread sends commands.
pub struct Sender {
    stream: TcpStream,
    keys: Keys,
}

impl Sender {
    pub fn send(&mut self, opcode: Opcode, payload: &[u8]) -> Result<(), WsError> {
        let mask = self.keys.mask();
        let bytes = encode_frame(opcode, payload, mask);
        self.stream.write_all(&bytes)?;
        self.stream.flush()?;
        Ok(())
    }

    pub fn send_text(&mut self, text: &str) -> Result<(), WsError> {
        self.send(Opcode::Text, text.as_bytes())
    }

    /// A close with the normal status, best effort: the connection is going
    /// away either way and a failure here has nowhere useful to go.
    pub fn close(&mut self) {
        let _ = self.send(Opcode::Close, &1000u16.to_be_bytes());
        let _ = self.stream.shutdown(std::net::Shutdown::Write);
    }
}

/// The receiving half, which owns the reassembly buffer.
pub struct Receiver {
    stream: TcpStream,
    buf: Vec<u8>,
    /// The message being assembled out of fragments, and what it started as.
    partial: Option<(Opcode, Vec<u8>)>,
}

/// What came off the socket that the caller has to act on.
#[derive(Debug, PartialEq, Eq)]
pub enum Incoming {
    Text(String),
    Ping(Vec<u8>),
    /// Nothing complete yet; the caller should come back.
    Pending,
}

impl Receiver {
    /// Wait for the next thing, up to `timeout`.
    ///
    /// A timeout is [`Incoming::Pending`] rather than an error, so that the
    /// reader thread wakes often enough to notice it has been asked to stop
    /// without anything having to interrupt a blocking read.
    pub fn next(&mut self, timeout: Duration) -> Result<Incoming, WsError> {
        loop {
            if let Some(message) = self.take_message()? {
                return Ok(message);
            }
            self.stream.set_read_timeout(Some(timeout))?;
            let mut chunk = [0u8; 65536];
            match self.stream.read(&mut chunk) {
                Ok(0) => {
                    return Err(WsError::Closed("the socket ended".into()));
                }
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(err)
                    if matches!(
                        err.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    return Ok(Incoming::Pending)
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(WsError::Io(err)),
            }
        }
    }

    /// Pull frames out of the buffer until one completes a message.
    fn take_message(&mut self) -> Result<Option<Incoming>, WsError> {
        loop {
            let Some((frame, used)) = decode_frame(&self.buf)? else {
                return Ok(None);
            };
            self.buf.drain(..used);

            match frame.opcode {
                Opcode::Ping => return Ok(Some(Incoming::Ping(frame.payload))),
                Opcode::Pong => continue,
                Opcode::Close => {
                    let why = close_reason(&frame.payload);
                    return Err(WsError::Closed(why));
                }
                Opcode::Binary => {
                    return Err(WsError::Protocol(
                        "a binary frame, but CDP is text".to_string(),
                    ))
                }
                Opcode::Text | Opcode::Continuation => {
                    let (start, mut data) = match (frame.opcode, self.partial.take()) {
                        (Opcode::Text, Some(_)) => {
                            return Err(WsError::Protocol(
                                "a new message began before the last one finished".to_string(),
                            ))
                        }
                        (Opcode::Text, None) => (Opcode::Text, Vec::new()),
                        (_, Some(partial)) => partial,
                        (_, None) => {
                            return Err(WsError::Protocol(
                                "a continuation with nothing to continue".to_string(),
                            ))
                        }
                    };
                    data.extend_from_slice(&frame.payload);
                    if data.len() > MAX_MESSAGE {
                        return Err(WsError::Protocol(format!(
                            "a message of at least {} bytes",
                            data.len()
                        )));
                    }
                    if !frame.fin {
                        self.partial = Some((start, data));
                        continue;
                    }
                    let text = String::from_utf8(data)
                        .map_err(|_| WsError::Protocol("a text frame that is not UTF-8".into()))?;
                    return Ok(Some(Incoming::Text(text)));
                }
            }
        }
    }
}

/// The close code and reason, as a sentence.
fn close_reason(payload: &[u8]) -> String {
    if payload.len() < 2 {
        return "no reason given".to_string();
    }
    let code = u16::from_be_bytes([payload[0], payload[1]]);
    let why = String::from_utf8_lossy(&payload[2..]);
    if why.is_empty() {
        format!("code {code}")
    } else {
        format!("code {code}, {why}")
    }
}

/// Connect, upgrade, and hand back the two halves.
pub fn connect(url: &str, timeout: Duration) -> Result<(Sender, Receiver), String> {
    let (host, port, path) = split_url(url)?;
    let address = format!("{host}:{port}");
    let addr = crate::http::resolve(&address)?;
    let stream = TcpStream::connect_timeout(&addr, timeout)
        .map_err(|e| format!("cannot reach the engine at {address}: {e}"))?;
    stream.set_nodelay(true).ok();
    stream.set_write_timeout(Some(timeout)).ok();
    stream.set_read_timeout(Some(timeout)).ok();

    let mut keys = Keys::new();
    let key = keys.handshake_key();
    let mut sending = stream
        .try_clone()
        .map_err(|e| format!("cannot use the socket twice: {e}"))?;
    sending
        .write_all(upgrade_request(&address, &path, &key).as_bytes())
        .map_err(|e| format!("cannot send the upgrade: {e}"))?;

    let (head, leftover) = crate::http::read_head(&stream, timeout)
        .map_err(|e| format!("cannot read the upgrade answer: {e}"))?;
    check_upgrade(&head, &key)?;

    Ok((
        Sender {
            stream: sending,
            keys,
        },
        Receiver {
            stream,
            buf: leftover,
            partial: None,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(payload: &[u8]) -> Frame {
        let bytes = encode_frame(Opcode::Text, payload, [0x37, 0xfa, 0x21, 0x3d]);
        let (frame, used) = decode_frame(&bytes).unwrap().expect("a whole frame");
        assert_eq!(used, bytes.len());
        frame
    }

    #[test]
    fn the_three_length_forms() {
        for len in [0usize, 1, 125, 126, 127, 65535, 65536, 70000] {
            let payload: Vec<u8> = (0..len).map(|i| i as u8).collect();
            let frame = round_trip(&payload);
            assert_eq!(frame.payload, payload, "{len} bytes");
            assert!(frame.fin);
            assert_eq!(frame.opcode, Opcode::Text);
        }
        // And the header sizes those forms imply.
        assert_eq!(encode_frame(Opcode::Text, &[], [0; 4]).len(), 6);
        assert_eq!(encode_frame(Opcode::Text, &[0; 126], [0; 4]).len(), 8 + 126);
        assert_eq!(
            encode_frame(Opcode::Text, &[0; 70000], [0; 4]).len(),
            14 + 70000
        );
    }

    #[test]
    fn what_a_client_sends_is_always_masked() {
        let bytes = encode_frame(Opcode::Text, b"hello", [1, 2, 3, 4]);
        assert_eq!(bytes[1] & 0x80, 0x80, "the mask bit");
        assert_eq!(&bytes[2..6], &[1, 2, 3, 4]);
        assert_ne!(
            &bytes[6..],
            b"hello",
            "the payload must not go out in clear"
        );
        let unmasked: Vec<u8> = bytes[6..]
            .iter()
            .enumerate()
            .map(|(i, &b)| b ^ [1, 2, 3, 4][i % 4])
            .collect();
        assert_eq!(unmasked, b"hello");
    }

    /// The RFC's own example of a masked frame, byte for byte.
    #[test]
    fn the_rfc_example_frame() {
        assert_eq!(
            encode_frame(Opcode::Text, b"Hello", [0x37, 0xfa, 0x21, 0x3d]),
            vec![0x81, 0x85, 0x37, 0xfa, 0x21, 0x3d, 0x7f, 0x9f, 0x4d, 0x51, 0x58]
        );
    }

    #[test]
    fn a_frame_that_has_not_all_arrived_is_not_a_frame_yet() {
        let bytes = encode_frame(Opcode::Text, &[7u8; 300], [9, 9, 9, 9]);
        for cut in [0, 1, 2, 3, 7, 8, 100, bytes.len() - 1] {
            assert_eq!(decode_frame(&bytes[..cut]).unwrap(), None, "cut at {cut}");
        }
        assert!(decode_frame(&bytes).unwrap().is_some());
    }

    #[test]
    fn a_message_can_arrive_in_pieces_with_a_ping_between_them() {
        let mut receiver = Receiver {
            stream: loopback(),
            buf: Vec::new(),
            partial: None,
        };
        let mut wire = Vec::new();
        wire.extend_from_slice(&server_frame(false, Opcode::Text, b"{\"a\":"));
        wire.extend_from_slice(&server_frame(true, Opcode::Ping, b"beat"));
        wire.extend_from_slice(&server_frame(true, Opcode::Continuation, b"1}"));
        receiver.buf = wire;

        assert_eq!(
            receiver.take_message().unwrap(),
            Some(Incoming::Ping(b"beat".to_vec()))
        );
        assert_eq!(
            receiver.take_message().unwrap(),
            Some(Incoming::Text("{\"a\":1}".to_string()))
        );
        assert_eq!(receiver.take_message().unwrap(), None);
    }

    #[test]
    fn a_close_frame_ends_the_connection_with_its_reason() {
        let mut receiver = Receiver {
            stream: loopback(),
            buf: server_frame(true, Opcode::Close, b"\x03\xe8bye"),
            partial: None,
        };
        match receiver.take_message() {
            Err(WsError::Closed(why)) => assert_eq!(why, "code 1000, bye"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn frames_the_protocol_does_not_allow_are_refused() {
        // A reserved bit, which means an extension nobody negotiated.
        assert!(decode_frame(&[0xc1, 0x00]).is_err());
        // An unknown opcode.
        assert!(decode_frame(&[0x85, 0x00]).is_err());
        // A fragmented ping.
        assert!(decode_frame(&[0x09, 0x00]).is_err());
        // A 300-byte ping.
        assert!(decode_frame(&[0x89, 126, 1, 44]).is_err());
    }

    #[test]
    fn the_accept_check_is_the_one_from_the_rfc() {
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        assert_eq!(accept_for(key), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
        let good = "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\
                    Connection: Upgrade\r\nSec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n";
        assert_eq!(check_upgrade(good, key), Ok(()));

        let wrong = good.replace(
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAA=",
        );
        assert!(check_upgrade(&wrong, key).is_err());
        assert!(check_upgrade("HTTP/1.1 500 Internal Error\r\n", key).is_err());
        assert!(check_upgrade("HTTP/1.1 101 Switching Protocols\r\n", key).is_err());
    }

    #[test]
    fn the_request_names_the_host_and_the_key() {
        let request = upgrade_request("127.0.0.1:9222", "/devtools/page/AB", "KEY");
        assert!(request.starts_with("GET /devtools/page/AB HTTP/1.1\r\n"));
        assert!(request.contains("Host: 127.0.0.1:9222\r\n"));
        assert!(request.contains("Sec-WebSocket-Key: KEY\r\n"));
        assert!(request.contains("Sec-WebSocket-Version: 13\r\n"));
        assert!(request.ends_with("\r\n\r\n"));
    }

    #[test]
    fn urls_split_into_host_port_and_path() {
        assert_eq!(
            split_url("ws://127.0.0.1:9222/devtools/page/ABC"),
            Ok((
                "127.0.0.1".to_string(),
                9222,
                "/devtools/page/ABC".to_string()
            ))
        );
        assert_eq!(
            split_url("ws://127.0.0.1:1/"),
            Ok(("127.0.0.1".to_string(), 1, "/".to_string()))
        );
        assert!(split_url("http://127.0.0.1:9222/").is_err());
        assert!(split_url("ws://127.0.0.1/devtools").is_err());
        assert!(split_url("ws://127.0.0.1:notaport/x").is_err());
    }

    #[test]
    fn masking_keys_are_not_all_the_same() {
        let mut keys = Keys::new();
        let first = keys.mask();
        assert!((0..8).any(|_| keys.mask() != first));
        assert_eq!(
            base64::decode(keys.handshake_key().as_bytes())
                .unwrap()
                .len(),
            16
        );
    }

    /// A socket that exists so a `Receiver` can be built in a test; nothing
    /// reads or writes it.
    fn loopback() -> TcpStream {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        TcpStream::connect(listener.local_addr().expect("addr")).expect("connect")
    }

    /// A frame the way a server sends one: unmasked.
    fn server_frame(fin: bool, opcode: Opcode, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![
            if fin { 0x80 } else { 0 } | opcode.bits(),
            payload.len() as u8,
        ];
        out.extend_from_slice(payload);
        out
    }
}
