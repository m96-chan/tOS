//! The end this crate exists for: a real engine, real frames, and a real
//! terminal parsing what would go down the pane's pseudoterminal.
//!
//! The terminal here is `tos_term::Terminal` with the compositor's own
//! `ImageFiles` installed, which is exactly what `Pane::spawn` gives a pane —
//! so the `t=s` path is the real one, names and unlinking included, and the
//! frames go through the store that holds them in a session. What is missing
//! compared with a booted tOS is the renderer and the PTY, neither of which
//! can say anything about whether the protocol is right.
//!
//! Every test here runs only when `TOS_BROWSER_ENGINE` names the engine to
//! use, and skips otherwise — not when a Chromium happens to be on `PATH`.
//! The program itself searches `PATH`, because a person who installed a
//! browser wants it found; a test is different. A machine that builds tOS is
//! not a machine that agreed to run whatever browser its image carries for
//! some other job, and the first run on GitHub's runner found one, started
//! it, and watched it abort — a Chromium of somebody else's, with sandbox
//! rules of somebody else's, proving nothing about this crate either way.
//! Naming the engine is the consent. The skips are not quiet: the reason is
//! printed, so that a run which proved nothing does not read like a run which
//! proved something.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tos_browser::cdp::{Client, Pending};
use tos_browser::engine::{self, Engine};
use tos_browser::graphics::{Painter, Raw, IMAGE_ID};
use tos_browser::input::{Key, KeyAction, KeyInput, Mods};
use tos_browser::json::Json;
use tos_browser::keys;
use tos_browser::motion::{self, Motion};
use tos_browser::scroll::{self, Animator, Dispatch, Step, Wheel};
use tos_browser::tabs::{Outcome, Tab, Tabs};
use tos_compositor::ImageFiles;
use tos_preview::fit::Cells;

/// The page the frames come from.
///
/// It has to be a page whose frames cost what a real one's do: a flat colour
/// compresses to four kilobytes and would make the terminal look faster than
/// it is, and a full-screen gradient to four hundred, which would make it look
/// slower. So this is what a page mostly is — black text on white, a screenful
/// of it — with one coloured block that moves every animation frame so the
/// engine has a reason to repaint. That lands near the 58 kB per frame the
/// engine was measured at. The two listeners put what they were sent into the
/// title, where a test can read it back.
const PAGE: &str = "data:text/html,\
<body style='margin:0;height:100vh;font:12px monospace;overflow:hidden;background:%23fff'>\
<div id=b style='position:absolute;width:160px;height:40px;background:%23c33'></div>\
<div id=t></div><script>\
document.title='ready';\
addEventListener('keydown',function(e){document.title='key '+e.key+' '+e.code+' '+e.keyCode});\
addEventListener('mousedown',function(e){document.title='click '+e.button+' '+e.clientX+' '+e.clientY});\
var rows=[];for(var i=0;i<28;i++){rows.push(i+' the quick brown fox jumps over the lazy dog \
0123456789 ABCDEFGHIJKLMNOPQRSTUVWXYZ')}\
document.getElementById('t').innerText=rows.join(String.fromCharCode(10));\
var b=document.getElementById('b'),n=0;\
function f(){n=n>400?0:n+3;b.style.left=n+'px';b.style.top=(n/2)+'px';\
requestAnimationFrame(f)}f();\
</script></body>";

const WIDTH: u32 = 640;
const HEIGHT: u32 = 360;
const CELL: (u32, u32) = (8, 16);

/// The same, with the target id the page connection belongs to, for the tests
/// that are about which targets exist.
fn connect_with_target() -> Option<(Engine, Client, String)> {
    let (engine, client) = connect()?;
    // `connect` found the page in `/json/list`; the id is the tail of the url
    // it found, which is exactly how the program itself gets it.
    let address = engine.address().expect("an address");
    let url = engine::page_target(&address, Duration::from_secs(20)).expect("a page");
    let target = engine::target_of(&url).expect("an id").to_string();
    Some((engine, client, target))
}

/// Connect to a fresh engine, or say why the test is not running.
fn connect() -> Option<(Engine, Client)> {
    if std::env::var_os(engine::ENGINE_ENV).is_none() {
        eprintln!(
            "skipped: {} is not set; name a Chromium to run this against",
            engine::ENGINE_ENV
        );
        return None;
    }
    match engine::locate() {
        Ok(path) => eprintln!("engine: {}", path.display()),
        Err(why) => {
            eprintln!("skipped: {why}");
            return None;
        }
    }
    let engine = Engine::launch(Duration::from_secs(30)).expect("the engine starts");
    let address = engine.address().expect("an address");
    let target = match engine::page_target(&address, Duration::from_secs(20)) {
        Ok(target) => target,
        Err(why) => panic!("{why}; the engine said: {}", engine.tail().join(" / ")),
    };
    let client = Client::connect(&target, Duration::from_secs(10)).expect("a connection");
    Some((engine, client))
}

/// Get the page ready: sized, loaded, and painting.
fn prepare(client: &mut Client) {
    client
        .call("Page.enable", Json::empty())
        .expect("Page.enable");
    client
        .call(
            "Emulation.setDeviceMetricsOverride",
            Json::object(vec![
                ("width", Json::number(WIDTH)),
                ("height", Json::number(HEIGHT)),
                ("deviceScaleFactor", Json::number(1)),
                ("mobile", Json::Bool(false)),
            ]),
        )
        .expect("the viewport");
    client
        .call(
            "Page.navigate",
            Json::object(vec![("url", Json::string(PAGE))]),
        )
        .expect("the page loads");
    wait_for_title(client, "ready", Duration::from_secs(10));
}

