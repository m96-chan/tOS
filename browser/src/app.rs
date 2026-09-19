//! The loop that makes the other modules a browser.
//!
//! One thread, one `poll`, two descriptors: the terminal and the pipe the CDP
//! reader knocks on. Everything else is a reaction to one of those two being
//! readable, which is what keeps this file a sequence of decisions rather than
//! a scheduler.
//!
//! The decisions worth knowing about are here rather than scattered: which
//! keys this program keeps for itself, what a wheel notch is worth, what a
//! click on the status row means, and what happens to the frames that arrive
//! faster than a pane can draw them.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tos_platform::tty::{self, ReadOutcome};
use tos_preview::fit::{Cells, Metrics};

use crate::cdp::{Client, Event};
use crate::engine::Engine;
use crate::graphics::Painter;
use crate::input::{Input, Key, KeyAction, KeyInput, MouseInput, MouseKind, Parser};
use crate::json::Json;
use crate::keys;
use crate::screen::{self, Pane};

/// How long the engine gets to print its port.
const ENGINE_TIMEOUT: Duration = Duration::from_secs(20);

/// How long it then gets to offer a page to drive.
const TARGET_TIMEOUT: Duration = Duration::from_secs(15);

/// CSS pixels per wheel notch.
///
/// Chromium's own idea of a wheel tick is three lines of about forty pixels.
/// A terminal mouse reports notches and nothing else — no acceleration, no
/// fractional scroll — so the number is a constant, and this is the one that
/// makes a page move by about the same amount it would in a window.
const WHEEL_PIXELS: f64 = 120.0;

/// How close in time and space two presses have to be to be a double click.
const DOUBLE_CLICK: Duration = Duration::from_millis(500);
const DOUBLE_CLICK_SLOP: i32 = 4;

/// The row the page starts on, one-based: the first is this program's.
const PAGE_ROW: u32 = 2;

/// Set by the signal handlers. A handler may do nothing else.
static QUIT: AtomicBool = AtomicBool::new(false);
static RESIZED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_quit(_signal: libc::c_int) {
    QUIT.store(true, Ordering::SeqCst);
}

extern "C" fn on_winch(_signal: libc::c_int) {
    RESIZED.store(true, Ordering::SeqCst);
}

