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

use std::time::{Duration, Instant};

use tos_browser::cdp::{Client, Pending};
use tos_browser::engine::{self, Engine};
use tos_browser::graphics::{Painter, Raw, IMAGE_ID};
use tos_browser::input::{Key, KeyAction, KeyInput, Mods};
use tos_browser::json::Json;
use tos_browser::keys;
use tos_browser::motion::{self, Motion};
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

// ---------------------------------------------------------------------------
// The format the frames go in
// ---------------------------------------------------------------------------

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

/// One wheel notch over the middle of the page, exactly as `app::send_mouse`
/// sends one: a notification, because the reply says nothing and a round trip
/// between two notches would be a round trip a person can feel.
fn notch(client: &mut Client) {
    client
        .notify(
            "Input.dispatchMouseEvent",
            Json::object(vec![
                ("type", Json::string("mouseWheel")),
                ("x", Json::number(WIDE / 2)),
                ("y", Json::number(TALL / 2)),
                ("modifiers", Json::number(0)),
                ("button", Json::string("none")),
                ("buttons", Json::number(0)),
                ("deltaX", Json::number(0)),
                ("deltaY", Json::number(120)),
            ]),
        )
        .expect("the notch goes out");
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

/// Ten notches, 200 ms apart: two seconds of scrolling, in the middle of the
/// 150 to 300 ms a hand leaves between them.
const NOTCHES: u32 = 10;
const EVERY: Duration = Duration::from_millis(200);

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

/// A hand on the wheel gets JPEG frames and nothing else.
///
/// This is the flicker, against a real engine: ten notches 200 ms apart, which
/// is what a hand does, driven through the policy in
/// `apps/browser/src/motion.rs`. With the rule that shipped first — a still
/// after 150 ms of frame quiet, with no notice taken of the wheel — every
/// notch ended in a lossless still, so the screen went JPEG, PNG, JPEG, PNG
/// several times a second and a page with colour in it flashed. What is
/// asserted is that no still is even *asked for* while the notches are going,
/// and that exactly one is painted once they stop.
#[test]
fn a_hand_on_the_wheel_gets_no_still_until_it_stops() {
    let Some((mut engine, mut client)) = connect() else {
        return;
    };
    prepare(&mut client);
    article(&mut client, WIDE, TALL);
    // A page that has loaded is not necessarily a page that has painted; one
    // screenshot forces the first paint, so the frames below are the wheel's.
    let _ = screenshot(&mut client, "png", None);
    cast(&mut client, "jpeg", Some(motion::QUALITY), WIDE, TALL);

    let mut rest = Motion::new(Instant::now());
    let mut in_flight: Option<(Pending, Instant)> = None;
    let mut sent = 0u32;
    let mut next = Instant::now();
    let mut last_notch = Instant::now();
    let mut requested: Vec<Instant> = Vec::new();
    let mut painted: Vec<Instant> = Vec::new();
    let mut discarded = 0usize;
    let mut frames = 0usize;
    let mut settled: Option<Instant> = None;

    let give_up = Instant::now() + Duration::from_secs(20);
    while Instant::now() < give_up {
        let now = Instant::now();
        if sent < NOTCHES && now >= next {
            notch(&mut client);
            rest.input(now);
            last_notch = now;
            sent += 1;
            next = now + EVERY;
        }
        // Frames first and the reply second, which is the order the loop
        // itself keeps: a frame that arrived on the same pass as the reply has
        // to have been counted against it before it is judged.
        for (_, stamp) in take_frames(&mut client) {
            frames += 1;
            rest.motion_frame(stamp, Instant::now());
        }
        match in_flight.take() {
            Some((pending, at)) => match client.take_reply(&pending) {
                Some(answer) => {
                    if rest.still_arrived(motion::now_seconds()) {
                        assert!(
                            still_picture(answer).is_some(),
                            "a still was painted with no picture in it"
                        );
                        painted.push(Instant::now());
                        settled = Some(Instant::now());
                    } else {
                        discarded += 1;
                    }
                }
                None => {
                    assert!(
                        at.elapsed() < Duration::from_secs(5),
                        "the engine never answered a screenshot"
                    );
                    in_flight = Some((pending, at));
                }
            },
            None => {
                if rest.wants_still(Instant::now()) {
                    requested.push(Instant::now());
                    rest.still_requested();
                    in_flight = Some((ask_for_a_still(&mut client), Instant::now()));
                }
            }
        }
        // Once a still is up, half a second of nothing else happening is what
        // says it was the only one.
        if let Some(settled) = settled {
            if sent == NOTCHES && settled.elapsed() > Duration::from_millis(600) {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    let _ = client.call("Page.stopScreencast", Json::empty());
    eprintln!(
        "{NOTCHES} notches every {EVERY:?}: {frames} frames, {} stills asked for, \
         {} painted, {discarded} thrown away because the page moved",
        requested.len(),
        painted.len(),
    );
    assert!(frames > 5, "only {frames} frames: the wheel moved nothing");
    assert!(
        requested.iter().all(|at| *at > last_notch),
        "a still was asked for while the wheel was still turning"
    );
    assert!(
        painted.iter().all(|at| *at > last_notch),
        "a still was painted while the wheel was still turning"
    );
    assert_eq!(
        painted.len(),
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
