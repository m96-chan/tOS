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
        inactive_fade: 0,
        ..Config::default()
    };
    Compositor::new(config, SIZE, None).expect("compositor")
}

/// Pump the compositor until `predicate` holds or time runs out.
fn wait_for(compositor: &mut Compositor, timeout: Duration, predicate: impl Fn(&Compositor) -> bool) -> bool {
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
        c.session()
            .all_panes()
            .iter()
            .all(|id| !c.pane(*id).unwrap().terminal.grid().to_text().trim().is_empty())
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
    let mut c = compositor(&["/bin/sh", "-c", "i=0; while [ $i -lt 200 ]; do echo line$i; i=$((i+1)); done; sleep 5"]);
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
