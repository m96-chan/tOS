//! The compositor itself.
//!
//! It owns the panes, the session layout, the fonts and the display, and runs
//! the loop that reads PTYs and input, updates terminals, and paints frames.

use std::collections::HashMap;
use std::io;
use std::time::{Duration, Instant};

use tos_font::{BitmapFont, FontStack, GlyphSource};
use tos_input::encode::{encode_alternate_scroll, EncodeContext};
use tos_input::{
    encode_focus, encode_key, encode_mouse, encode_paste, InputEvent, KeyEvent,
    MouseAction, MouseButton, MouseEvent,
};
use tos_platform::Display;
use tos_render::{render, RenderOptions, Rect as PixelRect, Surface};
use tos_session::{Action, Axis, Keymap, PaneId, Rect, Resolution, Session};
use tos_term::TermEvent;

use crate::chrome::{self, Chrome, StatusItem};
use crate::config::Config;
use crate::launcher;
use crate::overlay::{Overlay, OverlayOutcome};
use crate::pane::Pane;
use crate::selection::{Selection, SelectionMode};

/// How often the cursor and blinking text change phase.
const BLINK_INTERVAL: Duration = Duration::from_millis(530);
/// Longest a frame may wait when nothing is happening.
const IDLE_TIMEOUT_MS: i32 = 100;
/// How long to wait when a pane still has input queued for its child.
const WRITE_RETRY_TIMEOUT_MS: i32 = 4;
/// How long a transient status message stays up.
const MESSAGE_TIMEOUT: Duration = Duration::from_secs(3);
/// How close together two presses have to be to count as a double click.
const MULTI_CLICK_INTERVAL: Duration = Duration::from_millis(400);
/// The OSC 52 selector for the clipboard, which an explicit copy writes.
const CLIPBOARD: char = 'c';
/// The selector for primary, which is where the mouse puts what it selects
/// and where a middle click pastes from.
const PRIMARY: char = 'p';

/// The last left press, kept so that the one after it can tell whether it is
/// a second or a third click of the same gesture.
#[derive(Debug, Clone, Copy)]
struct Click {
    pane: PaneId,
    col: usize,
    row: usize,
    at: Instant,
    count: u32,
}

/// Which menu an open overlay is, and so what choosing a row means.
///
/// The overlay itself is generic; this is the compositor's side of it. The
/// other system menus — power, network, Bluetooth, audio — each add a variant
/// here and a match arm in [`Compositor::choose`], and reuse everything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayKind {
    /// A program from `$PATH`, which starts in a new pane.
    Launcher,
}

/// The running compositor.
pub struct Compositor {
    config: Config,
    session: Session,
    panes: HashMap<PaneId, Pane>,
    fonts: FontStack,
    keymap: Keymap,
    chrome: Chrome,
    /// Display size in pixels.
    size: (u32, u32),
    /// Selections stored by OSC 52 selector: 'c' is the clipboard, which an
    /// explicit copy writes, and 'p' is primary, which the mouse writes.
    clipboard: HashMap<char, Vec<u8>>,
    blink_visible: bool,
    last_blink: Instant,
    message: Option<(String, Instant)>,
    /// The open menu, if any. While it is open it owns the keyboard.
    overlay: Option<(OverlayKind, Overlay)>,
    needs_full_redraw: bool,
    running: bool,
    /// Pointer position in pixels, for mouse routing.
    pointer: (u32, u32),
    /// The pane a mouse button went down on.
    mouse_grab: Option<PaneId>,
    /// The previous left press, for double and triple click.
    last_click: Option<Click>,
    /// Some pane still has input queued, so the loop must not idle.
    pending_writes: bool,
}

impl Compositor {
    /// Build a compositor for a display of this size.
    pub fn new(config: Config, size: (u32, u32), physical_mm: Option<(u32, u32)>) -> io::Result<Self> {
        let fonts = build_fonts(&config, size, physical_mm);
        let mut compositor = Compositor {
            session: Session::new(),
            panes: HashMap::new(),
            fonts,
            keymap: Keymap::default_bindings(),
            chrome: Chrome::default(),
            size,
            clipboard: HashMap::new(),
            blink_visible: true,
            last_blink: Instant::now(),
            message: None,
            overlay: None,
            needs_full_redraw: true,
            running: true,
            pointer: (0, 0),
            mouse_grab: None,
            last_click: None,
            pending_writes: false,
            config,
        };

        // The first pane exists in the session already; give it a process.
        let root = compositor.session.root_pane();
        let area = compositor
            .session
            .active()
            .geometry(compositor.grid_area())
            .first()
            .map(|(_, rect)| *rect)
            .unwrap_or(Rect::new(0, 0, 80, 24));
        let pane = compositor.spawn_pane(area)?;
        compositor.panes.insert(root, pane);
        compositor.sync_layout();
        Ok(compositor)
    }

    fn spawn_pane(&self, area: Rect) -> io::Result<Pane> {
        self.spawn_pane_running(area, self.config.command.as_deref())
    }

    /// Start a pane on a particular command rather than the configured one.
    fn spawn_pane_running(&self, area: Rect, command: Option<&[String]>) -> io::Result<Pane> {
        Pane::spawn(area, self.cell_size(), self.config.scrollback, command)
    }

    pub fn cell_size(&self) -> (u32, u32) {
        let metrics = self.fonts.metrics();
        (metrics.cell_width.max(1), metrics.cell_height.max(1))
    }