/// Ask to be told about the three signals that matter, without `SA_RESTART`:
/// a `poll` that is interrupted is a `poll` that comes back and looks at the
/// flags, which is the whole point of setting them.
fn install_signals() {
    unsafe {
        for (signal, handler) in [
            (libc::SIGTERM, on_quit as *const () as usize),
            (libc::SIGINT, on_quit as *const () as usize),
            (libc::SIGHUP, on_quit as *const () as usize),
            (libc::SIGWINCH, on_winch as *const () as usize),
        ] {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = handler;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(signal, &action, std::ptr::null_mut());
        }
        // A page that closes its connection must not kill this program before
        // it has put the terminal back.
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}

/// What the person asked for on the command line.
pub struct Options {
    pub url: String,
}

/// What is on the status line, which is also what the program knows about the
/// page.
#[derive(Default)]
struct Status {
    url: String,
    title: String,
    /// `Some` while the url is being typed.
    editing: Option<String>,
    /// Whether that url is still the one the page had, untouched.
    ///
    /// A browser's `ctrl+l` selects the whole address, so the next thing typed
    /// replaces it and a backspace deletes it. There is no selection to draw in
    /// a status line, but the behaviour is what the reflex expects, and keeping
    /// the old url until then is what makes `ctrl+l` also a way to read where
    /// you are.
    editing_whole: bool,
    /// A message that replaces the title until the page says something else.
    note: Option<String>,
}

impl Status {
    fn line(&self) -> String {
        if let Some(note) = &self.note {
            return note.clone();
        }
        match (self.title.is_empty(), self.url.is_empty()) {
            (true, true) => "tos-browser".to_string(),
            (true, false) => self.url.clone(),
            (false, true) => self.title.clone(),
            (false, false) => format!("{}  —  {}", self.title, self.url),
        }
    }
}

/// Run until the person quits or something goes wrong.
pub fn run(options: Options) -> Result<(), String> {
    if unsafe { libc::isatty(1) } != 1 {
        return Err("stdout is not a terminal, so there is nowhere to put a page".to_string());
    }
    install_signals();
    std::panic::set_hook(Box::new(|info| {
        // A release build aborts here, so this is the only chance to put the
        // terminal back and stop the engine.
        screen::emergency();
        crate::engine::kill_engine();
        eprintln!("tos-browser: {info}");
    }));

    let mut engine = Engine::launch(ENGINE_TIMEOUT)?;
    let address = engine.address()?;
    let target = crate::engine::page_target(&address, TARGET_TIMEOUT).map_err(|why| {
        let tail = engine.tail();
        if tail.is_empty() {
            why
        } else {
            format!("{why}; the engine said: {}", tail.join(" / "))
        }
    })?;
    let mut client = Client::connect(&target, Duration::from_secs(10))?;

    let mut pane = Pane::enter(0, 1).map_err(|e| format!("cannot take the terminal: {e}"))?;
    let outcome = drive(&mut pane, &mut client, &mut engine, options);
    pane.leave();
    client.close();
    engine.kill();
    outcome
}

/// Everything between taking the terminal and giving it back.
fn drive(
    pane: &mut Pane,
    client: &mut Client,
    engine: &mut Engine,
    options: Options,
) -> Result<(), String> {
    let mut metrics = pane
        .metrics()
        .map_err(|e| format!("cannot measure the pane: {e}"))?;
    let mut painter = Painter::new();
    let mut parser = Parser::new();
    let mut status = Status::default();
    let mut clicks = Clicks::default();
    let mut buttons = 0u32;

    client.call("Page.enable", Json::empty())?;
    emulate(client, metrics)?;
    start_screencast(client, metrics)?;

    let url = normalise(&options.url);
    status.url = url.clone();
    status.note = Some(format!("loading {url}"));
    redraw_status(pane, metrics, &status)?;
    if let Err(why) = client.call(
        "Page.navigate",
        Json::object(vec![("url", Json::string(&url))]),
    ) {
        status.note = Some(why);
        redraw_status(pane, metrics, &status)?;
    }

    let mut last_check = Instant::now();
    let mut buf = [0u8; 8192];

    while !QUIT.load(Ordering::SeqCst) {
        if RESIZED.swap(false, Ordering::SeqCst) {
            metrics = pane
                .metrics()
                .map_err(|e| format!("cannot measure the pane: {e}"))?;
            pane.write(b"\x1b[2J").map_err(|e| e.to_string())?;
            emulate(client, metrics)?;
            restart_screencast(client, metrics)?;
            redraw_status(pane, metrics, &status)?;
        }

        // The engine is a child process and can die at any point; without this
        // the first sign would be a command that timed out fifteen seconds
        // later.
        if last_check.elapsed() > Duration::from_millis(500) {
            last_check = Instant::now();
            engine.check()?;
        }
        if let Some(ended) = client.ended() {
            return Err(format!("the engine stopped talking: {ended}"));
        }

        let ready = tty::poll_readable(&[pane.input_fd(), client.wake_fd()], 50)
            .map_err(|e| format!("cannot wait for input: {e}"))?;

        if ready.contains(&pane.input_fd()) {
            match tty::read_available(pane.input_fd(), &mut buf) {
                Ok(ReadOutcome::Data(n)) => {
                    for input in parser.feed(&buf[..n]) {
                        if !handle_input(
                            pane,
                            client,
                            &mut painter,
                            &mut parser,
                            &mut status,
                            &mut clicks,
                            &mut buttons,
                            metrics,
                            input,
                        )? {
                            return Ok(());
                        }
                    }
                }
                Ok(ReadOutcome::Eof) => return Ok(()),
                Ok(ReadOutcome::WouldBlock) => {}
                Err(err) => return Err(format!("cannot read the terminal: {err}")),
            }
        } else if let Some(input) = parser.flush() {
            // Nothing arrived, so a held escape was the Escape key after all.
            if !handle_input(
                pane,
                client,
                &mut painter,
                &mut parser,
                &mut status,
                &mut clicks,
                &mut buttons,
                metrics,
                input,
            )? {
                return Ok(());
            }
        }

        if ready.contains(&client.wake_fd()) {
            client.drain_wake();
        }
        handle_events(pane, client, &mut painter, &mut status, metrics)?;
    }
    Ok(())
}

/// Tell the engine how big the page is.
fn emulate(client: &mut Client, metrics: Metrics) -> Result<(), String> {
    let (width, height) = page_pixels(metrics);
    client.call(
        "Emulation.setDeviceMetricsOverride",
        Json::object(vec![
            ("width", Json::number(width)),
            ("height", Json::number(height)),
            ("deviceScaleFactor", Json::number(1)),
            ("mobile", Json::Bool(false)),
        ]),
    )?;
    Ok(())
}

fn start_screencast(client: &mut Client, metrics: Metrics) -> Result<(), String> {
    let (width, height) = page_pixels(metrics);
    client.call(
        "Page.startScreencast",
        Json::object(vec![
            ("format", Json::string("png")),
            ("maxWidth", Json::number(width)),
            ("maxHeight", Json::number(height)),
            ("everyNthFrame", Json::number(1)),
        ]),
    )?;
    Ok(())
}

fn restart_screencast(client: &mut Client, metrics: Metrics) -> Result<(), String> {
    let _ = client.call("Page.stopScreencast", Json::empty());
    start_screencast(client, metrics)
}

/// The page's size in pixels: the pane, less the status row.
fn page_pixels(metrics: Metrics) -> (u32, u32) {
    let (w, h) = metrics.usable_pixels();
    (w.max(1), h.max(1))
}

/// The page's size in cells, which is what the placement asks for.
fn page_cells(metrics: Metrics) -> Cells {
    Cells {
        cols: metrics.cols.max(1),
        rows: metrics.usable_rows(),
    }
}

fn redraw_status(pane: &mut Pane, metrics: Metrics, status: &Status) -> Result<(), String> {
    let line = status.line();
    let bytes = screen::status_line(metrics.cols, &line, status.editing.as_deref());
    pane.write(&bytes).map_err(|e| e.to_string())
}

/// Everything the engine said since the last look.
///
/// Frames are coalesced: every one is acknowledged, because the engine sends
/// no more until it has been, but only the last is drawn. A pane that cannot
/// keep up with sixty frames a second should fall behind by dropping frames,
/// not by drawing a queue of stale ones.
fn handle_events(
    pane: &mut Pane,
    client: &mut Client,
    painter: &mut Painter,
    status: &mut Status,
    metrics: Metrics,
) -> Result<(), String> {
    let events = client.events();
    if events.is_empty() {
        return Ok(());
    }
    let mut newest_frame: Option<Vec<u8>> = None;
    let mut ask_title = false;

    for Event { method, params } in events {
        match method.as_str() {
            "Page.screencastFrame" => {
                if let Some(session) = params.get("sessionId").and_then(Json::as_i64) {
                    let _ = client.notify(
                        "Page.screencastFrameAck",
                        Json::object(vec![("sessionId", Json::number(session as f64))]),
                    );
                }
                if let Some(data) = params.get("data").and_then(Json::as_str) {
                    match crate::base64::decode(data.as_bytes()) {
                        Ok(png) => newest_frame = Some(png),
                        // A frame that will not decode is a frame, not a
                        // session: the next one is along in sixteen
                        // milliseconds.
                        Err(_) => continue,
                    }
                }
            }
            "Page.frameNavigated" => {
                // Only the main frame, which is the one whose url is the
                // page's: an advert in an iframe navigating is not.
                let frame = params.get("frame");
                let is_main = frame.and_then(|f| f.get("parentId")).is_none();
                if let (true, Some(url)) = (
                    is_main,
                    frame.and_then(|f| f.get("url")).and_then(Json::as_str),
                ) {
                    status.url = url.to_string();
                    status.title.clear();
                    status.note = None;
                    ask_title = true;
                }
            }
            "Page.loadEventFired" => ask_title = true,
            "Page.javascriptDialogOpening" => {
                // Nothing here can show an alert, and a page whose dialog is
                // never answered stops rendering.
                let _ = client.notify(
                    "Page.handleJavaScriptDialog",
                    Json::object(vec![("accept", Json::Bool(false))]),
                );
            }
            _ => {}
        }
    }

    if ask_title {
        if let Ok(title) = client.call_within(
            "Runtime.evaluate",
            Json::object(vec![
                ("expression", Json::string("document.title")),
                ("returnByValue", Json::Bool(true)),
            ]),
            Duration::from_secs(2),
        ) {
            if let Some(text) = title.path(&["result", "value"]).and_then(Json::as_str) {
                status.title = text.to_string();
            }
        }
        redraw_status(pane, metrics, status)?;
    }

    if let Some(png) = newest_frame {
        let bytes = painter.frame(&png, page_cells(metrics), PAGE_ROW, 1);
        pane.write(&bytes).map_err(|e| e.to_string())?;
        // The picture does not move the cursor (`C=1`), but the status line
        // owns the cursor's position when the url is being typed, so it is
        // written again rather than left where the last frame found it.
        if status.editing.is_some() {
            redraw_status(pane, metrics, status)?;
        }
    }
    Ok(())
}

/// Handle one thing the terminal said. `false` means quit.
#[allow(clippy::too_many_arguments)]
fn handle_input(
    pane: &mut Pane,
    client: &mut Client,
    painter: &mut Painter,
    parser: &mut Parser,
    status: &mut Status,
    clicks: &mut Clicks,
    buttons: &mut u32,
    metrics: Metrics,
    input: Input,
) -> Result<bool, String> {
    match input {
        Input::Mode { .. } => {}
        Input::Key(key) => {
            if status.editing.is_some() {
                return edit_url(pane, client, status, metrics, key);
            }
            match command(&key) {
                Some(Command::Quit) => return Ok(false),
                Some(Command::EditUrl) => {
                    status.editing = Some(status.url.clone());
                    status.editing_whole = true;
                    redraw_status(pane, metrics, status)?;
                }
                Some(Command::Reload) => {
                    let _ = client.call("Page.reload", Json::empty());
                }
                Some(Command::Back) => go(client, status, -1),
                Some(Command::Forward) => go(client, status, 1),
                None => send_key(client, &key),
            }
        }
        Input::Mouse(report) => {
            send_mouse(client, parser, clicks, buttons, metrics, report);
        }
    }
    let _ = painter;
    Ok(true)
}

/// The keys this program keeps for itself. Everything else is the page's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Quit,
    EditUrl,
    Reload,
    Back,
    Forward,
}

