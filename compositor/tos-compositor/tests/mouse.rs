//! The mouse in the compositor's own chrome: menus that answer a click and
//! dividers that can be dragged. Real panes, real pointer events, and no
//! display anywhere.

use tos_compositor::chrome::pane_label;
use tos_compositor::status::{Piece, Settings};
use tos_compositor::{
    Bar, Compositor, Config, Hit, Overlay, OverlayItem, OverlayKind, Placement, Segment,
};
use tos_input::{InputEvent, KeyCode, KeyEvent, Modifiers, MouseAction, MouseButton, PointerEvent};
use tos_session::{Action, Axis, Direction, PaneId, Rect};

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

// ---- grabs that outlive what they were holding ---------------------------

/// Escape, which is how a menu is closed by anybody who did not want it.
fn escape(c: &mut Compositor) {
    c.handle_input(InputEvent::Key(KeyEvent::new(
        KeyCode::Escape,
        Modifiers::NONE,
    )));
}

#[test]
fn a_divider_held_while_the_workspace_changes_does_not_resize_the_one_that_arrives() {
    let mut c = quiet();
    assert!(c.perform(Action::NewWorkspace));
    let (_, _) = split_columns(&mut c);
    assert!(c.perform(Action::SelectWorkspace(1)));
    // The same two columns on both, so the id a press on this one hands over
    // names a live split on the other as well.
    let (at, _) = split_columns(&mut c);

    pointer(&mut c, at, Some(MouseButton::Left), MouseAction::Press);
    assert!(c.perform(Action::SelectWorkspace(2)));
    let before = geometry(&c);

    pointer(
        &mut c,
        (at.0 + 12, at.1),
        Some(MouseButton::Left),
        MouseAction::Drag,
    );
    assert_eq!(
        geometry(&c),
        before,
        "a grab from one workspace resized another"
    );
}

#[test]
fn a_menu_that_eats_the_release_leaves_no_divider_stuck_to_the_pointer() {
    let mut c = quiet();
    let (at, _) = split_columns(&mut c);
    pointer(&mut c, at, Some(MouseButton::Left), MouseAction::Press);
    // A binding still works while a button is held, and the menu it puts up
    // takes the release that would have ended the drag.
    open_menu(&mut c, &["ls"]);
    pointer(&mut c, at, Some(MouseButton::Left), MouseAction::Release);
    escape(&mut c);
    assert!(c.overlay().is_none(), "escape should have closed the menu");
    let before = geometry(&c);

    pointer(&mut c, (at.0 + 8, at.1), None, MouseAction::Motion);
    assert_eq!(
        geometry(&c),
        before,
        "a divider followed a pointer with no button held"
    );
}

#[test]
fn a_menu_that_eats_the_release_leaves_no_selection_following_the_pointer() {
    let mut c = quiet();
    let focus = c.session().focus();
    let rect = rect_of(&c, focus);
    pointer(
        &mut c,
        (rect.x, rect.y),
        Some(MouseButton::Left),
        MouseAction::Press,
    );
    open_menu(&mut c, &["ls"]);
    pointer(
        &mut c,
        (rect.x + 4, rect.y),
        Some(MouseButton::Left),
        MouseAction::Release,
    );
    escape(&mut c);

    let anchored = c.pane(focus).unwrap().selection;
    pointer(&mut c, (rect.x + 12, rect.y + 2), None, MouseAction::Motion);
    let pane = c.pane(focus).unwrap();
    assert_eq!(
        pane.selection, anchored,
        "a motion with no button held dragged the selection"
    );
    assert!(
        !pane.selection_in_progress,
        "the pane still believes it is being selected in"
    );
}

#[test]
fn a_wheel_notch_during_a_divider_drag_neither_drops_it_nor_moves_the_focus() {
    let mut c = quiet();
    let (at, _) = split_columns(&mut c);
    let panes = c.session().active().panes();
    let (left, right) = (rect_of(&c, panes[0]), rect_of(&c, panes[1]));
    let focus = c.session().focus();

    pointer(&mut c, at, Some(MouseButton::Left), MouseAction::Press);
    // Over the pane beside the divider, which is where the focus would go.
    wheel(&mut c, (at.0 - 4, at.1), MouseButton::WheelDown);
    pointer(
        &mut c,
        (at.0 + 6, at.1),
        Some(MouseButton::Left),
        MouseAction::Drag,
    );
    pointer(
        &mut c,
        (at.0 + 6, at.1),
        Some(MouseButton::Left),
        MouseAction::Release,
    );

    assert_eq!(
        rect_of(&c, panes[0]).width,
        left.width + 6,
        "a wheel notch aborted the drag"
    );
    assert_eq!(rect_of(&c, panes[1]).width, right.width - 6);
    assert_eq!(
        c.session().focus(),
        focus,
        "the wheel moved the focus out from under the drag"
    );
}

