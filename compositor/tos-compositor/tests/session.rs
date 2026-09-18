//! Whole-compositor tests: real processes on real PTYs, rendered to pixels.

use std::time::{Duration, Instant};

use tos_compositor::{Compositor, Config};
use tos_render::OwnedFramebuffer;
use tos_session::{Action, Axis, Direction};

const SIZE: (u32, u32) = (800, 480);

fn compositor(command: &[&str]) -> Compositor {
    let config = Config {
        command: Some(command.iter().map(|s| s.to_string()).collect()),
        bitmap_scale: Some(2),
        // Force the built-in face so the tests do not depend on the host's
        // fonts, and turn off the fade so colours can be asserted exactly.
        font: Some("/nonexistent".into()),
        // And a root with no machine under it, so the status bar says nothing
        // about the battery or the network of whoever is running the tests.
        system_root: "/nonexistent-so-this-machine-has-no-hardware".into(),
        inactive_fade: 0,
        ..Config::default()
    };
    Compositor::new(config, SIZE, None).expect("compositor")
}

/// Pump the compositor until `predicate` holds or time runs out.
fn wait_for(
    compositor: &mut Compositor,
    timeout: Duration,
    predicate: impl Fn(&Compositor) -> bool,
) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        compositor.pump_panes();
        if predicate(compositor) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    compositor.pump_panes();
    predicate(compositor)
}

fn render(compositor: &mut Compositor) -> OwnedFramebuffer {
    let mut framebuffer = OwnedFramebuffer::new(SIZE.0, SIZE.1);
    {
        let mut surface = framebuffer.surface();
        compositor.render_frame(&mut surface, false);
    }
    framebuffer
}

fn pane_text(compositor: &Compositor) -> String {
    let focus = compositor.session().focus();
    compositor.pane(focus).unwrap().terminal.grid().to_text()
}

#[test]
fn a_child_process_writes_into_its_pane() {
    let mut c = compositor(&["/bin/sh", "-c", "echo tos-is-running; sleep 5"]);
    assert!(
        wait_for(&mut c, Duration::from_secs(5), |c| {
            let focus = c.session().focus();
            c.pane(focus)
                .map(|p| p.terminal.grid().to_text().contains("tos-is-running"))
                .unwrap_or(false)
        }),
        "pane never showed the output: {:?}",
        pane_text(&c)
    );
}

#[test]
fn the_child_is_told_the_pane_size() {
    let mut c = compositor(&["/bin/sh", "-c", "stty size; sleep 5"]);
    let focus = c.session().focus();
    let expected = {
        let pane = c.pane(focus).unwrap();
        format!("{} {}", pane.terminal.rows(), pane.terminal.cols())
    };
    assert!(
        wait_for(&mut c, Duration::from_secs(5), |c| {
            pane_text(c).contains(&expected)
        }),
        "expected {expected:?}, got {:?}",
        pane_text(&c)
    );
}

#[test]
fn splitting_resizes_the_existing_child() {
    let mut c = compositor(&["/bin/sh", "-c", "sleep 5"]);
    let first = c.session().focus();
    let before = c.pane(first).unwrap().terminal.cols();

    c.perform(Action::Split(Axis::Columns));
    assert_eq!(c.session().all_panes().len(), 2);

    let after = c.pane(first).unwrap().terminal.cols();
    assert!(after < before, "{after} should be narrower than {before}");
    // Both panes fit the screen with room for the divider.
    let area = c.grid_area();
    let widths: u32 = c
        .session()
        .active()
        .geometry(area)
        .iter()
        .map(|(_, rect)| rect.width)
        .sum();
    assert_eq!(widths + 1, area.width);
}

#[test]
fn each_pane_runs_its_own_process() {
    let mut c = compositor(&["/bin/sh", "-c", "echo $$; sleep 5"]);
    c.perform(Action::Split(Axis::Rows));
    assert!(wait_for(&mut c, Duration::from_secs(5), |c| {
        c.session().all_panes().iter().all(|id| {
            !c.pane(*id)
                .unwrap()
                .terminal
                .grid()
                .to_text()
                .trim()
                .is_empty()
        })
    }));

    let pids: Vec<String> = c
        .session()
        .all_panes()
        .iter()
        .map(|id| {
            c.pane(*id)
                .unwrap()
                .terminal
                .grid()
                .to_text()
                .trim()
                .to_string()
        })
        .collect();
    assert_eq!(pids.len(), 2);
    assert_ne!(pids[0], pids[1], "panes should be separate processes");
}

