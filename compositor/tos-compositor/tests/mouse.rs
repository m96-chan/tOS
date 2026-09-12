//! The mouse in the compositor's own chrome: menus that answer a click and
//! dividers that can be dragged. Real panes, real pointer events, and no
//! display anywhere.

use tos_compositor::{Compositor, Config, Overlay, OverlayItem, OverlayKind, Placement};
use tos_input::{InputEvent, Modifiers, MouseAction, MouseButton, PointerEvent};
use tos_session::{Action, Axis, PaneId, Rect};

const SIZE: (u32, u32) = (800, 480);

/// A pane that will sit still, at a cell size a test can work in: nothing it
/// prints can move a divider or close a menu under the pointer.
fn quiet() -> Compositor {
    let config = Config {
        command: Some(vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()]),
        bitmap_scale: Some(2),
        // The built-in face, so the cell size does not depend on the host's
        // fonts and a test can turn cells into pixels itself.
        font: Some("/nonexistent".into()),
        ..Config::default()
    };
    Compositor::new(config, SIZE, None).expect("compositor")
}

fn pointer(c: &mut Compositor, cell: (u32, u32), button: Option<MouseButton>, action: MouseAction) {
    let (cw, ch) = c.cell_size();
    c.handle_input(InputEvent::Pointer(PointerEvent {
        button,
        action,
        x: (cell.0 * cw) as f64,
        y: (cell.1 * ch) as f64,
        modifiers: Modifiers::NONE,
    }));
}

fn click(c: &mut Compositor, cell: (u32, u32)) {
    pointer(c, cell, Some(MouseButton::Left), MouseAction::Press);
    pointer(c, cell, Some(MouseButton::Left), MouseAction::Release);
}

/// Press, drag and release, the way a hand does it.
fn drag(c: &mut Compositor, from: (u32, u32), to: (u32, u32)) {
    pointer(c, from, Some(MouseButton::Left), MouseAction::Press);
    pointer(c, to, Some(MouseButton::Left), MouseAction::Drag);
    pointer(c, to, Some(MouseButton::Left), MouseAction::Release);
}

fn wheel(c: &mut Compositor, cell: (u32, u32), button: MouseButton) {
    pointer(c, cell, Some(button), MouseAction::Press);
}

/// A menu of the test's own making, so that nothing here depends on what is
/// installed on the machine running it.
fn open_menu(c: &mut Compositor, labels: &[&str]) {
    let items = labels
        .iter()
        .map(|label| OverlayItem::new(*label))
        .collect();
    c.open_overlay(OverlayKind::Launcher, Overlay::new("run a program", items));
}

/// The cell a row of the open menu is drawn in, found by asking the placement
/// which row each cell is on rather than by working the box out again here.
fn row_cell(placement: &Placement, c: &Compositor, position: usize) -> (u32, u32) {
    let area = c.grid_area();
    (0..area.height)
        .flat_map(|row| (0..area.width).map(move |col| (col, row)))
        .find(|&(col, row)| placement.row_at(col, row) == Some(position))
        .expect("the row is on screen")
}

/// A cell inside the pane and well away from any box drawn over the middle of
/// the screen.
const CORNER: (u32, u32) = (0, 0);

fn geometry(c: &Compositor) -> Vec<(PaneId, Rect)> {
    c.session().active().geometry(c.grid_area())
}

fn rect_of(c: &Compositor, pane: PaneId) -> Rect {
    geometry(c)
        .into_iter()
        .find(|(id, _)| *id == pane)
        .map(|(_, rect)| rect)
        .expect("a pane that is laid out")
}

fn selecting_anywhere(c: &Compositor) -> bool {
    c.session()
        .active()
        .panes()
        .iter()
        .any(|id| c.pane(*id).is_some_and(|pane| pane.selection.is_some()))
}

// ---- menus --------------------------------------------------------------

#[test]
fn a_press_on_a_menu_row_runs_the_program_that_row_names() {
    let mut c = quiet();
    open_menu(&mut c, &["sh"]);
    let placement = c.overlay_placement().expect("the menu is on screen");
    let at = row_cell(&placement, &c, 0);

    click(&mut c, at);
    assert!(c.overlay().is_none(), "choosing a row closes the menu");
    assert_eq!(c.session().active().panes().len(), 2);
    let focus = c.session().focus();
    assert_eq!(c.pane(focus).unwrap().title, "sh");
}

#[test]
fn the_pointer_highlights_the_row_it_is_over_before_anything_is_pressed() {
    let mut c = quiet();
    open_menu(&mut c, &["ls", "vim", "cat"]);
    let placement = c.overlay_placement().unwrap();
    let at = row_cell(&placement, &c, 2);

    pointer(&mut c, at, None, MouseAction::Motion);
    let overlay = c.overlay().expect("still open");
    assert_eq!(overlay.selected_item().unwrap().label, "cat");
}