fn command(key: &KeyInput) -> Option<Command> {
    if key.action == KeyAction::Release {
        return None;
    }
    if key.mods.ctrl() && !key.mods.alt() {
        return match key.key {
            Key::Char('q') => Some(Command::Quit),
            Key::Char('l') => Some(Command::EditUrl),
            Key::Char('r') => Some(Command::Reload),
            _ => None,
        };
    }
    if key.mods.alt() && !key.mods.ctrl() {
        return match key.key {
            Key::Left => Some(Command::Back),
            Key::Right => Some(Command::Forward),
            _ => None,
        };
    }
    None
}

/// What a keystroke in the url bar did to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Edit {
    /// Still typing.
    Typing,
    /// Escape: leave the url alone.
    Cancel,
    /// Enter: go to what is in the buffer.
    Go,
    /// `ctrl+q`, which quits even from the url bar.
    Quit,
}

/// Apply one keystroke to the buffer. The whole of the url bar's behaviour,
/// with nothing to talk to, so that it can be tested a key at a time.
fn edit_step(buffer: &mut String, whole: bool, key: &KeyInput) -> Edit {
    match key.key {
        Key::Escape => Edit::Cancel,
        Key::Enter => Edit::Go,
        Key::Char('q') if key.mods.ctrl() => Edit::Quit,
        Key::Char('u') if key.mods.ctrl() => {
            buffer.clear();
            Edit::Typing
        }
        Key::Backspace => {
            if whole {
                buffer.clear();
            } else {
                buffer.pop();
            }
            Edit::Typing
        }
        _ => {
            if let Some(c) = key.text {
                if whole {
                    buffer.clear();
                }
                buffer.push(c);
            }
            Edit::Typing
        }
    }
}