#[test]
fn focus_moves_between_panes_and_typing_follows_it() {
    let mut c = compositor(&["/bin/sh", "-c", "read line; echo got:$line; sleep 5"]);
    let first = c.session().focus();
    c.perform(Action::Split(Axis::Columns));
    let second = c.session().focus();
    assert_ne!(first, second);

    // Type into the second pane.
    for byte in b"second\r" {
        type_key(&mut c, *byte);
    }
    assert!(wait_for(&mut c, Duration::from_secs(5), |c| {
        c.pane(second)
            .unwrap()
            .terminal
            .grid()
            .to_text()
            .contains("got:second")
    }));

    // Then move focus back and type into the first.
    let area = c.grid_area();
    let _ = area;
    assert!(c.perform(Action::Focus(Direction::Left)));
    assert_eq!(c.session().focus(), first);
    for byte in b"first\r" {
        type_key(&mut c, *byte);
    }
    assert!(
        wait_for(&mut c, Duration::from_secs(5), |c| {
            c.pane(first)
                .unwrap()
                .terminal
                .grid()
                .to_text()
                .contains("got:first")
        }),
        "first pane shows {:?}",
        c.pane(first).unwrap().terminal.grid().to_text()
    );
    // And the second pane did not see the second line.
    assert!(!c
        .pane(second)
        .unwrap()
        .terminal
        .grid()
        .to_text()
        .contains("got:first"));
}

fn type_key(compositor: &mut Compositor, byte: u8) {
    use tos_input::{InputEvent, KeyCode, KeyEvent, Modifiers};
    let code = match byte {
        b'\r' => KeyCode::Enter,
        other => KeyCode::Char(other as char),
    };
    compositor.handle_input(InputEvent::Key(KeyEvent::new(code, Modifiers::NONE)));
}

#[test]
fn a_pane_whose_child_exits_is_closed() {
    let mut c = compositor(&["/bin/sh", "-c", "sleep 5"]);
    c.perform(Action::Split(Axis::Columns));
    assert_eq!(c.session().all_panes().len(), 2);

    let victim = c.session().focus();
    c.pane_mut(victim).unwrap().write(b"\x04");
    c.pane_mut(victim).unwrap().pty.signal(15).unwrap();

    assert!(
        wait_for(&mut c, Duration::from_secs(5), |c| {
            c.session().all_panes().len() == 1
        }),
        "the pane was never cleaned up"
    );
    assert!(c.is_running());
}

#[test]
fn the_compositor_stops_when_the_last_child_exits() {
    let mut c = compositor(&["/bin/sh", "-c", "exit 0"]);
    assert!(wait_for(&mut c, Duration::from_secs(5), |c| !c.is_running()));
}

#[test]
fn workspaces_hold_separate_panes() {
    let mut c = compositor(&["/bin/sh", "-c", "sleep 5"]);
    c.perform(Action::NewWorkspace);
    assert_eq!(c.session().workspace_count(), 2);
    assert_eq!(c.session().all_panes().len(), 2);
    assert_eq!(c.session().active().panes().len(), 1);

    // Switching back shows the original pane again.
    c.perform(Action::SelectWorkspace(1));
    assert_eq!(c.session().focus(), c.session().root_pane());
}

#[test]
fn a_split_layout_renders_every_pane() {
    let mut c = compositor(&[
        "/bin/sh",
        "-c",
        "printf '\\033[41m          \\033[0m\\n'; sleep 5",
    ]);
    c.perform(Action::Split(Axis::Columns));
    // The block is painted with spaces, so waiting on text would pass before
    // anything arrived; wait on the cell's background instead.
    assert!(wait_for(&mut c, Duration::from_secs(5), |c| {
        c.session().all_panes().iter().all(|id| {
            c.pane(*id)
                .unwrap()
                .terminal
                .grid()
                .cell(0, 0)
                .map(|cell| cell.attrs.bg == tos_term::Color::Indexed(1))
                .unwrap_or(false)
        })
    }));

    let framebuffer = render(&mut c);
    let (cw, ch) = c.cell_size();
    let area = c.grid_area();
    let geometry = c.session().active().geometry(area);
    assert_eq!(geometry.len(), 2);

    // Each pane painted its own red block near its top left corner.
    for (_, rect) in &geometry {
        let x = rect.x * cw + cw / 2;
        let y = rect.y * ch + ch / 2;
        assert_eq!(
            framebuffer.pixel(x, y),
            0xcc5757,
            "pane at {rect:?} did not paint"
        );
    }

    // The divider between them is drawn in the chrome colour, not a pane's.
    let dividers = c.session().active().layout.dividers(area);
    assert_eq!(dividers.len(), 1);
}