#[test]
fn another_button_let_go_during_a_divider_drag_neither_drops_it_nor_moves_the_focus() {
    // A hand can have more than one button down, and the one that ends a
    // divider drag is the one that started it. Letting go of any button was
    // enough to drop the grab, and the drag that was still being made with
    // the left button then went to the panes — which is nothing at all in the
    // gap the pointer is in, so the divider simply stopped following it.
    let mut c = quiet();
    let (at, _) = split_columns(&mut c);
    let panes = c.session().active().panes();
    let (left, right) = (rect_of(&c, panes[0]), rect_of(&c, panes[1]));
    let focus = c.session().focus();

    pointer(&mut c, at, Some(MouseButton::Left), MouseAction::Press);
    pointer(&mut c, at, Some(MouseButton::Right), MouseAction::Press);
    pointer(&mut c, at, Some(MouseButton::Right), MouseAction::Release);
    pointer(
        &mut c,
        (at.0 + 6, at.1),
        Some(MouseButton::Left),
        MouseAction::Drag,
    );
    pointer(
        &mut c,
        (at.0 + 6, at.1),
        Some(MouseButton::Left),
        MouseAction::Release,
    );

    assert_eq!(
        rect_of(&c, panes[0]).width,
        left.width + 6,
        "the right button aborted the drag"
    );
    assert_eq!(rect_of(&c, panes[1]).width, right.width - 6);
    assert_eq!(
        c.session().focus(),
        focus,
        "the right button moved the focus out from under the drag"
    );
}

#[test]
fn a_divider_whose_split_was_freed_does_not_come_back_as_the_split_that_took_the_slot() {
    let mut c = quiet();
    assert!(c.perform(Action::NewWorkspace));
    assert!(c.perform(Action::SelectWorkspace(1)));
    assert!(c.perform(Action::Split(Axis::Columns)));
    assert!(c.perform(Action::Split(Axis::Rows)));

    let area = c.grid_area();
    let held = c
        .session()
        .active()
        .layout
        .placed_dividers(area)
        .into_iter()
        .find(|divider| divider.axis == Axis::Rows)
        .expect("the right column is split into rows");
    let at = (held.rect.x + held.rect.width / 2, held.rect.y);
    pointer(&mut c, at, Some(MouseButton::Left), MouseAction::Press);

    // The pane under the divider leaves for another workspace, which collapses
    // the split it was half of; the next split is handed the slot back.
    assert!(c.perform(Action::MovePaneToWorkspace(2)));
    assert!(c.perform(Action::Focus(Direction::Right)));
    assert!(c.perform(Action::Split(Axis::Rows)));
    let before = geometry(&c);

    pointer(
        &mut c,
        (at.0, at.1 + 3),
        Some(MouseButton::Left),
        MouseAction::Drag,
    );
    assert_eq!(
        geometry(&c),
        before,
        "a drag resized a split that was built after the grab"
    );
}

#[test]
fn a_divider_held_while_a_pane_is_split_beside_it_does_not_become_the_next_gap_along() {
    let mut c = quiet();
    assert!(c.perform(Action::Split(Axis::Columns)));
    assert!(c.perform(Action::Split(Axis::Columns)));
    let area = c.grid_area();
    let dividers = c.session().active().layout.placed_dividers(area);
    assert_eq!(dividers.len(), 2, "three columns have two gaps");
    let held = dividers[1];
    let at = (held.rect.x, held.rect.y + held.rect.height / 2);
    pointer(&mut c, at, Some(MouseButton::Left), MouseAction::Press);

    // Two keystrokes with the button still down. The split inserts a child
    // before the one the grab names, so the position it holds moves on.
    assert!(c.perform(Action::Focus(Direction::Left)));
    assert!(c.perform(Action::Split(Axis::Columns)));
    let before = geometry(&c);

    // Back the way it came, which is the hand asking for the gap it grabbed
    // to move left and nothing else.
    pointer(
        &mut c,
        (at.0 - 6, at.1),
        Some(MouseButton::Left),
        MouseAction::Drag,
    );
    assert_eq!(
        geometry(&c),
        before,
        "the drag moved a divider nobody was holding"
    );
}

