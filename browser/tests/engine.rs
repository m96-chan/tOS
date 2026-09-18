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
//! Every test here skips when there is no Chromium, because CI has none. They
//! are not skipped quietly: the reason is printed, so that a run which proved
//! nothing does not read like a run which proved something.

use std::time::{Duration, Instant};

use tos_browser::cdp::Client;
use tos_browser::engine::{self, Engine};
use tos_browser::graphics::{Painter, IMAGE_ID};
use tos_browser::input::{Key, KeyAction, KeyInput, Mods};
use tos_browser::json::Json;
use tos_browser::keys;
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

/// Connect to a fresh engine, or say why the test is not running.
fn connect() -> Option<(Engine, Client)> {
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