#[test]
fn cycling_focus_reaches_every_pane_and_gives_a_zoom_back() {
    // The directions stop at the edge of the workspace and refuse to move at
    // all while a pane is zoomed, which is what the cycle is for: it is the
    // way to the pane you cannot see from the one you are in.
    let mut c = compositor(&["/bin/sh", "-c", "sleep 5"]);
    let first = c.session().focus();
    c.perform(Action::Split(Axis::Columns));
    let second = c.session().focus();

    assert!(c.perform(Action::FocusNext));
    assert_eq!(c.session().focus(), first);
    assert!(c.perform(Action::FocusPrevious));
    assert_eq!(c.session().focus(), second);

    // Zoomed, the direction keys have nowhere to go and the cycle takes the
    // zoom off on its way out, so the pane it lands on is one that is drawn.
    assert!(c.perform(Action::ToggleZoom));
    assert!(!c.perform(Action::Focus(Direction::Left)));
    assert!(c.perform(Action::FocusNext));
    assert_eq!(c.session().focus(), first);
    let area = c.grid_area();
    assert_eq!(c.session().active().geometry(area).len(), 2);
}

#[test]
fn a_zoomed_pane_fills_the_workspace() {
    let mut c = compositor(&["/bin/sh", "-c", "sleep 5"]);
    c.perform(Action::Split(Axis::Columns));
    assert!(c.perform(Action::ToggleZoom));

    let area = c.grid_area();
    let geometry = c.session().active().geometry(area);
    assert_eq!(geometry.len(), 1);
    assert_eq!(geometry[0].1, area);
    // The zoomed pane's child is told it got bigger.
    let focus = c.session().focus();
    assert_eq!(c.pane(focus).unwrap().terminal.cols(), area.width as usize);
}

#[test]
fn output_scrolls_into_history_and_can_be_read_back() {
    let mut c = compositor(&[
        "/bin/sh",
        "-c",
        "i=0; while [ $i -lt 200 ]; do echo line$i; i=$((i+1)); done; sleep 5",
    ]);
    let focus = c.session().focus();
    assert!(wait_for(&mut c, Duration::from_secs(5), |c| {
        c.pane(focus).unwrap().terminal.grid().scrollback_len() > 50
    }));

    assert!(c.perform(Action::ScrollPage(-1)));
    assert!(c.pane(focus).unwrap().terminal.display_offset() > 0);
    // The frame drawn while scrolled back shows history, not the live screen.
    let framebuffer = render(&mut c);
    assert!(framebuffer.pixels().iter().any(|&px| px != 0));

    assert!(c.perform(Action::ScrollToBottom));
    assert_eq!(c.pane(focus).unwrap().terminal.display_offset(), 0);
}

#[test]
fn a_full_screen_application_gets_the_alternate_screen() {
    let mut c = compositor(&[
        "/bin/sh",
        "-c",
        "printf '\\033[?1049h\\033[2J\\033[1;1Hfullscreen'; sleep 5",
    ]);
    let focus = c.session().focus();
    assert!(wait_for(&mut c, Duration::from_secs(5), |c| {
        c.pane(focus).unwrap().terminal.modes.alt_screen
    }));
    assert!(pane_text(&c).contains("fullscreen"));
    // The alternate screen keeps no history.
    assert_eq!(c.pane(focus).unwrap().terminal.grid().scrollback_len(), 0);
}

#[test]
fn a_program_can_set_the_pane_title() {
    let mut c = compositor(&["/bin/sh", "-c", "printf '\\033]0;my title\\007'; sleep 5"]);
    let focus = c.session().focus();
    assert!(wait_for(&mut c, Duration::from_secs(5), |c| {
        c.pane(focus).unwrap().title == "my title"
    }));
}

