//! The loop that makes the other modules a browser.
//!
//! One thread, one `poll`, three descriptors: the terminal, the pipe the tab in
//! front knocks on, and the pipe the browser connection knocks on. Everything
//! else is a reaction to one of those being readable, which is what keeps this
//! file a sequence of decisions rather than a scheduler. A background tab's
//! pipe is not polled — it has no screencast and nothing urgent to say — but
//! its queue is drained every pass, so a page that renames itself or opens a
//! dialog while it is not being looked at is still heard.
//!
//! The decisions worth knowing about are here rather than scattered: which
//! keys this program keeps for itself and which the compositor took first,
//! what a wheel notch is worth, what a click on the status row means, what
//! happens to the frames that arrive faster than a pane can draw them, and
//! what is on the screen in the moment between two tabs.

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
use crate::tabs::{Outcome, Tab, Tabs};

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

/// How long a command on the path between two tabs may take.
///
/// Shorter than [`crate::cdp::CALL_TIMEOUT`], because the page being left may
/// well be the reason it is being left: a tab that has stopped answering must
/// not make the tab somebody asked for wait fifteen seconds to appear.
const SWITCH_TIMEOUT: Duration = Duration::from_secs(3);

/// How long a new tab's socket has to be accepted.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

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

/// Everything the loop owns that is not the terminal, the tabs or the engine.
struct Chrome {
    painter: Painter,
    parser: Parser,
    clicks: Clicks,
    buttons: u32,
    metrics: Metrics,
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
    let browser_url = engine.browser_url().to_string();
    let target = crate::engine::page_target(&address, TARGET_TIMEOUT).map_err(|why| {
        let tail = engine.tail();
        if tail.is_empty() {
            why
        } else {
            format!("{why}; the engine said: {}", tail.join(" / "))
        }
    })?;
    let client = Client::connect(&target, Duration::from_secs(10))?;
    let first = crate::engine::target_of(&target)
        .ok_or_else(|| format!("the engine's page has no target id: {target}"))?
        .to_string();

    // A second connection, to the browser rather than to a page. It is the
    // only one that can hear about a target this program did not open — a
    // `target=_blank`, a `window.open` — and the only one that can open, close
    // or raise one.
    let mut browser = Client::connect(&browser_url, Duration::from_secs(10))?;
    browser.call(
        "Target.setDiscoverTargets",
        Json::object(vec![("discover", Json::Bool(true))]),
    )?;
    let mut tabs = Tabs::new(Tab::new(first, client, "about:blank"));

    let mut pane = Pane::enter(0, 1).map_err(|e| format!("cannot take the terminal: {e}"))?;
    let outcome = drive(
        &mut pane,
        &mut tabs,
        &mut browser,
        &mut engine,
        &browser_url,
        options,
    );
    pane.leave();
    // Dropping the tabs closes every page socket, which is all a tab is once
    // the engine is about to be killed anyway.
    drop(tabs);
    browser.close();
    engine.kill();
    outcome
}