/// Type into the url bar. Returns `false` only if the person quit.
fn edit_url(
    pane: &mut Pane,
    client: &mut Client,
    status: &mut Status,
    metrics: Metrics,
    key: KeyInput,
) -> Result<bool, String> {
    if key.action == KeyAction::Release {
        return Ok(true);
    }
    let whole = std::mem::take(&mut status.editing_whole);
    let Some(buffer) = status.editing.as_mut() else {
        return Ok(true);
    };
    match edit_step(buffer, whole, &key) {
        Edit::Typing => {}
        Edit::Quit => return Ok(false),
        Edit::Cancel => status.editing = None,
        Edit::Go => {
            let url = normalise(buffer);
            status.editing = None;
            status.url = url.clone();
            status.note = Some(format!("loading {url}"));
            if let Err(why) = client.call(
                "Page.navigate",
                Json::object(vec![("url", Json::string(&url))]),
            ) {
                status.note = Some(why);
            }
        }
    }
    redraw_status(pane, metrics, status)?;
    Ok(true)
}

/// Walk the history by one entry.
///
/// The list and the index come from the engine every time rather than being
/// tracked here: a page that pushed state, a redirect, or a link opened in the
/// same tab all change the history without this program being told, and a
/// cached index would send the person somewhere they have never been.
fn go(client: &mut Client, status: &mut Status, delta: i64) {
    let Ok(history) = client.call("Page.getNavigationHistory", Json::empty()) else {
        return;
    };
    let index = history
        .get("currentIndex")
        .and_then(Json::as_i64)
        .unwrap_or(0);
    let entries = history
        .get("entries")
        .and_then(Json::as_array)
        .unwrap_or(&[]);
    let wanted = index + delta;
    if wanted < 0 || wanted as usize >= entries.len() {
        status.note = Some(match delta {
            d if d < 0 => "nothing to go back to".to_string(),
            _ => "nothing to go forward to".to_string(),
        });
        return;
    }
    if let Some(id) = entries[wanted as usize].get("id").and_then(Json::as_i64) {
        let _ = client.call(
            "Page.navigateToHistoryEntry",
            Json::object(vec![("entryId", Json::number(id as f64))]),
        );
    }
}

