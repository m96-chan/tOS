//! The Chromium that does the rendering, as a child process.
//!
//! tOS does not ship a browser engine and this crate does not contain one: the
//! person installs a Chromium and `tos-browser` drives it. So the first thing
//! this program does is find one, and the second is start it in a way that
//! cannot leave it running after the pane is gone.
//!
//! # The flags are not decoration
//!
//! `--headless --disable-gpu --ozone-platform=headless` is the combination
//! that was measured to work. The third is the one that looks redundant and is
//! not: a Chromium started headless *without* an ozone platform opens its
//! debugging port, accepts a connection, and then never answers on it — no
//! error, no output, no exit. That failure is why every wait in this program
//! has a deadline, and why the flags are written here as a constant rather
//! than assembled from options.
//!
//! `--remote-debugging-port=0` asks the kernel for a free port and Chromium
//! prints the one it got; taking a port from the child is the only way to run
//! two of these at once without them colliding.
//! `--disable-dev-shm-usage` keeps the engine off the same `/dev/shm` the
//! frames go through. `--remote-allow-origins=*` is needed because a
//! WebSocket handshake without an `Origin` is checked against a list that is
//! empty by default. `--no-sandbox` only when this program is root, because
//! Chromium refuses to start as root without it and adding it as anyone else
//! would be turning off a protection that was working.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::json::Json;

/// The engines that are looked for, in the order they are looked for.
///
/// `chromium-shell` first: it is Debian's `headless_shell`, the one this was
/// measured against, and the one with no window system code in it at all.
pub const CANDIDATES: [&str; 4] = [
    "chromium-shell",
    "chromium",
    "chromium-browser",
    "google-chrome",
];

/// The environment variable that overrides the search.
pub const ENGINE_ENV: &str = "TOS_BROWSER_ENGINE";

/// How many lines of the engine's stderr are kept to explain a death: the
/// first `HEAD` and the last `TAIL`, with whatever came between dropped.
///
/// Both ends, because Chromium says why it is dying at the top — one
/// `FATAL:` line, or "No usable sandbox!" — and then prints a stack trace and
/// a register dump some twenty lines long. A tail alone keeps the registers
/// and loses the sentence, which is exactly what happened the first time this
/// ran on a machine whose Chromium could not start.
const HEAD: usize = 8;
const TAIL: usize = 12;

/// The engine's pid, for the paths that cannot run a destructor.
///
/// A panic in a release build aborts — the workspace sets `panic = "abort"` —
/// so `Drop` is not a way to be sure the child dies. This is read by the panic
/// hook and by the signal path, both of which have to kill a process without
/// owning anything.
static PID: AtomicI32 = AtomicI32::new(0);

/// Kill the engine, from anywhere, without a `&mut` to it.
///
/// Signal-safe enough for what it is used for: one `kill(2)` on an integer
/// read out of an atomic.
pub fn kill_engine() {
    let pid = PID.swap(0, Ordering::SeqCst);
    if pid > 0 {
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }
}

/// Where the engine is, or a sentence about why there is none.
pub fn locate() -> Result<PathBuf, String> {
    if let Some(named) = std::env::var_os(ENGINE_ENV) {
        let path = PathBuf::from(&named);
        if is_executable(&path) {
            return Ok(path);
        }
        if let Some(found) = search_path(&path.to_string_lossy()) {
            return Ok(found);
        }
        return Err(format!(
            "{ENGINE_ENV} names {}, which is not an executable",
            path.display()
        ));
    }
    for candidate in CANDIDATES {
        if let Some(found) = search_path(candidate) {
            return Ok(found);
        }
    }
    Err(format!(
        "no browser engine on PATH: looked for {}; set {ENGINE_ENV} to one",
        CANDIDATES.join(", ")
    ))
}

fn search_path(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        let path = PathBuf::from(name);
        return is_executable(&path).then_some(path);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// The command line, which is fixed apart from the sandbox.
pub fn flags(as_root: bool) -> Vec<&'static str> {
    let mut flags = vec![
        "--headless",
        "--disable-gpu",
        "--disable-dev-shm-usage",
        "--ozone-platform=headless",
        "--remote-debugging-port=0",
        "--remote-allow-origins=*",
    ];
    if as_root {
        flags.push("--no-sandbox");
    }
    flags.push("about:blank");
    flags
}

/// The url out of `DevTools listening on ws://127.0.0.1:PORT/...`.
pub fn devtools_url(line: &str) -> Option<String> {
    let at = line.find("ws://")?;
    Some(line[at..].trim().to_string())
}

/// A running engine, killed when this is dropped and when the program dies.
pub struct Engine {
    child: Child,
    browser_url: String,
    tail: Arc<Mutex<Vec<String>>>,
}