#[test]
fn a_program_can_raise_a_notification_and_it_is_kept() {
    // Two of them, close together, which is what used to lose the first: the
    // status bar had one slot and the second overwrote it inside the three
    // seconds nobody had read it in.
    let mut c = compositor(&[
        "/bin/sh",
        "-c",
        "printf '\\033]9;first\\007\\033]777;notify;build;finished\\007'; sleep 5",
    ]);
    assert!(wait_for(&mut c, Duration::from_secs(5), |c| {
        c.notifications().waiting() > 0
    }));
    // The first is on the bar with the second behind it, and both name the
    // pane that raised them.
    assert_eq!(
        c.notifications().status_line().as_deref(),
        Some("pane 1: first (+1)")
    );
    let kept: Vec<String> = c
        .notifications()
        .history()
        .map(|notification| notification.status_text())
        .collect();
    assert_eq!(kept, ["pane 1: build: finished", "pane 1: first"]);
}

#[test]
fn a_program_can_query_the_terminal_and_get_an_answer() {
    // The child asks where the cursor is and reads the six byte reply back.
    // Echo is turned off first, so a reply that never left the compositor
    // could not reach the screen some other way and fake a pass.
    let script = concat!(
        // Canonical mode would hold the reply until a newline that never
        // comes, so the line discipline goes raw for the six byte read.
        "stty raw -echo; printf 'ESC[6n'; ",
        "reply=$(dd bs=1 count=6 2>/dev/null); stty sane; ",
        "printf 'answer:%s\\n' \"$(printf '%s' \"$reply\" | tr -d 'ESC')\"; sleep 5"
    )
    .replace("ESC", "\u{1b}");
    let mut c = compositor(&["/bin/sh", "-c", &script]);
    let focus = c.session().focus();
    assert!(
        wait_for(&mut c, Duration::from_secs(5), |c| {
            c.pane(focus)
                .unwrap()
                .terminal
                .grid()
                .to_text()
                .contains("answer:")
        }),
        "no reply reached the child: {:?}",
        pane_text(&c)
    );
    // A cursor position report for a cursor at the top left.
    assert!(
        pane_text(&c).contains("answer:[1;1R"),
        "got {:?}",
        pane_text(&c)
    );
}

#[test]
fn unfocused_panes_are_faded() {
    let config = Config {
        command: Some(vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf '\\033[41m          \\033[0m\\n'; sleep 5".into(),
        ]),
        bitmap_scale: Some(2),
        font: Some("/nonexistent".into()),
        inactive_fade: 60,
        ..Config::default()
    };
    let mut c = Compositor::new(config, SIZE, None).expect("compositor");
    c.perform(Action::Split(Axis::Columns));
    assert!(wait_for(&mut c, Duration::from_secs(5), |c| {
        c.session().all_panes().iter().all(|id| {
            c.pane(*id)
                .unwrap()
                .terminal
                .grid()
                .cell(0, 0)
                .map(|cell| cell.attrs.bg == tos_term::Color::Indexed(1))
                .unwrap_or(false)
        })
    }));

    let focus = c.session().focus();
    let area = c.grid_area();
    let geometry = c.session().active().geometry(area);
    let framebuffer = render(&mut c);
    let (cw, ch) = c.cell_size();

    let sample = |id| {
        let rect = geometry.iter().find(|(p, _)| *p == id).unwrap().1;
        framebuffer.pixel(rect.x * cw + cw / 2, rect.y * ch + ch / 2)
    };
    let focused = sample(focus);
    let other = c
        .session()
        .all_panes()
        .into_iter()
        .find(|p| *p != focus)
        .unwrap();
    assert_eq!(focused, 0xcc5757, "the focused pane keeps its colours");
    assert_ne!(sample(other), focused, "the unfocused pane should be faded");
}

// ---------------------------------------------------------------------------
// Regressions found in review
// ---------------------------------------------------------------------------

#[test]
fn a_large_paste_is_not_truncated() {
    // The PTY input buffer is a few kilobytes, so a big paste needs several
    // writes; dropping the remainder used to take the bracketed paste
    // terminator with it.
    let mut c = compositor(&["/bin/sh", "-c", "cat > /dev/null; sleep 5"]);
    let focus = c.session().focus();
    let payload = vec![b'x'; 200_000];
    c.pane_mut(focus).unwrap().write(&payload);

    // Drain until everything has been handed to the child.
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        c.pump_panes();
        if c.pane(focus).unwrap().pending_input() == 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(
        c.pane(focus).unwrap().pending_input(),
        0,
        "the paste never finished"
    );
    assert!(
        !c.pane(focus).unwrap().input_overflowed(),
        "nothing should have been dropped"
    );
}

