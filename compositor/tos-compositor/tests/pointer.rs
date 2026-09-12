//! The pointer on screen: that it is there, that it is where the hand put it,
//! that it leaves nothing behind, and that moving it does not cost the panel.
//!
//! Every assertion here is about pixels of a composed frame, because a pointer
//! that is not visible is the whole of the bug this exists for. The frames are
//! compared against each other rather than against named colours: what the
//! arrow is painted in comes from the theme, and the questions worth asking —
//! did something appear here, did it stop being there — are answered by the
//! difference between two frames and do not need to know.

use tos_compositor::pointer;
use tos_compositor::{Compositor, Config};
use tos_input::{InputEvent, KeyCode, KeyEvent, Modifiers, MouseAction, PointerEvent};
use tos_render::{OwnedFramebuffer, Rect};
use tos_session::{Action, Axis};

const SIZE: (u32, u32) = (800, 480);

/// A pane that will sit still: nothing it prints can repaint the cells a test
/// is watching for an arrow.
///
/// Twice the built-in face, which puts the cell at 22 pixels tall and the arrow
/// at one display pixel per pixel of its mask.
fn quiet() -> Compositor {
    quiet_at(2)
}

/// The same pane on a panel whose cells are big enough that the arrow is drawn
/// scaled up, which is every HiDPI machine and neither of the sizes the rest of
/// this file uses.
fn quiet_at(bitmap_scale: u32) -> Compositor {
    let config = Config {
        command: Some(
            ["/bin/sh", "-c", "sleep 30"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        ),
        bitmap_scale: Some(bitmap_scale),
        // The built-in face, so the cell size does not depend on the host's
        // fonts and a test can turn cells into pixels itself.
        font: Some("/nonexistent".into()),
        ..Config::default()
    };
    Compositor::new(config, SIZE, None).expect("compositor")
}

/// The retained framebuffer, kept across frames the way a real backend's is.
///
/// It has to be the same buffer every time or there is no retained path to
/// test: a fresh one would make every frame a full redraw and the uncovering
/// this file is about would be done by the clear.
struct Screen {
    buffer: OwnedFramebuffer,
}

impl Screen {
    fn new() -> Self {
        Screen {
            buffer: OwnedFramebuffer::new(SIZE.0, SIZE.1),
        }
    }

    /// Compose a frame and hand back a copy of it.
    fn frame(&mut self, compositor: &mut Compositor) -> Vec<u32> {
        let mut surface = self.buffer.surface();
        compositor.render_frame(&mut surface, true);
        self.buffer.pixels().to_vec()
    }

    /// Write a colour nothing should touch, to catch a frame that repainted
    /// more of the panel than it had any reason to.
    fn poke(&mut self, x: u32, y: u32, color: u32) {
        self.buffer.surface().put(x as i32, y as i32, color);
    }

    fn pixel(&self, x: u32, y: u32) -> u32 {
        self.buffer.pixel(x, y)
    }
}

fn move_to(compositor: &mut Compositor, x: u32, y: u32) -> bool {
    compositor.handle_input(InputEvent::Pointer(PointerEvent {
        button: None,
        action: MouseAction::Motion,
        x: x as f64,
        y: y as f64,
        modifiers: Modifiers::NONE,
    }))
}

fn type_a_letter(compositor: &mut Compositor) -> bool {
    compositor.handle_input(InputEvent::Key(KeyEvent::new(
        KeyCode::Char('x'),
        Modifiers::NONE,
    )))
}

/// Where the arrow lands when its hotspot is at `(x, y)`.
fn arrow(compositor: &Compositor, x: u32, y: u32) -> Rect {
    let (width, height) = pointer::size(compositor.cell_size());
    Rect::new(x as i32, y as i32, width, height)
}

/// How many pixels of `rect` differ between two frames.
fn differences(before: &[u32], after: &[u32], rect: Rect) -> usize {
    let mut count = 0;
    for y in rect.y..rect.bottom() {
        for x in rect.x..rect.right() {
            let index = (y as u32 * SIZE.0 + x as u32) as usize;
            if before[index] != after[index] {
                count += 1;
            }
        }
    }
    count
}

#[test]
fn the_pointer_is_drawn_at_the_pixel_it_was_moved_to() {
    let mut compositor = quiet();
    let mut screen = Screen::new();
    let empty = screen.frame(&mut compositor);

    assert!(
        move_to(&mut compositor, 200, 100),
        "a motion asked for no frame"
    );
    let shown = screen.frame(&mut compositor);

    let at = arrow(&compositor, 200, 100);
    assert!(
        differences(&empty, &shown, at) > 20,
        "nothing was drawn where the pointer was moved to"
    );
    // And only there. A few pixels wide is the point of an arrow: a pointer
    // that painted the cell it is in would be pointing at a word.
    let whole = Rect::new(0, 0, SIZE.0, SIZE.1);
    assert_eq!(
        differences(&empty, &shown, whole),
        differences(&empty, &shown, at),
        "the frame changed outside the arrow"
    );
}

#[test]
fn an_arrow_against_the_bottom_edge_is_cut_off_rather_than_drawn_at_a_smaller_scale() {
    // How big the arrow is drawn is the cell's business: it has to keep its
    // proportion to the text it is pointing at. The rectangle the frame hands
    // the drawing is clipped to the panel first, so taking the size from that
    // instead built a whole, unclipped arrow a third of the size in the band
    // along the bottom edge where the clip bites — a pointer that shrank as the
    // hand approached the edge and snapped back when it left.
    let mut compositor = quiet_at(4);
    let mut screen = Screen::new();
    let (width, height) = pointer::size(compositor.cell_size());
    assert!(
        height > 17,
        "these cells draw the arrow at one pixel per pixel, which is the one \
         size at which a scale taken from the wrong number still comes out right"
    );

    let empty = screen.frame(&mut compositor);
    // Far enough down that most of the arrow hangs off the bottom, and nowhere
    // near the right edge, which never clipped it because the width was never
    // read.
    let (x, y) = (SIZE.0 / 2, SIZE.1 - height / 2);
    move_to(&mut compositor, x, y);
    let shown = screen.frame(&mut compositor);

    // The bottom right corner of where the arrow belongs. An arrow rebuilt at a
    // third of its size fits entirely above and to the left of this, so one
    // changed pixel inside it is the whole assertion.
    let corner = Rect::new(
        (x + width / 2) as i32,
        (y + height / 3) as i32,
        width / 2,
        SIZE.1 - y - height / 3,
    );
    assert!(
        differences(&empty, &shown, corner) > 0,
        "the arrow was rebuilt smaller instead of being cut off by the edge"
    );
}

#[test]
fn a_compositor_that_has_never_seen_a_pointer_event_draws_no_pointer() {
    // The position starts at the top left corner, so "there is no pointer" and
    // "the pointer has not moved yet" would look the same anywhere else. This
    // watches the corner it would be in.
    let mut compositor = quiet();
    let mut screen = Screen::new();
    let first = screen.frame(&mut compositor);
    let second = screen.frame(&mut compositor);
    let corner = arrow(&compositor, 0, 0);
    assert_eq!(
        differences(&first, &second, corner),
        0,
        "something appeared in the corner without a pointing device"
    );

    // One event is all it takes, and it is the event rather than the movement:
    // this one puts the pointer exactly where it already was.
    assert!(move_to(&mut compositor, 0, 0));
    let shown = screen.frame(&mut compositor);
    assert!(
        differences(&second, &shown, corner) > 20,
        "the pointer did not appear once a device was heard from"
    );
}

#[test]
fn moving_the_pointer_uncovers_what_it_was_over() {
    let mut compositor = quiet();
    let mut screen = Screen::new();
    let empty = screen.frame(&mut compositor);

    move_to(&mut compositor, 200, 100);
    screen.frame(&mut compositor);
    move_to(&mut compositor, 400, 300);
    let moved = screen.frame(&mut compositor);

    let was = arrow(&compositor, 200, 100);
    assert_eq!(
        differences(&empty, &moved, was),
        0,
        "the pointer left a smear behind it"
    );
    let now = arrow(&compositor, 400, 300);
    assert!(
        differences(&empty, &moved, now) > 20,
        "the pointer did not arrive where it was moved to"
    );
}

#[test]
fn a_motion_inside_one_pane_repaints_that_pane_and_not_the_panel() {
    // The core of it: a visible pointer means every twitch of the mouse owes a
    // frame, and a frame that repainted the whole screen would make an idle
    // hand cost more than a program printing.
    let mut compositor = quiet();
    let mut screen = Screen::new();
    screen.frame(&mut compositor);

    // A row of the pane the arrow will never be on. A full redraw clears the
    // surface, and a pane repaints only the rows something marked, so this
    // colour surviving is the assertion.
    let (_, cell_height) = compositor.cell_size();
    let witness = cell_height * 3 + 1;
    screen.poke(700, witness, 0x00ff00);

    move_to(&mut compositor, 100, 200);
    screen.frame(&mut compositor);
    assert_eq!(
        screen.pixel(700, witness),
        0x00ff00,
        "a bare motion repainted the whole panel"
    );

    move_to(&mut compositor, 140, 220);
    screen.frame(&mut compositor);
    assert_eq!(
        screen.pixel(700, witness),
        0x00ff00,
        "moving the pointer again repainted the whole panel"
    );
}

#[test]
fn a_pointer_that_has_stopped_moving_stops_asking_for_frames() {
    // The other half of a motion producing a frame. `needs_render` is asked on
    // every pass of the loop, and a pointer that went on answering yes would
    // hold the loop awake and the panes repainting for as long as a hand was
    // resting on the mouse.
    let mut compositor = quiet();
    let mut screen = Screen::new();
    screen.frame(&mut compositor);
    assert!(!compositor.needs_render());

    assert!(move_to(&mut compositor, 120, 90));
    assert!(compositor.needs_render(), "a motion asked for no frame");
    screen.frame(&mut compositor);
    assert!(
        !compositor.needs_render(),
        "a pointer standing still asks for a frame on every pass"
    );
}

#[test]
fn a_pointer_over_a_divider_is_uncovered_without_repainting_the_panel() {
    // Dividers are drawn only on a full redraw, so the obvious way to uncover
    // an arrow that was sitting on one is to ask for the panel back. Dragging a
    // divider is a gesture that sits on one for the length of the drag, which
    // is exactly when that would be worst.
    let mut compositor = quiet();
    compositor.perform(Action::Split(Axis::Columns));
    let mut screen = Screen::new();
    let empty = screen.frame(&mut compositor);

    let geometry = compositor
        .session()
        .active()
        .geometry(compositor.grid_area());
    assert_eq!(geometry.len(), 2, "the split did not happen");
    let (cell_width, cell_height) = compositor.cell_size();
    // The column between the two panes, whichever way round the layout put
    // them.
    let left = geometry
        .iter()
        .map(|(_, r)| r.right())
        .min()
        .expect("a pane");
    let on_divider = (left * cell_width, cell_height * 4);

    let witness = cell_height * 9 + 1;
    screen.poke(20, witness, 0x00ff00);
    move_to(&mut compositor, on_divider.0, on_divider.1);
    screen.frame(&mut compositor);
    assert_eq!(
        screen.pixel(20, witness),
        0x00ff00,
        "landing on a divider repainted the whole panel"
    );

    move_to(&mut compositor, 40, cell_height * 4);
    let moved = screen.frame(&mut compositor);
    assert_eq!(
        differences(
            &empty,
            &moved,
            arrow(&compositor, on_divider.0, on_divider.1)
        ),
        0,
        "the divider was left with an arrow painted over it"
    );
}

#[test]
fn the_pointer_is_drawn_over_an_open_overlay() {
    let mut compositor = quiet();
    compositor.perform(Action::ShowBindings);
    let mut screen = Screen::new();
    let menu = screen.frame(&mut compositor);

    // The middle of the screen, which is inside the box: a menu that swallowed
    // the arrow would be a menu nobody can point at, and pointing at one is the
    // entire reason a menu has rows.
    let (x, y) = (SIZE.0 / 2, SIZE.1 / 2);
    move_to(&mut compositor, x, y);
    let shown = screen.frame(&mut compositor);
    assert!(
        differences(&menu, &shown, arrow(&compositor, x, y)) > 20,
        "the overlay was painted over the pointer"
    );
}

#[test]
fn typing_puts_the_pointer_away_and_the_next_motion_brings_it_back() {
    let mut compositor = quiet();
    let mut screen = Screen::new();
    let empty = screen.frame(&mut compositor);

    move_to(&mut compositor, 300, 200);
    let shown = screen.frame(&mut compositor);
    let at = arrow(&compositor, 300, 200);
    assert!(differences(&empty, &shown, at) > 20);

    assert!(
        type_a_letter(&mut compositor),
        "the keystroke that hides the pointer asked for no frame"
    );
    let typed = screen.frame(&mut compositor);
    assert_eq!(
        differences(&empty, &typed, at),
        0,
        "the pointer stayed on screen while somebody typed"
    );
    // And nothing else has to happen for it to stay away.
    type_a_letter(&mut compositor);
    let again = screen.frame(&mut compositor);
    assert_eq!(differences(&empty, &again, at), 0);

    assert!(move_to(&mut compositor, 300, 200));
    let back = screen.frame(&mut compositor);
    assert_eq!(
        differences(&shown, &back, at),
        0,
        "the pointer came back in a different shape, or not at all"
    );
}

#[test]
fn a_pointer_at_the_far_corner_hangs_off_the_edge_rather_than_being_pushed_back() {
    // The hotspot is the tip, so a pointer at the last pixel of the panel has
    // almost all of its arrow off screen. Clamping the whole arrow on would
    // stop the tip tracking the hand, which is the one thing it is for.
    let mut compositor = quiet();
    let mut screen = Screen::new();
    let empty = screen.frame(&mut compositor);

    let (x, y) = (SIZE.0 - 1, SIZE.1 - 1);
    move_to(&mut compositor, x, y);
    let shown = screen.frame(&mut compositor);
    assert_eq!(
        shown[(y * SIZE.0 + x) as usize],
        empty[(y * SIZE.0 + x) as usize],
        "the corner pixel is the tip's outline, which is the background colour"
    );

    // And moving away from it leaves nothing behind, which is the case that
    // reaches the strip at the edges where whole cells do not divide the panel.
    move_to(&mut compositor, 300, 300);
    let moved = screen.frame(&mut compositor);
    assert_eq!(
        differences(&empty, &moved, Rect::new(x as i32 - 2, y as i32 - 2, 3, 3)),
        0,
        "the corner kept a piece of the arrow"
    );
}
