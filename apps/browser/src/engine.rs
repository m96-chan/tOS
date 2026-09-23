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
//! # The group, and why not the pid
//!
//! What is started here is usually not a browser. On Debian — which is what a
//! tOS rootfs is — `/usr/bin/chromium-shell` and `/usr/bin/chromium` are shell
//! scripts that run the real binary under `/usr/lib/chromium/` as a child and
//! wait for it, and `/usr/bin/google-chrome` is a wrapper too. So the pid this
//! program gets back from `spawn` is `/bin/sh`, and a `kill(2)` on it takes
//! the shell and leaves the browser: reparented to init, still holding its
//! debugging port, still painting the page it had. Seven sessions of that on
//! an installed machine left seven engines nobody was looking at, between them
//! keeping two processors busy.
//!
//! So the engine is started in a process group of its own and every kill here
//! signals the *group*: the wrapper, the browser it ran, and the zygote, gpu
//! and renderer processes the browser forked, all of which inherit the group
//! and none of which this program otherwise knows the pid of.
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
use std::os::unix::process::CommandExt;
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

/// What to kill to stop the engine, for the paths that cannot run a
/// destructor.
///
/// A panic in a release build aborts — the workspace sets `panic = "abort"` —
/// so `Drop` is not a way to be sure the child dies. This is read by the panic
/// hook and by the signal path, both of which have to kill a process without
/// owning anything.
///
/// It is written the way `kill(2)` wants it: the engine's process group,
/// negated, or the wrapper's bare pid if a group of its own could not be made.
/// Zero when there is nothing to kill.
static TARGET: AtomicI32 = AtomicI32::new(0);

/// Kill the engine and everything it started, from anywhere, without a `&mut`
/// to it.
///
/// Signal-safe enough for what it is used for: one `kill(2)` on an integer
/// read out of an atomic. The panic hook and the signal path do not go through
/// [`Engine::kill`] and have no connection to ask the browser to close on, so
/// they take the group with `SIGKILL` and leave the lock file behind.
pub fn kill_engine() {
    signal_all(TARGET.swap(0, Ordering::SeqCst), libc::SIGKILL);
}

/// Signal the engine: the whole group where there is one.
///
/// `target` is already in `kill(2)`'s own notation — negative for a group —
/// so this is one `kill(2)` and a check, which is all a signal handler may do.
fn signal_all(target: i32, signal: libc::c_int) {
    // 0 is "this program's own group" and -1 is "every process we are allowed
    // to signal". Either would be this program killing itself, so a target
    // that was never recorded kills nothing.
    if target == 0 || target == -1 {
        return;
    }
    unsafe {
        libc::kill(target, signal);
    }
}

/// Start `command` as the leader of a new process group, and say what to kill.
///
/// The group is asked for twice on purpose: in the child before `exec`, and
/// again in the parent. Either call alone is a race — the parent can reach the
/// kill before the child has reached `setpgid`, and the child can `exec`
/// before the parent has got round to it — and `setpgid(2)` on a process that
/// already leads its own group changes nothing, so doing both closes the
/// window. The parent's call failing means the child's has already run.
fn spawn_in_own_group(command: &mut Command) -> std::io::Result<(Child, i32)> {
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn()?;
    let pid = child.id() as i32;
    unsafe {
        libc::setpgid(pid, pid);
    }
    Ok((child, group_target(pid)))
}

/// `kill(2)`'s argument for everything `pid` leads: `-pid` once `pid` is a
/// group of its own, and `pid` alone if it somehow is not.
///
/// The second case should not happen and is not an error: a program that is
/// running is better stopped by its pid than not at all. It is checked rather
/// than assumed because the number is about to be handed to `kill(2)` with a
/// minus in front of it, and the group this program is in is one of the things
/// that could be on the other end of that.
fn group_target(pid: i32) -> i32 {
    let group = unsafe { libc::getpgid(pid) };
    let ours = unsafe { libc::getpgrp() };
    if group == pid && group != ours {
        -pid
    } else {
        pid
    }
}