    /// The part of the screen panes are laid out in, in cells.
    pub fn grid_area(&self) -> Rect {
        let (cw, ch) = self.cell_size();
        let cols = (self.size.0 / cw).max(1);
        let rows = (self.size.1 / ch).max(1);
        let status = if self.config.status_bar && rows > 2 { 1 } else { 0 };
        Rect::new(0, 0, cols, rows - status)
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn pane(&self, id: PaneId) -> Option<&Pane> {
        self.panes.get(&id)
    }

    pub fn pane_mut(&mut self, id: PaneId) -> Option<&mut Pane> {
        self.panes.get_mut(&id)
    }

    pub fn clipboard(&self, selector: char) -> Option<&[u8]> {
        self.clipboard.get(&selector).map(|v| v.as_slice())
    }

    /// File descriptors that should be polled for readiness.
    pub fn pty_fds(&self) -> Vec<std::os::unix::io::RawFd> {
        self.panes.values().map(|p| p.pty.fd()).collect()
    }

    /// React to the display changing size.
    pub fn resize(&mut self, size: (u32, u32)) {
        if size == self.size {
            return;
        }
        self.size = size;
        self.sync_layout();
        self.needs_full_redraw = true;
    }

    /// Give every pane the area the layout says it has.
    pub fn sync_layout(&mut self) {
        let area = self.grid_area();
        let cell = self.cell_size();
        let geometry = self.session.active().geometry(area);
        for (id, rect) in geometry {
            if let Some(pane) = self.panes.get_mut(&id) {
                pane.set_area(rect, cell);
            }
        }
    }

    /// Read from every PTY and let the terminals catch up.
    pub fn pump_panes(&mut self) -> bool {
        let mut changed = false;
        let mut finished = Vec::new();
        let mut buf = vec![0u8; 64 * 1024];

        for (&id, pane) in self.panes.iter_mut() {
            let before = pane.terminal.damage().is_dirty();
            let alive = pane.pump(&mut buf);
            if !alive && !pane.pty.is_alive() {
                finished.push(id);
            }
            if pane.terminal.damage().is_dirty() || before {
                changed = true;
            }
        }

        // Terminal events are handled outside the borrow above.
        let ids: Vec<PaneId> = self.panes.keys().copied().collect();
        for id in ids {
            if self.handle_terminal_events(id) {
                changed = true;
            }
            if let Some(pane) = self.panes.get_mut(&id) {
                pane.flush_responses();
                // A large paste does not fit the PTY buffer in one write, so
                // the remainder is retried until the child has read it.
                if pane.flush_input() {
                    self.pending_writes = true;
                }
                if pane.input_overflowed() {
                    self.message =
                        Some(("input dropped: pane is not reading".into(), Instant::now()));
                    changed = true;
                }
            }
        }

        for id in finished {
            self.close_pane(id);
            changed = true;
        }
        changed
    }

    fn handle_terminal_events(&mut self, id: PaneId) -> bool {
        // The events are taken first so the pane borrow ends before any of
        // them need to touch the compositor as a whole.
        let events = match self.panes.get_mut(&id) {
            Some(pane) => pane.terminal.take_events(),
            None => return false,
        };
        if events.is_empty() {
            return false;
        }

        let mut changed = false;
        for event in events {
            match event {
                TermEvent::TitleChanged(title) => {
                    if let Some(pane) = self.panes.get_mut(&id) {
                        pane.title = title;
                    }
                    changed = true;
                }
                TermEvent::IconTitleChanged(_) | TermEvent::CwdChanged(_) => {}
                TermEvent::Bell => {
                    // A visible bell is the only kind a display server with no
                    // audio stack can offer.
                    self.message = Some((format!("bell in pane {}", id.0 + 1), Instant::now()));
                    changed = true;
                }
                TermEvent::Notify { title, body } => {
                    let text = if title.is_empty() {
                        body
                    } else {
                        format!("{title}: {body}")
                    };
                    self.message = Some((text, Instant::now()));
                    changed = true;
                }
                TermEvent::ClipboardStore { selection, data } => {
                    self.clipboard.insert(selection, data);
                }
                TermEvent::ClipboardLoad { selection } => {
                    let data = self.clipboard.get(&selection).cloned().unwrap_or_default();
                    if let Some(pane) = self.panes.get_mut(&id) {
                        pane.terminal.report_clipboard(selection, &data);
                    }
                }
                TermEvent::Repaint | TermEvent::CursorStyleChanged(_) => changed = true,
                TermEvent::ReportingChanged | TermEvent::WindowOp(..) => {}
            }
        }
        changed
    }

    // ---- input ----------------------------------------------------------

    /// Handle one input event. Returns true when something needs repainting.
    pub fn handle_input(&mut self, event: InputEvent) -> bool {
        match event {
            InputEvent::Key(key) => self.handle_key(key),
            // A host terminal reports cells; a device reports pixels. The two
            // are separate types so the conversion can never be skipped.
            InputEvent::Mouse(mouse) => self.route_mouse(
                mouse.col as u32,
                mouse.row as u32,
                mouse.button,
                mouse.action,
                mouse.modifiers,
            ),
            InputEvent::Pointer(pointer) => {
                let (cw, ch) = self.cell_size();
                let x = pointer.x.max(0.0) as u32;
                let y = pointer.y.max(0.0) as u32;
                self.pointer = (x, y);
                self.route_mouse(
                    x / cw,
                    y / ch,
                    pointer.button,
                    pointer.action,
                    pointer.modifiers,
                )
            }
            InputEvent::Paste(text) => {
                self.paste_text(&text);
                true
            }
            InputEvent::FocusGained => self.forward_focus(true),
            InputEvent::FocusLost => self.forward_focus(false),
        }
    }

    fn forward_focus(&mut self, gained: bool) -> bool {
        let focus = self.session.focus();
        if let Some(pane) = self.panes.get_mut(&focus) {
            if pane.terminal.modes.focus_events {
                let bytes = encode_focus(gained).to_vec();
                pane.write(&bytes);
            }
        }
        false
    }

    fn handle_key(&mut self, key: KeyEvent) -> bool {
        // An open overlay owns the keyboard: not the bindings, not the pane
        // underneath. Anything else and a menu would be typing into a shell.
        if self.overlay.is_some() {
            return self.overlay_key(&key);
        }
        match self.keymap.resolve(&key) {
            Resolution::Action(action) => self.perform(action),
            Resolution::Pending => {
                self.message = Some(("leader".to_string(), Instant::now()));
                true
            }
            Resolution::Passthrough => {
                let focus = self.session.focus();
                let Some(pane) = self.panes.get_mut(&focus) else {
                    return false;
                };
                let ctx = EncodeContext::from_terminal(&pane.terminal);
                let bytes = encode_key(&key, &ctx);
                if bytes.is_empty() {
                    return false;
                }
                // Typing returns the view to the live screen, as it must: the
                // input is going to a program that is drawing there.
                let scrolled = pane.terminal.display_offset() != 0;
                pane.terminal.reset_display_offset();
                pane.write(&bytes);
                scrolled
            }
        }
    }

    /// Route a mouse event that is already in display cell coordinates.
    fn route_mouse(
        &mut self,
        cell_x: u32,
        cell_y: u32,
        button: Option<MouseButton>,
        action: MouseAction,
        modifiers: tos_input::Modifiers,
    ) -> bool {
        let area = self.grid_area();
        let geometry = self.session.active().geometry(area);

        // A drag that started in a pane keeps going there even once the
        // pointer leaves it, which is what makes selection usable. A grab on a
        // pane that has since closed is dropped rather than wedging the mouse.
        let grabbed = self.mouse_grab.and_then(|id| {
            geometry
                .iter()
                .find(|(pane, _)| *pane == id)
                .map(|(pane, rect)| (*pane, *rect))
        });
        if self.mouse_grab.is_some() && grabbed.is_none() {
            self.mouse_grab = None;
        }

        let hit = geometry
            .iter()
            .find(|(_, rect)| rect.contains(cell_x, cell_y))
            .map(|(pane, rect)| (*pane, *rect));

        // A press always re-targets: it starts a new interaction, and the pane
        // under the pointer is the one it belongs to. Everything else follows
        // the grab, so a drag can leave the pane it started in.
        let target = if action == MouseAction::Press {
            if let Some((pane, _)) = hit {
                if self.mouse_grab.is_some_and(|grabbed| grabbed != pane) {
                    self.release_grab();
                }
            }
            hit.or(grabbed)
        } else {
            grabbed.or(hit)
        };
        let Some((pane_id, rect)) = target else {
            return false;
        };
        // A pane with no area cannot be interacted with, and clamping into it
        // would be a division by an empty range.
        if rect.is_empty() {
            return false;
        }

        let mut changed = false;
        if action == MouseAction::Press && self.session.focus() != pane_id {
            let previous = self.session.focus();
            self.session.set_focus(pane_id);
            self.clear_selection(previous);
            self.needs_full_redraw = true;
            changed = true;
        }

        // Clamp into the pane so a drag past its edge still selects sensibly.
        let local_col = cell_x.clamp(rect.x, rect.right() - 1) - rect.x;
        let local_row = cell_y.clamp(rect.y, rect.bottom() - 1) - rect.y;
        let local = MouseEvent {
            button,
            action,
            col: local_col as usize,
            row: local_row as usize,
            modifiers,
        };

        let tracking = match self.panes.get(&pane_id) {
            Some(pane) => pane.terminal.mouse(),
            None => return changed,
        };

        // Wheel events scroll the compositor's own scrollback unless the
        // program is tracking the mouse itself.
        if let Some(button) = button {
            if button.is_wheel() && !tracking.is_enabled() {
                if action != MouseAction::Press {
                    return changed;
                }
                return self.scroll_wheel(pane_id, button) || changed;
            }
        }

        if tracking.is_enabled() {
            if let Some(bytes) = encode_mouse(&local, tracking) {
                if let Some(pane) = self.panes.get_mut(&pane_id) {
                    pane.write(&bytes);
                }
            }
            return changed;
        }

        // Otherwise the mouse belongs to the compositor, and selects text.
        let mode = if action == MouseAction::Press && button == Some(MouseButton::Left) {
            SelectionMode::for_clicks(self.count_click(pane_id, local.col, local.row))
        } else {
            SelectionMode::Cell
        };
        let mut copied = None;
        let mut paste = false;
        if let Some(pane) = self.panes.get_mut(&pane_id) {
            let at = pane.anchor_at(local.col, local.row);
            match action {
                MouseAction::Press if button == Some(MouseButton::Left) => {
                    pane.selecting = true;
                    pane.set_selection(Some(Selection::new(at, modifiers.alt(), mode)));
                    self.mouse_grab = Some(pane_id);
                    changed = true;
                }
                MouseAction::Drag | MouseAction::Motion if pane.selecting => {
                    if let Some(mut selection) = pane.selection {
                        selection.drag_to(at);
                        pane.set_selection(Some(selection));
                    }
                    changed = true;
                }
                MouseAction::Release if pane.selecting => {
                    pane.selecting = false;
                    self.mouse_grab = None;
                    copied = pane.selected_text();
                    changed = true;
                }
                MouseAction::Press if button == Some(MouseButton::Middle) => {
                    paste = true;
                }
                _ => {}
            }
        }

        // What the mouse selects goes to primary, the way X11 has always done
        // it, so that dragging over a word does not throw away whatever was
        // deliberately copied to the clipboard.
        if let Some(text) = copied {
            self.clipboard.insert(PRIMARY, text.into_bytes());
        }
        if paste {
            let data = self.clipboard.get(&PRIMARY).cloned().unwrap_or_default();
            let text = String::from_utf8_lossy(&data).into_owned();
            self.paste_text(&text);
            changed = true;
        }
        changed
    }

    /// How many presses in a row this one is, counting only presses close
    /// enough in time and on the same cell of the same pane to be one gesture.
    ///
    /// This lives here rather than in `tos-input` because the count is about
    /// where the presses landed, and only the compositor knows that: it owns
    /// the pane geometry that turns a pointer position into a cell. A device
    /// driver sees pixels, and a host terminal hands over reports that never
    /// carried a click count in the first place.
    fn count_click(&mut self, pane: PaneId, col: usize, row: usize) -> u32 {
        let now = Instant::now();
        let count = match self.last_click {
            Some(last)
                if last.pane == pane
                    && (last.col, last.row) == (col, row)
                    && now.duration_since(last.at) <= MULTI_CLICK_INTERVAL =>
            {
                last.count + 1
            }
            _ => 1,
        };
        self.last_click = Some(Click {
            pane,
            col,
            row,
            at: now,
            count,
        });
        count
    }

    /// Forget a pane's selection, which is what every event that moves the
    /// text out from under it has to do.
    fn clear_selection(&mut self, id: PaneId) {
        if let Some(pane) = self.panes.get_mut(&id) {
            pane.clear_selection();
        }
    }

    /// Abandon an interaction that was still in progress in another pane.
    fn release_grab(&mut self) {
        if let Some(pane) = self.mouse_grab.take() {
            if let Some(pane) = self.panes.get_mut(&pane) {
                pane.selecting = false;
            }
        }
    }

    fn scroll_wheel(&mut self, id: PaneId, button: MouseButton) -> bool {
        const LINES: usize = 3;
        let Some(pane) = self.panes.get_mut(&id) else {
            return false;
        };
        // On the alternate screen there is no history, so the convention is to
        // send arrow keys instead, which is what pagers expect.
        if pane.terminal.modes.alt_screen {
            if pane.terminal.mouse().alternate_scroll {
                let ctx = EncodeContext::from_terminal(&pane.terminal);
                if let Some(bytes) = encode_alternate_scroll(button, LINES, &ctx) {
                    pane.write(&bytes);
                }
            }
            return false;
        }
        let delta = match button {
            MouseButton::WheelUp => LINES as isize,
            MouseButton::WheelDown => -(LINES as isize),
            _ => return false,
        };
        pane.terminal.scroll_display(delta)
    }

    fn paste_text(&mut self, text: &str) {
        let focus = self.session.focus();
        let Some(pane) = self.panes.get_mut(&focus) else {
            return;
        };
        let bracketed = pane.terminal.modes.bracketed_paste;
        let bytes = encode_paste(text, bracketed);
        pane.terminal.reset_display_offset();
        pane.write(&bytes);
    }

    // ---- actions --------------------------------------------------------

    /// Carry out a compositor action. Returns true when a repaint is needed.
    pub fn perform(&mut self, action: Action) -> bool {
        let before = self.session.focus();
        let changed = self.perform_action(action);
        // Moving focus away ends whatever was being selected there: the
        // selection belongs to an interaction the user has left behind.
        if self.session.focus() != before {
            self.clear_selection(before);
        }
        changed
    }

    fn perform_action(&mut self, action: Action) -> bool {
        let area = self.grid_area();
        match action {
            Action::Split(axis) => self.split(axis),
            Action::ClosePane => {
                let focus = self.session.focus();
                self.close_pane(focus);
                true
            }
            Action::Focus(direction) => {
                let moved = self.session.focus_direction(area, direction);
                self.needs_full_redraw |= moved;
                moved
            }
            Action::Resize(direction, amount) => {
                if self.session.resize_focused(area, direction, amount) {
                    self.sync_layout();
                    self.needs_full_redraw = true;
                    true
                } else {
                    false
                }
            }
            Action::ToggleZoom => {
                let changed = self.session.toggle_zoom();
                if changed {
                    self.sync_layout();
                    self.needs_full_redraw = true;
                }
                changed
            }
            Action::Balance => {
                self.session.balance();
                self.sync_layout();
                self.needs_full_redraw = true;
                true
            }
            Action::NewWorkspace => {
                let pane_id = self.session.new_workspace();
                match self.spawn_pane(self.grid_area()) {
                    Ok(pane) => {
                        self.panes.insert(pane_id, pane);
                        self.sync_layout();
                        self.needs_full_redraw = true;
                        true
                    }
                    Err(e) => {
                        // Undo the workspace rather than leave an empty one.
                        self.session.close_pane(pane_id);
                        self.report_error("new workspace", e);
                        true
                    }
                }
            }
            Action::NextWorkspace => {
                self.session.next_workspace();
                self.sync_layout();
                self.needs_full_redraw = true;
                true
            }
            Action::PreviousWorkspace => {
                self.session.previous_workspace();
                self.sync_layout();
                self.needs_full_redraw = true;
                true
            }
            Action::SelectWorkspace(n) => {
                let changed = self.session.select_workspace(n);
                if changed {
                    self.sync_layout();
                    self.needs_full_redraw = true;
                }
                changed
            }
            Action::MovePaneToWorkspace(n) => {
                let moved = self.session.move_focused_to_workspace(area, n);
                if moved {
                    self.sync_layout();
                    self.needs_full_redraw = true;
                }
                moved
            }
            Action::Scroll(lines) => self.scroll_focused(lines as isize),
            Action::ScrollPage(pages) => {
                let rows = self
                    .panes
                    .get(&self.session.focus())
                    .map(|p| p.terminal.rows() as isize)
                    .unwrap_or(1);
                // Negative pages mean back in time, the same convention
                // `Action::Scroll` uses.
                self.scroll_focused(pages as isize * rows.max(1))
            }
            Action::ScrollToBottom => {
                let focus = self.session.focus();
                if let Some(pane) = self.panes.get_mut(&focus) {
                    pane.terminal.reset_display_offset();
                    return true;
                }
                false
            }
            Action::Copy => {
                let focus = self.session.focus();
                if let Some(text) = self.panes.get(&focus).and_then(|p| p.selected_text()) {
                    self.clipboard.insert(CLIPBOARD, text.into_bytes());
                    self.message = Some(("copied".to_string(), Instant::now()));
                    return true;
                }
                false
            }
            Action::Paste => {
                let data = self.clipboard.get(&CLIPBOARD).cloned().unwrap_or_default();
                let text = String::from_utf8_lossy(&data).into_owned();
                self.paste_text(&text);
                true
            }
            Action::BeginSelection => {
                let focus = self.session.focus();
                if let Some(pane) = self.panes.get_mut(&focus) {
                    let cursor = pane.terminal.cursor();
                    let at = pane.anchor_at(cursor.x, cursor.y);
                    pane.set_selection(Some(Selection::new(at, false, SelectionMode::Cell)));
                    return true;
                }
                false
            }
            Action::OpenLauncher => {
                // The scan happens here, once, rather than per keystroke.
                let overlay = Overlay::new("run a program", launcher::programs_on_path());
                self.open_overlay(OverlayKind::Launcher, overlay);
                true
            }
            Action::Refresh => {
                self.needs_full_redraw = true;
                true
            }
            Action::Quit => {
                self.running = false;
                true
            }
        }
    }

    // ---- overlays -------------------------------------------------------

    /// Put a menu up over the panes.
    pub fn open_overlay(&mut self, kind: OverlayKind, overlay: Overlay) {
        self.overlay = Some((kind, overlay));
        // The overlay covers cells the panes are not going to repaint, and
        // closing it uncovers them again, so both ends need a full frame.
        self.needs_full_redraw = true;
    }

    pub fn overlay(&self) -> Option<&Overlay> {
        self.overlay.as_ref().map(|(_, overlay)| overlay)
    }

    fn close_overlay(&mut self) {
        self.overlay = None;
        self.needs_full_redraw = true;
    }

    /// Give a key to the open overlay. Returns true when a repaint is needed.
    fn overlay_key(&mut self, key: &KeyEvent) -> bool {
        let Some((kind, overlay)) = &mut self.overlay else {
            return false;
        };
        let kind = *kind;
        match overlay.handle_key(key) {
            OverlayOutcome::Consumed => false,
            OverlayOutcome::Changed => true,
            OverlayOutcome::Cancelled => {
                // Cancelling changes nothing but the screen.
                self.close_overlay();
                true
            }
            OverlayOutcome::Chosen(index) => {
                let label = overlay.items()[index].label.clone();
                self.close_overlay();
                self.choose(kind, &label);
                true
            }
        }
    }

    /// Act on the row an overlay reported. One arm per menu.
    fn choose(&mut self, kind: OverlayKind, label: &str) {
        match kind {
            OverlayKind::Launcher => self.launch(label),
        }
    }

    /// Open a pane running `program`, using the same path a split does.
    fn launch(&mut self, program: &str) {
        if tos_pty::which(program).is_none() {
            // The list came from $PATH, so this means it went away in between;
            // spawning would leave a pane that dies on its own.
            self.message = Some((format!("not found: {program}"), Instant::now()));
            return;
        }
        let command = vec![program.to_string()];
        if let Some(id) = self.split_running(Axis::Columns, Some(&command)) {
            // Until the program sets a title of its own, its name is the most
            // truthful thing the status bar can say about the pane.
            if let Some(pane) = self.panes.get_mut(&id) {
                pane.title = program.to_string();
            }
        }
    }

    fn scroll_focused(&mut self, lines: isize) -> bool {
        let focus = self.session.focus();
        match self.panes.get_mut(&focus) {
            // A negative delta means further back in history.
            Some(pane) => pane.terminal.scroll_display(-lines),
            None => false,
        }
    }

    fn split(&mut self, axis: Axis) -> bool {
        self.split_running(axis, None);
        true
    }

    /// Split the focused pane, running `command` in the new one. `None` runs
    /// whatever the configuration says a pane runs.
    ///
    /// Returns the new pane, or `None` when there was no room or the process
    /// could not be started; either way the message says so.
    fn split_running(&mut self, axis: Axis, command: Option<&[String]>) -> Option<PaneId> {
        let area = self.grid_area();
        let Some(new_id) = self.session.split_focused(area, axis) else {
            // Refusing is the right answer when the pane is too small; saying
            // so beats silently creating a pane with nowhere to go.
            self.message = Some(("no room to split".to_string(), Instant::now()));
            return None;
        };
        let pane_area = self
            .session
            .active()
            .geometry(area)
            .into_iter()
            .find(|(id, _)| *id == new_id)
            .map(|(_, rect)| rect)
            .unwrap_or(Rect::new(0, 0, 80, 24));

        let spawned = match command {
            Some(command) => self.spawn_pane_running(pane_area, Some(command)),
            None => self.spawn_pane(pane_area),
        };
        match spawned {
            Ok(pane) => {
                self.panes.insert(new_id, pane);
                self.sync_layout();
                self.needs_full_redraw = true;
                Some(new_id)
            }
            Err(e) => {
                // The layout must not keep a pane with no process behind it.
                self.session.close_pane(new_id);
                self.sync_layout();
                self.report_error("split", e);
                None
            }
        }
    }

    /// Close a pane, ending the session when it was the last one.
    pub fn close_pane(&mut self, id: PaneId) {
        let closed = self.session.close_pane(id);
        if closed.is_empty() {
            // The session refused, which means this was the final pane.
            self.panes.remove(&id);
            self.running = false;
            return;
        }
        for pane in closed {
            self.panes.remove(&pane);
            if self.mouse_grab == Some(pane) {
                self.mouse_grab = None;
            }
        }
        self.sync_layout();
        self.needs_full_redraw = true;
    }

    fn report_error(&mut self, what: &str, error: io::Error) {
        self.message = Some((format!("{what} failed: {error}"), Instant::now()));
    }

    // ---- rendering ------------------------------------------------------

    /// Advance the blink phase and any animations. Returns true when anything
    /// changed.
    pub fn tick(&mut self) -> bool {
        let mut changed = false;
        // Animated images move on their own clock. The compositor owns that
        // clock and hands the time to each terminal, which keeps the terminal
        // model free of time of its own.
        let now = Instant::now();
        for pane in self.panes.values_mut() {
            if pane.terminal.advance_animations(now) {
                changed = true;
            }
        }
        if self.last_blink.elapsed() >= BLINK_INTERVAL {
            self.blink_visible = !self.blink_visible;
            self.last_blink = Instant::now();
            changed = true;
        }
        if let Some((_, at)) = &self.message {
            if at.elapsed() >= MESSAGE_TIMEOUT {
                self.message = None;
                changed = true;
            }
        }
        changed
    }

    /// Whether anything has changed since the last frame.
    pub fn needs_render(&self) -> bool {
        self.needs_full_redraw
            || self
                .panes
                .values()
                .any(|pane| pane.terminal.damage().is_dirty())
    }

    /// Paint a frame.
    pub fn render_frame(&mut self, surface: &mut Surface<'_>, retained: bool) {
        let force = self.needs_full_redraw || !retained;
        let (cw, ch) = self.cell_size();
        let area = self.grid_area();
        let focus = self.session.focus();
        let geometry = self.session.active().geometry(area);

        if force {
            surface.clear(self.chrome.background);
        }

        let mut drawn: Vec<PaneId> = Vec::with_capacity(geometry.len());
        for (id, rect) in &geometry {
            // Mutable because the pane owns its texture cache, which the
            // renderer fills in as it draws.
            let Some(pane) = self.panes.get_mut(id) else {
                continue;
            };
            // A pane that is synchronising its output asked not to be drawn
            // mid-update, so the previous frame stays on screen.
            if pane.terminal.modes.synchronized_output && !force {
                continue;
            }
            drawn.push(*id);
            let pixel_rect = PixelRect::new(
                (rect.x * cw) as i32,
                (rect.y * ch) as i32,
                rect.width * cw,
                rect.height * ch,
            );
            let options = RenderOptions {
                blink_visible: self.blink_visible,
                focused: *id == focus,
                draw_cursor: true,
                selection: pane.display_selection(),
                selection_background: self.chrome.accent,
                force,
                inactive_fade: self.config.inactive_fade,
            };
            render(
                surface,
                pixel_rect,
                &pane.terminal,
                &mut self.fonts,
                &mut pane.textures,
                &options,
            );
        }

        if force {
            let focused_rect = geometry
                .iter()
                .find(|(id, _)| *id == focus)
                .map(|(_, rect)| *rect);
            for (axis, divider) in self.session.active().layout.dividers(area) {
                // Only draw dividers that the zoom state leaves visible.
                if self.session.active().zoomed().is_some() {
                    break;
                }
                let touches_focus = focused_rect
                    .map(|rect| {
                        divider.x <= rect.right()
                            && divider.right() >= rect.x
                            && divider.y <= rect.bottom()
                            && divider.bottom() >= rect.y
                    })
                    .unwrap_or(false);
                let color = if touches_focus {
                    self.chrome.divider_focused
                } else {
                    self.chrome.divider
                };
                chrome::draw_divider(
                    surface,
                    &mut self.fonts,
                    PixelRect::new(
                        (divider.x * cw) as i32,
                        (divider.y * ch) as i32,
                        divider.width * cw,
                        divider.height * ch,
                    ),
                    axis,
                    color,
                    self.chrome.background,
                );
            }
        }

        if self.config.status_bar {
            self.draw_status(surface, area, ch);
        }

        // Last, and over everything: the overlay is modal, and the panes below
        // it have already painted whatever they wanted to this frame.
        if let Some((_, overlay)) = &mut self.overlay {
            let over = PixelRect::new(0, 0, area.width * cw, area.height * ch);
            overlay.draw(surface, &mut self.fonts, over, &self.chrome);
        }

        for (id, pane) in self.panes.iter_mut() {
            // A pane skipped for synchronized output was not painted, so its
            // damage still describes work outstanding; clearing it here would
            // lose the whole update.
            let skipped = !drawn.contains(id);
            if !skipped {
                pane.terminal.clear_damage();
            }
        }
        self.needs_full_redraw = false;
    }

    fn draw_status(&mut self, surface: &mut Surface<'_>, area: Rect, cell_height: u32) {
        let active = self.session.active_index();
        let items: Vec<StatusItem> = (0..self.session.workspace_count())
            .map(|i| {
                StatusItem::new(
                    self.session.workspaces()[i].name.clone(),
                    i == active,
                )
            })
            .collect();

        let focus = self.session.focus();
        let right = match &self.message {
            Some((text, _)) => text.clone(),
            None => {
                let panes = self.session.active().panes();
                let index = panes.iter().position(|p| *p == focus).unwrap_or(0);
                match self.panes.get(&focus) {
                    Some(pane) => {
                        let scrolled = pane.terminal.display_offset();
                        let label = chrome::pane_label(index, &pane.terminal, &pane.title);
                        if scrolled > 0 {
                            format!("{label}  [scrollback {scrolled}]")
                        } else {
                            label
                        }
                    }
                    None => String::new(),
                }
            }
        };

        let bar = PixelRect::new(
            0,
            (area.height * cell_height) as i32,
            self.size.0,
            cell_height,
        );
        chrome::draw_status_bar(surface, &mut self.fonts, bar, &self.chrome, &items, &right);
    }

    /// Feed bytes straight into the focused pane's terminal, bypassing the
    /// PTY. Used by `--preload` so a screenshot can show known content.
    pub fn inject(&mut self, bytes: &[u8]) {
        let focus = self.session.focus();
        if let Some(pane) = self.panes.get_mut(&focus) {
            pane.terminal.advance(bytes);
        }
    }

    /// How long the loop may wait for something to happen. An animation with a
    /// frame due sooner than the idle timeout pulls the wait in, so playback
    /// keeps its own pace instead of the poll timer's.
    fn frame_timeout_ms(&self) -> i32 {
        let now = Instant::now();
        let soonest = self
            .panes
            .values()
            .filter_map(|pane| pane.terminal.next_animation_delay(now))
            .min();
        match soonest {
            Some(delay) => delay.as_millis().clamp(1, IDLE_TIMEOUT_MS as u128) as i32,
            None => IDLE_TIMEOUT_MS,
        }
    }

    /// Run one iteration: poll, read, handle, and render if needed.
    pub fn run_once(
        &mut self,
        display: &mut dyn Display,
        input_fds: &[std::os::unix::io::RawFd],
        mut read_input: impl FnMut(std::os::unix::io::RawFd) -> Vec<InputEvent>,
    ) -> io::Result<()> {
        let mut fds = self.pty_fds();
        fds.extend_from_slice(input_fds);
        // With input still queued the loop has to come back promptly to retry
        // it, rather than waiting for something to read.
        let timeout = if self.pending_writes {
            WRITE_RETRY_TIMEOUT_MS
        } else {
            self.frame_timeout_ms()
        };
        self.pending_writes = false;
        let ready = tos_platform::tty::poll_readable(&fds, timeout)?;

        let mut dirty = false;
        for fd in &ready {
            if input_fds.contains(fd) {
                for event in read_input(*fd) {
                    dirty |= self.handle_input(event);
                }
            }
        }
        dirty |= self.pump_panes();
        dirty |= self.tick();

        if dirty || self.needs_render() {
            let retained = display.retains_contents();
            display.frame(&mut |surface| self.render_frame(surface, retained))?;
        }
        Ok(())
    }
}

/// Assemble the font stack from configuration and display characteristics.
fn build_fonts(config: &Config, size: (u32, u32), physical_mm: Option<(u32, u32)>) -> FontStack {
    let pixel_size = config
        .font_size
        .unwrap_or_else(|| tos_platform::suggested_font_size(size.0, size.1, physical_mm));

    let primary: Option<Box<dyn GlyphSource>> = load_ttf(config, pixel_size);

    let mut stack = match primary {
        Some(font) => FontStack::new(font),
        None => {
            let scale = config
                .bitmap_scale
                .map(BitmapFont::new)
                .unwrap_or_else(|| BitmapFont::for_display(size.0, size.1));
            FontStack::new(Box::new(scale))
        }
    };
    // The built-in face is always available as a last resort, so a missing
    // glyph in the main font never leaves a hole.
    stack.push_fallback(Box::new(BitmapFont::for_display(size.0, size.1)));
    stack
}

fn load_ttf(config: &Config, pixel_size: f32) -> Option<Box<dyn GlyphSource>> {
    let font = match &config.font {
        Some(path) => tos_font::TtfFont::from_path(path, pixel_size).ok()?,
        None => tos_font::TtfFont::system(pixel_size)?,
    };
    Some(Box::new(font))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tos_input::KeyCode;

    fn compositor() -> Compositor {
        let config = Config {
            command: Some(vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()]),
            bitmap_scale: Some(1),
            font: Some("/nonexistent-so-the-bitmap-font-is-used".into()),
            ..Config::default()
        };
        Compositor::new(config, (640, 360), None).expect("compositor")
    }