impl Engine {
    /// Start one and wait, for no longer than `timeout`, for it to say where
    /// its debugging port is.
    pub fn launch(timeout: Duration) -> Result<Engine, String> {
        let path = locate()?;
        let as_root = unsafe { libc::geteuid() } == 0;
        let mut child = Command::new(&path)
            .args(flags(as_root))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot start {}: {e}", path.display()))?;
        PID.store(child.id() as i32, Ordering::SeqCst);

        let stderr = child.stderr.take().ok_or("the engine has no stderr")?;
        let tail: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let (found, url) = std::sync::mpsc::channel();
        let keep = Arc::clone(&tail);
        // A thread rather than a poll, because the line has to be read as it
        // arrives: a pipe nobody reads fills, and an engine whose stderr is
        // full stops.
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if let Some(url) = devtools_url(&line) {
                    let _ = found.send(url);
                }
                if let Ok(mut tail) = keep.lock() {
                    tail.push(line);
                    if tail.len() > HEAD + TAIL {
                        // The first lines stay; the ring is the rest.
                        tail.remove(HEAD);
                    }
                }
            }
        });

        let browser_url = match url.recv_timeout(timeout) {
            Ok(url) => url,
            Err(_) => {
                let why = describe_tail(&tail);
                let mut engine = Engine {
                    child,
                    browser_url: String::new(),
                    tail,
                };
                engine.kill();
                return Err(format!(
                    "{} did not say where its debugging port is within {} seconds{why}",
                    path.display(),
                    timeout.as_secs()
                ));
            }
        };

        Ok(Engine {
            child,
            browser_url,
            tail,
        })
    }

    /// The browser-level WebSocket url the engine printed.
    pub fn browser_url(&self) -> &str {
        &self.browser_url
    }

    /// `host:port` of the engine's HTTP endpoints.
    pub fn address(&self) -> Result<String, String> {
        let (host, port, _) = crate::ws::split_url(&self.browser_url)?;
        Ok(format!("{host}:{port}"))
    }

    /// The last lines the engine wrote, for an error message.
    pub fn tail(&self) -> Vec<String> {
        self.tail.lock().map(|t| t.clone()).unwrap_or_default()
    }

    /// `Ok` while the engine is running; the reason, with its own last words,
    /// once it is not.
    pub fn check(&mut self) -> Result<(), String> {
        match self.child.try_wait() {
            Ok(Some(status)) => Err(format!(
                "the browser engine exited ({status}){}",
                describe_tail(&self.tail)
            )),
            Ok(None) => Ok(()),
            Err(err) => Err(format!("cannot tell whether the engine is running: {err}")),
        }
    }

    /// Stop it, politely and then not.
    pub fn kill(&mut self) {
        PID.store(0, Ordering::SeqCst);
        let pid = self.child.id() as i32;
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        // A tenth of a second to close its files, then the signal that is not
        // a request. Chromium leaves a lock file behind if it is only ever
        // SIGKILLed, and waits forever if it is only ever asked.
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                _ => break,
            }
        }
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
        let _ = self.child.wait();
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.kill();
    }
}

fn describe_tail(tail: &Arc<Mutex<Vec<String>>>) -> String {
    let lines = tail.lock().map(|t| t.clone()).unwrap_or_default();
    if lines.is_empty() {
        return String::new();
    }
    format!("; it said: {}", lines.join(" / "))
}