fn send_key(client: &mut Client, key: &KeyInput) {
    match keys::dispatch(key) {
        Some(params) => {
            let _ = client.notify("Input.dispatchKeyEvent", params);
        }
        // A character with no key of its own — an input method's output, an
        // emoji — is text that was produced rather than a key that was
        // pressed, and that is exactly what insertText is for.
        None => {
            if key.action != KeyAction::Release {
                if let Some(c) = key.text {
                    let _ = client.notify("Input.insertText", keys::insert_text(&c.to_string()));
                }
            }
        }
    }
}

/// Presses close enough together in time and place to be one gesture.
#[derive(Default)]
struct Clicks {
    at: Option<(Instant, i32, i32, u32)>,
    count: u32,
}

impl Clicks {
    fn press(&mut self, button: u32, x: i32, y: i32) -> u32 {
        let now = Instant::now();
        let same = match self.at {
            Some((when, px, py, pb)) => {
                pb == button
                    && now.duration_since(when) < DOUBLE_CLICK
                    && (px - x).abs() <= DOUBLE_CLICK_SLOP
                    && (py - y).abs() <= DOUBLE_CLICK_SLOP
            }
            None => false,
        };
        self.count = if same { self.count + 1 } else { 1 };
        self.at = Some((now, x, y, button));
        self.count
    }
}

fn button_name(button: Option<u32>) -> &'static str {
    match button {
        Some(0) => "left",
        Some(1) => "middle",
        Some(2) => "right",
        _ => "none",
    }
}

/// The `buttons` mask CDP wants: left 1, right 2, middle 4.
fn button_bit(button: Option<u32>) -> u32 {
    match button {
        Some(0) => 1,
        Some(1) => 4,
        Some(2) => 2,
        _ => 0,
    }
}