    #[test]
    fn a_new_compositor_has_one_pane_with_a_process() {
        let compositor = compositor();
        assert_eq!(compositor.panes.len(), 1);
        assert_eq!(compositor.session.all_panes().len(), 1);
    }

    #[test]
    fn the_grid_leaves_room_for_the_status_bar() {
        let compositor = compositor();
        let (_, ch) = compositor.cell_size();
        let total_rows = 360 / ch;
        assert_eq!(compositor.grid_area().height, total_rows - 1);
    }

    #[test]
    fn splitting_starts_a_second_process() {
        let mut compositor = compositor();
        assert!(compositor.perform(Action::Split(Axis::Columns)));
        assert_eq!(compositor.panes.len(), 2);
        // Both panes are narrower than the whole screen.
        let area = compositor.grid_area();
        for (_, rect) in compositor.session.active().geometry(area) {
            assert!(rect.width < area.width);
        }
    }

    #[test]
    fn panes_are_told_their_new_size() {
        let mut compositor = compositor();
        let before = compositor
            .pane(compositor.session.focus())
            .unwrap()
            .terminal
            .cols();
        compositor.perform(Action::Split(Axis::Columns));
        let after = compositor
            .pane(compositor.session.focus())
            .unwrap()
            .terminal
            .cols();
        assert!(after < before, "{after} should be narrower than {before}");
    }

