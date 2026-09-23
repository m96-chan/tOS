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
//!
//! The mailbox keeps a reply only while somebody is coming back for it. Most
//! of what this program says to the engine is said with [`Client::notify`] —
//! the acknowledgement of a screencast frame, sixty times a second, a
//! `mouseWheel` nine times a notch, a key as it is pressed — and Chromium
//! answers every one of them whether or not the answer is worth anything. A
//! mailbox that filed all of those would grow for as long as the pane is open:
//! an hour of reading is hundreds of thousands of replies nobody will ever
//! ask for. So the id of a command whose reply *will* be collected is
//! registered before the command goes out, and the reader thread keeps a reply
//! only if it finds its id there — a hash lookup on the hot path, and nothing
//! kept for the rest. What registers an id gives it back: a [`Pending`] that
//! is dropped without its reply, and a call that gives up waiting, both leave
//! the mailbox as they found it.

use std::collections::{HashMap, HashSet, VecDeque};
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::json::Json;
use crate::ws::{self, Incoming, Opcode, Sender};

/// How long a command may take before it is a failure rather than a wait.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(15);

/// A command that has gone out and whose reply has not been taken yet.
///
/// The method travels with the id so that a reply collected long after the
/// fact says what it was a reply to, exactly as [`Client::call`]'s errors do.
///
/// It carries the mailbox too, because a command that is given up on is the
/// ordinary case — a tab switched away from while the engine was drawing, a
/// still that timed out — and the reply to it must not be kept. Dropping this
/// is what says so: the id stops being wanted, and a reply that arrived in the
/// meantime goes with it. That is also why it is neither `Clone` nor `Eq`;
/// two of these for one id would be two claims on one reply, and the first
/// drop would cancel the second.
pub struct Pending {
    id: i64,
    method: String,
    mailbox: Arc<(Mutex<Mailbox>, Condvar)>,
}

impl Drop for Pending {
    fn drop(&mut self) {
        let (lock, _) = &*self.mailbox;
        if let Ok(mut mailbox) = lock.lock() {
            mailbox.forget(self.id);
        }
    }
}

impl std::fmt::Debug for Pending {
    /// The command, not the mailbox behind it.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pending")
            .field("id", &self.id)
            .field("method", &self.method)
            .finish()
    }
}

/// An event as it arrived: the method and its parameters.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub method: String,
    pub params: Json,
}

/// What has arrived, and who is still waiting for it.
#[derive(Default)]
struct Mailbox {
    /// The ids somebody has said they will come back for.
    ///
    /// A reply whose id is not here is a reply nobody asked to keep, and it is
    /// dropped where it is read. This is the whole of what bounds the map.
    wanted: HashSet<i64>,
    replies: HashMap<i64, Json>,
    events: VecDeque<Event>,
    /// Set once the socket has gone, with the reason.
    ended: Option<String>,
}

impl Mailbox {
    /// Say that the reply to this id is going to be collected.
    ///
    /// Said before the command goes out, so that there is no window in which a
    /// reply could arrive at a mailbox not yet willing to keep it.
    fn want(&mut self, id: i64) {
        self.wanted.insert(id);
    }

    /// Stop waiting for this id, and throw away a reply that beat the giving
    /// up. Forgetting something already forgotten is nothing.
    fn forget(&mut self, id: i64) {
        self.wanted.remove(&id);
        self.replies.remove(&id);
    }

    /// The reply to this id if it has come; taking it ends the waiting.
    fn take(&mut self, id: i64) -> Option<Json> {
        let reply = self.replies.remove(&id)?;
        self.wanted.remove(&id);
        Some(reply)
    }
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
        let message = message(id, method, params);

        self.want(id)?;
        if let Err(why) = self.transmit(&message) {
            self.forget(id);
            return Err(format!("cannot send {method}: {why}"));
        }