/// The WebSocket url of a page target, once the engine has one.
///
/// The engine prints its *browser* endpoint, which drives the browser and not
/// a page. The page endpoint is in `/json/list`, which is also the first thing
/// this program asks the engine for — so a Chromium that opened its port and
/// then stopped answering is caught here, by the deadline, rather than by a
/// CDP command that never returns.
pub fn page_target(address: &str, timeout: Duration) -> Result<String, String> {
    let deadline = Instant::now() + timeout;
    let mut last = String::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(format!(
                "the engine at {address} has no page to drive{}",
                if last.is_empty() {
                    String::new()
                } else {
                    format!(": {last}")
                }
            ));
        }
        match crate::http::get(address, "/json/list", left.min(Duration::from_secs(2))) {
            Ok(body) => match first_page(&body) {
                Ok(url) => return Ok(url),
                Err(why) => last = why,
            },
            Err(why) => last = why,
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The WebSocket url of any page target, from the browser's own url.
///
/// A second tab has no entry in `/json/list` until the engine has got round to
/// listing it, and asking over HTTP for something the browser connection just
/// told us about would be a round trip and a race. Every endpoint the engine
/// serves is `/devtools/<kind>/<id>` on the one port, so the page endpoint is
/// the browser endpoint with the last two path elements replaced — which is
/// exactly what [`page_target`] finds by asking, and this finds by knowing.
pub fn target_url(browser_url: &str, target: &str) -> Result<String, String> {
    let (host, port, _) = crate::ws::split_url(browser_url)?;
    Ok(format!("ws://{host}:{port}/devtools/page/{target}"))
}

/// The target id at the end of a target's WebSocket url.
pub fn target_of(url: &str) -> Option<&str> {
    let id = url.rsplit('/').next()?;
    (!id.is_empty()).then_some(id)
}

/// The first page target's WebSocket url in a `/json/list` answer.
pub fn first_page(body: &str) -> Result<String, String> {
    let value = Json::parse(body).map_err(|e| format!("the target list is not JSON: {e}"))?;
    let targets = value
        .as_array()
        .ok_or_else(|| "the target list is not a list".to_string())?;
    targets
        .iter()
        .find(|target| target.get("type").and_then(Json::as_str) == Some("page"))
        .and_then(|target| target.get("webSocketDebuggerUrl"))
        .and_then(Json::as_str)
        .map(|url| url.to_string())
        .ok_or_else(|| format!("no page among {} targets", targets.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_flags_are_the_ones_that_were_measured() {
        let plain = flags(false);
        assert_eq!(
            plain,
            vec![
                "--headless",
                "--disable-gpu",
                "--disable-dev-shm-usage",
                "--ozone-platform=headless",
                "--remote-debugging-port=0",
                "--remote-allow-origins=*",
                "about:blank",
            ]
        );
        assert!(!plain.contains(&"--no-sandbox"), "not unless we are root");
        assert!(flags(true).contains(&"--no-sandbox"));
        assert_eq!(
            flags(true).last(),
            Some(&"about:blank"),
            "the url goes last"
        );
    }

    #[test]
    fn the_port_comes_out_of_the_line_chromium_prints() {
        let line = "DevTools listening on ws://127.0.0.1:37021/devtools/browser/8f-4a";
        assert_eq!(
            devtools_url(line).as_deref(),
            Some("ws://127.0.0.1:37021/devtools/browser/8f-4a")
        );
        // Chromium prefixes its stderr with a timestamp and a level.
        let noisy = "[0918/120000.1:INFO:main.cc(50)] DevTools listening on ws://127.0.0.1:1/x\n";
        assert_eq!(devtools_url(noisy).as_deref(), Some("ws://127.0.0.1:1/x"));
        assert_eq!(devtools_url("[0918] some other warning"), None);
    }

    #[test]
    fn a_page_is_picked_out_of_the_target_list() {
        let body = r#"[
          {"type":"browser","webSocketDebuggerUrl":"ws://127.0.0.1:1/b"},
          {"type":"page","title":"about:blank",
           "webSocketDebuggerUrl":"ws://127.0.0.1:1/devtools/page/AB"}
        ]"#;
        assert_eq!(
            first_page(body),
            Ok("ws://127.0.0.1:1/devtools/page/AB".into())
        );
        assert!(first_page("[]").is_err());
        assert!(first_page(r#"[{"type":"browser"}]"#).is_err());
        assert!(first_page("not json").is_err());
    }

    #[test]
    fn a_second_tab_is_reached_on_the_port_the_browser_answered_on() {
        let browser = "ws://127.0.0.1:37021/devtools/browser/8f-4a";
        assert_eq!(
            target_url(browser, "AB12"),
            Ok("ws://127.0.0.1:37021/devtools/page/AB12".into())
        );
        // And the id comes back out of a url the list gave us.
        assert_eq!(
            target_of("ws://127.0.0.1:1/devtools/page/AB12"),
            Some("AB12")
        );
        assert_eq!(target_of("ws://127.0.0.1:1/devtools/page/"), None);
        assert!(target_url("not a url", "AB12").is_err());
    }

    #[test]
    fn a_missing_engine_is_a_sentence_that_names_what_was_looked_for() {
        // An override that names nothing, which is the case a person hits
        // after a typo, and the message has to say which variable.
        let failed = with_engine_env(Some("/nonexistent/chromium"), locate).unwrap_err();
        assert!(failed.contains("/nonexistent/chromium"), "{failed}");
        assert!(failed.contains(ENGINE_ENV), "{failed}");
    }

    #[test]
    fn an_override_that_names_a_real_program_is_taken() {
        let found = with_engine_env(Some("/bin/sh"), locate);
        assert_eq!(found.as_deref().map(|p| p.to_str().unwrap()), Ok("/bin/sh"));
    }

    /// Set the variable, run, put it back. The tests that use it are in one
    /// module and the environment is per process, so they are serialised by a
    /// mutex rather than left to race.
    fn with_engine_env<T>(value: Option<&str>, run: impl FnOnce() -> T) -> T {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let before = std::env::var_os(ENGINE_ENV);
        match value {
            Some(value) => std::env::set_var(ENGINE_ENV, value),
            None => std::env::remove_var(ENGINE_ENV),
        }
        let out = run();
        match before {
            Some(before) => std::env::set_var(ENGINE_ENV, before),
            None => std::env::remove_var(ENGINE_ENV),
        }
        out
    }
}