    #[test]
    fn closing_the_last_pane_stops_the_compositor() {
        let mut compositor = compositor();
        assert!(compositor.is_running());
        compositor.perform(Action::ClosePane);
        assert!(!compositor.is_running());
    }

    #[test]
    fn closing_one_of_two_panes_keeps_running() {
        let mut compositor = compositor();
        compositor.perform(Action::Split(Axis::Rows));
        compositor.perform(Action::ClosePane);
        assert!(compositor.is_running());
        assert_eq!(compositor.panes.len(), 1);
    }

    #[test]
    fn resizing_the_display_resizes_the_panes() {
        let mut compositor = compositor();
        let before = compositor
            .pane(compositor.session.focus())
            .unwrap()
            .terminal
            .cols();
        compositor.resize((320, 360));
        let after = compositor
            .pane(compositor.session.focus())
            .unwrap()
            .terminal
            .cols();
        assert!(after < before);
    }

    #[test]
    fn workspaces_get_their_own_pane() {
        let mut compositor = compositor();
        compositor.perform(Action::NewWorkspace);
        assert_eq!(compositor.session.workspace_count(), 2);
        assert_eq!(compositor.panes.len(), 2);
        // Only the active workspace's pane is laid out.
        assert_eq!(compositor.session.active().panes().len(), 1);
    }