fn send_mouse(
    client: &mut Client,
    parser: &Parser,
    clicks: &mut Clicks,
    buttons: &mut u32,
    metrics: Metrics,
    report: MouseInput,
) {
    let (x, y) = crate::input::page_point(&report, parser.pixel_coordinates(), metrics.cell, 1);
    if y < 0 {
        // The status row is this program's, and a click on it is not the
        // page's business. A release is still forwarded, so that a drag that
        // ended up there does not leave the page with a button held down.
        if report.kind != MouseKind::Release {
            return;
        }
    }
    let y = y.max(0);

    let (kind, extra) = match report.kind {
        MouseKind::Press => {
            *buttons |= button_bit(report.button);
            let count = clicks.press(report.button.unwrap_or(0), x, y);
            ("mousePressed", vec![("clickCount", Json::number(count))])
        }
        MouseKind::Release => {
            *buttons &= !button_bit(report.button);
            ("mouseReleased", vec![("clickCount", Json::number(1))])
        }
        MouseKind::Move => ("mouseMoved", Vec::new()),
        MouseKind::Wheel => (
            "mouseWheel",
            vec![
                ("deltaX", Json::number(report.wheel.0 as f64 * WHEEL_PIXELS)),
                ("deltaY", Json::number(report.wheel.1 as f64 * WHEEL_PIXELS)),
            ],
        ),
    };

    let mut fields = vec![
        ("type", Json::string(kind)),
        ("x", Json::number(x)),
        ("y", Json::number(y)),
        ("modifiers", Json::number(report.mods.cdp())),
        ("button", Json::string(button_name(report.button))),
        ("buttons", Json::number(*buttons)),
    ];
    fields.extend(extra);
    let _ = client.notify("Input.dispatchMouseEvent", Json::object(fields));
}