#[test]
fn every_byte_of_a_large_paste_reaches_the_child() {
    // The child puts its terminal in raw mode first: in canonical mode the
    // line discipline itself caps how much it will buffer for one line, which
    // has nothing to do with the compositor.
    let path = std::env::temp_dir().join("tos-paste-payload");
    let _ = std::fs::remove_file(&path);
    let script = format!(
        "stty raw -echo; printf 'READY\r\n'; head -c 100000 > {}; printf 'DONE\r\n'; sleep 5",
        path.display()
    );
    let mut c = compositor(&["/bin/sh", "-c", &script]);
    let focus = c.session().focus();

    assert!(
        wait_for(&mut c, Duration::from_secs(5), |c| {
            c.pane(focus)
                .unwrap()
                .terminal
                .grid()
                .to_text()
                .contains("READY")
        }),
        "the child never got ready"
    );

    let payload = vec![b'y'; 100_000];
    c.pane_mut(focus).unwrap().write(&payload);

    let arrived = wait_for(&mut c, Duration::from_secs(20), |c| {
        c.pane(focus)
            .unwrap()
            .terminal
            .grid()
            .to_text()
            .contains("DONE")
    });
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let _ = std::fs::remove_file(&path);
    assert!(
        arrived,
        "the child never finished reading; got {size} bytes"
    );
    assert_eq!(size, payload.len() as u64, "bytes were lost on the way");
}

#[test]
fn a_pane_too_small_to_split_is_left_alone() {
    let config = Config {
        command: Some(vec!["/bin/sh".into(), "-c".into(), "sleep 5".into()]),
        bitmap_scale: Some(8),
        font: Some("/nonexistent".into()),
        ..Config::default()
    };
    // A tiny display holds very few cells, so the splits soon have no room.
    let mut c = Compositor::new(config, (200, 200), None).expect("compositor");
    let mut created = 1;
    for _ in 0..12 {
        c.perform(Action::Split(Axis::Columns));
        let panes = c.session().all_panes().len();
        assert!(panes >= created, "a pane vanished");
        created = panes;
    }

    // Whatever was created, every pane has room and can be clicked.
    let area = c.grid_area();
    for (pane, rect) in c.session().active().geometry(area) {
        assert!(!rect.is_empty(), "{pane:?} has no area: {rect:?}");
        assert!(rect.right() <= area.right(), "{pane:?} escapes the screen");
    }
    assert!(c.is_running());
}

#[test]
fn a_closed_pane_does_not_wedge_the_mouse() {
    use tos_input::{InputEvent, Modifiers, MouseAction, MouseButton, PointerEvent};

    let mut c = compositor(&["/bin/sh", "-c", "sleep 30"]);
    c.perform(Action::Split(Axis::Columns));
    let victim = c.session().focus();

    // Start a drag in the pane, then let its child die mid-drag.
    let press = |c: &mut Compositor, action, x: f64, y: f64| {
        c.handle_input(InputEvent::Pointer(PointerEvent {
            button: Some(MouseButton::Left),
            action,
            x,
            y,
            modifiers: Modifiers::NONE,
        }))
    };
    let area = c.grid_area();
    let (cw, ch) = c.cell_size();
    let rect = c
        .session()
        .active()
        .geometry(area)
        .into_iter()
        .find(|(id, _)| *id == victim)
        .unwrap()
        .1;
    let x = (rect.x * cw + cw / 2) as f64;
    let y = (rect.y * ch + ch / 2) as f64;
    press(&mut c, MouseAction::Press, x, y);
    assert_eq!(c.session().focus(), victim);

    c.pane_mut(victim).unwrap().pty.signal(15).unwrap();
    assert!(wait_for(&mut c, Duration::from_secs(5), |c| {
        c.session().all_panes().len() == 1
    }));

    // The mouse still works: clicking the surviving pane focuses it.
    let survivor = c.session().all_panes()[0];
    let rect = c.session().active().geometry(c.grid_area())[0].1;
    let x = (rect.x * cw + cw / 2) as f64;
    let y = (rect.y * ch + ch / 2) as f64;
    press(&mut c, MouseAction::Press, x, y);
    assert_eq!(c.session().focus(), survivor);
}