    #[test]
    fn scrolling_moves_the_viewport_and_comes_back() {
        let mut compositor = compositor();
        let focus = compositor.session.focus();
        let rows = compositor.pane(focus).unwrap().terminal.rows();
        let filler: String = (0..rows * 2).map(|i| format!("line {i}\r\n")).collect();
        compositor.inject(filler.as_bytes());

        assert!(compositor.perform(Action::ScrollPage(-1)));
        assert!(compositor.pane(focus).unwrap().terminal.display_offset() > 0);
        assert!(compositor.perform(Action::ScrollToBottom));
        assert_eq!(compositor.pane(focus).unwrap().terminal.display_offset(), 0);
    }

    #[test]
    fn a_title_change_reaches_the_pane() {
        let mut compositor = compositor();
        compositor.inject(b"\x1b]0;hello\x07");
        let focus = compositor.session.focus();
        compositor.handle_terminal_events(focus);
        assert_eq!(compositor.pane(focus).unwrap().title, "hello");
    }

    #[test]
    fn osc_52_writes_to_the_compositor_clipboard() {
        let mut compositor = compositor();
        let payload = tos_term::graphics::encode_base64(b"from the app");
        compositor.inject(format!("\x1b]52;c;{payload}\x07").as_bytes());
        let focus = compositor.session.focus();
        compositor.handle_terminal_events(focus);
        assert_eq!(compositor.clipboard('c'), Some(&b"from the app"[..]));
    }

