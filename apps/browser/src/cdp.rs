//! Talking to the engine: commands out, events in.
//!
//! The DevTools protocol is two streams sharing one socket. Commands carry an
//! `id` and come back as a reply with the same `id`; events arrive whenever
//! the engine has something to say and are addressed to nobody. A client has
//! to serve both without one starving the other, and it has to do it while the
//! main thread is sitting in `poll` waiting for a key.
//!
//! So: a thread owns the socket's reading half and sorts what arrives into
//! replies and events; the main thread takes commands and drains events. The
//! two meet at a mutex and a condition variable, and — this is the part that
//! matters for a program with a terminal — at a pipe. The reader writes one
//! byte to the pipe whenever something arrives, so the main loop can wait on
//! the terminal and on the engine in a single `poll` instead of choosing which
//! one to block on or spinning between them.
//!
//! Every command has a deadline. An engine that stops answering is the failure
//! this program was written around, and a client that waits forever for a
//! reply turns it into a pane that cannot even be quit.

use std::collections::{HashMap, VecDeque};
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::json::Json;
use crate::ws::{self, Incoming, Opcode, Sender};

/// How long a command may take before it is a failure rather than a wait.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(15);

/// An event as it arrived: the method and its parameters.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub method: String,
    pub params: Json,
}

#[derive(Default)]
struct Mailbox {
    replies: HashMap<i64, Json>,
    events: VecDeque<Event>,
    /// Set once the socket has gone, with the reason.
    ended: Option<String>,
}