#[test]
fn a_press_outside_the_menu_dismisses_it_without_reaching_the_pane() {
    let mut c = quiet();
    let focus = c.session().focus();
    open_menu(&mut c, &["ls", "vim"]);

    click(&mut c, CORNER);
    assert!(c.overlay().is_none(), "the menu should have gone");
    assert_eq!(c.session().active().panes().len(), 1, "nothing was chosen");
    assert!(c.pane(focus).unwrap().selection.is_none());
}

#[test]
fn a_drag_over_a_pane_with_a_menu_open_selects_nothing_underneath() {
    let mut c = quiet();
    open_menu(&mut c, &["ls", "vim"]);

    drag(&mut c, CORNER, (6, 0));
    assert!(
        !selecting_anywhere(&c),
        "the pane under the menu started a selection"
    );
    assert!(c.clipboard('p').is_none(), "and copied it to primary");
    let focus = c.session().focus();
    assert!(!c.pane(focus).unwrap().selection_in_progress);
}

#[test]
fn the_wheel_scrolls_the_menu_and_the_row_under_the_pointer_still_chooses() {
    let names: Vec<String> = (0..40).map(|i| format!("program-{i}")).collect();
    let labels: Vec<&str> = names.iter().map(String::as_str).collect();
    let mut c = quiet();
    open_menu(&mut c, &labels);
    let placement = c.overlay_placement().unwrap();
    // A cell that does not move: what it names is what changes under it.
    let at = row_cell(&placement, &c, 1);

    wheel(&mut c, at, MouseButton::WheelDown);
    let scrolled = c.overlay().unwrap().scroll();
    assert!(scrolled > 0, "a list of forty should scroll");

    // None of these names is on $PATH, which is what makes them answerable:
    // the launcher says which name it was handed, and that name is the row
    // that ended up under a pointer which never moved.
    click(&mut c, at);
    assert!(c.overlay().is_none());
    assert_eq!(
        c.notifications().status_line(),
        Some(format!("not found: program-{}", scrolled + 1))
    );
}

// ---- dividers -----------------------------------------------------------

/// Two panes side by side, and the cell in the middle of the gap between
/// them.
fn split_columns(c: &mut Compositor) -> ((u32, u32), Axis) {
    assert!(c.perform(Action::Split(Axis::Columns)));
    let area = c.grid_area();
    let divider = c
        .session()
        .active()
        .layout
        .placed_dividers(area)
        .into_iter()
        .next()
        .expect("a gap between the two panes");
    (
        (divider.rect.x, divider.rect.y + divider.rect.height / 2),
        divider.axis,
    )
}

#[test]
fn dragging_a_divider_resizes_the_panes_either_side_of_it() {
    let mut c = quiet();
    let (at, axis) = split_columns(&mut c);
    assert_eq!(axis, Axis::Columns, "a vertical line between two columns");
    let panes = c.session().active().panes();
    let (left, right) = (rect_of(&c, panes[0]), rect_of(&c, panes[1]));

    drag(&mut c, at, (at.0 + 6, at.1));
    assert_eq!(rect_of(&c, panes[0]).width, left.width + 6);
    assert_eq!(rect_of(&c, panes[1]).width, right.width - 6);
    // And the terminals were told, rather than only the tree.
    assert_eq!(
        c.pane(panes[0]).unwrap().terminal.grid().cols() as u32,
        left.width + 6
    );
}

#[test]
fn a_divider_drag_leaves_no_selection_behind_it() {
    let mut c = quiet();
    let (at, _) = split_columns(&mut c);
    // Right across the pane to the right of it, which is where a selection
    // would have been dragged out.
    drag(&mut c, at, (at.0 + 10, at.1));
    assert!(!selecting_anywhere(&c), "a divider drag selected text");
    assert!(c.clipboard('p').is_none());
}

#[test]
fn a_divider_dragged_past_the_smallest_a_pane_can_be_refuses_to_move() {
    let mut c = quiet();
    let (at, _) = split_columns(&mut c);
    let before = geometry(&c);

    let edge = c.grid_area().width - 1;
    drag(&mut c, at, (edge, at.1));
    assert_eq!(geometry(&c), before, "a pane was squeezed out of existence");
}

#[test]
fn a_divider_cannot_be_grabbed_while_a_pane_is_zoomed() {
    let mut c = quiet();
    let (at, _) = split_columns(&mut c);
    let before = geometry(&c);
    assert!(c.perform(Action::ToggleZoom));

    drag(&mut c, at, (at.0 + 6, at.1));
    assert!(c.perform(Action::ToggleZoom));
    assert_eq!(
        geometry(&c),
        before,
        "a divider that is not drawn was dragged anyway"
    );
}

#[test]
fn a_press_in_a_pane_still_selects_once_the_divider_has_been_let_go() {
    let mut c = quiet();
    let (at, _) = split_columns(&mut c);
    drag(&mut c, at, (at.0 + 4, at.1));

    let focus = c.session().focus();
    let rect = rect_of(&c, focus);
    drag(&mut c, (rect.x, rect.y), (rect.x + 4, rect.y));
    assert!(
        c.pane(focus).unwrap().selection.is_some(),
        "the divider drag held on to the mouse"
    );
}
