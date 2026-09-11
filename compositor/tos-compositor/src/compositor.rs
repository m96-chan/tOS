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
use tos_render::{render, RenderOptions, Rect as PixelRect, Selection, Surface};
use tos_session::{Action, Axis, Keymap, PaneId, Rect, Resolution, Session};
use tos_term::TermEvent;

use crate::chrome::{self, Chrome, StatusItem};
use crate::config::Config;
use crate::pane::Pane;

/// How often the cursor and blinking text change phase.
const BLINK_INTERVAL: Duration = Duration::from_millis(530);
/// Longest a frame may wait when nothing is happening.
const IDLE_TIMEOUT_MS: i32 = 100;
/// How long a transient status message stays up.
const MESSAGE_TIMEOUT: Duration = Duration::from_secs(3);

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
    /// Selections stored by OSC 52 selector; 'c' is the clipboard.
    clipboard: HashMap<char, Vec<u8>>,
    blink_visible: bool,
    last_blink: Instant,
    message: Option<(String, Instant)>,
    needs_full_redraw: bool,
    running: bool,
    /// Pointer position in pixels, for mouse routing.
    pointer: (u32, u32),
    /// The pane a mouse button went down on.
    mouse_grab: Option<PaneId>,
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
            needs_full_redraw: true,
            running: true,
            pointer: (0, 0),
            mouse_grab: None,
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
        Pane::spawn(
            area,
            self.cell_size(),
            self.config.scrollback,
            self.config.command.as_deref(),
        )
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
            InputEvent::Mouse(mouse) => self.handle_mouse(mouse),
            InputEvent::PointerMotion { x, y } => {
                self.pointer = (x.max(0.0) as u32, y.max(0.0) as u32);
                false
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

    fn handle_mouse(&mut self, mouse: MouseEvent) -> bool {
        // Mouse coordinates arrive in pixels from the device layer.
        let (cw, ch) = self.cell_size();
        let (px, py) = (mouse.col as u32, mouse.row as u32);
        self.pointer = (px, py);
        let (cell_x, cell_y) = (px / cw, py / ch);

        let area = self.grid_area();
        let target = self
            .session
            .active()
            .geometry(area)
            .into_iter()
            .find(|(_, rect)| rect.contains(cell_x, cell_y));
        // A drag that started in a pane keeps going there even once the
        // pointer leaves it, which is what makes selection usable.
        let (pane_id, rect) = match (target, self.mouse_grab) {
            (Some(hit), None) => hit,
            (_, Some(grabbed)) => {
                let area = self
                    .session
                    .active()
                    .geometry(area)
                    .into_iter()
                    .find(|(id, _)| *id == grabbed);
                match area {
                    Some(hit) => hit,
                    None => return false,
                }
            }
            (None, None) => return false,
        };

        let mut changed = false;
        if mouse.action == MouseAction::Press && self.session.focus() != pane_id {
            self.session.set_focus(pane_id);
            self.needs_full_redraw = true;
            changed = true;
        }

        // Clamp into the pane so a drag past its edge still selects sensibly.
        let local = MouseEvent {
            col: cell_x.clamp(rect.x, rect.right().saturating_sub(1)).saturating_sub(rect.x) as usize,
            row: cell_y.clamp(rect.y, rect.bottom().saturating_sub(1)).saturating_sub(rect.y) as usize,
            ..mouse
        };

        let (tracking, alt_screen) = match self.panes.get(&pane_id) {
            Some(pane) => (
                pane.terminal.mouse(),
                pane.terminal.modes.alt_screen,
            ),
            None => return changed,
        };
        let _ = alt_screen;

        // Wheel events scroll the compositor's own scrollback unless the
        // program is tracking the mouse itself.
        if let Some(button) = mouse.button {
            if button.is_wheel() && !tracking.is_enabled() {
                if mouse.action != MouseAction::Press {
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
        let mut copied = None;
        let mut paste = false;
        if let Some(pane) = self.panes.get_mut(&pane_id) {
            match mouse.action {
                MouseAction::Press if mouse.button == Some(MouseButton::Left) => {
                    pane.selecting = true;
                    pane.selection = Some(Selection::new(
                        (local.col, local.row),
                        (local.col, local.row),
                        mouse.modifiers.alt(),
                    ));
                    self.mouse_grab = Some(pane_id);
                    changed = true;
                }
                MouseAction::Drag | MouseAction::Motion if pane.selecting => {
                    if let Some(selection) = &mut pane.selection {
                        selection.end = (local.col, local.row);
                    }
                    changed = true;
                }
                MouseAction::Release if pane.selecting => {
                    pane.selecting = false;
                    self.mouse_grab = None;
                    copied = pane.selected_text();
                    changed = true;
                }
                MouseAction::Press if mouse.button == Some(MouseButton::Middle) => {
                    paste = true;
                }
                _ => {}
            }
        }

        if let Some(text) = copied {
            self.clipboard.insert('c', text.into_bytes());
            self.message = Some(("copied".to_string(), Instant::now()));
        }
        if paste {
            let data = self.clipboard.get(&'c').cloned().unwrap_or_default();
            let text = String::from_utf8_lossy(&data).into_owned();
            self.paste_text(&text);
            changed = true;
        }
        changed
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
                let moved = self.session.move_focused_to_workspace(n);
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
                    self.clipboard.insert('c', text.into_bytes());
                    self.message = Some(("copied".to_string(), Instant::now()));
                    return true;
                }
                false
            }
            Action::Paste => {
                let data = self.clipboard.get(&'c').cloned().unwrap_or_default();
                let text = String::from_utf8_lossy(&data).into_owned();
                self.paste_text(&text);
                true
            }
            Action::BeginSelection => {
                let focus = self.session.focus();
                if let Some(pane) = self.panes.get_mut(&focus) {
                    let cursor = pane.terminal.cursor();
                    pane.selection =
                        Some(Selection::new((cursor.x, cursor.y), (cursor.x, cursor.y), false));
                    return true;
                }
                false
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

    fn scroll_focused(&mut self, lines: isize) -> bool {
        let focus = self.session.focus();
        match self.panes.get_mut(&focus) {
            // A negative delta means further back in history.
            Some(pane) => pane.terminal.scroll_display(-lines),
            None => false,
        }
    }

    fn split(&mut self, axis: Axis) -> bool {
        let Some(new_id) = self.session.split_focused(axis) else {
            return false;
        };
        let area = self
            .session
            .active()
            .geometry(self.grid_area())
            .into_iter()
            .find(|(id, _)| *id == new_id)
            .map(|(_, rect)| rect)
            .unwrap_or(Rect::new(0, 0, 80, 24));

        match self.spawn_pane(area) {
            Ok(pane) => {
                self.panes.insert(new_id, pane);
                self.sync_layout();
                self.needs_full_redraw = true;
                true
            }
            Err(e) => {
                // The layout must not keep a pane with no process behind it.
                self.session.close_pane(new_id);
                self.sync_layout();
                self.report_error("split", e);
                true
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
        }
        self.sync_layout();
        self.needs_full_redraw = true;
    }

    fn report_error(&mut self, what: &str, error: io::Error) {
        self.message = Some((format!("{what} failed: {error}"), Instant::now()));
    }

    // ---- rendering ------------------------------------------------------

    /// Advance the blink phase. Returns true when it changed.
    pub fn tick(&mut self) -> bool {
        let mut changed = false;
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

        for (id, rect) in &geometry {
            let Some(pane) = self.panes.get(id) else {
                continue;
            };
            // A pane that is synchronising its output asked not to be drawn
            // mid-update, so the previous frame stays on screen.
            if pane.terminal.modes.synchronized_output && !force {
                continue;
            }
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
                selection: pane.selection,
                selection_background: self.chrome.accent,
                force,
                inactive_fade: self.config.inactive_fade,
            };
            render(surface, pixel_rect, &pane.terminal, &mut self.fonts, &options);
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

        for pane in self.panes.values_mut() {
            pane.terminal.clear_damage();
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

    /// Run one iteration: poll, read, handle, and render if needed.
    pub fn run_once(
        &mut self,
        display: &mut dyn Display,
        input_fds: &[std::os::unix::io::RawFd],
        mut read_input: impl FnMut(std::os::unix::io::RawFd) -> Vec<InputEvent>,
    ) -> io::Result<()> {
        let mut fds = self.pty_fds();
        fds.extend_from_slice(input_fds);
        let ready = tos_platform::tty::poll_readable(&fds, IDLE_TIMEOUT_MS)?;

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