/// What a person typed, turned into something `Page.navigate` will take.
///
/// A scheme is left alone. Anything else gets `https://`, because a bare
/// `example.com` is what people type and a `Page.navigate` without a scheme
/// fails with an error rather than guessing. What is deliberately not here is
/// a search engine: sending what somebody typed to a third party because it
/// did not parse as a host is a decision about their privacy, and it is not
/// this program's to make.
pub fn normalise(input: &str) -> String {
    let text = input.trim();
    if text.is_empty() {
        return "about:blank".to_string();
    }
    if text.contains("://") || text.starts_with("about:") || text.starts_with("data:") {
        return text.to_string();
    }
    if text.starts_with('/') {
        return format!("file://{text}");
    }
    format!("https://{text}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::Mods;

    fn typed(c: char) -> KeyInput {
        KeyInput {
            key: Key::Char(c),
            mods: Mods::default(),
            action: KeyAction::Press,
            text: Some(c),
        }
    }

    fn key(k: Key, mods: u32) -> KeyInput {
        KeyInput {
            key: k,
            mods: Mods(mods),
            action: KeyAction::Press,
            text: None,
        }
    }

    #[test]
    fn the_programs_own_keys_and_nothing_else() {
        assert_eq!(
            command(&key(Key::Char('q'), Mods::CTRL)),
            Some(Command::Quit)
        );
        assert_eq!(
            command(&key(Key::Char('l'), Mods::CTRL)),
            Some(Command::EditUrl)
        );
        assert_eq!(
            command(&key(Key::Char('r'), Mods::CTRL)),
            Some(Command::Reload)
        );
        assert_eq!(command(&key(Key::Left, Mods::ALT)), Some(Command::Back));
        assert_eq!(command(&key(Key::Right, Mods::ALT)), Some(Command::Forward));

        // Everything else belongs to the page.
        assert_eq!(command(&key(Key::Char('q'), 0)), None);
        assert_eq!(command(&key(Key::Char('l'), Mods::ALT)), None);
        assert_eq!(command(&key(Key::Left, Mods::CTRL)), None);
        assert_eq!(command(&key(Key::Char('a'), Mods::CTRL)), None);
        assert_eq!(command(&key(Key::Char('t'), Mods::CTRL)), None);

        // And a release of one of them is not a second command.
        let mut released = key(Key::Char('q'), Mods::CTRL);
        released.action = KeyAction::Release;
        assert_eq!(command(&released), None);
    }

    #[test]
    fn what_a_person_types_becomes_a_url_the_engine_takes() {
        assert_eq!(normalise("example.com"), "https://example.com");
        assert_eq!(normalise("  example.com/a b "), "https://example.com/a b");
        assert_eq!(normalise("http://example.com"), "http://example.com");
        assert_eq!(normalise("https://example.com"), "https://example.com");
        assert_eq!(normalise("about:blank"), "about:blank");
        assert_eq!(normalise("data:text/html,hi"), "data:text/html,hi");
        assert_eq!(normalise("/etc/hostname"), "file:///etc/hostname");
        assert_eq!(normalise(""), "about:blank");
        assert_eq!(normalise("   "), "about:blank");
    }

    #[test]
    fn a_page_is_the_pane_less_the_status_row() {
        let metrics = Metrics {
            cols: 80,
            rows: 24,
            cell: (8, 16),
        };
        assert_eq!(page_pixels(metrics), (640, 368));
        assert_eq!(page_cells(metrics), Cells { cols: 80, rows: 23 });
        // 23 rows of 16 pixels is what the page is told it has, and 23 rows is
        // what the placement asks for: the two have to agree or the picture is
        // resampled every frame.
        assert_eq!(
            page_cells(metrics).rows * metrics.cell.1,
            page_pixels(metrics).1
        );
    }

    #[test]
    fn two_quick_presses_in_the_same_place_are_a_double_click() {
        let mut clicks = Clicks::default();
        assert_eq!(clicks.press(0, 10, 10), 1);
        assert_eq!(clicks.press(0, 10, 10), 2);
        assert_eq!(clicks.press(0, 12, 11), 3, "a little movement is allowed");
        assert_eq!(clicks.press(0, 100, 10), 1, "a lot is not");
        assert_eq!(clicks.press(0, 100, 10), 2);
        assert_eq!(clicks.press(2, 100, 10), 1, "and the other button is new");
    }

    #[test]
    fn the_button_names_and_the_mask_agree_with_each_other() {
        assert_eq!(button_name(Some(0)), "left");
        assert_eq!(button_name(Some(1)), "middle");
        assert_eq!(button_name(Some(2)), "right");
        assert_eq!(button_name(None), "none");
        assert_eq!(button_bit(Some(0)), 1);
        assert_eq!(button_bit(Some(2)), 2, "right is two, not four");
        assert_eq!(button_bit(Some(1)), 4);
        assert_eq!(button_bit(None), 0);
    }

    #[test]
    fn the_url_bar_starts_with_the_whole_address_selected() {
        // ctrl+l, then typing: what is there goes, the way it would in a
        // browser where the address was selected.
        let mut buffer = "https://example.com/a".to_string();
        assert_eq!(edit_step(&mut buffer, true, &typed('x')), Edit::Typing);
        assert_eq!(buffer, "x");
        // And from then on it is ordinary typing.
        edit_step(&mut buffer, false, &typed('y'));
        assert_eq!(buffer, "xy");
        edit_step(&mut buffer, false, &key(Key::Backspace, 0));
        assert_eq!(buffer, "x");

        // A backspace as the first thing deletes the lot, not one character.
        let mut buffer = "https://example.com/a".to_string();
        edit_step(&mut buffer, true, &key(Key::Backspace, 0));
        assert!(buffer.is_empty());

        // ctrl+u empties it whenever.
        let mut buffer = "half typed".to_string();
        edit_step(&mut buffer, false, &key(Key::Char('u'), Mods::CTRL));
        assert!(buffer.is_empty());
    }

    #[test]
    fn the_url_bar_knows_when_it_is_finished() {
        let mut buffer = "example.com".to_string();
        assert_eq!(edit_step(&mut buffer, false, &key(Key::Enter, 0)), Edit::Go);
        assert_eq!(
            buffer, "example.com",
            "enter does not change what was typed"
        );
        assert_eq!(
            edit_step(&mut buffer, false, &key(Key::Escape, 0)),
            Edit::Cancel
        );
        assert_eq!(
            edit_step(&mut buffer, false, &key(Key::Char('q'), Mods::CTRL)),
            Edit::Quit
        );
    }

    #[test]
    fn the_status_line_says_what_is_known() {
        let mut status = Status::default();
        assert_eq!(status.line(), "tos-browser");
        status.url = "https://example.com".to_string();
        assert_eq!(status.line(), "https://example.com");
        status.title = "Example".to_string();
        assert_eq!(status.line(), "Example  —  https://example.com");
        status.note = Some("loading".to_string());
        assert_eq!(status.line(), "loading");
    }
}