        let deadline = Instant::now() + timeout;
        let (lock, signal) = &*self.mailbox;
        let mut mailbox = lock
            .lock()
            .map_err(|_| "the connection is poisoned".to_string())?;
        loop {
            if let Some(reply) = mailbox.take(id) {
                return outcome(method, &reply);
            }
            if let Some(ended) = mailbox.ended.clone() {
                mailbox.forget(id);
                return Err(format!("{method}: {ended}"));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                // Nothing is coming that anybody will read. A reply after this
                // is the engine answering a question that has been withdrawn.
                mailbox.forget(id);
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

    /// Send a command and come back for the reply later.
    ///
    /// [`Client::call`] is the right shape for everything on a person's
    /// critical path — a navigation, a history entry — because there is
    /// nothing useful to do until the engine has answered. It is the wrong
    /// shape for the lossless still: that is tens of milliseconds of engine at
    /// a pane's size, and a loop that sits in `call` for them is a loop that
    /// is not reading the terminal. So the command goes out here and the reply
    /// is collected by [`Client::take_reply`] on whichever pass it has
    /// arrived on.
    ///
    /// Nothing new is needed underneath: a reply is filed in the mailbox under
    /// its own id by the same reader thread, and the same wake pipe knocks for
    /// it, so a `poll` that came back for an event comes back for this too.
    ///
    /// The returned [`Pending`] is the claim on that reply, and dropping it
    /// withdraws the claim: a caller that stops caring — the tab was switched
    /// away from, the still took too long — has only to let it go.
    pub fn send(&mut self, method: &str, params: Json) -> Result<Pending, String> {
        self.next_id += 1;
        let id = self.next_id;
        let message = message(id, method, params);
        self.want(id)?;
        if let Err(why) = self.transmit(&message) {
            self.forget(id);
            return Err(format!("cannot send {method}: {why}"));
        }
        Ok(Pending {
            id,
            method: method.to_string(),
            mailbox: Arc::clone(&self.mailbox),
        })
    }

    /// The reply to a [`Client::send`], if it has come.
    ///
    /// `None` is "not yet, ask again" and nothing else: the caller keeps the
    /// [`Pending`] and its own deadline, because how long a command is worth
    /// waiting for is the caller's decision rather than this module's. A
    /// connection that has ended answers straight away, with why, so that a
    /// caller is never left asking a socket that is gone.
    pub fn take_reply(&self, pending: &Pending) -> Option<Result<Json, String>> {
        let (lock, _) = &*self.mailbox;
        let mut mailbox = lock.lock().ok()?;
        if let Some(reply) = mailbox.take(pending.id) {
            return Some(outcome(&pending.method, &reply));
        }
        let ended = mailbox.ended.clone()?;
        // A dead socket answers nothing further, so the wait ends here rather
        // than at the drop.
        mailbox.forget(pending.id);
        Some(Err(format!("{}: {ended}", pending.method)))
    }

    /// Send a command and do not wait for its reply.
    ///
    /// For the acknowledgement of a screencast frame, which has to happen
    /// sixty times a second and whose reply says nothing: waiting for it would
    /// put a round trip between every pair of frames.
    ///
    /// The id is spent and never registered, so the reply Chromium sends all
    /// the same is read off the socket and dropped. This is the command that
    /// runs all day, and nothing it does is kept.
    pub fn notify(&mut self, method: &str, params: Json) -> Result<(), String> {
        self.next_id += 1;
        let message = message(self.next_id, method, params);
        self.transmit(&message)
            .map_err(|why| format!("cannot send {method}: {why}"))
    }

    /// Register an id as one whose reply is going to be collected.
    fn want(&self, id: i64) -> Result<(), String> {
        let (lock, _) = &*self.mailbox;
        let mut mailbox = lock
            .lock()
            .map_err(|_| "the connection is poisoned".to_string())?;
        mailbox.want(id);
        Ok(())
    }

    /// Give an id back: nobody is coming for its reply after all.
    fn forget(&self, id: i64) {
        let (lock, _) = &*self.mailbox;
        if let Ok(mut mailbox) = lock.lock() {
            mailbox.forget(id);
        }
    }

    /// Put one message on the wire.
    fn transmit(&self, message: &str) -> Result<(), String> {
        let mut sender = self
            .sender
            .lock()
            .map_err(|_| "the connection is poisoned".to_string())?;
        sender.send_text(message).map_err(|e| e.to_string())
    }

    /// How many replies the mailbox is holding for somebody to collect.
    ///
    /// A client with nothing in flight holds none, however long it has been
    /// running and however many frames it has acknowledged. That is the whole
    /// claim this module makes about its memory, so it is worth being able to
    /// ask.
    pub fn replies_held(&self) -> usize {
        let (lock, _) = &*self.mailbox;
        lock.lock()
            .map(|mailbox| mailbox.replies.len())
            .unwrap_or(0)
    }

    /// How many commands are still expecting a reply to be kept for them.
    pub fn replies_wanted(&self) -> usize {
        let (lock, _) = &*self.mailbox;
        lock.lock().map(|mailbox| mailbox.wanted.len()).unwrap_or(0)
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

/// One command on the wire: the id it will be answered under, and what it asks.
fn message(id: i64, method: &str, params: Json) -> String {
    Json::object(vec![
        ("id", Json::number(id as f64)),
        ("method", Json::string(method)),
        ("params", params),
    ])
    .to_string()
}

/// What one reply means: the result, or the engine's refusal in words.
fn outcome(method: &str, reply: &Json) -> Result<Json, String> {
    if let Some(error) = reply.get("error") {
        let what = error
            .get("message")
            .and_then(Json::as_str)
            .unwrap_or("the engine refused it");
        return Err(format!("{method}: {what}"));
    }
    Ok(reply.get("result").cloned().unwrap_or(Json::Null))
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
        // The one lookup that stands between an hour of browsing and a map of
        // hundreds of thousands of answers to questions nobody asked.
        if mailbox.wanted.contains(&id) {
            mailbox.replies.insert(id, value);
        }
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

    /// A mailbox and a [`Pending`] on it, as `Client::send` would leave them.
    fn awaiting(id: i64, method: &str) -> (Arc<(Mutex<Mailbox>, Condvar)>, Pending) {
        let mailbox = Arc::new((Mutex::new(Mailbox::default()), Condvar::new()));
        mailbox.0.lock().expect("a mailbox").want(id);
        let pending = Pending {
            id,
            method: method.to_string(),
            mailbox: Arc::clone(&mailbox),
        };
        (mailbox, pending)
    }

    /// How much the mailbox is holding on to: replies, and claims on them.
    fn held(mailbox: &Arc<(Mutex<Mailbox>, Condvar)>) -> (usize, usize) {
        let mailbox = mailbox.0.lock().expect("a mailbox");
        (mailbox.replies.len(), mailbox.wanted.len())
    }

    #[test]
    fn a_reply_goes_to_the_call_that_asked_and_an_event_to_the_queue() {
        let mut mailbox = Mailbox::default();
        mailbox.want(4);
        sort(&mut mailbox, r#"{"id":4,"result":{"frameId":"A"}}"#);
        sort(
            &mut mailbox,
            r#"{"method":"Page.screencastFrame","params":{"data":"x","sessionId":9}}"#,
        );
        sort(&mut mailbox, "not json at all");
        sort(&mut mailbox, r#"{"neither":true}"#);

        assert_eq!(
            mailbox.take(4).and_then(|r| r
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
        mailbox.want(1);
        sort(
            &mut mailbox,
            r#"{"id":1,"error":{"code":-32000,"message":"Cannot navigate to invalid URL"}}"#,
        );
        let reply = mailbox.take(1).expect("a reply");
        assert_eq!(
            reply.path(&["error", "message"]).and_then(Json::as_str),
            Some("Cannot navigate to invalid URL")
        );
    }

    #[test]
    fn nothing_is_kept_for_a_command_nobody_will_come_back_for() {
        let mut mailbox = Mailbox::default();
        // What `Client::notify` leaves behind: an id spent and never claimed.
        sort(&mut mailbox, r#"{"id":7,"result":{}}"#);
        assert!(
            mailbox.replies.is_empty(),
            "the reply to a notification was filed"
        );
        assert!(mailbox.wanted.is_empty());
    }

    #[test]
    fn a_call_that_asked_still_gets_its_reply() {
        let mut mailbox = Mailbox::default();
        mailbox.want(3);
        sort(&mut mailbox, r#"{"id":3,"result":{"data":"png"}}"#);
        let reply = mailbox.take(3).expect("the reply the call asked for");
        assert_eq!(
            reply.path(&["result", "data"]).and_then(Json::as_str),
            Some("png")
        );
        assert_eq!(
            (mailbox.replies.len(), mailbox.wanted.len()),
            (0, 0),
            "taking a reply ends the waiting for it"
        );
    }

    #[test]
    fn a_pending_dropped_before_its_reply_leaves_nothing_behind() {
        let (mailbox, pending) = awaiting(11, "Page.captureScreenshot");
        drop(pending);
        // The tab was switched away from; the engine answers all the same.
        {
            let mut inner = mailbox.0.lock().expect("a mailbox");
            sort(&mut inner, r#"{"id":11,"result":{"data":"a megabyte"}}"#);
        }
        assert_eq!(held(&mailbox), (0, 0));
    }

    #[test]
    fn a_pending_dropped_after_its_reply_takes_the_reply_with_it() {
        let (mailbox, pending) = awaiting(12, "Page.captureScreenshot");
        {
            let mut inner = mailbox.0.lock().expect("a mailbox");
            sort(&mut inner, r#"{"id":12,"result":{"data":"a megabyte"}}"#);
        }
        assert_eq!(held(&mailbox), (1, 1), "the reply was wanted when it came");
        drop(pending);
        assert_eq!(held(&mailbox), (0, 0));
    }

    #[test]
    fn ten_thousand_acknowledgements_leave_an_empty_mailbox() {
        // Three minutes of screencast at sixty frames a second.
        let mut mailbox = Mailbox::default();
        for id in 1..=10_000 {
            sort(&mut mailbox, &format!(r#"{{"id":{id},"result":{{}}}}"#));
        }
        assert_eq!(mailbox.replies.len(), 0);
        assert_eq!(mailbox.wanted.len(), 0);
    }

    #[test]
    fn a_still_among_the_acknowledgements_is_the_one_thing_kept() {
        let (mailbox, pending) = awaiting(5_000, "Page.captureScreenshot");
        {
            let mut inner = mailbox.0.lock().expect("a mailbox");
            for id in 1..=10_000 {
                sort(
                    &mut inner,
                    &format!(r#"{{"id":{id},"result":{{"n":{id}}}}}"#),
                );
            }
            assert_eq!(
                inner.replies.len(),
                1,
                "only the one reply somebody asked for"
            );
            assert_eq!(
                inner
                    .take(5_000)
                    .and_then(|r| r.path(&["result", "n"]).and_then(Json::as_i64)),
                Some(5_000)
            );
        }
        drop(pending);
        assert_eq!(held(&mailbox), (0, 0));
    }
}