/// Whether anything is left of the engine's group.
///
/// Signal 0 asks `kill(2)` whether it could send rather than sending, and
/// `ESRCH` is the answer that the group is empty. A process nobody has waited
/// for is still a member of it, which is why the wrapper is reaped before this
/// is believed.
fn group_alive(target: i32) -> bool {
    if target >= 0 {
        // No group of its own; the child's own exit status is the whole
        // answer, and the caller has it.
        return false;
    }
    if unsafe { libc::kill(target, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
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
    /// What to kill, in `kill(2)`'s notation; see [`TARGET`].
    target: i32,
    browser_url: String,
    tail: Arc<Mutex<Vec<String>>>,
}

impl Engine {
    /// Start one and wait, for no longer than `timeout`, for it to say where
    /// its debugging port is.
    pub fn launch(timeout: Duration) -> Result<Engine, String> {
        let path = locate()?;
        let as_root = unsafe { libc::geteuid() } == 0;
        let mut command = Command::new(&path);
        command
            .args(flags(as_root))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let (mut child, target) = spawn_in_own_group(&mut command)
            .map_err(|e| format!("cannot start {}: {e}", path.display()))?;
        TARGET.store(target, Ordering::SeqCst);

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
                    target,
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
            target,
            browser_url,
            tail,
        })
    }

    /// The engine's process group, once it has one of its own.
    ///
    /// `None` means the group could not be made and the wrapper's pid is all
    /// there is to kill, which is the case this module exists to avoid.
    pub fn group(&self) -> Option<i32> {
        (self.target < 0).then_some(-self.target)
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

    /// Stop it, politely and then not — and the group, not the pid.
    pub fn kill(&mut self) {
        TARGET.store(0, Ordering::SeqCst);
        let target = self.target;
        signal_all(target, libc::SIGTERM);
        // Half a second to close its files, then the signal that is not a
        // request. Chromium flushes its profile and takes its SingletonLock
        // with it when it is asked to stop, leaves the lock behind if it is
        // only ever SIGKILLed, and waits forever if it is only ever asked.
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            // The wrapper first — a process nobody has waited for is still a
            // member of its own group — and then the group it led, which is
            // where the browser and its renderers are.
            let waited = !matches!(self.child.try_wait(), Ok(None));
            if waited && !group_alive(target) {
                return;
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        signal_all(target, libc::SIGKILL);
        // The wrapper is this program's child and has to be waited for. The
        // rest of the group are init's children by the time the signal lands,
        // and die on it with nothing here left to reap.
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
    fn a_child_leads_a_group_that_is_not_the_one_this_program_is_in() {
        // The shape of the Debian wrapper: a shell that runs something else
        // and waits for it, rather than exec-ing it. The trailing `:` is what
        // stops the shell optimising the wait away and becoming the sleep.
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("sleep 30; :")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let (mut child, target) = spawn_in_own_group(&mut command).expect("a shell starts");
        let pid = child.id() as i32;
        let ours = unsafe { libc::getpgrp() };

        assert_eq!(
            target, -pid,
            "the target is the child's group, kill(2)'s way"
        );
        assert_eq!(unsafe { libc::getpgid(pid) }, pid, "and the child leads it");
        assert_ne!(-target, ours, "a group of its own, not the one we are in");
        assert!(
            group_alive(target),
            "the group has the shell in it at least"
        );

        signal_all(target, libc::SIGKILL);
        let status = child.wait().expect("the shell is reaped");
        assert!(!status.success(), "it was killed, not asked: {status}");
    }

    #[test]
    fn a_target_that_was_never_recorded_kills_nothing() {
        // 0 is this program's own group and -1 is every process it may signal,
        // so either of those reaching `kill(2)` would end this test process
        // and every other test with it. Getting to the end is the assertion.
        signal_all(0, libc::SIGKILL);
        signal_all(-1, libc::SIGKILL);
        assert!(
            !group_alive(0),
            "a pid with no group of its own is not a group"
        );
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