    #[test]
    fn a_clipboard_query_is_answered() {
        let mut compositor = compositor();
        compositor.clipboard.insert('c', b"stored".to_vec());
        compositor.inject(b"\x1b]52;c;?\x07");
        let focus = compositor.session.focus();
        compositor.handle_terminal_events(focus);
        let response = compositor
            .pane_mut(focus)
            .unwrap()
            .terminal
            .take_output();
        let text = String::from_utf8(response).unwrap();
        assert!(text.starts_with("\x1b]52;c;"), "got {text:?}");
        assert!(text.contains(&tos_term::graphics::encode_base64(b"stored")));
    }

    #[test]
    fn keys_reach_the_focused_pane_and_bindings_do_not() {
        let mut compositor = compositor();
        let typing = KeyEvent::new(KeyCode::Char('x'), tos_input::Modifiers::NONE);
        compositor.handle_input(InputEvent::Key(typing));
        // A binding is consumed by the compositor instead.
        let split = KeyEvent::new(
            KeyCode::Char('d'),
            tos_input::Modifiers::SUPER,
        );
        compositor.handle_input(InputEvent::Key(split));
        assert_eq!(compositor.panes.len(), 2);
    }

    #[test]
    fn the_ctrl_shift_bindings_win_over_the_encoder() {
        // Ctrl+shift+enter is a key the kitty protocol can encode, so the
        // keymap has to claim it before the encoder is ever asked.
        let mut compositor = compositor();
        let ctrl_shift = tos_input::Modifiers::CTRL.union(tos_input::Modifiers::SHIFT);
        compositor.handle_input(InputEvent::Key(KeyEvent::new(KeyCode::Enter, ctrl_shift)));
        assert_eq!(compositor.panes.len(), 2, "ctrl+shift+enter should split");

        compositor.handle_input(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('t'),
            ctrl_shift,
        )));
        assert_eq!(
            compositor.session.workspace_count(),
            2,
            "ctrl+shift+t should open a workspace"
        );
    }

    // ---- launcher -------------------------------------------------------

    /// An overlay of the compositor's own making, so a test does not depend on
    /// what happens to be installed on the machine running it.
    fn open_with(compositor: &mut Compositor, labels: &[&str]) {
        let items = labels
            .iter()
            .map(|label| crate::overlay::OverlayItem::new(*label))
            .collect();
        compositor.open_overlay(OverlayKind::Launcher, Overlay::new("run a program", items));
    }

    fn type_into_overlay(compositor: &mut Compositor, text: &str) {
        for c in text.chars() {
            compositor.handle_input(InputEvent::Key(KeyEvent::new(
                KeyCode::Char(c),
                tos_input::Modifiers::NONE,
            )));
        }
    }

    #[test]
    fn the_launcher_binding_opens_an_overlay_of_programs() {
        let mut compositor = compositor();
        compositor.handle_input(InputEvent::Key(KeyEvent::new(
            KeyCode::Char(' '),
            tos_input::Modifiers::SUPER,
        )));
        let overlay = compositor.overlay().expect("the launcher should be open");
        assert!(!overlay.items().is_empty(), "no programs on $PATH");
        // The list is whatever is on this machine, so check it by a property
        // rather than by name.
        assert!(overlay
            .items()
            .iter()
            .all(|item| tos_pty::which(&item.label).is_some()));
    }

    #[test]
    fn the_overlay_swallows_keys_the_pane_and_the_bindings_would_get() {
        let mut compositor = compositor();
        open_with(&mut compositor, &["ls", "vim"]);
        // Super+d splits when no overlay is open.
        compositor.handle_input(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('d'),
            tos_input::Modifiers::SUPER,
        )));
        assert_eq!(compositor.panes.len(), 1, "a binding fired under the overlay");
        // And the pane below sees nothing of what is typed into the query.
        type_into_overlay(&mut compositor, "vi");
        let focus = compositor.session.focus();
        assert_eq!(compositor.pane(focus).unwrap().pending_input(), 0);
        let overlay = compositor.overlay().unwrap();
        assert_eq!(overlay.query(), "vi");
        assert_eq!(overlay.selected_item().unwrap().label, "vim");
    }

    #[test]
    fn escape_leaves_the_panes_alone() {
        let mut compositor = compositor();
        open_with(&mut compositor, &["ls"]);
        assert!(compositor.handle_input(InputEvent::Key(KeyEvent::new(
            KeyCode::Escape,
            tos_input::Modifiers::NONE,
        ))));
        assert!(compositor.overlay().is_none());
        assert_eq!(compositor.panes.len(), 1);
    }

    #[test]
    fn choosing_a_program_opens_a_pane_running_it() {
        // cat is on every machine this could run on, and it stays up, so a
        // pane that is still alive afterwards is one where the exec worked.
        let mut compositor = compositor();
        let before = compositor.session.focus();
        open_with(&mut compositor, &["ls", "cat"]);
        type_into_overlay(&mut compositor, "cat");
        compositor.handle_input(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            tos_input::Modifiers::NONE,
        )));

        assert!(compositor.overlay().is_none(), "choosing should close it");
        assert_eq!(compositor.panes.len(), 2);
        let (&id, pane) = compositor
            .panes
            .iter_mut()
            .find(|(id, _)| **id != before)
            .expect("a new pane");
        assert!(pane.program.ends_with("cat"), "ran {:?}", pane.program);
        assert_eq!(pane.title, "cat");
        assert!(pane.pty.is_alive());
        assert!(compositor.session.active().panes().contains(&id));
    }

    #[test]
    fn choosing_a_program_that_has_gone_says_so_instead_of_spawning() {
        let mut compositor = compositor();
        open_with(&mut compositor, &["definitely-not-a-program-1a2b3c"]);
        compositor.handle_input(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            tos_input::Modifiers::NONE,
        )));
        assert_eq!(compositor.panes.len(), 1);
        let message = compositor.message.as_ref().map(|(text, _)| text.clone());
        assert_eq!(
            message.as_deref(),
            Some("not found: definitely-not-a-program-1a2b3c")
        );
    }

    #[test]
    fn the_overlay_is_drawn_over_the_panes() {
        let mut compositor = compositor();
        compositor.inject(b"tOS");
        let mut framebuffer = tos_render::OwnedFramebuffer::new(640, 360);
        {
            let mut surface = framebuffer.surface();
            compositor.render_frame(&mut surface, true);
        }
        // The status bar already highlights the active workspace in the accent
        // colour, so this counts rather than asks whether any is present.
        let accent = compositor.chrome.accent.pack();
        let count = |fb: &tos_render::OwnedFramebuffer| {
            fb.pixels().iter().filter(|&&px| px == accent).count()
        };
        let before = count(&framebuffer);

        open_with(&mut compositor, &["ls", "vim"]);
        {
            let mut surface = framebuffer.surface();
            compositor.render_frame(&mut surface, true);
        }
        // The query cursor and the selected row are both accent coloured, and
        // both are far bigger than anything the status bar draws.
        assert!(
            count(&framebuffer) > before,
            "the overlay should be on top of the panes"
        );
    }

    #[test]
    fn rendering_produces_pixels() {
        let mut compositor = compositor();
        compositor.inject(b"tOS");
        let mut framebuffer = tos_render::OwnedFramebuffer::new(640, 360);
        {
            let mut surface = framebuffer.surface();
            compositor.render_frame(&mut surface, true);
        }
        let background = compositor.chrome.background.pack();
        assert!(
            framebuffer.pixels().iter().any(|&px| px != background),
            "the frame should not be blank"
        );
    }

    /// Transmit a two frame animation into the focused pane and start it.
    fn inject_animation(compositor: &mut Compositor) {
        let pixels = tos_term::graphics::encode_base64(&[0x80u8; 8 * 16 * 4]);
        compositor.inject(format!("\x1b_Ga=T,f=32,s=8,v=16,i=1;{pixels}\x1b\\").as_bytes());
        compositor.inject(format!("\x1b_Ga=f,f=32,s=8,v=16,i=1,z=40;{pixels}\x1b\\").as_bytes());
        compositor.inject(b"\x1b_Ga=a,i=1,r=1,z=40,s=3\x1b\\");
    }

    #[test]
    fn a_running_animation_shortens_the_wait_for_the_next_frame() {
        let mut compositor = compositor();
        assert_eq!(compositor.frame_timeout_ms(), IDLE_TIMEOUT_MS);
        inject_animation(&mut compositor);
        // The first tick only starts the clock; after it the wait is a gap.
        compositor.tick();
        let timeout = compositor.frame_timeout_ms();
        assert!(timeout > 0 && timeout <= 40, "waited {timeout}ms");
    }

    #[test]
    fn a_new_animation_frame_asks_for_a_repaint() {
        let mut compositor = compositor();
        inject_animation(&mut compositor);
        // The first tick starts the animation's clock.
        compositor.tick();
        let mut framebuffer = tos_render::OwnedFramebuffer::new(640, 360);
        {
            let mut surface = framebuffer.surface();
            compositor.render_frame(&mut surface, true);
        }
        assert!(!compositor.needs_render());

        // Step past the first frame's gap the way the tick does, but with a
        // time of the test's choosing.
        let focus = compositor.session.focus();
        let due = Instant::now() + Duration::from_millis(40);
        assert!(compositor
            .pane_mut(focus)
            .unwrap()
            .terminal
            .advance_animations(due));
        assert!(compositor.needs_render());
    }

    #[test]
    fn damage_is_cleared_after_a_frame() {
        let mut compositor = compositor();
        compositor.inject(b"hello");
        let mut framebuffer = tos_render::OwnedFramebuffer::new(640, 360);
        {
            let mut surface = framebuffer.surface();
            compositor.render_frame(&mut surface, true);
        }
        assert!(!compositor.needs_render());
    }
}
