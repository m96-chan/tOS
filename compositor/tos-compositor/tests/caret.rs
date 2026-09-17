//! The caret, drawn: that it blinks on its own clock, and that typing holds
//! it solid (#169).
//!
//! These are pixels rather than state, because the bug they are about was
//! never in the state: `blink_visible` flipped on time and the block stayed on
//! screen anyway, since nothing marked the row it was drawn on as needing
//! another look.

use std::time::{Duration, Instant};

use tos_compositor::{Compositor, Config};
use tos_input::{InputEvent, KeyCode, KeyEvent, Modifiers};
use tos_render::OwnedFramebuffer;

const SIZE: (u32, u32) = (800, 480);
/// Long enough for the phase to have flipped, whatever the interval is.
const PAST_A_PHASE: Duration = Duration::from_millis(600);

fn compositor() -> Compositor {
    Compositor::new(
        Config {
            command: Some(vec!["/bin/sh".into(), "-c".into(), "cat".into()]),
            bitmap_scale: Some(2),
            font: Some("/nonexistent".into()),
            inactive_fade: 0,
            ..Config::default()
        },
        SIZE,
        None,
    )
    .expect("compositor")
}

/// The columns of `row` that hold the caret's colour.
fn caret_cells(fb: &OwnedFramebuffer, c: &Compositor, row: u32) -> Vec<u32> {
    let cursor = c
        .pane(c.session().focus())
        .expect("a pane")
        .terminal
        .palette()
        .cursor
        .pack();
    let (cw, ch) = c.cell_size();
    (0..SIZE.0 / cw)
        .filter(|col| {
            (0..ch)
                .flat_map(|dy| (0..cw).map(move |dx| (dx, dy)))
                .filter(|(dx, dy)| fb.pixel(col * cw + dx, row * ch + dy) == cursor)
                .count()
                > 100
        })
        .collect()
}

fn frame(c: &mut Compositor, fb: &mut OwnedFramebuffer) {
    let mut surface = fb.surface();
    c.render_frame(&mut surface, true);
}

/// Run the loop's own clock for `how_long`, drawing whenever it would.
fn run(c: &mut Compositor, fb: &mut OwnedFramebuffer, how_long: Duration) {
    let until = Instant::now() + how_long;
    while Instant::now() < until {
        c.pump_panes();
        if c.tick() || c.needs_render() {
            frame(c, fb);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn the_caret_goes_out_on_its_own_clock() {
    // What the report was: the caret stayed lit through every frame drawn
    // over four intervals, because a phase change marked no row dirty, and it
    // then changed state on the first keystroke instead.
    let mut c = compositor();
    let mut fb = OwnedFramebuffer::new(SIZE.0, SIZE.1);
    run(&mut c, &mut fb, Duration::from_millis(300));
    let row = c
        .pane(c.session().focus())
        .expect("a pane")
        .terminal
        .cursor()
        .y as u32;
    frame(&mut c, &mut fb);
    assert!(
        !caret_cells(&fb, &c, row).is_empty(),
        "the caret should be lit to start with"
    );

    let mut went_out = false;
    let until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < until && !went_out {
        run(&mut c, &mut fb, Duration::from_millis(100));
        went_out = caret_cells(&fb, &c, row).is_empty();
    }
    assert!(
        went_out,
        "the caret never went out: it is painted every frame and erased by \
         nothing, so a phase change has to repaint the row under it"
    );
}

#[test]
fn typing_holds_the_caret_solid() {
    // And the other half: a keystroke that landed in the phase's off half used
    // to draw the character with no caret at all, which is the caret appearing
    // to jump a beat after the letter it was sitting on.
    let mut c = compositor();
    let mut fb = OwnedFramebuffer::new(SIZE.0, SIZE.1);
    run(&mut c, &mut fb, Duration::from_millis(300));
    let focus = c.session().focus();
    let row = c.pane(focus).expect("a pane").terminal.cursor().y as u32;

    // Wait out a phase so the keystroke lands on the far side of one, which is
    // the half the caret used to disappear in.
    std::thread::sleep(PAST_A_PHASE);
    c.tick();
    frame(&mut c, &mut fb);

    c.handle_input(InputEvent::Key(KeyEvent::new(
        KeyCode::Char('a'),
        Modifiers::NONE,
    )));
    let until = Instant::now() + Duration::from_secs(5);
    let mut echoed = false;
    while Instant::now() < until && !echoed {
        c.pump_panes();
        frame(&mut c, &mut fb);
        echoed = c
            .pane(focus)
            .unwrap()
            .terminal
            .grid()
            .display_row(row as usize)
            .to_text()
            .contains('a');
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(echoed, "the pane never echoed the keystroke");

    let cursor_x = c.pane(focus).unwrap().terminal.cursor().x as u32;
    assert_eq!(
        caret_cells(&fb, &c, row),
        vec![cursor_x],
        "the caret should be lit, and in the cell the cursor is in"
    );
}