#[test]
fn a_synchronized_update_is_painted_once_it_ends() {
    // DECSET 2026 asks the compositor not to draw a half-finished update; the
    // damage from that update must survive until it is drawn.
    let mut c = compositor(&["/bin/sh", "-c", "sleep 30"]);
    let focus = c.session().focus();
    let mut framebuffer = OwnedFramebuffer::new(SIZE.0, SIZE.1);
    {
        let mut surface = framebuffer.surface();
        c.render_frame(&mut surface, true);
    }

    // Begin a synchronized update and draw a red block inside it. The frame
    // is drawn against a retained buffer, which is when the pane is skipped.
    c.inject(b"\x1b[?2026h");
    c.inject(b"\x1b[41m          \x1b[0m");
    {
        let mut surface = framebuffer.surface();
        c.render_frame(&mut surface, true);
    }
    assert!(
        c.pane(focus).unwrap().terminal.damage().is_dirty(),
        "damage must survive a frame that skipped the pane"
    );

    c.inject(b"\x1b[?2026l");
    {
        let mut surface = framebuffer.surface();
        c.render_frame(&mut surface, true);
    }
    assert_eq!(
        framebuffer.pixel(1, 1),
        0xcc5757,
        "the update was never drawn"
    );
}

#[test]
fn a_pane_holding_a_synchronized_update_open_asks_for_no_frame() {
    // The damage left standing on a skipped pane is not a frame to draw but a
    // frame owed once the program lets go, and DECSET 2026 has no timeout
    // anywhere in tOS: reading it as a frame to draw costs one composed frame
    // per pass of the loop for as long as the update stays open.
    // `needs_render` declines to ask for that frame — and `pump_panes`, which
    // the loop ors into the same decision, has to decline as well or the
    // decline means nothing.
    let mut c = compositor(&["/bin/sh", "-c", "sleep 30"]);
    let focus = c.session().focus();
    let mut framebuffer = OwnedFramebuffer::new(SIZE.0, SIZE.1);
    // The first frame is the full redraw every session starts owing.
    {
        let mut surface = framebuffer.surface();
        c.render_frame(&mut surface, true);
    }

    c.inject(b"\x1b[?2026h");
    c.inject(b"\x1b[41m          \x1b[0m");
    {
        let mut surface = framebuffer.surface();
        c.render_frame(&mut surface, true);
    }
    assert!(
        c.pane(focus).unwrap().terminal.damage().is_dirty(),
        "damage must survive a frame that skipped the pane"
    );
    assert!(!c.needs_render(), "a skipped pane asked to be drawn again");
    assert!(
        !c.pump_panes(),
        "the pump reported the damage the frame deliberately left standing"
    );

    c.inject(b"\x1b[?2026l");
    assert!(
        c.needs_render(),
        "the frame owed by the finished update never came"
    );
}

#[test]
fn a_host_terminal_mouse_click_lands_where_a_device_pointer_at_the_same_spot_does() {
    // This test used to build its click out of `geometry`, which is already
    // in compositor cells, and assert it landed on the pane it came from. It
    // pinned `route_mouse` and never touched the step in front of it, which
    // is the step that was missing: a host terminal reports its own cells,
    // and the nested backend gives each of those one framebuffer pixel of
    // width and two of height, so a host column is a pixel column and a host
    // row is half of one — and only then is it a cell of the grid panes are
    // laid out in. Handing the host's numbers straight to `route_mouse`
    // pointed at somewhere several times off the far edge of that grid, which
    // matched no pane and dropped the click.
    //
    // Pointing at one place twice, once the way a host terminal says it and
    // once the way a device does, is the assertion. The two arrive in
    // different units and have to agree about where the hand is.
    use tos_input::{InputEvent, Modifiers, MouseAction, MouseButton, MouseEvent, PointerEvent};

    let mut c = compositor(&["/bin/sh", "-c", "sleep 30"]);
    c.perform(Action::Split(Axis::Columns));
    let right = c.session().focus();
    let left = c
        .session()
        .all_panes()
        .into_iter()
        .find(|p| *p != right)
        .unwrap();

    let area = c.grid_area();
    let geometry = c.session().active().geometry(area);
    let rect_of = |pane| geometry.iter().find(|(p, _)| *p == pane).unwrap().1;
    let (cw, ch) = c.cell_size();
    // The framebuffer pixel in the middle of a pane, which is the spot both
    // kinds of report are going to name.
    let middle = |rect: tos_session::Rect| {
        (
            (rect.x + rect.width / 2) * cw + cw / 2,
            (rect.y + rect.height / 2) * ch + ch / 2,
        )
    };

    for (pane, other) in [(left, right), (right, left)] {
        // Put the focus on the other pane first, through the path that is
        // already in pixels, so the press being tested has somewhere to move
        // the focus from.
        let (x, y) = middle(rect_of(other));
        c.handle_input(InputEvent::Pointer(PointerEvent {
            x: x as f64,
            y: y as f64,
            button: Some(MouseButton::Left),
            action: MouseAction::Press,
            modifiers: Modifiers::NONE,
        }));
        assert_eq!(c.session().focus(), other, "the device pointer missed");

        let (x, y) = middle(rect_of(pane));
        c.handle_input(InputEvent::Mouse(MouseEvent {
            button: Some(MouseButton::Left),
            action: MouseAction::Press,
            col: x as usize,
            row: (y / 2) as usize,
            pixel: None,
            modifiers: Modifiers::NONE,
        }));
        assert_eq!(
            c.session().focus(),
            pane,
            "the host terminal's cells and the device's pixels disagree about \
             where the hand is"
        );
    }
}

