//! One HTTP GET, to loopback, with a deadline.
//!
//! The engine publishes its targets over plain HTTP — `/json/version` and
//! `/json/list` — and the WebSocket url of the page to drive is in the second
//! of them. That is the whole of this program's use for HTTP: one request, to
//! a port on this machine, whose answer is a few kilobytes of JSON. So this is
//! a request builder, a header parser and a body reader, and not a client: no
//! redirects, no chunked transfer (the engine sends `Content-Length`), no TLS,
//! no connection reuse, no `https`. Anything else in the answer is a reason to
//! say what happened and stop.
//!
//! The deadline is the point. A Chromium started without
//! `--ozone-platform=headless` opens its port and then never answers on it,
//! which is a hang with no error and no output — the failure this program is
//! most likely to hit and the one hardest to diagnose from the outside. Every
//! wait here is bounded, so that failure comes back as a sentence.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

/// Turn `host:port` into an address, preferring IPv4.
///
/// `127.0.0.1` is what Chromium binds and what it prints, so this is usually a
/// parse rather than a lookup. `localhost` is resolved through the system,
/// where it may answer `::1` first — an address the engine is not listening
/// on — so IPv4 is taken when both are offered.
pub fn resolve(address: &str) -> Result<SocketAddr, String> {
    let mut addresses: Vec<SocketAddr> = address
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve {address}: {e}"))?
        .collect();
    addresses.sort_by_key(|a| !a.is_ipv4());
    addresses
        .into_iter()
        .next()
        .ok_or_else(|| format!("{address} resolves to nothing"))
}

/// Read up to and including the blank line that ends a response head.
///
/// Returns the head as text and whatever bytes came after it, which for an
/// upgraded connection is the first frames and must not be thrown away.
pub fn read_head(stream: &TcpStream, timeout: Duration) -> std::io::Result<(String, Vec<u8>)> {
    let deadline = Instant::now() + timeout;
    let mut stream = stream;
    let mut buf = Vec::new();
    loop {
        if let Some(end) = find(&buf, b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..end + 2]).into_owned();
            return Ok((head, buf[end + 4..].to_vec()));
        }
        if buf.len() > 64 << 10 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "a response head of more than 64 kB",
            ));
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "the engine did not finish answering",
            ));
        }
        stream.set_read_timeout(Some(left))?;
        let mut chunk = [0u8; 4096];
        match stream.read(&mut chunk) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "the engine closed before the head was complete",
                ))
            }
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
}

/// `GET path` from `host:port`, with everything bounded by `timeout`.
pub fn get(address: &str, path: &str, timeout: Duration) -> Result<String, String> {
    let deadline = Instant::now() + timeout;
    let addr = resolve(address)?;
    let mut stream = TcpStream::connect_timeout(&addr, timeout)
        .map_err(|e| format!("cannot reach {address}: {e}"))?;
    stream.set_write_timeout(Some(timeout)).ok();
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {address}\r\nAccept: application/json\r\n\
         Connection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("cannot ask {address} for {path}: {e}"))?;

    let left = deadline.saturating_duration_since(Instant::now());
    let (head, mut body) = read_head(&stream, left).map_err(|e| format!("{address}{path}: {e}"))?;
    let status = head.lines().next().unwrap_or_default();
    if !status.contains(" 200") {
        return Err(format!("{address}{path} answered {status:?}"));
    }
    let length = content_length(&head);

    loop {
        if let Some(length) = length {
            if body.len() >= length {
                body.truncate(length);
                break;
            }
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(format!("{address}{path} did not finish answering"));
        }
        stream.set_read_timeout(Some(left)).ok();
        let mut chunk = [0u8; 8192];
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => body.extend_from_slice(&chunk[..n]),
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(format!("{address}{path}: {err}")),
        }
        if body.len() > 8 << 20 {
            return Err(format!("{address}{path} answered with more than 8 MB"));
        }
    }

    String::from_utf8(body).map_err(|_| format!("{address}{path} answered with invalid UTF-8"))
}

/// The `Content-Length` of a response head, if it gave one.
pub fn content_length(head: &str) -> Option<usize> {
    for line in head.split("\r\n") {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                return value.trim().parse().ok();
            }
        }
    }
    None
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// A server that answers one request with whatever bytes it was given.
    fn serve_once(response: &'static [u8]) -> String {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let address = listener.local_addr().expect("addr").to_string();
        std::thread::spawn(move || {
            if let Ok((mut socket, _)) = listener.accept() {
                let mut request = [0u8; 1024];
                let _ = socket.read(&mut request);
                let _ = socket.write_all(response);
            }
        });
        address
    }

    #[test]
    fn a_json_answer_comes_back_as_its_body() {
        let address = serve_once(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 13\r\n\r\n\
              {\"ok\":true}\r\n",
        );
        assert_eq!(
            get(&address, "/json/list", Duration::from_secs(5)),
            Ok("{\"ok\":true}\r\n".to_string())
        );
    }

    #[test]
    fn a_body_that_ends_with_the_connection_is_still_a_body() {
        let address = serve_once(b"HTTP/1.1 200 OK\r\n\r\n[]");
        assert_eq!(
            get(&address, "/json/list", Duration::from_secs(5)),
            Ok("[]".to_string())
        );
    }

    #[test]
    fn anything_but_200_is_an_error_that_says_so() {
        let address = serve_once(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
        let failed = get(&address, "/json/list", Duration::from_secs(5)).unwrap_err();
        assert!(failed.contains("404"), "{failed}");
    }

    #[test]
    fn a_server_that_never_answers_is_a_timeout_and_not_a_hang() {
        // Accepted and then ignored, which is what a Chromium without a
        // headless platform does to every request it is sent.
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let address = listener.local_addr().expect("addr").to_string();
        let held = std::thread::spawn(move || listener.accept().map(|(socket, _)| socket));

        let started = Instant::now();
        assert!(get(&address, "/json/version", Duration::from_millis(300)).is_err());
        assert!(started.elapsed() < Duration::from_secs(3));
        drop(held.join());
    }

    #[test]
    fn the_head_is_split_off_and_the_rest_is_kept() {
        let address = serve_once(b"HTTP/1.1 101 Switching\r\nUpgrade: websocket\r\n\r\n\x81\x02hi");
        let stream = TcpStream::connect(resolve(&address).unwrap()).expect("connect");
        (&stream)
            .write_all(b"GET / HTTP/1.1\r\n\r\n")
            .expect("write");
        let (head, rest) = read_head(&stream, Duration::from_secs(5)).expect("head");
        assert!(head.starts_with("HTTP/1.1 101 Switching\r\n"));
        assert!(head.ends_with("Upgrade: websocket\r\n"));
        assert_eq!(rest, b"\x81\x02hi");
    }

    #[test]
    fn content_length_is_found_whatever_its_spelling() {
        let head = "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\ncontent-length: 42\r\n";
        assert_eq!(content_length(head), Some(42));
        assert_eq!(content_length("HTTP/1.1 200 OK\r\n"), None);
    }

    #[test]
    fn loopback_resolves_to_a_v4_address() {
        let addr = resolve("localhost:9222").or_else(|_| resolve("127.0.0.1:9222"));
        assert!(addr.expect("resolves").is_ipv4());
    }
}