fn title(client: &mut Client) -> String {
    client
        .call_within(
            "Runtime.evaluate",
            Json::object(vec![
                ("expression", Json::string("document.title")),
                ("returnByValue", Json::Bool(true)),
            ]),
            Duration::from_secs(5),
        )
        .ok()
        .and_then(|value| {
            value
                .path(&["result", "value"])
                .and_then(Json::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default()
}

fn wait_for_title(client: &mut Client, wanted: &str, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    let mut last = String::new();
    while Instant::now() < deadline {
        last = title(client);
        if last.starts_with(wanted) {
            return last;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    last
}

fn a_terminal(dir: &std::path::Path) -> tos_term::Terminal {
    sized_terminal(dir, WIDTH, HEIGHT)
}

/// A terminal with the compositor's own file reader, one row taller than the
/// page so that the status line has somewhere to go.
fn sized_terminal(dir: &std::path::Path, width: u32, height: u32) -> tos_term::Terminal {
    let mut terminal = tos_term::Terminal::new(
        (width / CELL.0) as usize,
        (height / CELL.1) as usize + 1,
        tos_term::TerminalConfig::default(),
    );
    terminal.set_medium_reader(Box::new(ImageFiles::at(
        vec![dir.to_path_buf()],
        dir.to_path_buf(),
    )));
    terminal
}

/// The next screencast frame as the engine sent it, with the capture time it
/// carries. Acknowledges everything it takes, as the program does.
fn take_frames(client: &mut Client) -> Vec<(Vec<u8>, Option<f64>)> {
    let mut frames = Vec::new();
    for event in client.events() {
        if event.method != "Page.screencastFrame" {
            continue;
        }
        if let Some(session) = event.params.get("sessionId").and_then(Json::as_i64) {
            let _ = client.notify(
                "Page.screencastFrameAck",
                Json::object(vec![("sessionId", Json::number(session as f64))]),
            );
        }
        let Some(data) = event.params.get("data").and_then(Json::as_str) else {
            continue;
        };
        let stamp = event
            .params
            .path(&["metadata", "timestamp"])
            .and_then(Json::as_f64);
        if let Ok(bytes) = tos_browser::base64::decode(data.as_bytes()) {
            frames.push((bytes, stamp));
        }
    }
    frames
}

/// Start a screencast in one format at one size.
fn cast(client: &mut Client, format: &str, quality: Option<u32>, width: u32, height: u32) {
    let mut fields = vec![
        ("format", Json::string(format)),
        ("maxWidth", Json::number(width)),
        ("maxHeight", Json::number(height)),
        ("everyNthFrame", Json::number(1)),
    ];
    if let Some(quality) = quality {
        fields.push(("quality", Json::number(quality)));
    }
    client
        .call("Page.startScreencast", Json::object(fields))
        .expect("the screencast starts");
}

fn temp_dir(what: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("tos-browser-it-{what}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a directory");
    dir
}

/// A screencast, decoded here and drawn, with the numbers it cost.
#[test]
fn frames_reach_a_terminal_through_shared_memory() {
    let Some((mut engine, mut client)) = connect() else {
        return;
    };
    prepare(&mut client);

    let dir = temp_dir("shm");
    let mut painter = Painter::at(&dir);
    let mut terminal = a_terminal(&dir);
    let cells = Cells {
        cols: WIDTH / CELL.0,
        rows: HEIGHT / CELL.1,
    };

    cast(&mut client, "jpeg", Some(motion::QUALITY), WIDTH, HEIGHT);

    let run_for = Duration::from_secs(3);
    let started = Instant::now();
    let (mut frames, mut bytes, mut escape_bytes) = (0usize, 0usize, 0usize);
    let (mut decoding, mut drawing) = (Duration::ZERO, Duration::ZERO);

    while started.elapsed() < run_for {
        for (jpeg, stamp) in take_frames(&mut client) {
            assert_eq!(&jpeg[..2], b"\xff\xd8", "the engine promised JPEG");
            // The clock the ordering rule in `tos_browser::motion` leans on:
            // CDP says seconds since the epoch, and the engine is a child of
            // this process, so it had better be this epoch.
            let stamp = stamp.expect("a frame says when it was captured");
            let drift = (motion::now_seconds() - stamp).abs();
            assert!(
                drift < 60.0,
                "metadata.timestamp is {stamp}, which is {drift:.1} s from this clock"
            );

            let at = Instant::now();
            let image = tos_term::jpeg::decode(&jpeg, 64 * 1024 * 1024).expect("a frame decodes");
            decoding += at.elapsed();
            assert_eq!((image.width, image.height), (WIDTH, HEIGHT));

            let at = Instant::now();
            let raw = Raw::rgb(&image.rgb, image.width, image.height);
            let sequence = painter.frame(raw, cells, 2, 1);
            terminal.advance(&sequence);
            drawing += at.elapsed();

            frames += 1;
            bytes += jpeg.len();
            escape_bytes += sequence.len();
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let _ = client.call("Page.stopScreencast", Json::empty());

    assert!(frames > 10, "only {frames} frames in three seconds");
    eprintln!(
        "shared memory: {frames} frames in {:?} ({:.1}/s), {} bytes of JPEG on average, \
         {} bytes down the pane per frame, {:?} decoding and {:?} in the terminal per frame",
        started.elapsed(),
        frames as f64 / started.elapsed().as_secs_f64(),
        bytes / frames,
        escape_bytes / frames,
        decoding / frames as u32,
        drawing / frames as u32,
    );

    // One image and one placement, however many frames went through: the
    // whole argument for a fixed id.
    let store = terminal.graphics();
    assert_eq!(store.placements().count(), 1, "a placement per frame leaks");
    let image = store.image(IMAGE_ID).expect("the frame is in the store");
    assert_eq!((image.width, image.height), (WIDTH, HEIGHT));
    let placement = store.placements().next().expect("one placement");
    assert_eq!(
        (placement.cols as u32, placement.rows as u32),
        (cells.cols, cells.rows)
    );
    assert_eq!(placement.row, 1, "row two, counted from zero");

    // And nothing is left in the directory that stands in for /dev/shm: the
    // terminal unlinked every name it read.
    let left: Vec<_> = dir.read_dir().expect("readable").flatten().collect();
    assert!(left.len() <= 16, "{} names left behind", left.len());
    painter.clean_up();
    std::fs::remove_dir_all(&dir).ok();

    client.close();
    engine.kill();
}

/// The same, inline, which is the path a terminal that cannot read `/dev/shm`
/// takes — and the one whose cost is worth knowing, because raw pixels made
/// it four times what it was.
///
/// The fallback sends the decoded pixels rather than the encoded frame,
/// because the encoded frame is a JPEG and no terminal's graphics path reads
/// one. `apps/browser/src/graphics.rs` argues that; this measures it.
#[test]
fn frames_also_reach_a_terminal_inline() {
    let Some((mut engine, mut client)) = connect() else {
        return;
    };
    prepare(&mut client);

    let dir = temp_dir("inline");
    let mut terminal = a_terminal(&dir);
    let cells = Cells {
        cols: WIDTH / CELL.0,
        rows: HEIGHT / CELL.1,
    };
    cast(&mut client, "jpeg", Some(motion::QUALITY), WIDTH, HEIGHT);

    let started = Instant::now();
    let (mut frames, mut escape_bytes) = (0usize, 0usize);
    while started.elapsed() < Duration::from_secs(2) {
        for (jpeg, _) in take_frames(&mut client) {
            let image = tos_term::jpeg::decode(&jpeg, 64 * 1024 * 1024).expect("a frame decodes");
            let raw = Raw::rgb(&image.rgb, image.width, image.height);
            let sequence = tos_browser::graphics::inline_command(&raw, cells);
            terminal.advance(&sequence);
            frames += 1;
            escape_bytes += sequence.len();
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let _ = client.call("Page.stopScreencast", Json::empty());

    assert!(frames > 2, "only {frames} frames inline");
    eprintln!(
        "inline: {frames} frames in {:?} ({:.1}/s), {} bytes down the pane per frame",
        started.elapsed(),
        frames as f64 / started.elapsed().as_secs_f64(),
        escape_bytes / frames
    );
    let store = terminal.graphics();
    assert_eq!(store.placements().count(), 1);
    assert_eq!(
        store.image(IMAGE_ID).map(|i| (i.width, i.height)),
        Some((WIDTH, HEIGHT))
    );
    std::fs::remove_dir_all(&dir).ok();

    client.close();
    engine.kill();
}

/// The key table, checked against a page rather than against itself.
#[test]
fn a_key_arrives_as_the_key_the_page_expects() {
    let Some((mut engine, mut client)) = connect() else {
        return;
    };
    prepare(&mut client);

    let cases: &[(KeyInput, &str)] = &[
        (
            KeyInput {
                key: Key::Char('a'),
                mods: Mods::default(),
                action: KeyAction::Press,
                text: Some('a'),
            },
            "key a KeyA 65",
        ),
        (
            KeyInput {
                key: Key::Enter,
                mods: Mods::default(),
                action: KeyAction::Press,
                text: None,
            },
            "key Enter Enter 13",
        ),
        (
            KeyInput {
                key: Key::Left,
                mods: Mods::default(),
                action: KeyAction::Press,
                text: None,
            },
            "key ArrowLeft ArrowLeft 37",
        ),
        (
            KeyInput {
                key: Key::Char('7'),
                mods: Mods::default(),
                action: KeyAction::Press,
                text: Some('7'),
            },
            "key 7 Digit7 55",
        ),
        (
            KeyInput {
                key: Key::Function(5),
                mods: Mods::default(),
                action: KeyAction::Press,
                text: None,
            },
            "key F5 F5 116",
        ),
    ];

    for (input, expected) in cases {
        client
            .call(
                "Runtime.evaluate",
                Json::object(vec![
                    ("expression", Json::string("document.title='waiting'")),
                    ("returnByValue", Json::Bool(true)),
                ]),
            )
            .expect("the title is reset");
        let params = keys::dispatch(input).expect("a key with a name");
        client
            .call("Input.dispatchKeyEvent", params)
            .expect("the key is dispatched");
        let seen = wait_for_title(&mut client, "key ", Duration::from_secs(5));
        assert_eq!(&seen, expected, "for {input:?}");
    }

    client.close();
    engine.kill();
}

/// A click lands where the terminal said it did.
#[test]
fn a_click_lands_where_the_cell_was() {
    let Some((mut engine, mut client)) = connect() else {
        return;
    };
    prepare(&mut client);

    // Cell column 11, row 4 of a pane whose first row is the status line: the
    // middle of that cell, one row up.
    let report = tos_browser::input::MouseInput {
        kind: tos_browser::input::MouseKind::Press,
        button: Some(0),
        mods: Mods::default(),
        x: 11,
        y: 4,
        wheel: (0, 0),
    };
    let (x, y) = tos_browser::input::page_point(&report, false, CELL, 1);
    assert_eq!((x, y), (84, 40));

    client
        .call(
            "Input.dispatchMouseEvent",
            Json::object(vec![
                ("type", Json::string("mousePressed")),
                ("x", Json::number(x)),
                ("y", Json::number(y)),
                ("button", Json::string("left")),
                ("buttons", Json::number(1)),
                ("clickCount", Json::number(1)),
                ("modifiers", Json::number(0)),
            ]),
        )
        .expect("the click is dispatched");

    let seen = wait_for_title(&mut client, "click ", Duration::from_secs(5));
    assert_eq!(seen, format!("click 0 {x} {y}"));

    client.close();
    engine.kill();
}

// ---------------------------------------------------------------------------
// Tabs
// ---------------------------------------------------------------------------

/// A page with a link that asks for a window of its own, and the page behind
/// it.
///
/// Served over HTTP rather than handed over as a `data:` url like [`PAGE`]. A
/// data: url is an opaque origin and Chromium refuses a top-level navigation
/// to one, so a link out of a data: page into a new tab would fail for a
/// reason that has nothing to do with this crate. The link is positioned and
/// sized so that a click at a known point lands on it without the test having
/// to ask the page where anything is.
const FIRST_PAGE: &str = "<!doctype html><body style='margin:0;background:#fff'>\
<a id=l href='/second' target=_blank \
style='position:absolute;left:0;top:0;width:240px;height:80px;background:#cc3'>open</a>\
<script>document.title='first'</script></body>";

const SECOND_PAGE: &str = "<!doctype html><body style='margin:0;background:#39c'>\
<script>document.title='second'</script></body>";

/// Serve those two pages on a port of the kernel's choosing, for as long as
/// the test binary runs.
fn serve() -> String {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a port to serve on");
    let address = listener.local_addr().expect("an address");
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut head = [0u8; 2048];
            let read = stream.read(&mut head).unwrap_or(0);
            let request = String::from_utf8_lossy(&head[..read]).to_string();
            let body = if request.starts_with("GET /second") {
                SECOND_PAGE
            } else {
                FIRST_PAGE
            };
            let answer = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(answer.as_bytes());
        }
    });
    format!("http://{address}/")
}

/// The viewport the program would set, on whichever tab is being driven.
fn viewport(client: &mut Client) {
    client
        .call(
            "Emulation.setDeviceMetricsOverride",
            Json::object(vec![
                ("width", Json::number(WIDTH)),
                ("height", Json::number(HEIGHT)),
                ("deviceScaleFactor", Json::number(1)),
                ("mobile", Json::Bool(false)),
            ]),
        )
        .expect("the viewport");
}

/// The browser-level connection, with target discovery on, and the tab list
/// the program would be holding.
fn tabbed(engine: &Engine, page: Client, target: String) -> (Client, Tabs<Client>) {
    let mut browser =
        Client::connect(engine.browser_url(), Duration::from_secs(10)).expect("the browser socket");
    browser
        .call(
            "Target.setDiscoverTargets",
            Json::object(vec![("discover", Json::Bool(true))]),
        )
        .expect("discovery");
    (browser, Tabs::new(Tab::new(target, page, "about:blank")))
}

/// Feed what the browser connection has said into the tab list until `done` is
/// satisfied or the time is up: what `app::handle_target_events` does, with
/// the drawing left out.
fn pump(
    browser: &mut Client,
    tabs: &mut Tabs<Client>,
    browser_url: &str,
    timeout: Duration,
    done: impl Fn(&Tabs<Client>) -> bool,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        for event in browser.events() {
            let outcome = tabs.take(&event, |target| {
                let socket = engine::target_url(browser_url, target)?;
                Client::connect(&socket, Duration::from_secs(5))
            });
            match outcome {
                Outcome::Failed(why) => panic!("a tab that would not open: {why}"),
                Outcome::Gone { mut tab, why } => {
                    if let Some(why) = why {
                        eprintln!("a tab went: {why}");
                    }
                    tab.connection.close();
                }
                _ => {}
            }
        }
        if done(tabs) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// An engine with two tabs in it: the second opened by a click on a
/// `target=_blank` link in the first, which is how a person opens one.
fn two_tabs() -> Option<(Engine, Client, Tabs<Client>, String)> {
    let (engine, page, target) = connect_with_target()?;
    let browser_url = engine.browser_url().to_string();
    let base = serve();
    let (mut browser, mut tabs) = tabbed(&engine, page, target);

    {
        let first = tabs.active_mut().expect("the first tab");
        first
            .connection
            .call("Page.enable", Json::empty())
            .expect("Page.enable");
        viewport(&mut first.connection);
        first
            .connection
            .call(
                "Page.navigate",
                Json::object(vec![("url", Json::string(&base))]),
            )
            .expect("the page loads");
        assert_eq!(
            wait_for_title(&mut first.connection, "first", Duration::from_secs(10)),
            "first"
        );

        // A click on the link, dispatched the way a terminal's mouse report
        // would be: press and release at a point inside it.
        for (kind, buttons) in [("mousePressed", 1), ("mouseReleased", 0)] {
            first
                .connection
                .call(
                    "Input.dispatchMouseEvent",
                    Json::object(vec![
                        ("type", Json::string(kind)),
                        ("x", Json::number(40)),
                        ("y", Json::number(20)),
                        ("button", Json::string("left")),
                        ("buttons", Json::number(buttons)),
                        ("clickCount", Json::number(1)),
                        ("modifiers", Json::number(0)),
                    ]),
                )
                .expect("the click is dispatched");
        }
    }

    assert!(
        pump(
            &mut browser,
            &mut tabs,
            &browser_url,
            Duration::from_secs(15),
            |tabs| tabs.len() == 2,
        ),
        "the link with target=_blank opened no tab"
    );
    Some((engine, browser, tabs, browser_url))
}

/// The whole reason tabs exist: a link that wants a window gets a tab, that
/// tab is the one in front, and it is the one painting.
#[test]
fn a_link_that_wants_a_window_becomes_the_tab_in_front() {
    let Some((mut engine, mut browser, mut tabs, browser_url)) = two_tabs() else {
        return;
    };
    assert_eq!(tabs.len(), 2);
    assert_eq!(
        tabs.active_index(),
        1,
        "a tab the person opened is the one they are taken to"
    );

    // The urls come from the browser connection, with nothing asked of either
    // page — and the second tab's is the one the link pointed at.
    assert!(
        pump(
            &mut browser,
            &mut tabs,
            &browser_url,
            Duration::from_secs(10),
            |tabs| tabs
                .iter()
                .nth(1)
                .is_some_and(|tab| tab.url.ends_with("/second")),
        ),
        "the second tab's url never arrived: {:?}",
        tabs.iter().map(|tab| tab.url.clone()).collect::<Vec<_>>()
    );

    // The titles come from the pages, which is the only place they are right:
    // the browser connection would have said "127.0.0.1:NNNN/second" here.
    let mut titles = Vec::new();
    for index in 0..tabs.len() {
        let tab = tabs.get_mut(index).expect("a tab");
        titles.push(
            tos_browser::app::page_title(&mut tab.connection).unwrap_or_else(|| "?".to_string()),
        );
    }
    assert_eq!(titles, ["first", "second"]);

    // And it paints: raised, sized, cast, and a PNG comes out of it.
    let target = tabs.active_target().expect("a target").to_string();
    browser
        .call(
            "Target.activateTarget",
            Json::object(vec![("targetId", Json::string(&target))]),
        )
        .expect("the tab is raised");
    let second = tabs.active_mut().expect("the second tab");
    second
        .connection
        .call("Page.enable", Json::empty())
        .expect("Page.enable");
    viewport(&mut second.connection);
    second
        .connection
        .call(
            "Page.startScreencast",
            Json::object(vec![
                ("format", Json::string("png")),
                ("maxWidth", Json::number(WIDTH)),
                ("maxHeight", Json::number(HEIGHT)),
                ("everyNthFrame", Json::number(1)),
            ]),
        )
        .expect("the screencast starts");
    let png = wait_for_frame(&mut second.connection, Duration::from_secs(10))
        .expect("the tab in front paints");
    assert_eq!(&png[..4], b"\x89PNG");

    browser.close();
    drop(tabs);
    engine.kill();
}

/// `ctrl+t`: a target this program asked for, reached on a url it worked out
/// rather than looked up, and not announced twice as a tab.
#[test]
fn a_tab_this_program_opens_is_reachable_and_counted_once() {
    let Some((mut engine, page, target)) = connect_with_target() else {
        return;
    };
    let browser_url = engine.browser_url().to_string();
    let base = serve();
    let (mut browser, mut tabs) = tabbed(&engine, page, target);

    let created = browser
        .call(
            "Target.createTarget",
            Json::object(vec![("url", Json::string("about:blank"))]),
        )
        .expect("a new target");
    let opened = created
        .get("targetId")
        .and_then(Json::as_str)
        .expect("the engine says which")
        .to_string();
    let socket = engine::target_url(&browser_url, &opened).expect("a socket url");
    let connection = Client::connect(&socket, Duration::from_secs(10))
        .expect("the url worked out from the browser's own");
    tabs.open(Tab::new(opened, connection, "about:blank"));
    assert_eq!(tabs.len(), 2);
    assert_eq!(tabs.active_index(), 1);

    // The `Target.targetCreated` for it has no opener, so the list must not
    // take it as a second tab for the same page.
    assert!(!pump(
        &mut browser,
        &mut tabs,
        &browser_url,
        Duration::from_secs(3),
        |tabs| tabs.len() > 2,
    ));
    assert_eq!(
        tabs.len(),
        2,
        "the tab this program opened was counted twice"
    );

    // And it drives like any other tab.
    let tab = tabs.active_mut().expect("the new tab");
    tab.connection
        .call("Page.enable", Json::empty())
        .expect("Page.enable");
    tab.connection
        .call(
            "Page.navigate",
            Json::object(vec![("url", Json::string(&base))]),
        )
        .expect("it navigates");
    assert_eq!(
        wait_for_title(&mut tab.connection, "first", Duration::from_secs(10)),
        "first"
    );
    let mut gone = tabs.close(1).expect("the tab");
    gone.connection.close();

    browser.close();
    drop(tabs);
    engine.kill();
}

/// `ctrl+w`: the engine closes the page, the list loses the tab, and what is
/// left is the tab it was opened from.
#[test]
fn closing_a_tab_leaves_the_one_it_was_opened_from() {
    let Some((mut engine, mut browser, mut tabs, browser_url)) = two_tabs() else {
        return;
    };
    let closing = tabs.active_target().expect("a target").to_string();

    // What `Command::CloseTab` does: the target in the engine, then the socket.
    let index = tabs.active_index();
    let mut tab = tabs.close(index).expect("the tab");
    browser
        .call(
            "Target.closeTarget",
            Json::object(vec![("targetId", Json::string(&closing))]),
        )
        .expect("the target closes");
    tab.connection.close();

    assert_eq!(tabs.len(), 1);
    assert_eq!(tabs.active_index(), 0);
    // And the engine agrees. Its `targetDestroyed` arrives for a tab that has
    // already gone, which must not be an error or a second tab lost.
    assert!(
        pump(
            &mut browser,
            &mut tabs,
            &browser_url,
            Duration::from_secs(10),
            |tabs| tabs.len() == 1,
        ),
        "the engine's own news about the closed target upset the list"
    );
    let left = tabs.active_mut().expect("the tab that is left");
    assert_eq!(
        tos_browser::app::page_title(&mut left.connection).as_deref(),
        Some("first"),
        "what is left is not the page the link was on"
    );

    browser.close();
    drop(tabs);
    engine.kill();
}

/// A page that closes itself takes its tab with it, with no key pressed.
#[test]
fn a_page_that_calls_window_close_removes_its_own_tab() {
    let Some((mut engine, mut browser, mut tabs, browser_url)) = two_tabs() else {
        return;
    };
    let closing = tabs.active_target().expect("a target").to_string();
    {
        let second = tabs.active_mut().expect("the second tab");
        // A page may close a window that was opened by script, which is what
        // the link with target=_blank made this one. `notify` rather than
        // `call`: the reply to an evaluation that closes the page never comes.
        let _ = second.connection.notify(
            "Runtime.evaluate",
            Json::object(vec![("expression", Json::string("window.close()"))]),
        );
    }

    assert!(
        pump(
            &mut browser,
            &mut tabs,
            &browser_url,
            Duration::from_secs(10),
            |tabs| tabs.len() == 1,
        ),
        "window.close() left the tab where it was"
    );
    assert_eq!(tabs.index_of(&closing), None);
    assert_eq!(tabs.active_index(), 0);

    browser.close();
    drop(tabs);
    engine.kill();
}

/// The next screencast frame, decoded, or nothing within the time.
fn wait_for_frame(client: &mut Client, timeout: Duration) -> Option<Vec<u8>> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        for event in client.events() {
            if event.method != "Page.screencastFrame" {
                continue;
            }
            if let Some(session) = event.params.get("sessionId").and_then(Json::as_i64) {
                let _ = client.notify(
                    "Page.screencastFrameAck",
                    Json::object(vec![("sessionId", Json::number(session as f64))]),
                );
            }
            if let Some(data) = event.params.get("data").and_then(Json::as_str) {
                if let Ok(png) = tos_browser::base64::decode(data.as_bytes()) {
                    return Some(png);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    None
}
/// The engine is a wrapper, a browser and a handful of helpers, and stopping
/// it has to be the end of all of them.
///
/// On Debian — a tOS rootfs is Debian — `/usr/bin/chromium-shell` is a shell
/// script that runs `/usr/lib/chromium/chromium-shell` as its child, so the
/// pid `spawn` returns is `/bin/sh` and a signal to that pid alone leaves a
/// browser behind with the page still painting and the debugging port still
/// open. That is what was found on an installed machine: seven sessions, seven
/// engines, none of them being looked at. This test is the shape of that bug —
/// the group is read before the engine is dropped, and has to be empty after.
#[test]
fn killing_the_engine_leaves_nothing_of_its_process_group() {
    let Some((engine, mut client)) = connect() else {
        return;
    };
    prepare(&mut client);
    let address = engine.address().expect("an address");
    let group = engine
        .group()
        .expect("the engine is started in a group of its own");
    assert_ne!(group, own_group(), "the engine's group is not this test's");

    let before = group_members(group);
    assert!(
        before.len() >= 2,
        "an engine is a wrapper and a browser at least, and this group has {}",
        describe(&before)
    );
    eprintln!("group {group}: {}", describe(&before));

    drop(client);
    drop(engine);

    // Two seconds: the polite stop inside `Engine::kill` is allowed half of
    // one, and the SIGKILL after it is not something a process can be slow
    // about.
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut left = group_members(group);
    while !left.is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
        left = group_members(group);
    }
    assert!(
        left.is_empty(),
        "the engine was killed and these are still running: {}",
        describe(&left)
    );
    assert!(
        std::net::TcpStream::connect(&address).is_err(),
        "something is still listening on {address}"
    );
}

/// Every process in `group` that is still running, as pid and command line.
///
/// A process that has exited and not yet been waited for is still in the
/// group as far as the kernel is concerned, and is not what this is looking
/// for: the browser this test is about reparents to init, which reaps it in
/// its own time. So state `Z` is not a member here.
fn group_members(group: i32) -> Vec<(i32, String)> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return found;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Ok(pid) = name.to_string_lossy().parse::<i32>() else {
            continue;
        };
        let Some((state, pgrp)) = state_and_group(&entry.path()) else {
            continue;
        };
        if pgrp == group && state != 'Z' {
            found.push((pid, command_of(&entry.path())));
        }
    }
    found.sort();
    found
}

/// The group this test process is in, read the same way as anybody else's.
fn own_group() -> i32 {
    state_and_group(std::path::Path::new("/proc/self"))
        .expect("this process has a /proc entry")
        .1
}

/// The run state and process group out of `/proc/<pid>/stat`.
///
/// The second field is the command in brackets and may contain spaces and
/// brackets of its own, so the fields are counted from the last `)` rather
/// than from the start of the line.
fn state_and_group(dir: &std::path::Path) -> Option<(char, i32)> {
    let text = std::fs::read_to_string(dir.join("stat")).ok()?;
    let after_command = &text[text.rfind(')')? + 1..];
    let mut fields = after_command.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let _parent = fields.next()?;
    let group = fields.next()?.parse().ok()?;
    Some((state, group))
}

/// What a process was started as, short enough to put in a failure.
fn command_of(dir: &std::path::Path) -> String {
    let Ok(raw) = std::fs::read(dir.join("cmdline")) else {
        return String::from("(gone)");
    };
    let line = String::from_utf8_lossy(&raw).replace('\0', " ");
    let line = line.trim().to_string();
    match line.char_indices().nth(90) {
        Some((at, _)) => format!("{}...", &line[..at]),
        None => line,
    }
}

fn describe(members: &[(i32, String)]) -> String {
    members
        .iter()
        .map(|(pid, command)| format!("{pid} {command}"))
        .collect::<Vec<_>>()
        .join("; ")

    // ---------------------------------------------------------------------------
    // The format the frames go in
    // ---------------------------------------------------------------------------
}

/// A page the shape of a real article, which is what the format tests need.
///
/// [`PAGE`] is a screenful of monospace text and one moving block, which is
/// right for measuring a frame path and wrong for measuring a format: it is
/// almost all sharp black-on-white edges, which is the worst case a JPEG ever
/// meets, and at 640x360 it is dense enough to come out at 28 dB — a number
/// about that page rather than about quality 85. So this is prose at a
/// reading size, in a column, with a picture beside it and links in it, which
/// is what the measurement in `docs/design/browser.md` was taken on.
///
/// It is still until `f()` is called, and then it scrolls, which is the two
/// things the two tests below want: a frame that can be captured twice and
/// get the same picture, and a viewport that changes completely every frame.
const ARTICLE: &str = "data:text/html,\
<body style='margin:0;font:13px/17px serif;background:%23fff;color:%23202122'>\
<div style='position:absolute;right:20px;top:60px;width:320px;height:240px;\
background:linear-gradient(135deg,%23c33,%233c3,%2333c,%23fc0)'></div>\
<div id=t style='padding:8px 340px 8px 16px'></div><script>\
var w='the quick brown fox jumps over a lazy dog while Blink lays out a page of \
prose and the encoder works out what it costs to send'.split(' ');\
var rows=[];for(var i=0;i<400;i++){var s=[];for(var j=0;j<28;j++){\
s.push(w[(i*7+j*3)%25w.length])}\
rows.push('<p style=margin:4px>'+i+' '+s.join(' ')+' <a href=%23 \
style=color:%230645ad>a link</a></p>')}\
document.getElementById('t').innerHTML=rows.join('');\
document.title='article';\
var n=0;function f(){n=(n+4)%252000;scrollTo(0,n);requestAnimationFrame(f)}\
</script></body>";

/// Load [`ARTICLE`] at `width` by `height` and wait for it to be there.
fn article(client: &mut Client, width: u32, height: u32) {
    client
        .call(
            "Emulation.setDeviceMetricsOverride",
            Json::object(vec![
                ("width", Json::number(width)),
                ("height", Json::number(height)),
                ("deviceScaleFactor", Json::number(1)),
                ("mobile", Json::Bool(false)),
            ]),
        )
        .expect("a pane-sized viewport");
    client
        .call(
            "Page.navigate",
            Json::object(vec![("url", Json::string(ARTICLE))]),
        )
        .expect("the article loads");
    assert_eq!(
        wait_for_title(client, "article", Duration::from_secs(15)),
        "article"
    );
}

/// One `Page.captureScreenshot`, in whichever format.
fn screenshot(client: &mut Client, format: &str, quality: Option<u32>) -> Vec<u8> {
    let mut fields = vec![("format", Json::string(format))];
    if let Some(quality) = quality {
        fields.push(("quality", Json::number(quality)));
    }
    let answer = client
        .call("Page.captureScreenshot", Json::object(fields))
        .expect("a screenshot");
    let data = answer
        .get("data")
        .and_then(Json::as_str)
        .expect("a screenshot carries its picture");
    tos_browser::base64::decode(data.as_bytes()).expect("valid base64")
}

/// What quality 85 costs, against the lossless picture of the same frame.
///
/// This is the number `docs/design/browser.md`'s JPEG section rests on: the
/// frames are only allowed to be lossy while the page is moving, and how
/// lossy is a thing to measure rather than to trust. 35 dB is the floor and
/// the measured figure on this page is 37; anything near the floor means
/// either the encoder's defaults moved or `tos_term::jpeg` has a bug the
/// fixtures did not catch.
///
/// Read the floor as being about this page. A screenful of small monospace
/// text is 28 dB at the same quality and is not a worse decoder or a worse
/// encoder — it is what 4:2:0 and a quantisation table do to sharp edges, and
/// it is exactly the case the motion-and-rest policy exists to keep off the
/// screen while somebody is reading.
#[test]
fn a_jpeg_frame_at_quality_85_is_the_png_of_the_same_frame() {
    let Some((mut engine, mut client)) = connect() else {
        return;
    };
    prepare(&mut client);
    article(&mut client, WIDE, TALL);
    // A frame after the load event is not necessarily a frame that has been
    // painted; one screenshot forces one.
    let _ = screenshot(&mut client, "png", None);

    let png = screenshot(&mut client, "png", None);
    let jpeg = screenshot(&mut client, "jpeg", Some(motion::QUALITY));
    assert_eq!(&png[..4], b"\x89PNG");
    assert_eq!(&jpeg[..2], b"\xff\xd8");

    let lossless = tos_term::png::decode(&png, 64 * 1024 * 1024).expect("the PNG decodes");
    let at = Instant::now();
    let lossy = tos_term::jpeg::decode(&jpeg, 64 * 1024 * 1024).expect("the JPEG decodes");
    let decoding = at.elapsed();
    assert_eq!(
        (lossy.width, lossy.height),
        (lossless.width, lossless.height)
    );

    // Mean squared error over every channel of every pixel, and the
    // peak-signal-to-noise ratio that is the usual way of saying it.
    let mut squares = 0f64;
    let mut samples = 0usize;
    for (rgba, rgb) in lossless.rgba.chunks_exact(4).zip(lossy.rgb.chunks_exact(3)) {
        for channel in 0..3 {
            let error = rgba[channel] as f64 - rgb[channel] as f64;
            squares += error * error;
            samples += 1;
        }
    }
    let mse = squares / samples as f64;
    let psnr = if mse == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (255.0f64 * 255.0 / mse).log10()
    };
    eprintln!(
        "quality {} at {}x{}: {psnr:.1} dB, {} kB of JPEG against {} kB of PNG, \
         decoded in {decoding:?}",
        motion::QUALITY,
        lossy.width,
        lossy.height,
        jpeg.len() / 1024,
        png.len() / 1024,
    );
    assert!(
        psnr >= 35.0,
        "quality {} came out at {psnr:.1} dB",
        motion::QUALITY
    );

    client.close();
    engine.kill();
}

// ---------------------------------------------------------------------------
// What the branch is for: frames a second, at the size a pane actually is
// ---------------------------------------------------------------------------

/// A pane's worth of page: 160 by 48 cells on the 8x16 face, with a row left
/// over for the status line. The measurement that chose JPEG was taken at
/// 1280x770; 768 is the same pane rounded to a whole number of cells, which
/// is the only height a pane can actually have.
const WIDE: u32 = 1280;
const TALL: u32 = 768;

/// The frame path as it was before this branch: the encoded file itself,
/// named in `/dev/shm`, decoded by the terminal on its parse loop.
///
/// Built here rather than kept in `graphics.rs`, because the program no
/// longer has this path and a dead branch kept alive for a benchmark is a
/// branch that rots. It is byte for byte what `Painter::frame` used to emit.
fn png_through_the_terminal(
    dir: &std::path::Path,
    counter: &mut u64,
    png: &[u8],
    cells: Cells,
) -> Vec<u8> {
    *counter += 1;
    let name = format!("tos-browser-before-{}-{counter}", std::process::id());
    let partial = dir.join(format!("{name}.part"));
    std::fs::write(&partial, png).expect("a frame file");
    std::fs::rename(&partial, dir.join(&name)).expect("renamed into place");
    let control = format!(
        "a=T,f=100,i={IMAGE_ID},p=1,c={},r={},C=1,q=2",
        cells.cols, cells.rows
    );
    let payload = tos_browser::base64::encode(format!("/{name}").as_bytes());
    format!("\x1b[2;1H\x1b_G{control},t=s;{payload}\x1b\\").into_bytes()
}

/// What the frame path costs, before and after, at the size a pane is.
///
/// "Before" is a PNG screencast handed to the terminal as a file, which is
/// what `main` did until this branch: the engine encodes a PNG, the terminal
/// decodes it on the thread that parses escape sequences. "After" is a JPEG
/// screencast at quality 85 decoded in this process and handed over as raw
/// pixels. Both run against the same page, scrolling, at the same size,
/// through the same `/dev/shm` transport and the same terminal, for the same
/// three seconds.
///
/// **What is asserted is the terminal's cost, not the frame rate**, and the
/// reason is worth knowing. `docs/design/browser.md` records 33.8 fps for PNG
/// against 57.8 for JPEG, measured on a slower machine against a real
/// ja.wikipedia page; on a fast host with `--cpus=2` and this page the
/// engine's PNG encoder keeps up and both formats arrive at very nearly 60,
/// so a frame-rate assertion here would be asserting something about the
/// machine. The cost this branch actually controls is the compositor's: a PNG
/// decode on the parse loop against a copy out of tmpfs, which is the same
/// several-fold difference whatever the engine manages. Both numbers are
/// printed; only the one that is a property of the code is asserted.
#[test]
fn raw_pixels_cost_the_terminal_a_fraction_of_what_a_png_frame_did() {
    let Some((mut engine, mut client)) = connect() else {
        return;
    };
    prepare(&mut client);
    article(&mut client, WIDE, TALL);
    client
        .call(
            "Runtime.evaluate",
            Json::object(vec![("expression", Json::string("f()"))]),
        )
        .expect("the page starts scrolling");

    let cells = Cells {
        cols: WIDE / CELL.0,
        rows: TALL / CELL.1,
    };
    let run_for = Duration::from_secs(3);

    // Before: PNG, decoded by the terminal on its parse loop.
    let before_dir = temp_dir("before");
    let mut terminal = sized_terminal(&before_dir, WIDE, TALL);
    let mut counter = 0u64;
    cast(&mut client, "png", None, WIDE, TALL);
    let started = Instant::now();
    let (mut before_frames, mut before_bytes) = (0usize, 0usize);
    let mut before_terminal = Duration::ZERO;
    while started.elapsed() < run_for {
        for (png, _) in take_frames(&mut client) {
            let sequence = png_through_the_terminal(&before_dir, &mut counter, &png, cells);
            let at = Instant::now();
            terminal.advance(&sequence);
            before_terminal += at.elapsed();
            before_frames += 1;
            before_bytes += png.len();
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let before_seconds = started.elapsed().as_secs_f64();
    let _ = client.call("Page.stopScreencast", Json::empty());
    // `Page.stopScreencast` returns before the last frames do, and the ones
    // still coming are in the old format. Only a test that changes format
    // mid-session meets this — the program picks one at `Page.enable` and
    // keeps it — but here it is the difference between a measurement and a
    // panic, so the queue is drained until it stays empty.
    let give_up = Instant::now() + Duration::from_secs(2);
    let mut quiet_since = Instant::now();
    while Instant::now() < give_up && quiet_since.elapsed() < Duration::from_millis(300) {
        if !take_frames(&mut client).is_empty() {
            quiet_since = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(before_frames > 5, "only {before_frames} PNG frames");
    assert_eq!(
        terminal
            .graphics()
            .image(IMAGE_ID)
            .map(|i| (i.width, i.height)),
        Some((WIDE, TALL)),
        "the before path did not put a frame in the store"
    );

    // After: JPEG at quality 85, decoded here, raw pixels over.
    let after_dir = temp_dir("after");
    let mut painter = Painter::at(&after_dir);
    let mut terminal = sized_terminal(&after_dir, WIDE, TALL);
    cast(&mut client, "jpeg", Some(motion::QUALITY), WIDE, TALL);
    let started = Instant::now();
    let (mut after_frames, mut after_bytes) = (0usize, 0usize);
    let (mut decoding, mut after_terminal) = (Duration::ZERO, Duration::ZERO);
    let mut stragglers = 0usize;
    while started.elapsed() < run_for {
        for (jpeg, _) in take_frames(&mut client) {
            if jpeg.get(..2) != Some(b"\xff\xd8") {
                // A PNG from the pass above that outlived the drain. Counted
                // rather than ignored, because a lot of them would mean the
                // measurement below is of the wrong thing.
                stragglers += 1;
                continue;
            }
            let at = Instant::now();
            let image = tos_term::jpeg::decode(&jpeg, 64 * 1024 * 1024).expect("a frame decodes");
            decoding += at.elapsed();
            let raw = Raw::rgb(&image.rgb, image.width, image.height);
            let sequence = painter.frame(raw, cells, 2, 1);
            let at = Instant::now();
            terminal.advance(&sequence);
            after_terminal += at.elapsed();
            after_frames += 1;
            after_bytes += jpeg.len();
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let after_seconds = started.elapsed().as_secs_f64();
    let _ = client.call("Page.stopScreencast", Json::empty());
    assert!(after_frames > 5, "only {after_frames} JPEG frames");
    assert!(
        stragglers < after_frames / 10,
        "{stragglers} frames of the old format against {after_frames} of the new"
    );
    assert_eq!(
        terminal
            .graphics()
            .image(IMAGE_ID)
            .map(|i| (i.width, i.height)),
        Some((WIDE, TALL)),
        "the after path did not put a frame in the store"
    );

    let before_fps = before_frames as f64 / before_seconds;
    let after_fps = after_frames as f64 / after_seconds;
    let before_each = before_terminal / before_frames as u32;
    let after_each = after_terminal / after_frames as u32;
    eprintln!(
        "{WIDE}x{TALL} end to end:\n  \
         before  png  decoded by the terminal: {before_fps:.1} fps, \
         {} kB a frame, {before_each:?} in the terminal\n  \
         after   jpeg q{} decoded here:        {after_fps:.1} fps, \
         {} kB a frame, {:?} decoding, {after_each:?} in the terminal\n  \
         the terminal's share is {:.1}x smaller",
        before_bytes / before_frames / 1024,
        motion::QUALITY,
        after_bytes / after_frames / 1024,
        decoding / after_frames as u32,
        before_each.as_secs_f64() / after_each.as_secs_f64().max(f64::EPSILON),
    );

    assert!(
        after_each * 2 < before_each,
        "the terminal spent {after_each:?} a frame on raw pixels against \
         {before_each:?} on a PNG: the decode was supposed to leave the \
         compositor's thread"
    );
    // And the path as a whole keeps up with something worth calling a
    // browser, whichever format the engine is fast enough to manage.
    assert!(after_fps > 25.0, "only {after_fps:.1} fps end to end");

    painter.clean_up();
    std::fs::remove_dir_all(&before_dir).ok();
    std::fs::remove_dir_all(&after_dir).ok();
    client.close();
    engine.kill();
}

// ---------------------------------------------------------------------------
// When the lossless still is taken, and what it costs the loop
// ---------------------------------------------------------------------------

/// How far down the page is, asked of the page.
fn scroll_y(client: &mut Client) -> f64 {
    client
        .call_within(
            "Runtime.evaluate",
            Json::object(vec![
                ("expression", Json::string("window.scrollY")),
                ("returnByValue", Json::Bool(true)),
            ]),
            Duration::from_secs(5),
        )
        .ok()
        .and_then(|value| value.path(&["result", "value"]).and_then(Json::as_f64))
        .expect("the page says where it is")
}

/// Where the page was and when it was captured, for every screencast frame
/// that has arrived — without decoding the picture, because these tests are
/// about where the page is rather than what it looks like.
///
/// `Page.screencastFrame` carries `metadata.scrollOffsetY`, which is the
/// number the whole of this section is about: it is what the person sees move.
/// Acknowledges everything it takes, as the program does.
fn take_offsets(client: &mut Client) -> Vec<(f64, Option<f64>)> {
    let mut frames = Vec::new();
    for event in client.events() {
        if event.method != "Page.screencastFrame" {
            continue;
        }
        if let Some(session) = event.params.get("sessionId").and_then(Json::as_i64) {
            let _ = client.notify(
                "Page.screencastFrameAck",
                Json::object(vec![("sessionId", Json::number(session as f64))]),
            );
        }
        let Some(offset) = event
            .params
            .path(&["metadata", "scrollOffsetY"])
            .and_then(Json::as_f64)
        else {
            continue;
        };
        let stamp = event
            .params
            .path(&["metadata", "timestamp"])
            .and_then(Json::as_f64);
        frames.push((offset, stamp));
    }
    frames
}

/// The program's own dispatch, with a count of what went through it.
///
/// `tos_browser::app::Wire` is the one implementation of
/// `scroll::Dispatch` that is not a fake, so the events these tests put on the
/// wire are the ones the program puts on it, built by the same code.
struct Counted {
    wire: tos_browser::app::Wire,
    sent: AtomicUsize,
}

impl Dispatch for Counted {
    fn send(&self, step: Step) -> Result<(), String> {
        self.sent.fetch_add(1, Ordering::SeqCst);
        self.wire.send(step)
    }
}

/// One step of the animation, sent down the middle of the page exactly as the
/// animator thread sends one: one `mouseWheel`, no reply waited for.
fn wheel_step(client: &mut Client, step: Step) {
    client
        .notify(
            "Input.dispatchMouseEvent",
            Json::object(vec![
                ("type", Json::string("mouseWheel")),
                ("x", Json::number(step.at.0)),
                ("y", Json::number(step.at.1)),
                ("deltaX", Json::number(step.delta.0)),
                ("deltaY", Json::number(step.delta.1)),
                ("modifiers", Json::number(0)),
                ("button", Json::string("none")),
                ("buttons", Json::number(0)),
            ]),
        )
        .expect("the wheel event goes out");
}

/// What one run of the wheel did to the page.
struct Roll {
    /// Every screencast frame between the first notch and the end, as how far
    /// the page moved since the frame before it. A zero is a frame in which
    /// the page stood still, which is the whole of what pulsing looks like.
    frames: Vec<(f64, Instant)>,
    /// When each notch was turned.
    notches: Vec<Instant>,
    /// When the last notch was turned.
    last_notch: Instant,
    /// Every moment `motion` would have asked for a lossless still.
    wanted: Vec<Instant>,
    /// How many `mouseWheel` events the animation cost.
    events: usize,
}

impl Roll {
    /// The frames in which the page actually moved, with the first one's index
    /// and the last one's.
    fn movement(&self) -> (usize, usize) {
        let first = self
            .frames
            .iter()
            .position(|(step, _)| *step != 0.0)
            .expect("the wheel moved the page");
        let last = self
            .frames
            .iter()
            .rposition(|(step, _)| *step != 0.0)
            .expect("the wheel moved the page");
        (first, last)
    }

    /// How long the movement lasted, first moving frame to last.
    fn span(&self) -> Duration {
        let (first, last) = self.movement();
        self.frames[last].1.duration_since(self.frames[first].1)
    }

    /// The longest the page stood still in the middle of the scroll, and how
    /// many frames in a row it did.
    ///
    /// A frame that carries the same offset as the one before it is a frame in
    /// which the page did not move. One of those on its own is the screencast's
    /// cadence beating against the animation's: frames come every 16.7 ms on
    /// this host and ticks every [`tos_browser::scroll::TICK`], so about twice
    /// a second a frame falls in a gap and the next one carries two ticks.
    /// That is a sixtieth of a second, and on the machine this is really for —
    /// where a frame is 24 to 27 ms and every one of them holds a tick or two
    /// — it cannot happen at all. What the person saw and called pulsing is
    /// the page stopping long enough to be a *pause*, which is what this
    /// measures.
    fn longest_stall(&self) -> (Duration, usize) {
        let (first, last) = self.movement();
        let mut longest = Duration::ZERO;
        let mut in_a_row = 0usize;
        let mut worst_row = 0usize;
        let mut moved_at = self.frames[first].1;
        for (step, at) in &self.frames[first + 1..=last] {
            if *step == 0.0 {
                in_a_row += 1;
                worst_row = worst_row.max(in_a_row);
                continue;
            }
            in_a_row = 0;
            longest = longest.max(at.duration_since(moved_at));
            moved_at = *at;
        }
        (longest, worst_row)
    }

    /// The biggest and the smallest a frame moved over the middle of the run,
    /// and the ratio between them — which is what a steady hand's evenness
    /// *is*.
    ///
    /// The middle is from the second notch to the last one: the hand rolling
    /// steadily, with the ramp-up in front of it and the settling behind it
    /// left out, because neither is supposed to look like the middle.
    ///
    /// One frame at each end is forgiven, for the reason [`Roll::longest_stall`]
    /// forgives one: the screencast's cadence beats against the tick's — 16.7
    /// against 16 ms on the host these are measured on — so about twice a
    /// second one frame carries two ticks or none, and that is the cadence
    /// rather than the curve. The raw figures are printed beside the forgiven
    /// ones so that a run which needed the forgiveness says so.
    fn swing(&self) -> (f64, f64, f64) {
        let from = *self.notches.get(1).unwrap_or(&self.last_notch);
        let mut steps: Vec<f64> = self
            .frames
            .iter()
            .filter(|(_, at)| *at >= from && *at <= self.last_notch)
            .map(|(step, _)| step.abs())
            .collect();
        assert!(
            steps.len() > 4,
            "only {} frames in the middle of the run to judge it by",
            steps.len()
        );
        steps.sort_by(|a, b| a.partial_cmp(b).expect("a frame moved by a number"));
        let (low, high) = (steps[1], steps[steps.len() - 2]);
        (high, low, high / low)
    }

    /// The same without the forgiveness: the largest and smallest frame in the
    /// middle, whatever caused them.
    fn raw_swing(&self) -> (f64, f64) {
        let from = *self.notches.get(1).unwrap_or(&self.last_notch);
        let steps = self
            .frames
            .iter()
            .filter(|(_, at)| *at >= from && *at <= self.last_notch)
            .map(|(step, _)| step.abs());
        steps.fold((0.0f64, f64::INFINITY), |(high, low), step| {
            (high.max(step), low.min(step))
        })
    }

    /// How long after the last notch the page finally stopped.
    fn settled_after(&self) -> Duration {
        let (_, last) = self.movement();
        self.frames[last]
            .1
            .saturating_duration_since(self.last_notch)
    }

    /// The step profile, for `--nocapture`.
    fn profile(&self) -> String {
        self.frames
            .iter()
            .map(|(step, _)| {
                if *step == 0.0 {
                    "[0]".to_string()
                } else {
                    format!("{}", step.round() as i64)
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Turn the wheel `notches` times, `apart` milliseconds between them, through
/// the real [`Wheel`] — its thread, its clock, its `mouseWheel` events — with
/// this loop doing exactly what `app::drive` does around it: it adds a notch,
/// reads `Wheel::activity` into the still's clock, and takes whatever frames
/// have arrived.
///
/// Driving the thread rather than ticking an [`Animator`] here is the point of
/// the test. What the person saw on the installed machine was the animation
/// sharing a thread with a loop that spends nine milliseconds decoding a frame
/// and writes 2.9 MB of pixels; these profiles are only worth anything if the
/// ticks are timed the way the program times them.
///
/// It runs until the animation has finished *and* the still policy has asked
/// for a picture, which is `motion::INPUT_QUIET` past the last tick — long
/// enough that every frame the wheel caused is in hand.
fn roll(client: &mut Client, notches: u32, apart: Duration) -> Roll {
    let at = (WIDE as i32 / 2, TALL as i32 / 2);
    let counted = Arc::new(Counted {
        wire: tos_browser::app::Wire::new(client.notifier()),
        sent: AtomicUsize::new(0),
    });
    let wheel = Wheel::start();
    let mut rest = Motion::new(Instant::now());
    let mut sent = 0u32;
    let mut next_notch = Instant::now();
    let mut last_notch = next_notch;
    let mut was = scroll_y(client);
    let mut roll = Roll {
        frames: Vec::new(),
        notches: Vec::new(),
        last_notch,
        wanted: Vec::new(),
        events: 0,
    };

    let give_up = Instant::now() + Duration::from_secs(30);
    while Instant::now() < give_up {
        let now = Instant::now();
        if sent < notches && now >= next_notch {
            wheel.notch(
                "the tab in front",
                counted.clone(),
                at,
                (0.0, tos_browser::app::WHEEL_PIXELS),
            );
            rest.input(now);
            roll.notches.push(now);
            last_notch = now;
            sent += 1;
            next_notch = now + apart;
        }
        if let Some(when) = wheel.activity() {
            rest.input(when);
        }
        for (offset, stamp) in take_offsets(client) {
            let seen = Instant::now();
            rest.motion_frame(stamp, seen);
            roll.frames.push((offset - was, seen));
            was = offset;
        }
        if rest.wants_still(Instant::now()) {
            // Nothing is ever sent for it; what is recorded is that the policy
            // would have, which is what `app::rest_shot` asks every pass.
            roll.wanted.push(Instant::now());
            rest.still_requested();
            rest.still_failed();
        }
        if sent == notches && wheel.owed() == (0.0, 0.0) && !roll.wanted.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(sent, notches, "the notches never all went out");
    roll.last_notch = last_notch;
    roll.events = counted.sent.load(Ordering::SeqCst);
    roll
}

/// A page loaded, painted, casting, and quiet: everything the wheel tests do
/// before they touch the wheel.
fn a_page_to_scroll(client: &mut Client) {
    prepare(client);
    article(client, WIDE, TALL);
    // A page that has loaded is not necessarily a page that has painted; one
    // screenshot forces the first paint, so the frames below are the wheel's.
    let _ = screenshot(client, "png", None);
    cast(client, "jpeg", Some(motion::QUALITY), WIDE, TALL);
    // And the first frames of the screencast are the page arriving rather than
    // the page scrolling.
    std::thread::sleep(Duration::from_millis(400));
    let _ = take_offsets(client);
    assert_eq!(scroll_y(client), 0.0, "the page starts at the top");
}

/// Ask for the lossless still without waiting for it, as the loop does.
fn ask_for_a_still(client: &mut Client) -> Pending {
    client
        .send(
            "Page.captureScreenshot",
            Json::object(vec![("format", Json::string("png"))]),
        )
        .expect("the still goes out")
}

/// The picture out of a still's reply, or nothing if it carried none.
fn still_picture(answer: Result<Json, String>) -> Option<Vec<u8>> {
    answer
        .ok()
        .and_then(|reply| reply.get("data").and_then(Json::as_str).map(str::to_string))
        .and_then(|data| tos_browser::base64::decode(data.as_bytes()).ok())
}

/// Twelve notches, 50 ms apart: a flick, faster than a hand really rolls —
/// the 150 to 300 ms a hand leaves between notches is the comfortable case,
/// and this is the one a scroll animation has to survive.
const NOTCHES: u32 = 12;
const EVERY: Duration = Duration::from_millis(50);

/// Six notches, 100 ms apart: a steady hand, which is what a person rolling a
/// wheel actually produces and the case the exponential lost.
const STEADY: u32 = 6;
const STEADILY: Duration = Duration::from_millis(100);

/// A still photographs itself into the screencast, exactly once.
///
/// `motion::SHUTTER_FRAMES` is the number the whole rest policy is built on,
/// and it is a property of the engine rather than of this crate: a
/// `Page.captureScreenshot` forces a capture of the page's surface, and the
/// screencast is watching that same surface. Taking it for granted is what
/// made the first version of the policy loop — the frame the still provoked
/// was read as the page moving, which cleared the rest, which asked for
/// another still. So it is asserted here, on a page nothing at all is
/// happening to, along with where in the still's window the frame lands.
#[test]
fn a_still_photographs_itself_into_the_screencast_exactly_once() {
    let Some((mut engine, mut client)) = connect() else {
        return;
    };
    prepare(&mut client);
    article(&mut client, WIDE, TALL);
    let _ = screenshot(&mut client, "png", None);
    cast(&mut client, "jpeg", Some(motion::QUALITY), WIDE, TALL);
    // Let the load's own frames go by, and check that a page nobody is
    // touching then produces none of its own.
    let settle = Instant::now() + Duration::from_secs(2);
    while Instant::now() < settle {
        take_frames(&mut client);
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut idle = 0usize;
    let quiet = Instant::now() + Duration::from_secs(2);
    while Instant::now() < quiet {
        idle += take_frames(&mut client).len();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(idle, 0, "the page moved on its own, so this proves nothing");

    for round in 0..5 {
        let requested = motion::now_seconds();
        let pending = ask_for_a_still(&mut client);
        let mut answer = None;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Some(reply) = client.take_reply(&pending) {
                answer = Some(reply);
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let replied = motion::now_seconds();
        assert!(
            still_picture(answer.expect("the still came back")).is_some(),
            "a still with no picture in it"
        );
        // Everything the screenshot provoked, including anything that was
        // already queued when the reply was taken.
        let mut stamps = Vec::new();
        let until = Instant::now() + Duration::from_millis(600);
        while Instant::now() < until {
            stamps.extend(
                take_frames(&mut client)
                    .into_iter()
                    .filter_map(|(_, at)| at),
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        eprintln!(
            "still {round}: {:.0} ms to the reply, {} frame(s) at [{}] ms from the request",
            (replied - requested) * 1000.0,
            stamps.len(),
            stamps
                .iter()
                .map(|at| format!("{:+.0}", (at - requested) * 1000.0))
                .collect::<Vec<_>>()
                .join(", "),
        );
        assert_eq!(
            stamps.len(),
            motion::SHUTTER_FRAMES as usize,
            "a still provoked {} screencast frames, not {}",
            stamps.len(),
            motion::SHUTTER_FRAMES,
        );
        // And it is stamped inside the still's own window, which is what makes
        // crediting the still with its reply enough to keep it off the screen.
        assert!(
            stamps[0] >= requested && stamps[0] <= replied,
            "the shutter frame is stamped outside the still it belongs to"
        );
    }

    let _ = client.call("Page.stopScreencast", Json::empty());
    client.close();
    engine.kill();
}

/// One notch is an animation that arrives and stops.
///
/// This is the difference a person sees as "scrolling is choppy", and it is
/// not a frame rate: one `Input.dispatchMouseEvent` of `deltaY: 120` moves the
/// page 120 pixels in a **single** screencast frame, whatever the screencast
/// is capable of. `Input.synthesizeScrollGesture` was the answer to that for
/// one branch, and it brought a worse problem with it — see
/// [`notches_faster_than_the_engine_never_stop_the_page`].
///
/// So a notch is a curve of its own and the animation is this program's. What
/// is asserted is the shape of it: enough frames that it is an animation, a
/// page that ends exactly one notch down rather than approaching it for ever,
/// and a settling that is over inside 320 ms — `scroll::D` plus the engine's
/// own lag, with room for a host slower than the one this was measured on.
#[test]
fn one_notch_is_an_animation_and_not_a_jump() {
    let Some((mut engine, mut client)) = connect() else {
        return;
    };
    a_page_to_scroll(&mut client);

    let run = roll(&mut client, 1, Duration::ZERO);
    let _ = client.call("Page.stopScreencast", Json::empty());
    let (first, last) = run.movement();
    eprintln!(
        "one notch at D={:?}: {} wheel events, {} frames over {:?}\n  {}",
        scroll::D,
        run.events,
        last + 1 - first,
        run.span(),
        run.profile(),
    );
    eprintln!(
        "  the page stood still for at most {:?}",
        run.longest_stall().0
    );

    assert!(
        last + 1 - first >= 6,
        "only {} frames for one notch: the page jumped\n  {}",
        last + 1 - first,
        run.profile()
    );
    assert!(
        run.settled_after() <= Duration::from_millis(320),
        "one notch was still moving {:?} after it: {}",
        run.settled_after(),
        run.profile()
    );
    assert!(
        run.span() >= Duration::from_millis(100),
        "one notch took {:?}: {}",
        run.span(),
        run.profile()
    );
    assert_eq!(
        scroll_y(&mut client),
        tos_browser::app::WHEEL_PIXELS,
        "an animated notch still ends exactly one notch down"
    );
    assert!(
        run.wanted.iter().all(|at| *at > run.frames[last].1),
        "a still was asked for while the page was still moving"
    );

    client.close();
    engine.kill();
}

/// A steady hand moves the page steadily. This is the case the exponential
/// lost.
///
/// A notch every 100 ms is what a person rolling a wheel actually does, and it
/// was the undoing of both animations that came before this one.
/// `Input.synthesizeScrollGesture`, one gesture per coalesced pile, gave the
/// offset each frame carried as:
///
/// ```text
/// 12 12 12 11 12 9 [0] 12 23 23 24 23 18 [0] 10 12 23 25 22 …
/// ```
///
/// — every `[0]` a frame in which the page stood still, because a gesture is
/// an animation with its own beginning and end and the hand's notches do not
/// fall on them. The exponential that replaced it never stopped, but it
/// front-loaded every notch:
///
/// ```text
/// 25 18 12 9 6 4 | 39 27 19 13 9 7 | 41 28 20 14 10 | 43 30 21 15 …
/// ```
///
/// — a tenfold swing inside every notch, ten times a second, which on the VM
/// the person saw as the page shaking up and down. Neither is a stall and
/// neither is a frame rate: both are the *shape* of the delivery.
///
/// So what is asserted here is the shape. Each notch is its own ease-out over
/// `scroll::D` and the curves overlap, so no frame in the middle of the run
/// may be more than three times any other — which is the difference between a
/// page that moves and a page that lurches.
#[test]
fn a_steady_hand_moves_the_page_steadily() {
    let Some((mut engine, mut client)) = connect() else {
        return;
    };
    a_page_to_scroll(&mut client);

    let run = roll(&mut client, STEADY, STEADILY);
    let _ = client.call("Page.stopScreencast", Json::empty());
    let (first, last) = run.movement();
    let (high, low, swing) = run.swing();
    let (raw_high, raw_low) = run.raw_swing();
    eprintln!(
        "{STEADY} notches every {STEADILY:?} at D={:?}: {} wheel events, \
         {} frames, settled {:?} after the last notch\n  {}",
        scroll::D,
        run.events,
        last + 1 - first,
        run.settled_after(),
        run.profile(),
    );
    eprintln!(
        "  the middle of the run swings {low:.0} to {high:.0} ({swing:.1}x); \
         raw {raw_low:.0} to {raw_high:.0}"
    );

    let (stall, in_a_row) = run.longest_stall();
    eprintln!("  the page stood still for at most {stall:?}, {in_a_row} frames in a row");
    assert!(
        swing <= 3.0,
        "a frame moved {high:.0} pixels and another {low:.0} — {swing:.1}x — in the \
         middle of a steady hand, which is the lurching\n  {}",
        run.profile()
    );
    assert!(
        stall <= Duration::from_millis(60) && in_a_row <= 1,
        "the page stood still for {stall:?} ({in_a_row} frames in a row) in the \
         middle of a scroll, which is the pulsing\n  {}",
        run.profile()
    );
    assert_eq!(
        scroll_y(&mut client),
        STEADY as f64 * tos_browser::app::WHEEL_PIXELS,
        "the animation lost a notch, or invented one"
    );
    assert!(
        run.wanted.iter().all(|at| *at > run.frames[last].1),
        "a still was asked for while the page was still moving"
    );

    client.close();
    engine.kill();
}

/// A hand on the wheel gets JPEG frames and nothing else, and the page stops
/// when the hand does.
///
/// Two things at once, because they are the same run. Twelve notches 50 ms
/// apart is faster than a hand really rolls and is the case a gesture handled
/// worst: `… 48 70 25 44 [0] 45 46 47 … 47 [116] 12 [0] 5 5 13 17 …` — stops,
/// a 116-pixel jump, another stop, and a slow tail that went on after the
/// wheel had stopped. That tail is the other half of what the person reported:
/// "when I stop the wheel I want it to stop."
///
/// Every notch is a curve of `scroll::D` and nothing else, so the last one to
/// arrive is the last one to finish whatever else is running and however much
/// it all comes to: a big pile moves *further* rather than for longer, and
/// there is no ceiling anywhere to give the scrolling a speed limit.
/// **350 ms after the last notch is the deadline** — `scroll::D` and the
/// engine's own lag — and it holds for any pile.
///
/// The still policy is asserted on the same run, because it is the flicker
/// this branch's predecessor fixed and the thing most likely to break when the
/// wheel changes shape: with the first rule that shipped — a still after
/// 150 ms of frame quiet, with no notice taken of the wheel — every notch
/// ended in a lossless PNG and a page with colour in it flashed several times
/// a second. No still may be *asked for* until the animation is over, and then
/// exactly one.
#[test]
fn a_hand_on_the_wheel_gets_no_still_until_it_stops() {
    let Some((mut engine, mut client)) = connect() else {
        return;
    };
    a_page_to_scroll(&mut client);

    let run = roll(&mut client, NOTCHES, EVERY);
    let _ = client.call("Page.stopScreencast", Json::empty());
    let (first, last) = run.movement();
    eprintln!(
        "{NOTCHES} notches every {EVERY:?} at D={:?}: {} wheel events, \
         {} frames, settled {:?} after the last notch, {} stills wanted\n  {}",
        scroll::D,
        run.events,
        last + 1 - first,
        run.settled_after(),
        run.wanted.len(),
        run.profile(),
    );

    assert!(
        last + 1 - first > 20,
        "only {} frames for {NOTCHES} notches: the page jumped",
        last + 1 - first
    );
    let (stall, in_a_row) = run.longest_stall();
    eprintln!("  the page stood still for at most {stall:?}, {in_a_row} frames in a row");
    assert!(
        stall <= Duration::from_millis(60) && in_a_row <= 1,
        "the page stood still for {stall:?} ({in_a_row} frames in a row) in the \
         middle of a scroll, which is the pulsing\n  {}",
        run.profile()
    );
    assert!(
        run.settled_after() <= Duration::from_millis(350),
        "the page went on moving for {:?} after the wheel stopped",
        run.settled_after()
    );
    assert_eq!(
        scroll_y(&mut client),
        NOTCHES as f64 * tos_browser::app::WHEEL_PIXELS,
        "the animation lost a notch, or invented one"
    );
    assert!(
        run.wanted.iter().all(|at| *at > run.frames[last].1),
        "a still was asked for while the page was still moving"
    );
    assert_eq!(
        run.wanted.len(),
        1,
        "the scroll should cost one lossless still, at the end of it"
    );

    client.close();
    engine.kill();
}

/// The loop keeps its hands free while the engine draws a still.
///
/// `Page.captureScreenshot` at a pane's size is 66 to 98 ms on the VirtualBox
/// machine this was measured on, and the first version took it with a blocking
/// call — so every key pressed in that window arrived a tenth of a second
/// late, and a key that seemed to need pressing twice was the report that
/// found it. Here the still goes out with `Client::send`, the key goes out
/// immediately afterwards, and the page has acted on it before the still's
/// reply is collected.
#[test]
fn a_key_is_handled_while_the_still_is_in_flight() {
    let Some((mut engine, mut client)) = connect() else {
        return;
    };
    prepare(&mut client);
    // A pane's worth of viewport, because that is what makes the screenshot
    // slow enough to be worth not waiting for.
    client
        .call(
            "Emulation.setDeviceMetricsOverride",
            Json::object(vec![
                ("width", Json::number(WIDE)),
                ("height", Json::number(TALL)),
                ("deviceScaleFactor", Json::number(1)),
                ("mobile", Json::Bool(false)),
            ]),
        )
        .expect("a pane-sized viewport");
    client
        .call(
            "Runtime.evaluate",
            Json::object(vec![
                ("expression", Json::string("document.title='waiting'")),
                ("returnByValue", Json::Bool(true)),
            ]),
        )
        .expect("the title is reset");

    let at = Instant::now();
    let pending = ask_for_a_still(&mut client);
    // What the loop does next is read the terminal, not wait.
    let params = keys::dispatch(&KeyInput {
        key: Key::Char('k'),
        mods: Mods::default(),
        action: KeyAction::Press,
        text: Some('k'),
    })
    .expect("a key with a name");
    client
        .notify("Input.dispatchKeyEvent", params)
        .expect("the key is dispatched");
    let dispatched = at.elapsed();
    let early = client.take_reply(&pending);
    assert!(
        early.is_none(),
        "the still replied before a key could even be sent, so this proves \
         nothing about the loop"
    );

    // The page acts on the key while the engine is still drawing.
    let seen = wait_for_title(&mut client, "key ", Duration::from_secs(5));
    assert_eq!(&seen, "key k KeyK 75");

    // And the still comes back afterwards, whole.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut answer = None;
    while Instant::now() < deadline {
        if let Some(reply) = client.take_reply(&pending) {
            answer = Some(reply);
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let png = still_picture(answer.expect("the still came back")).expect("a picture");
    assert_eq!(&png[..4], b"\x89PNG");
    eprintln!(
        "the key went out {dispatched:?} after the still was asked for; \
         the whole still took {:?} and is {} kB of PNG",
        at.elapsed(),
        png.len() / 1024,
    );
    assert!(
        dispatched < Duration::from_millis(20),
        "sending a key took {dispatched:?}, which is not \"immediately\""
    );

    client.close();
    engine.kill();
}

/// What a few seconds of an ordinary page leaves behind in the mailbox.
///
/// The two commands this program sends by the thousand go out with
/// `Client::notify` — a `Page.screencastFrameAck` per frame, sixty times a
/// second, and fourteen `Input.dispatchMouseEvent` per wheel notch — and Chromium
/// answers every one of them. Filing those answers under their ids was a leak
/// with a rate: an hour of reading was hundreds of thousands of entries. The
/// claim now is that nothing is kept for a command nobody will come back for,
/// and the only place to prove it is against an engine that really does reply.
///
/// A still asked for and given up on is the other half: the reply is a
/// megabyte of PNG, and dropping its `Pending` has to be enough to be rid of
/// it however late it arrives.
#[test]
fn nothing_is_kept_for_the_acknowledgements_and_the_wheel() {
    let Some((mut engine, mut client)) = connect() else {
        return;
    };
    a_page_to_scroll(&mut client);
    assert_eq!(
        (client.replies_held(), client.replies_wanted()),
        (0, 0),
        "the page was loaded with calls, and a call collects its own reply"
    );

    let at = (WIDE as i32 / 2, TALL as i32 / 2);
    let mut animator = Animator::default();
    let mut notches = 0u32;
    let mut next_notch = Instant::now() + Duration::from_millis(400);
    let mut frames = 0usize;
    let mut events = 0usize;

    let until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < until {
        let now = Instant::now();
        if notches < NOTCHES && now >= next_notch {
            animator.notch(at, (0.0, tos_browser::app::WHEEL_PIXELS), now);
            notches += 1;
            next_notch = now + EVERY;
        }
        while let Some(step) = animator.tick(Instant::now()) {
            wheel_step(&mut client, step);
            events += 1;
        }
        frames += take_offsets(&mut client).len();
        std::thread::sleep(Duration::from_millis(1));
    }
    // Whatever the engine was still saying about the last of them.
    std::thread::sleep(Duration::from_millis(500));
    frames += take_offsets(&mut client).len();

    assert_eq!(notches, NOTCHES, "the notches never all went out");
    assert!(
        frames > 20,
        "only {frames} frames in three seconds; the page was not casting, so \
         this proves nothing about the acknowledgements"
    );
    eprintln!(
        "{frames} frames acknowledged and {events} wheel events sent; the \
         mailbox holds {} replies and wants {}",
        client.replies_held(),
        client.replies_wanted()
    );
    assert_eq!(
        (client.replies_held(), client.replies_wanted()),
        (0, 0),
        "{} replies to commands nobody asked about",
        client.replies_held()
    );

    // And the still that is given up on.
    let pending = ask_for_a_still(&mut client);
    assert_eq!(
        client.replies_wanted(),
        1,
        "the still is the one thing outstanding"
    );
    drop(pending);
    std::thread::sleep(Duration::from_millis(1000));
    let _ = take_offsets(&mut client);
    assert_eq!(
        (client.replies_held(), client.replies_wanted()),
        (0, 0),
        "a still nobody is waiting for was kept anyway"
    );

    client.close();
    engine.kill();
}