#[test]
fn a_device_pointer_click_lands_in_the_right_pane() {
    use tos_input::{InputEvent, Modifiers, MouseAction, MouseButton, PointerEvent};

    let mut c = compositor(&["/bin/sh", "-c", "sleep 30"]);
    c.perform(Action::Split(Axis::Rows));
    let bottom = c.session().focus();
    let top = c
        .session()
        .all_panes()
        .into_iter()
        .find(|p| *p != bottom)
        .unwrap();

    let (cw, ch) = c.cell_size();
    let area = c.grid_area();
    let geometry = c.session().active().geometry(area);
    let click = |c: &mut Compositor, rect: tos_session::Rect| {
        c.handle_input(InputEvent::Pointer(PointerEvent {
            button: Some(MouseButton::Left),
            action: MouseAction::Press,
            // Pixels, as a device reports them.
            x: ((rect.x + rect.width / 2) * cw) as f64,
            y: ((rect.y + rect.height / 2) * ch) as f64,
            modifiers: Modifiers::NONE,
        }));
    };
    let rect_of = |pane| geometry.iter().find(|(p, _)| *p == pane).unwrap().1;

    click(&mut c, rect_of(top));
    assert_eq!(c.session().focus(), top);
    click(&mut c, rect_of(bottom));
    assert_eq!(c.session().focus(), bottom);
}

#[test]
fn colours_from_the_configuration_reach_the_screen() {
    let mut palette = tos_term::Palette::new();
    palette.background = tos_term::Rgb::new(0x12, 0x34, 0x56);
    let config = Config {
        command: Some(vec!["/bin/sh".into(), "-c".into(), "sleep 5".into()]),
        bitmap_scale: Some(2),
        font: Some("/nonexistent".into()),
        palette,
        chrome: tos_compositor::chrome::Chrome {
            background: tos_term::Rgb::new(0x65, 0x43, 0x21),
            ..tos_compositor::chrome::Chrome::default()
        },
        ..Config::default()
    };
    let mut c = Compositor::new(config, SIZE, None).expect("compositor");
    let (_, ch) = c.cell_size();
    let bar = c.grid_area().height * ch;
    let framebuffer = render(&mut c);

    // The middle of the pane is empty, so it shows the terminal's own
    // background; the far right of the status bar is past every label.
    assert_eq!(framebuffer.pixel(SIZE.0 / 2, SIZE.1 / 2), 0x123456);
    assert_eq!(framebuffer.pixel(SIZE.0 - 1, bar + ch / 2), 0x654321);
}

