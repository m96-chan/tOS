//! The end this crate exists for: a real engine, real frames, and a real
//! terminal parsing what would go down the pane's pseudoterminal.
//!
//! The terminal here is `tos_term::Terminal` with the compositor's own
//! `ImageFiles` installed, which is exactly what `Pane::spawn` gives a pane —
//! so the `t=s` path is the real one, names and unlinking included, and the
//! PNGs are decoded by the decoder that runs in a session. What is missing
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

use std::time::{Duration, Instant};

use tos_browser::cdp::Client;
use tos_browser::engine::{self, Engine};
use tos_browser::graphics::{Painter, IMAGE_ID};
use tos_browser::input::{Key, KeyAction, KeyInput, Mods};
use tos_browser::json::Json;
use tos_browser::keys;
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
    let mut terminal = tos_term::Terminal::new(
        (WIDTH / CELL.0) as usize,
        (HEIGHT / CELL.1) as usize + 1,
        tos_term::TerminalConfig::default(),
    );
    terminal.set_medium_reader(Box::new(ImageFiles::at(
        vec![dir.to_path_buf()],
        dir.to_path_buf(),
    )));
    terminal
}

fn temp_dir(what: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("tos-browser-it-{what}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a directory");
    dir
}

/// A screencast, decoded and drawn, with the numbers it cost.
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

    client
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

    let run_for = Duration::from_secs(3);
    let started = Instant::now();
    let (mut frames, mut bytes, mut escape_bytes) = (0usize, 0usize, 0usize);
    let mut drawing = Duration::ZERO;

    while started.elapsed() < run_for {
        for event in client.events() {
            if event.method != "Page.screencastFrame" {
                continue;
            }
            if let Some(session) = event.params.get("sessionId").and_then(Json::as_i64) {
                client
                    .notify(
                        "Page.screencastFrameAck",
                        Json::object(vec![("sessionId", Json::number(session as f64))]),
                    )
                    .expect("the ack goes out");
            }
            let data = event
                .params
                .get("data")
                .and_then(Json::as_str)
                .expect("a frame carries its picture");
            let png = tos_browser::base64::decode(data.as_bytes()).expect("valid base64");
            assert_eq!(&png[..4], b"\x89PNG", "the engine promised PNG");

            let at = Instant::now();
            let sequence = painter.frame(&png, cells, 2, 1);
            terminal.advance(&sequence);
            drawing += at.elapsed();

            frames += 1;
            bytes += png.len();
            escape_bytes += sequence.len();
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let _ = client.call("Page.stopScreencast", Json::empty());

    assert!(frames > 10, "only {frames} frames in three seconds");
    eprintln!(
        "shared memory: {frames} frames in {:?} ({:.1}/s), {} bytes of PNG on average, \
         {} bytes down the pane per frame, {:?} in the terminal per frame",
        started.elapsed(),
        frames as f64 / started.elapsed().as_secs_f64(),
        bytes / frames,
        escape_bytes / frames,
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
/// takes — and the one whose cost is worth knowing.
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
    client
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

    let started = Instant::now();
    let (mut frames, mut escape_bytes) = (0usize, 0usize);
    while started.elapsed() < Duration::from_secs(2) {
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
            let data = event
                .params
                .get("data")
                .and_then(Json::as_str)
                .unwrap_or("");
            let png = tos_browser::base64::decode(data.as_bytes()).expect("valid base64");
            let sequence = tos_browser::graphics::inline_command(&png, cells);
            terminal.advance(&sequence);
            frames += 1;
            escape_bytes += sequence.len();
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let _ = client.call("Page.stopScreencast", Json::empty());

    assert!(frames > 5, "only {frames} frames inline");
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