/// Everything between taking the terminal and giving it back.
fn drive(
    pane: &mut Pane,
    tabs: &mut Tabs<Client>,
    browser: &mut Client,
    engine: &mut Engine,
    browser_url: &str,
    options: Options,
) -> Result<(), String> {
    let metrics = pane
        .metrics()
        .map_err(|e| format!("cannot measure the pane: {e}"))?;
    let mut chrome = Chrome {
        painter: Painter::new(),
        parser: Parser::new(),
        clicks: Clicks::default(),
        buttons: 0,
        metrics,
        editing: None,
        editing_whole: false,
    };

    activate(tabs, browser, &mut chrome)?;

    let url = normalise(&options.url);
    if let Some(tab) = tabs.active_mut() {
        tab.url = url.clone();
        tab.note = Some(format!("loading {url}"));
        tab.loading = true;
    }
    redraw_row(pane, tabs, &chrome)?;
    if let Some(why) = navigate(tabs, &url) {
        if let Some(tab) = tabs.active_mut() {
            tab.note = Some(why);
        }
        redraw_row(pane, tabs, &chrome)?;
    }

    let mut last_check = Instant::now();
    let mut buf = [0u8; 8192];

    while !QUIT.load(Ordering::SeqCst) {
        if tabs.is_empty() {
            // The last tab closed itself, which is the page saying the browser
            // is over — the same thing `ctrl+w` on the last tab means.
            return Ok(());
        }
        if RESIZED.swap(false, Ordering::SeqCst) {
            chrome.metrics = pane
                .metrics()
                .map_err(|e| format!("cannot measure the pane: {e}"))?;
            pane.write(b"\x1b[2J").map_err(|e| e.to_string())?;
            let metrics = chrome.metrics;
            if let Some(tab) = tabs.active_mut() {
                emulate(&mut tab.connection, metrics)?;
                restart_screencast(&mut tab.connection, metrics)?;
            }
            redraw_row(pane, tabs, &chrome)?;
        }

        // The engine is a child process and can die at any point; without this
        // the first sign would be a command that timed out fifteen seconds
        // later.
        if last_check.elapsed() > Duration::from_millis(500) {
            last_check = Instant::now();
            engine.check()?;
        }
        if let Some(ended) = browser.ended() {
            return Err(format!("the engine stopped talking: {ended}"));
        }
        reap_dead_tabs(pane, tabs, browser, &mut chrome)?;

        let wake = tabs.active().map(|tab| tab.connection.wake_fd());
        let mut watching = vec![pane.input_fd(), browser.wake_fd()];
        watching.extend(wake);
        let ready =
            tty::poll_readable(&watching, 50).map_err(|e| format!("cannot wait for input: {e}"))?;

        if ready.contains(&pane.input_fd()) {
            match tty::read_available(pane.input_fd(), &mut buf) {
                Ok(ReadOutcome::Data(n)) => {
                    let inputs = chrome.parser.feed(&buf[..n]);
                    for input in inputs {
                        if !handle_input(pane, tabs, browser, &mut chrome, browser_url, input)? {
                            return Ok(());
                        }
                    }
                }
                Ok(ReadOutcome::Eof) => return Ok(()),
                Ok(ReadOutcome::WouldBlock) => {}
                Err(err) => return Err(format!("cannot read the terminal: {err}")),
            }
        } else if let Some(input) = chrome.parser.flush() {
            // Nothing arrived, so a held escape was the Escape key after all.
            if !handle_input(pane, tabs, browser, &mut chrome, browser_url, input)? {
                return Ok(());
            }
        }

        if ready.contains(&browser.wake_fd()) {
            browser.drain_wake();
        }
        if let Some(wake) = wake {
            // By the tab that owns the descriptor rather than by whichever tab
            // is active now: handling a key may have switched tabs since the
            // poll, and the pipe that was readable is the one to empty.
            if ready.contains(&wake) {
                if let Some(tab) = tabs.iter().find(|tab| tab.connection.wake_fd() == wake) {
                    tab.connection.drain_wake();
                }
            }
        }
        // Which pages exist first, then what the page in front is doing: a
        // frame is read from whichever tab is active once the list has settled,
        // and never from one that has just been left behind.
        handle_target_events(pane, tabs, browser, &mut chrome, browser_url)?;
        handle_page_events(pane, tabs, &mut chrome)?;
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

/// What a page calls itself, asked of the page.
///
/// Deliberately not `Target.targetInfoChanged`'s `title`, which would cost
/// nothing and be wrong: against `chromium-shell` that field is derived from
/// the url and a `document.title` set by a script never changes it. See
/// [`crate::tabs`] for the measurement. `None` means the page did not answer
/// in time, and a tab keeps the name it had rather than losing it to a page
/// that is busy.
pub fn page_title(client: &mut Client) -> Option<String> {
    let answer = client
        .call_within(
            "Runtime.evaluate",
            Json::object(vec![
                ("expression", Json::string("document.title")),
                ("returnByValue", Json::Bool(true)),
            ]),
            Duration::from_secs(2),
        )
        .ok()?;
    answer
        .path(&["result", "value"])
        .and_then(Json::as_str)
        .map(str::to_string)
}

/// Send the active tab somewhere. The sentence, if it would not go.
fn navigate(tabs: &mut Tabs<Client>, url: &str) -> Option<String> {
    let tab = tabs.active_mut()?;
    tab.connection
        .call(
            "Page.navigate",
            Json::object(vec![("url", Json::string(url))]),
        )
        .err()
}

/// Start the active tab painting: sized, told it is in front, and casting.
///
/// Every command here is one a background tab did not have run on it, because
/// a background tab is a page with no screencast and no viewport of ours.
/// `Page.enable` is idempotent, so a tab that has been active before is not a
/// special case.
fn activate(
    tabs: &mut Tabs<Client>,
    browser: &mut Client,
    chrome: &mut Chrome,
) -> Result<(), String> {
    let Some(target) = tabs.active_target().map(str::to_string) else {
        return Ok(());
    };
    // Chromium treats a target that is not in front as a hidden page: its
    // animations stop and its `requestAnimationFrame` never fires, so the
    // screencast of a tab that was never activated is one frame and then
    // nothing.
    let _ = browser.call_within(
        "Target.activateTarget",
        Json::object(vec![("targetId", Json::string(&target))]),
        SWITCH_TIMEOUT,
    );
    let metrics = chrome.metrics;
    let Some(tab) = tabs.active_mut() else {
        return Ok(());
    };
    tab.connection
        .call_within("Page.enable", Json::empty(), SWITCH_TIMEOUT)?;
    emulate(&mut tab.connection, metrics)?;
    // Whatever this tab queued and nobody has read goes in the bin, frames
    // above all: the newest of them is older than the tab was, and painting it
    // would put the page as it looked before it was left behind on screen
    // under the row of the tab that has just been chosen. Asking for the title
    // afterwards is what makes that safe: the only thing worth having in that
    // queue was the news that the page had finished loading, and this is that
    // news asked for directly.
    let _ = tab.connection.events();
    if let Some(title) = page_title(&mut tab.connection) {
        tab.title = title;
    }
    start_screencast(&mut tab.connection, metrics)?;
    Ok(())
}

/// Stop a tab painting, if it is still in the list.
fn deactivate(tabs: &mut Tabs<Client>, target: &str) {
    let Some(index) = tabs.index_of(target) else {
        return;
    };
    if let Some(tab) = tabs.get_mut(index) {
        let _ = tab
            .connection
            .call_within("Page.stopScreencast", Json::empty(), SWITCH_TIMEOUT);
    }
}

/// Follow a change of which tab is in front all the way through.
///
/// `was` is the target that was in front before whatever just happened. When
/// it is still the one in front this does nothing at all, so every path that
/// might have switched can call it and none of them has to know whether it
/// did.
fn switched(
    pane: &mut Pane,
    tabs: &mut Tabs<Client>,
    browser: &mut Client,
    chrome: &mut Chrome,
    was: Option<String>,
) -> Result<(), String> {
    let now = tabs.active_target().map(str::to_string);
    if now == was {
        return Ok(());
    }
    if let Some(was) = &was {
        deactivate(tabs, was);
    }
    if now.is_none() {
        return Ok(());
    }
    // The picture on the screen is the page that is no longer in front. It
    // goes now rather than when the new tab paints, because the new tab may
    // take a network's worth of time to paint anything and the old page under
    // the new tab's title would be a lie for all of it.
    pane.write(&crate::graphics::clear_command())
        .map_err(|e| e.to_string())?;
    pane.write(b"\x1b[2;1H\x1b[J").map_err(|e| e.to_string())?;
    if let Err(why) = activate(tabs, browser, chrome) {
        if let Some(tab) = tabs.active_mut() {
            tab.note = Some(why);
        }
    }
    Ok(())
}

/// Open a page in a new tab and switch to it.
fn open_tab(
    tabs: &mut Tabs<Client>,
    browser: &mut Client,
    browser_url: &str,
    url: &str,
) -> Result<(), String> {
    let created = browser.call(
        "Target.createTarget",
        Json::object(vec![("url", Json::string(url))]),
    )?;
    let target = created
        .get("targetId")
        .and_then(Json::as_str)
        .ok_or_else(|| "the engine opened a page and did not say which".to_string())?
        .to_string();
    let connection = connect_tab(browser_url, &target)?;
    tabs.open(Tab::new(target, connection, url));
    Ok(())
}

/// Connect to a target and start listening to its page, whether or not it is
/// the tab in front.
///
/// `Page.enable` from the moment the tab exists rather than from the moment it
/// is looked at: a tab that is loading in the background still has to tell the
/// strip when it has a name, and the events it sends before anybody asks are
/// the only notice there is.
fn connect_tab(browser_url: &str, target: &str) -> Result<Client, String> {
    let socket = crate::engine::target_url(browser_url, target)?;
    let mut connection = Client::connect(&socket, CONNECT_TIMEOUT)?;
    connection.call_within("Page.enable", Json::empty(), SWITCH_TIMEOUT)?;
    Ok(connection)
}

/// Close one tab: the page in the engine, and the socket to it.
///
/// In that order, and both. A target closed while its connection is still open
/// is a page the engine keeps alive for the debugger that is still attached;
/// a connection closed without the target is a page that goes on rendering for
/// nobody.
///
/// Dropping the connection is also what makes a frame that was in flight
/// harmless: it is in that client's mailbox, and the mailbox goes with the
/// client. Nothing can paint it over the tab that comes next, because nothing
/// will ever read it.
fn close_tab(tabs: &mut Tabs<Client>, browser: &mut Client, index: usize) {
    let Some(mut tab) = tabs.close(index) else {
        return;
    };
    let _ = browser.call_within(
        "Target.closeTarget",
        Json::object(vec![("targetId", Json::string(&tab.target))]),
        SWITCH_TIMEOUT,
    );
    tab.connection.close();
}

/// Drop any tab whose socket has gone.
///
/// A target that is closed takes its socket with it, so this and
/// `Target.targetDestroyed` are two ways of hearing the same news and either
/// may arrive first. Which is why neither a sentence nor an error comes out of
/// here: a tab that went because the page called `window.close` must not be
/// reported as a failure just because the socket noticed before the browser
/// connection did, and whether a person sees a message for that would
/// otherwise depend on which of two sockets was read first. A tab that died
/// for a reason worth a sentence gets one from `Target.targetCrashed`, which
/// arrives on the browser connection either way; an engine that died is caught
/// by [`Engine::check`] and by the browser connection ending, neither of which
/// is a page.
fn reap_dead_tabs(
    pane: &mut Pane,
    tabs: &mut Tabs<Client>,
    browser: &mut Client,
    chrome: &mut Chrome,
) -> Result<(), String> {
    let dead: Vec<usize> = tabs
        .iter()
        .enumerate()
        .filter(|(_, tab)| tab.connection.ended().is_some())
        .map(|(index, _)| index)
        .collect();
    if dead.is_empty() {
        return Ok(());
    }
    let was = tabs.active_target().map(str::to_string);
    for index in dead.into_iter().rev() {
        tabs.close(index);
    }
    if tabs.is_empty() {
        // The loop's own check ends the program, cleanly.
        return Ok(());
    }
    switched(pane, tabs, browser, chrome, was)?;
    redraw_row(pane, tabs, chrome)
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

/// Draw the top row.
///
/// Three things share it, and which one is showing is a decision rather than a
/// layout. A url being typed takes the whole row however many tabs are open:
/// it is the one moment the person is writing rather than reading, and half a
/// url beside a strip would be neither. One tab is the row this program had
/// before it had tabs, byte for byte — a browser showing one page should not
/// look like a browser with a tab bar in it. More than one is the strip.
fn redraw_row(pane: &mut Pane, tabs: &Tabs<Client>, chrome: &Chrome) -> Result<(), String> {
    let cols = chrome.metrics.cols;
    let Some(active) = tabs.active() else {
        return Ok(());
    };
    let bytes = if chrome.editing.is_some() || tabs.len() < 2 {
        screen::status_line(cols, &active.line(), chrome.editing.as_deref())
    } else {
        let labels: Vec<screen::TabLabel> = tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| screen::TabLabel {
                title: tab.label(),
                active: index == tabs.active_index(),
            })
            .collect();
        screen::tab_line(cols, &labels, &active.url)
    };
    pane.write(&bytes).map_err(|e| e.to_string())
}

/// Everything the browser connection said: which pages there are.
fn handle_target_events(
    pane: &mut Pane,
    tabs: &mut Tabs<Client>,
    browser: &mut Client,
    chrome: &mut Chrome,
    browser_url: &str,
) -> Result<(), String> {
    let events = browser.events();
    if events.is_empty() {
        return Ok(());
    }
    let was = tabs.active_target().map(str::to_string);
    let mut redraw = false;
    let mut note: Option<String> = None;

    for event in &events {
        let outcome = tabs.take(event, |target| connect_tab(browser_url, target));
        match outcome {
            Outcome::Ignored => {}
            Outcome::Opened | Outcome::Renamed => redraw = true,
            Outcome::Gone { mut tab, why } => {
                tab.connection.close();
                redraw = true;
                note = note.or(why);
            }
            Outcome::Failed(why) => {
                note = Some(why);
                redraw = true;
            }
        }
    }

    if tabs.is_empty() {
        // Nothing left to show, so the program is over. A page that closed
        // itself is a clean exit; one that died says why on the way out, in the
        // shell, where the terminal has been given back and it can be read.
        return match note {
            Some(why) => Err(why),
            None => Ok(()),
        };
    }
    if let (Some(note), Some(tab)) = (note, tabs.active_mut()) {
        tab.note = Some(note);
    }
    switched(pane, tabs, browser, chrome, was)?;
    if redraw {
        redraw_row(pane, tabs, chrome)?;
    }
    Ok(())
}

/// Everything every tab said since the last look.
///
/// Every tab and not only the one in front, because a background tab is a page
/// that is still running: it loads, it navigates, it renames itself and it can
/// open a dialog that stops it rendering, and the strip has to say so. What a
/// background tab does not produce is frames, because its screencast was
/// stopped when it stopped being in front, so this costs one drained queue per
/// tab and nothing else.
///
/// Frames are coalesced: every one is acknowledged, because the engine sends
/// no more until it has been, but only the last is drawn. A pane that cannot
/// keep up with sixty frames a second should fall behind by dropping frames,
/// not by drawing a queue of stale ones.
fn handle_page_events(
    pane: &mut Pane,
    tabs: &mut Tabs<Client>,
    chrome: &mut Chrome,
) -> Result<(), String> {
    let active = tabs.active_index();
    let mut newest_frame: Option<Vec<u8>> = None;
    let mut redraw = false;

    for index in 0..tabs.len() {
        let Some(tab) = tabs.get_mut(index) else {
            continue;
        };
        let events = tab.connection.events();
        if events.is_empty() {
            continue;
        }
        let mut ask_title = false;

        for Event { method, params } in events {
            match method.as_str() {
                "Page.screencastFrame" => {
                    if let Some(session) = params.get("sessionId").and_then(Json::as_i64) {
                        let _ = tab.connection.notify(
                            "Page.screencastFrameAck",
                            Json::object(vec![("sessionId", Json::number(session as f64))]),
                        );
                    }
                    // A frame from a tab that is not in front is a frame from
                    // before it was left: it is acknowledged, so that the tab
                    // is not left waiting, and thrown away.
                    if index != active {
                        continue;
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
                        tab.url = url.to_string();
                        tab.title.clear();
                        tab.note = None;
                        tab.loading = true;
                        redraw = true;
                    }
                }
                "Page.loadEventFired" => {
                    tab.loading = false;
                    ask_title = true;
                    redraw = true;
                }
                "Page.javascriptDialogOpening" => {
                    // Nothing here can show an alert, and a page whose dialog
                    // is never answered stops rendering.
                    let _ = tab.connection.notify(
                        "Page.handleJavaScriptDialog",
                        Json::object(vec![("accept", Json::Bool(false))]),
                    );
                }
                _ => {}
            }
        }

        if ask_title {
            if let Some(title) = page_title(&mut tab.connection) {
                tab.title = title;
            }
        }
    }

    if redraw {
        redraw_row(pane, tabs, chrome)?;
    }
    if let Some(png) = newest_frame {
        let bytes = chrome
            .painter
            .frame(&png, page_cells(chrome.metrics), PAGE_ROW, 1);
        pane.write(&bytes).map_err(|e| e.to_string())?;
        // The picture does not move the cursor (`C=1`), but the status line
        // owns the cursor's position when the url is being typed, so it is
        // written again rather than left where the last frame found it.
        if chrome.editing.is_some() {
            redraw_row(pane, tabs, chrome)?;
        }
    }
    Ok(())
}

/// Handle one thing the terminal said. `false` means quit.
fn handle_input(
    pane: &mut Pane,
    tabs: &mut Tabs<Client>,
    browser: &mut Client,
    chrome: &mut Chrome,
    browser_url: &str,
    input: Input,
) -> Result<bool, String> {
    match input {
        Input::Mode { .. } => {}
        Input::Key(key) => {
            if chrome.editing.is_some() {
                return edit_url(pane, tabs, chrome, key);
            }
            let was = tabs.active_target().map(str::to_string);
            match command(&key) {
                Some(Command::Quit) => return Ok(false),
                Some(Command::EditUrl) => {
                    chrome.editing = tabs.active().map(|tab| tab.url.clone());
                    chrome.editing_whole = true;
                    redraw_row(pane, tabs, chrome)?;
                }
                Some(Command::Reload) => {
                    if let Some(tab) = tabs.active_mut() {
                        let _ = tab.connection.call("Page.reload", Json::empty());
                    }
                }
                Some(Command::Back) => go(tabs, -1),
                Some(Command::Forward) => go(tabs, 1),
                Some(Command::NewTab) => {
                    match open_tab(tabs, browser, browser_url, "about:blank") {
                        Ok(()) => {
                            switched(pane, tabs, browser, chrome, was)?;
                            // A new tab is a tab somebody is about to type an
                            // address into, so it opens with the cursor in the
                            // url bar — and with nothing in it, because there
                            // is no address here to replace.
                            chrome.editing = Some(String::new());
                            chrome.editing_whole = false;
                        }
                        Err(why) => {
                            if let Some(tab) = tabs.active_mut() {
                                tab.note = Some(why);
                            }
                        }
                    }
                    redraw_row(pane, tabs, chrome)?;
                }
                Some(Command::CloseTab) => {
                    // Closing the only tab is closing the browser, which is
                    // what every browser does and what `ctrl+q` does here.
                    if tabs.len() < 2 {
                        return Ok(false);
                    }
                    close_tab(tabs, browser, tabs.active_index());
                    switched(pane, tabs, browser, chrome, was)?;
                    redraw_row(pane, tabs, chrome)?;
                }
                Some(what @ (Command::NextTab | Command::PreviousTab | Command::SelectTab(_))) => {
                    let moved = match what {
                        Command::NextTab => tabs.select_next(),
                        Command::PreviousTab => tabs.select_previous(),
                        Command::SelectTab(number) => tabs.select(number),
                        _ => false,
                    };
                    if moved {
                        switched(pane, tabs, browser, chrome, was)?;
                        redraw_row(pane, tabs, chrome)?;
                    }
                }
                None => {
                    if let Some(tab) = tabs.active_mut() {
                        send_key(&mut tab.connection, &key);
                    }
                }
            }
        }
        Input::Mouse(report) => {
            let metrics = chrome.metrics;
            let pixels = chrome.parser.pixel_coordinates();
            let clicks = &mut chrome.clicks;
            let buttons = &mut chrome.buttons;
            if let Some(tab) = tabs.active_mut() {
                send_mouse(
                    &mut tab.connection,
                    pixels,
                    clicks,
                    buttons,
                    metrics,
                    report,
                );
            }
        }
    }
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
    NewTab,
    CloseTab,
    NextTab,
    PreviousTab,
    /// The nth tab, counted from one.
    SelectTab(usize),
}

/// Which keys this program answers, and which it hands to the page.
///
/// The tab keys are the ones a browser has taught everybody — `ctrl+t`,
/// `ctrl+w`, `ctrl+tab`, `alt+1`..`alt+9` — and every one of them was checked
/// against `compositor/tos-session/src/keys.rs` before being taken, because a
/// key the compositor binds is a key that never reaches a pane at all. What
/// the compositor has near these: `ctrl+shift+t` opens a workspace,
/// `ctrl+shift+w` closes a pane, `ctrl+alt+shift+t` renames a workspace,
/// `ctrl+shift+1`..`9` and `super+1`..`9` select workspaces, `ctrl+a` is the
/// leader and `ctrl+space` opens the input method. None of those is one of
/// these: the compositor's tab-ish keys all carry shift or super, and this
/// program's all carry neither. `alt` it uses for nothing at all, which is why
/// the digits are there rather than on ctrl, where `ctrl+1` would be a key a
/// page can legitimately be sent.
///
/// `ctrl+tab` reaches the pane as `CSI 9;5u` and `ctrl+shift+tab` as
/// `CSI 9;6u`, because this program asks for the Kitty keyboard protocol's
/// disambiguate flag (`screen::KEYBOARD_FLAGS`) and `tos_input::encode` sends
/// a modified tab in the protocol's form rather than as a bare `\t`. In a
/// terminal that does not speak it, ctrl+tab arrives as a plain tab and goes
/// to the page — which is the right failure, since the page is where tab
/// usually belongs.
fn command(key: &KeyInput) -> Option<Command> {
    if key.action == KeyAction::Release {
        return None;
    }
    if key.mods.ctrl() && !key.mods.alt() {
        return match key.key {
            Key::Char('q') => Some(Command::Quit),
            Key::Char('l') => Some(Command::EditUrl),
            Key::Char('r') => Some(Command::Reload),
            Key::Char('t') => Some(Command::NewTab),
            Key::Char('w') => Some(Command::CloseTab),
            Key::Tab if key.mods.shift() => Some(Command::PreviousTab),
            Key::Tab => Some(Command::NextTab),
            _ => None,
        };
    }
    if key.mods.alt() && !key.mods.ctrl() {
        return match key.key {
            Key::Left => Some(Command::Back),
            Key::Right => Some(Command::Forward),
            Key::Char(digit @ '1'..='9') => Some(Command::SelectTab(digit as usize - '0' as usize)),
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
    tabs: &mut Tabs<Client>,
    chrome: &mut Chrome,
    key: KeyInput,
) -> Result<bool, String> {
    if key.action == KeyAction::Release {
        return Ok(true);
    }
    let whole = std::mem::take(&mut chrome.editing_whole);
    let Some(buffer) = chrome.editing.as_mut() else {
        return Ok(true);
    };
    match edit_step(buffer, whole, &key) {
        Edit::Typing => {}
        Edit::Quit => return Ok(false),
        Edit::Cancel => chrome.editing = None,
        Edit::Go => {
            let url = normalise(buffer);
            chrome.editing = None;
            if let Some(tab) = tabs.active_mut() {
                tab.url = url.clone();
                tab.note = Some(format!("loading {url}"));
                tab.loading = true;
            }
            if let Some(why) = navigate(tabs, &url) {
                if let Some(tab) = tabs.active_mut() {
                    tab.note = Some(why);
                }
            }
        }
    }
    redraw_row(pane, tabs, chrome)?;
    Ok(true)
}

/// Walk the active tab's history by one entry.
///
/// The list and the index come from the engine every time rather than being
/// tracked here: a page that pushed state, a redirect, or a link opened in the
/// same tab all change the history without this program being told, and a
/// cached index would send the person somewhere they have never been.
fn go(tabs: &mut Tabs<Client>, delta: i64) {
    let Some(tab) = tabs.active_mut() else {
        return;
    };
    let Ok(history) = tab
        .connection
        .call("Page.getNavigationHistory", Json::empty())
    else {
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
        tab.note = Some(match delta {
            d if d < 0 => "nothing to go back to".to_string(),
            _ => "nothing to go forward to".to_string(),
        });
        return;
    }
    let entry = entries[wanted as usize].get("id").and_then(Json::as_i64);
    if let Some(id) = entry {
        let _ = tab.connection.call(
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
    pixel_coordinates: bool,
    clicks: &mut Clicks,
    buttons: &mut u32,
    metrics: Metrics,
    report: MouseInput,
) {
    let (x, y) = crate::input::page_point(&report, pixel_coordinates, metrics.cell, 1);
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
        assert_eq!(command(&key(Key::Tab, 0)), None, "tab is the page's");

        // And a release of one of them is not a second command.
        let mut released = key(Key::Char('q'), Mods::CTRL);
        released.action = KeyAction::Release;
        assert_eq!(command(&released), None);
    }

    #[test]
    fn the_tab_keys_are_the_ones_a_browser_taught_and_the_compositor_left() {
        assert_eq!(
            command(&key(Key::Char('t'), Mods::CTRL)),
            Some(Command::NewTab)
        );
        assert_eq!(
            command(&key(Key::Char('w'), Mods::CTRL)),
            Some(Command::CloseTab)
        );
        assert_eq!(command(&key(Key::Tab, Mods::CTRL)), Some(Command::NextTab));
        assert_eq!(
            command(&key(Key::Tab, Mods::CTRL | Mods::SHIFT)),
            Some(Command::PreviousTab)
        );
        for n in 1..=9usize {
            let digit = Key::Char((b'0' + n as u8) as char);
            assert_eq!(
                command(&key(digit, Mods::ALT)),
                Some(Command::SelectTab(n)),
                "alt+{n}"
            );
        }
        // There is no tab zero, and a digit on its own is typing.
        assert_eq!(command(&key(Key::Char('0'), Mods::ALT)), None);
        assert_eq!(command(&key(Key::Char('1'), 0)), None);
        // ctrl+digit stays the page's: a page may bind it, and the compositor
        // puts its own workspace digits on ctrl+shift and on super.
        assert_eq!(command(&key(Key::Char('1'), Mods::CTRL)), None);
        // alt+tab is the window manager's everywhere, and is not taken here.
        assert_eq!(command(&key(Key::Tab, Mods::ALT)), None);
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
        let mut tab: Tab<()> = Tab::new("t", (), "");
        assert_eq!(tab.line(), "tos-browser");
        tab.url = "https://example.com".to_string();
        assert_eq!(tab.line(), "https://example.com");
        tab.title = "Example".to_string();
        assert_eq!(tab.line(), "Example  —  https://example.com");
        tab.note = Some("loading".to_string());
        assert_eq!(tab.line(), "loading");
    }
}