// ---- the pane list on the status bar -------------------------------------

/// A quiet pane on a bar that is nothing but the list of panes. `panes` is not
/// in the default layout, so a test that wants a pane's own label to click on
/// has to ask for it, and asking for it alone keeps the bar's left end to the
/// one segment this is about.
fn labelled_panes() -> Compositor {
    let config = Config {
        command: Some(vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()]),
        bitmap_scale: Some(2),
        font: Some("/nonexistent".into()),
        status: Settings {
            left: vec![Segment::Panes],
            right: Vec::new(),
            ..Settings::default()
        },
        ..Config::default()
    };
    Compositor::new(config, SIZE, None).expect("compositor")
}

/// The cell a pane's label is drawn in, found by laying the bar out the way
/// the compositor lays it out rather than by counting characters here. A click
/// that misses says so, since the focus it was aiming at does not move.
fn label_cell(c: &Compositor, pane: PaneId) -> (u32, u32) {
    let labels: Vec<Piece> = c
        .session()
        .active()
        .panes()
        .iter()
        .enumerate()
        .filter_map(|(index, id)| {
            let found = c.pane(*id)?;
            Some(
                Piece::new(pane_label(index, &found.terminal, &found.title))
                    .clicking(Hit::Pane(*id)),
            )
        })
        .collect();
    let bar = Bar::lay_out(&[labels], &[], c.grid_area().width);
    let col = bar
        .pieces()
        .iter()
        .find(|piece| piece.hit == Some(Hit::Pane(pane)))
        .map(|piece| piece.col)
        .expect("every pane has a label on the bar");
    (col, c.grid_area().height)
}

/// What the child on the other end of a pane's PTY believes its window to be.
/// Asked of the kernel rather than worked out from the pane, because a program
/// drawing itself at the wrong size is the whole of the complaint and the PTY
/// is the only place the program hears its size from.
fn child_size(c: &Compositor, pane: PaneId) -> (u32, u32) {
    let fd = c.pane(pane).expect("a live pane").pty.fd();
    let mut ws = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let answered = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) };
    assert_eq!(answered, 0, "the pty would not say how big it is");
    (u32::from(ws.ws_col), u32::from(ws.ws_row))
}

#[test]
fn clicking_a_hidden_pane_on_the_bar_resizes_the_zoom_it_drops() {
    // The bar is the one way to reach a pane the zoom is hiding, and reaching
    // one leaves the zoom. The panes were never told: the workspace came back
    // as two columns while the pane that had been zoomed, its terminal and its
    // child were all still the size of the whole workspace, and the program
    // inside it drew at that size until something else resynced.
    let mut c = labelled_panes();
    assert!(c.perform(Action::Split(Axis::Columns)));
    let panes = c.session().active().panes();
    let zoomed = c.session().focus();
    let hidden = *panes
        .iter()
        .find(|id| **id != zoomed)
        .expect("two panes after a split");
    assert!(c.perform(Action::ToggleZoom));
    assert_eq!(c.session().active().zoomed(), Some(zoomed));

    let label = label_cell(&c, hidden);
    click(&mut c, label);
    assert_eq!(c.session().focus(), hidden, "the click missed the label");
    assert!(
        c.session().active().zoomed().is_none(),
        "focusing a hidden pane leaves the zoom"
    );

    let rect = rect_of(&c, zoomed);
    let terminal = &c.pane(zoomed).expect("the pane is still there").terminal;
    assert_eq!(
        (terminal.grid().cols() as u32, terminal.grid().rows() as u32),
        (rect.width, rect.height),
        "the pane that had been zoomed kept the zoomed terminal"
    );
    assert_eq!(
        child_size(&c, zoomed),
        (rect.width, rect.height),
        "and the program inside it was never told it had shrunk"
    );
}