#[test]
fn the_cheat_sheet_is_the_running_keymap() {
    // The sheet the compositor shows and the sheet the keymap describes are
    // the same list, because there is only one of them. Nothing here is a
    // second copy that could fall behind.
    let mut c = compositor(&["/bin/sh", "-c", "sleep 5"]);
    assert!(c.perform(Action::ShowBindings));

    let overlay = c.overlay().expect("the sheet should be open");
    assert!(
        overlay.title().contains("leader ctrl+a"),
        "{:?}",
        overlay.title()
    );
    let expected = tos_session::cheat_sheet(&tos_session::Keymap::default_bindings());
    let shown: Vec<(&str, &str)> = overlay
        .items()
        .iter()
        .map(|item| (item.label.as_str(), item.detail.as_str()))
        .collect();
    let wanted: Vec<(&str, &str)> = expected
        .iter()
        .map(|row| (row.action.as_str(), row.keys.as_str()))
        .collect();
    assert_eq!(shown, wanted);

    // And the sheet says how to get the sheet back.
    let (_, keys) = shown
        .iter()
        .find(|(action, _)| *action == "show these bindings")
        .expect("the sheet should list itself");
    assert!(keys.contains("leader ?"), "{keys:?}");
}

#[test]
fn a_question_mark_after_the_leader_opens_the_sheet() {
    use tos_input::{InputEvent, KeyCode, KeyEvent, Modifiers};

    let mut c = compositor(&["/bin/sh", "-c", "sleep 5"]);
    let press = |c: &mut Compositor, code, modifiers| {
        c.handle_input(InputEvent::Key(KeyEvent::new(code, modifiers)));
    };
    press(&mut c, KeyCode::Char('a'), Modifiers::CTRL);
    press(&mut c, KeyCode::Char('/'), Modifiers::SHIFT);
    assert!(c.overlay().is_some(), "leader ? should open the sheet");

    // It draws, which is the part a list of long rows could break.
    render(&mut c);

    press(&mut c, KeyCode::Escape, Modifiers::NONE);
    assert!(c.overlay().is_none());
}

#[test]
fn reading_a_row_does_nothing_but_close_the_sheet() {
    use tos_input::{InputEvent, KeyCode, KeyEvent, Modifiers};

    // Enter on a launcher row starts a program; on the sheet there is nothing
    // to start, and pressing it must not leave a pane behind.
    let mut c = compositor(&["/bin/sh", "-c", "sleep 5"]);
    let before = c.session().all_panes().len();
    c.perform(Action::ShowBindings);
    c.handle_input(InputEvent::Key(KeyEvent::new(
        KeyCode::Enter,
        Modifiers::NONE,
    )));
    assert!(c.overlay().is_none());
    assert_eq!(c.session().all_panes().len(), before);
}

#[test]
fn every_row_has_room_for_its_keys() {
    // The keys are drawn to the right of the description and only when both
    // fit, so a row that is too wide loses the one thing it is there to say.
    // The overlay is at most 64 columns, which leaves 62 inside the border,
    // and it keeps a space either side of the keys.
    const INNER: usize = 62;
    for row in tos_session::cheat_sheet(&tos_session::Keymap::default_bindings()) {
        let width = row.action.chars().count() + 1 + row.keys.chars().count() + 2;
        assert!(width <= INNER, "{row:?} needs {width} columns");
    }
}

#[test]
fn a_refused_move_to_a_workspace_leaves_the_panes_the_size_they_are_drawn() {
    // A refused move used to rewrite the workspace it had failed to leave and
    // then skip its resync, because the arm only resynced when the pane went.
    // The next frame was drawn from geometry no PTY had been told about, and
    // every program in the workspace rendered at the wrong width until some
    // unrelated key happened to resync it.
    let mut c = compositor(&["/bin/sh", "-c", "sleep 5"]);
    for _ in 0..4 {
        c.perform(Action::Split(Axis::Columns));
    }

    // A workspace whose focused pane has no room left to divide has no room
    // for a pane from anywhere else either, which is the refusal.
    c.perform(Action::NewWorkspace);
    loop {
        let before = c.session().active().panes().len();
        c.perform(Action::Split(Axis::Columns));
        if c.session().active().panes().len() == before {
            break;
        }
    }
    c.perform(Action::SelectWorkspace(1));

    let before = c.session().active().panes();
    c.perform(Action::MovePaneToWorkspace(2));
    for (id, rect) in c.session().active().geometry(c.grid_area()) {
        let grid = c.pane(id).expect("pane").terminal.grid();
        assert_eq!(
            (grid.cols() as u32, grid.rows() as u32),
            (rect.width, rect.height),
            "{id:?} is drawn in {rect:?} but its terminal is {}x{}",
            grid.cols(),
            grid.rows(),
        );
    }
    assert_eq!(c.session().active().panes(), before, "the pane stayed put");
}