/// A connection to one CDP target.
pub struct Client {
    sender: Arc<Mutex<Sender>>,
    mailbox: Arc<(Mutex<Mailbox>, Condvar)>,
    stop: Arc<AtomicBool>,
    /// Read end of the pipe the reader thread knocks on.
    wake_read: RawFd,
    wake_write: RawFd,
    next_id: i64,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl Client {
    /// Connect to a target's WebSocket url and start reading it.
    pub fn connect(url: &str, timeout: Duration) -> Result<Client, String> {
        let (sender, mut receiver) = ws::connect(url, timeout)?;
        let sender = Arc::new(Mutex::new(sender));
        let mailbox = Arc::new((Mutex::new(Mailbox::default()), Condvar::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let mut fds = [0 as RawFd; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(format!(
                "cannot make a pipe to wake the loop: {}",
                std::io::Error::last_os_error()
            ));
        }
        let (wake_read, wake_write) = (fds[0], fds[1]);
        // Non-blocking on both ends: the reader must never block on a pipe the
        // main loop has not drained, and the main loop must never block
        // draining one that is already empty.
        tos_platform::tty::set_nonblocking(wake_read).ok();
        tos_platform::tty::set_nonblocking(wake_write).ok();

        let thread_mailbox = Arc::clone(&mailbox);
        let thread_sender = Arc::clone(&sender);
        let thread_stop = Arc::clone(&stop);
        let reader = std::thread::spawn(move || {
            let ended = loop {
                if thread_stop.load(Ordering::SeqCst) {
                    break "the connection was closed from this end".to_string();
                }
                match receiver.next(Duration::from_millis(200)) {
                    Ok(Incoming::Pending) => continue,
                    Ok(Incoming::Ping(payload)) => {
                        if let Ok(mut sender) = thread_sender.lock() {
                            let _ = sender.send(Opcode::Pong, &payload);
                        }
                    }
                    Ok(Incoming::Text(text)) => {
                        let (lock, signal) = &*thread_mailbox;
                        if let Ok(mut mailbox) = lock.lock() {
                            sort(&mut mailbox, &text);
                        }
                        signal.notify_all();
                        // One byte, best effort: a full pipe already means the
                        // main loop has been told.
                        let byte = b"\x01";
                        unsafe {
                            libc::write(wake_write, byte.as_ptr() as *const libc::c_void, 1);
                        }
                    }
                    Err(err) => break err.to_string(),
                }
            };
            let (lock, signal) = &*thread_mailbox;
            if let Ok(mut mailbox) = lock.lock() {
                mailbox.ended.get_or_insert(ended);
            }
            signal.notify_all();
            let byte = b"\x01";
            unsafe {
                libc::write(wake_write, byte.as_ptr() as *const libc::c_void, 1);
            }
        });

        Ok(Client {
            sender,
            mailbox,
            stop,
            wake_read,
            wake_write,
            next_id: 0,
            reader: Some(reader),
        })
    }

    /// The descriptor to poll alongside the terminal.
    pub fn wake_fd(&self) -> RawFd {
        self.wake_read
    }

    /// Empty the wake pipe. What it held is only ever "look in the mailbox".
    pub fn drain_wake(&self) {
        let mut buf = [0u8; 256];
        while unsafe {
            libc::read(
                self.wake_read,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
            )
        } > 0
        {}
    }

    /// Send a command and wait for its reply.
    pub fn call(&mut self, method: &str, params: Json) -> Result<Json, String> {
        self.call_within(method, params, CALL_TIMEOUT)
    }

    /// The same, with a deadline of the caller's choosing.
    pub fn call_within(
        &mut self,
        method: &str,
        params: Json,
        timeout: Duration,
    ) -> Result<Json, String> {
        self.next_id += 1;
        let id = self.next_id;
        let message = Json::object(vec![
            ("id", Json::number(id as f64)),
            ("method", Json::string(method)),
            ("params", params),
        ])
        .to_string();

        {
            let mut sender = self
                .sender
                .lock()
                .map_err(|_| "the connection is poisoned".to_string())?;
            sender
                .send_text(&message)
                .map_err(|e| format!("cannot send {method}: {e}"))?;
        }

        let deadline = Instant::now() + timeout;
        let (lock, signal) = &*self.mailbox;
        let mut mailbox = lock
            .lock()
            .map_err(|_| "the connection is poisoned".to_string())?;
        loop {
            if let Some(reply) = mailbox.replies.remove(&id) {
                if let Some(error) = reply.get("error") {
                    let what = error
                        .get("message")
                        .and_then(Json::as_str)
                        .unwrap_or("the engine refused it");
                    return Err(format!("{method}: {what}"));
                }
                return Ok(reply.get("result").cloned().unwrap_or(Json::Null));
            }
            if let Some(ended) = &mailbox.ended {
                return Err(format!("{method}: {ended}"));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(format!(
                    "{method}: no answer in {} seconds",
                    timeout.as_secs()
                ));
            }
            let (next, _) = signal
                .wait_timeout(mailbox, left)
                .map_err(|_| "the connection is poisoned".to_string())?;
            mailbox = next;
        }
    }

    /// Send a command and do not wait for its reply.
    ///
    /// For the acknowledgement of a screencast frame, which has to happen
    /// sixty times a second and whose reply says nothing: waiting for it would
    /// put a round trip between every pair of frames.
    pub fn notify(&mut self, method: &str, params: Json) -> Result<(), String> {
        self.next_id += 1;
        let message = Json::object(vec![
            ("id", Json::number(self.next_id as f64)),
            ("method", Json::string(method)),
            ("params", params),
        ])
        .to_string();
        let mut sender = self
            .sender
            .lock()
            .map_err(|_| "the connection is poisoned".to_string())?;
        sender
            .send_text(&message)
            .map_err(|e| format!("cannot send {method}: {e}"))
    }

    /// Take every event that has arrived since the last time.
    pub fn events(&self) -> Vec<Event> {
        let (lock, _) = &*self.mailbox;
        match lock.lock() {
            Ok(mut mailbox) => mailbox.events.drain(..).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// The reason the connection ended, if it has.
    pub fn ended(&self) -> Option<String> {
        let (lock, _) = &*self.mailbox;
        lock.lock().ok().and_then(|mailbox| mailbox.ended.clone())
    }

    /// Close politely and stop the reader thread.
    pub fn close(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Ok(mut sender) = self.sender.lock() {
            sender.close();
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.close();
        unsafe {
            libc::close(self.wake_read);
            libc::close(self.wake_write);
        }
    }
}

/// Put one message where it belongs.
///
/// A message that is neither — not JSON, or an object with no `id` and no
/// `method` — is dropped. There is nothing useful to do with it and a
/// connection is not worth ending over one.
fn sort(mailbox: &mut Mailbox, text: &str) {
    let Ok(value) = Json::parse(text) else {
        return;
    };
    if let Some(id) = value.get("id").and_then(Json::as_i64) {
        mailbox.replies.insert(id, value);
        return;
    }
    if let Some(method) = value.get("method").and_then(Json::as_str) {
        let event = Event {
            method: method.to_string(),
            params: value.get("params").cloned().unwrap_or(Json::Null),
        };
        // A page that is repainting can produce events faster than a pane can
        // draw them. The queue is bounded so that a slow frame cannot become
        // unbounded memory; what goes is the oldest, because the newest frame
        // is the one worth having.
        if mailbox.events.len() >= 512 {
            mailbox.events.pop_front();
        }
        mailbox.events.push_back(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reply_goes_to_the_call_that_asked_and_an_event_to_the_queue() {
        let mut mailbox = Mailbox::default();
        sort(&mut mailbox, r#"{"id":4,"result":{"frameId":"A"}}"#);
        sort(
            &mut mailbox,
            r#"{"method":"Page.screencastFrame","params":{"data":"x","sessionId":9}}"#,
        );
        sort(&mut mailbox, "not json at all");
        sort(&mut mailbox, r#"{"neither":true}"#);

        assert_eq!(
            mailbox.replies.remove(&4).and_then(|r| r
                .path(&["result", "frameId"])
                .and_then(Json::as_str)
                .map(str::to_string)),
            Some("A".to_string())
        );
        assert_eq!(mailbox.events.len(), 1);
        assert_eq!(mailbox.events[0].method, "Page.screencastFrame");
        assert_eq!(
            mailbox.events[0]
                .params
                .get("sessionId")
                .and_then(Json::as_i64),
            Some(9)
        );
    }

    #[test]
    fn the_event_queue_drops_the_oldest_rather_than_growing_forever() {
        let mut mailbox = Mailbox::default();
        for i in 0..600 {
            sort(
                &mut mailbox,
                &format!(r#"{{"method":"Page.screencastFrame","params":{{"n":{i}}}}}"#),
            );
        }
        assert_eq!(mailbox.events.len(), 512);
        assert_eq!(
            mailbox.events[0].params.get("n").and_then(Json::as_i64),
            Some(88),
            "the newest frames are the ones kept"
        );
    }

    #[test]
    fn an_error_reply_is_an_error_and_not_a_result() {
        let mut mailbox = Mailbox::default();
        sort(
            &mut mailbox,
            r#"{"id":1,"error":{"code":-32000,"message":"Cannot navigate to invalid URL"}}"#,
        );
        let reply = mailbox.replies.remove(&1).expect("a reply");
        assert_eq!(
            reply.path(&["error", "message"]).and_then(Json::as_str),
            Some("Cannot navigate to invalid URL")
        );
    }
}
