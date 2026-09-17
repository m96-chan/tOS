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

/// Run it until `lit` describes the caret, and say whether it ever did.
///
/// Waiting for the state rather than for a length of time: how much of the
/// first phase `Compositor::new` spends is the machine's business, and a test
/// that assumed the rest of it would start in the half it wanted would stop
/// testing what it says it tests the day the machine got slower.
fn run_until(c: &mut Compositor, fb: &mut OwnedFramebuffer, row: u32, lit: bool) -> bool {
    let until = Instant::now() + Duration::from_secs(10);
    while Instant::now() < until {
        run(c, fb, Duration::from_millis(60));
        frame(c, fb);
        if caret_cells(fb, c, row).is_empty() != lit {
            return true;
        }
    }
    false
}

#[test]
fn the_caret_goes_out_on_its_own_clock() {
    // What the report was: the caret stayed lit through every frame drawn
    // over four intervals, because a phase change marked no row dirty, and it
    // changed state on the first keystroke instead.
    //
    // Counted rather than waited for. Something else repainting that row —
    // a notification retiring, anything that asks for a full frame — moves
    // the caret too, so "it changed at least once in ten seconds" is a test
    // that passes on the bug. What only the phase can do is change it every
    // interval, so that is what is asserted.
    let mut c = compositor();
    let mut fb = OwnedFramebuffer::new(SIZE.0, SIZE.1);
    run(&mut c, &mut fb, Duration::from_millis(200));
    let row = c
        .pane(c.session().focus())
        .expect("a pane")
        .terminal
        .cursor()
        .y as u32;
    frame(&mut c, &mut fb);

    let mut lit = !caret_cells(&fb, &c, row).is_empty();
    let mut changes = 0;
    let until = Instant::now() + Duration::from_millis(3200);
    while Instant::now() < until {
        run(&mut c, &mut fb, Duration::from_millis(40));
        frame(&mut c, &mut fb);
        let now_lit = !caret_cells(&fb, &c, row).is_empty();
        if now_lit != lit {
            lit = now_lit;
            changes += 1;
        }
    }
    // Six intervals of room, three changes asked for.
    assert!(
        changes >= 3,
        "the caret changed {changes} times in three seconds: it is painted \
         every frame and erased by nothing, so a phase change has to repaint \
         the row under it"
    );
}

#[test]
fn typing_holds_the_caret_solid() {
    // And the other half: a keystroke that lands in the phase's off half used
    // to draw the character with no caret at all, which is the caret appearing
    // to sit on the letter and then jump a beat later.
    //
    // The assertion is on the frame that first shows the echoed character and
    // not on whatever is on screen once the echo has been waited for: the
    // phase comes back up on its own within an interval, so a test that looked
    // afterwards would be asking whether the caret exists rather than whether
    // typing brought it back.
    let mut c = compositor();
    let mut fb = OwnedFramebuffer::new(SIZE.0, SIZE.1);
    run(&mut c, &mut fb, Duration::from_millis(200));
    let focus = c.session().focus();
    let row = c.pane(focus).expect("a pane").terminal.cursor().y as u32;

    // Typed into the half the caret is down in, which is the half it used to
    // disappear in — waited for rather than slept towards, so that this is
    // still the case it says it is on a machine that took longer to start.
    assert!(
        run_until(&mut c, &mut fb, row, false),
        "the caret never went out to type into"
    );

    c.handle_input(InputEvent::Key(KeyEvent::new(
        KeyCode::Char('a'),
        Modifiers::NONE,
    )));
    let until = Instant::now() + Duration::from_secs(5);
    let mut echoed = false;
    while Instant::now() < until && !echoed {
        c.pump_panes();
        echoed = c
            .pane(focus)
            .unwrap()
            .terminal
            .grid()
            .display_row(row as usize)
            .to_text()
            .contains('a');
        if !echoed {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    assert!(echoed, "the pane never echoed the keystroke");
    // The first frame after it arrived, and no other.
    frame(&mut c, &mut fb);

    let cursor_x = c.pane(focus).unwrap().terminal.cursor().x as u32;
    assert_eq!(
        caret_cells(&fb, &c, row),
        vec![cursor_x],
        "the caret should be lit, and in the cell the cursor is in"
    );
}

/// How much ink is on `row`, counting anything that is not the background.
fn row_ink(fb: &OwnedFramebuffer, c: &Compositor, row: u32) -> usize {
    let background = c
        .pane(c.session().focus())
        .expect("a pane")
        .terminal
        .palette()
        .background
        .pack();
    let (_, ch) = c.cell_size();
    (0..ch)
        .flat_map(|dy| (0..SIZE.0).map(move |x| (x, dy)))
        .filter(|(x, dy)| fb.pixel(*x, row * ch + dy) != background)
        .count()
}

#[test]
fn text_the_phase_hides_is_repainted_too() {
    // SGR 5, the same bug from the other side: `render` leaves a cell with
    // `BLINK` blank while the phase is down, and nothing marked those rows
    // either, so blinking text was frozen in whichever state its last repaint
    // caught it in.
    let mut c = compositor();
    let mut fb = OwnedFramebuffer::new(SIZE.0, SIZE.1);
    run(&mut c, &mut fb, Duration::from_millis(200));
    let focus = c.session().focus();
    let row = c.pane(focus).expect("a pane").terminal.cursor().y as u32;
    // On its own row, with the caret moved off it, so the ink counted below
    // is the text and nothing else.
    c.inject(b"\x1b[5mblink\x1b[0m\r\n");
    frame(&mut c, &mut fb);
    let lit = row_ink(&fb, &c, row);
    assert!(lit > 0, "the text was never drawn");

    // Down and back up. Not "down to nothing": the row holds what the pane
    // draws on it whatever the phase is, and what the phase takes away is the
    // text — so the reading to make is that it moves, and moves back.
    let mut went_out = false;
    let mut came_back = false;
    let until = Instant::now() + Duration::from_secs(10);
    while Instant::now() < until && !came_back {
        run(&mut c, &mut fb, Duration::from_millis(60));
        frame(&mut c, &mut fb);
        let ink = row_ink(&fb, &c, row);
        if ink < lit {
            went_out = true;
        } else if went_out && ink == lit {
            came_back = true;
        }
    }
    assert!(
        went_out,
        "blinking text never went out: `render` blanks a BLINK cell while the \
         phase is down, so its row has to be repainted like the caret's"
    );
    assert!(came_back, "blinking text went out and stayed out");
}
