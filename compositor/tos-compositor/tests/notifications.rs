//! Notifications on screen: that one appears, and — the harder half — that the
//! screen goes back to what it was when its time is up.
//!
//! A notification with nowhere on the status bar to live is drawn as a banner
//! over the top row of the panes, and nothing in a pane's grid knows it is
//! there. So the cells underneath are never damaged by its leaving, and the
//! frame after it retires differs from the frame before only if the compositor
//! asked for a full redraw. That ask is what these tests are about, and the
//! only way to see it is in pixels: the queue empties either way, and a banner
//! nobody erased is still on screen with an empty queue behind it.

use std::time::{Duration, Instant};

use tos_compositor::status::Settings;
use tos_compositor::{Compositor, Config, Segment};
use tos_render::OwnedFramebuffer;

const SIZE: (u32, u32) = (800, 480);

/// A pane that will sit still, so that nothing it prints repaints the row the
/// banner is drawn over — which would erase the banner for the wrong reason
/// and let the bug through.
///
/// The cursor is hidden for the same reason: it blinks on its own clock, it
/// sits on row 0 of a pane with nothing in it, and three seconds of waiting is
/// long enough for it to flip.
fn quiet(status_bar: bool, status: Settings) -> Compositor {
    let config = Config {
        command: Some(
            ["/bin/sh", "-c", "sleep 30"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        ),
        bitmap_scale: Some(2),
        // The built-in face, so the cell size does not depend on the host's
        // fonts, and a root with no machine under it so the bar says nothing
        // about the battery or the network of whoever is running the tests.
        font: Some("/nonexistent".into()),
        system_root: "/nonexistent-so-this-machine-has-no-hardware".into(),
        inactive_fade: 0,
        status_bar,
        status,
        ..Config::default()
    };
    let mut compositor = Compositor::new(config, SIZE, None).expect("compositor");
    let focus = compositor.session().focus();
    compositor
        .pane_mut(focus)
        .expect("the first pane")
        .terminal
        .advance(b"\x1b[?25l");
    compositor
}

/// The retained framebuffer, kept across frames the way a real backend's is.
///
/// It has to be the same buffer every time or there is no retained path to
/// test: a fresh one would make every frame a full redraw, and the uncovering
/// these tests are about would be done by the clear.
struct Screen {
    buffer: OwnedFramebuffer,
}

impl Screen {
    fn new() -> Self {
        Screen {
            buffer: OwnedFramebuffer::new(SIZE.0, SIZE.1),
        }
    }

    /// Compose a frame and hand back a copy of the row the banner is drawn
    /// over. Only that row, because the rest of the panel has a clock on it
    /// that can turn over while a test is waiting out three seconds.
    fn top_row(&mut self, compositor: &mut Compositor) -> Vec<u32> {
        let cell_height = compositor.cell_size().1;
        {
            let mut surface = self.buffer.surface();
            compositor.render_frame(&mut surface, true);
        }
        self.buffer.pixels()[..(SIZE.0 * cell_height) as usize].to_vec()
    }
}

/// How many pixels of the top row differ between two frames. A count rather
/// than the rows themselves, because two frames that disagree are worth one
/// number in a failure and not seventeen thousand.
fn differences(before: &[u32], after: &[u32]) -> usize {
    before
        .iter()
        .zip(after)
        .filter(|(before, after)| before != after)
        .count()
}

/// Raise an application notification in the focused pane, the way OSC 9 does.
fn notify(compositor: &mut Compositor) {
    let focus = compositor.session().focus();
    compositor
        .pane_mut(focus)
        .expect("the focused pane")
        .terminal
        .advance(b"\x1b]9;build finished\x07");
    compositor.pump_panes();
    assert!(
        compositor.notifications().status_line().is_some(),
        "the notification was never raised"
    );
}

/// Tick until the notification has served its time, the way the frame loop
/// does. Real time rather than an instant the test names, because the tick
/// that takes one belongs to the compositor and only its own tests can call
/// it.
fn wait_out_the_notification(compositor: &mut Compositor) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        compositor.tick();
        if compositor.notifications().status_line().is_none() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("the notification never expired");
}

/// The shape of both tests below: the top row before the banner, with the
/// banner on it, and after the banner's time is up.
fn the_top_row_before_during_and_after(compositor: &mut Compositor) {
    let mut screen = Screen::new();
    let before = screen.top_row(compositor);
    notify(compositor);
    let banner = screen.top_row(compositor);
    assert!(
        differences(&before, &banner) > 0,
        "the banner was never drawn, so there is nothing here to erase"
    );

    wait_out_the_notification(compositor);
    let after = screen.top_row(compositor);
    assert!(
        differences(&banner, &after) > 0,
        "the expired banner is still on screen: the frame after it retired is \
         pixel-identical to the frame it was drawn in"
    );
    assert_eq!(
        differences(&before, &after),
        0,
        "the row the banner covered did not come back as it was"
    );
}

#[test]
fn an_expired_banner_is_erased_when_the_bar_has_no_message_segment() {
    // A bar configured by hand replaces the default segment list wholesale, so
    // one that does not name `message` has nowhere to show a notification and
    // is given the banner instead. The redraw that erases the banner used to
    // ask only whether there was a bar at all — which there is here — so the
    // banner was drawn over pane row 0 and then never taken down.
    let mut compositor = quiet(
        true,
        Settings {
            left: Vec::new(),
            right: vec![Segment::Network, Segment::Battery, Segment::Clock],
            ..Settings::default()
        },
    );
    the_top_row_before_during_and_after(&mut compositor);
}

#[test]
fn an_expired_banner_is_erased_when_there_is_no_bar_at_all() {
    // `--no-status-bar` is the case the uncovering was written for, and the
    // one that has to go on working now both places ask the same question.
    let mut compositor = quiet(false, Settings::default());
    the_top_row_before_during_and_after(&mut compositor);
}
