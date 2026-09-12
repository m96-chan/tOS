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
    encode_focus, encode_key, encode_mouse, encode_paste, InputEvent, KeyEvent, MouseAction,
    MouseButton, MouseEvent,
};
use tos_platform::Display;
use tos_render::{render, Rect as PixelRect, RenderOptions, Surface};
use tos_session::{describe, Action, Axis, Keymap, PaneId, Rect, Resolution, Session};
use tos_system::power::PowerAction;
use tos_system::Sysfs;
use tos_term::TermEvent;

use crate::chrome::{self, Chrome, StatusItem};
use crate::config::Config;
use crate::launcher;
use crate::lock::{self, LockOutcome, LockScreen};
use crate::notify::{self, Chosen, Notifications};
use crate::overlay::{Overlay, OverlayItem, OverlayOutcome};
use crate::pane::Pane;
use crate::power;
use crate::selection::{Selection, SelectionMode};
use crate::system::Machine;

/// How often the cursor and blinking text change phase.
const BLINK_INTERVAL: Duration = Duration::from_millis(530);
/// Longest a frame may wait when nothing is happening.
const IDLE_TIMEOUT_MS: i32 = 100;
/// Longest it may wait when the screen is dark.
///
/// Nothing on a blanked screen can need repainting, so the only thing worth
/// coming back for is a deadline of its own, and those are folded into the
/// wait below and win whenever they are nearer. This is what is left when
/// there is none: a heartbeat rather than a poll loop. Not unbounded, because
/// the loop is also where `main` looks at the flags its signal handlers set
/// and where a pane that has gone is noticed, and a dark screen should never
/// be the reason either of those takes a noticeable while.
const BLANKED_TIMEOUT_MS: i32 = 60_000;
/// How long to wait when a pane still has input queued for its child.
const WRITE_RETRY_TIMEOUT_MS: i32 = 4;
/// The most a program may put into one selection with OSC 52. The clipboard
/// carries text a person copies and pastes, and 64 KiB is already a thousand
/// full lines — far more than anyone pastes into a shell. The cap is what
/// keeps a pane from parking megabytes in the compositor that nobody will
/// ever paste.
const MAX_CLIPBOARD_BYTES: usize = 64 * 1024;
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
    /// What has been said on the status bar, newest first.
    Notifications,
    /// A line of text: the new name for the active workspace.
    RenameWorkspace,
    /// The key bindings, which are a list to read rather than to choose from.
    Bindings,
    /// The three ways a machine stops: power off, reboot, suspend.
    Power,
    /// The second half of a power off or a reboot: the menu that has to be
    /// answered before it happens. The action is carried in the kind rather
    /// than looked up again from the row, so that the thing being confirmed is
    /// decided once, by the menu that asked.
    ConfirmPower(PowerAction),
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
    /// Everything the compositor and its panes have had to say, queued for the
    /// status bar and kept for the history list.
    notifications: Notifications,
    /// The open menu, if any. While it is open it owns the keyboard.
    overlay: Option<(OverlayKind, Overlay)>,
    /// The lock screen, if the session is locked. While it is up it owns the
    /// input — every kind of it, not only the keys — and the screen shows
    /// nothing of the session.
    lock: Option<LockScreen>,
    /// The last pane died while the screen was locked.
    ///
    /// Ending the session is a way out of a locked screen, so a locked one
    /// cannot be allowed to take it. The session ends when the password is
    /// accepted instead.
    session_ended_while_locked: bool,
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
    /// When somebody last did anything. Input only: see [`Compositor::handle_input`].
    last_activity: Instant,
    /// Whether the display has been told to stop showing anything.
    ///
    /// What the display was last told, rather than what it is about to be
    /// told, so that the keystroke which arrives at a panel that is still
    /// physically dark is recognised as one.
    blanked: bool,
    /// Whether the lock deadline has already been acted on this idle period.
    ///
    /// A deadline fires once, not on every pass after it. Without this a
    /// machine with no password would read the credential file and find it
    /// missing a few times a second for as long as nobody touched it.
    idle_lock_done: bool,
    /// The display refused to go dark, so this idle period stops asking.
    blank_refused: bool,
    /// The machine underneath the session: battery, link, volume and adapter,
    /// re-read on a timer rather than on damage, because nothing a person does
    /// to a pane is what makes a cable go in. See [`crate::system`].
    machine: Machine,
    /// A suspend has been agreed to and has not happened yet.
    ///
    /// The compositor cannot carry one out itself: giving up DRM master and
    /// the input grabs is the loop's business, because the loop is what holds
    /// the display and the devices. So this is a flag the loop takes, the same
    /// shape as the VT switch the kernel asks about. See [`crate::power`].
    suspend_requested: bool,
    /// How this session is ending, when it is ending because somebody asked
    /// the machine to stop rather than asked tOS to.
    ///
    /// Kept rather than acted on for the same reason, and for one more: a
    /// `reboot(2)` from inside the loop would leave the console in graphics
    /// mode with its keyboard off, so whatever the kernel says on the way down
    /// — including why it could not unmount something — would be said onto a
    /// screen nobody can read. The session ends first, the terminal and the
    /// display go back, and only then does the machine stop.
    shutdown: Option<PowerAction>,
}

impl Compositor {
    /// Build a compositor for a display of this size.
    pub fn new(
        config: Config,
        size: (u32, u32),
        physical_mm: Option<(u32, u32)>,
    ) -> io::Result<Self> {
        let fonts = build_fonts(&config, size, physical_mm);
        let mut compositor = Compositor {
            session: Session::new(),
            panes: HashMap::new(),
            fonts,
            keymap: Keymap::default_bindings(),
            chrome: config.chrome,
            size,
            clipboard: HashMap::new(),
            blink_visible: true,
            last_blink: Instant::now(),
            notifications: Notifications::new(),
            overlay: None,
            lock: None,
            session_ended_while_locked: false,
            needs_full_redraw: true,
            running: true,
            pointer: (0, 0),
            mouse_grab: None,
            last_click: None,
            pending_writes: false,
            last_activity: Instant::now(),
            blanked: false,
            idle_lock_done: false,
            blank_refused: false,
            machine: Machine::at(Sysfs::new(&config.system_root)),
            suspend_requested: false,
            shutdown: None,
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
        Pane::spawn(
            area,
            self.cell_size(),
            self.config.scrollback,
            &self.config.palette,
            command,
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
        let status = if self.config.status_bar && rows > 2 {
            1
        } else {
            0
        };
        Rect::new(0, 0, cols, rows - status)
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    /// The machine underneath: what the last poll found, and the seams that
    /// change it.
    pub fn machine(&self) -> &Machine {
        &self.machine
    }

    pub fn machine_mut(&mut self) -> &mut Machine {
        &mut self.machine
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

    pub fn notifications(&self) -> &Notifications {
        &self.notifications
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
                    self.notifications
                        .status("input dropped: pane is not reading");
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
                    self.notifications.from_pane(id, "", "bell");
                    changed = true;
                }
                TermEvent::Notify { title, body } => {
                    // Which pane asked is part of the notification: it is the
                    // one thing the application cannot say for itself, and the
                    // history list uses it to take you there.
                    self.notifications.from_pane(id, title, body);
                    changed = true;
                }
                TermEvent::ClipboardStore { selection, data } => {
                    // An oversized selection is dropped whole rather than
                    // truncated: half a copied command line is exactly the
                    // kind of thing that does damage when it lands in a
                    // shell, and what the user already had is worth more than
                    // a mangled replacement. They are told, because from the
                    // pane's side the copy looked like it worked.
                    if data.len() > MAX_CLIPBOARD_BYTES {
                        self.notifications.status(format!(
                            "clipboard write refused in pane {}: over {} KiB",
                            id.0 + 1,
                            MAX_CLIPBOARD_BYTES / 1024
                        ));
                        changed = true;
                    } else if is_clipboard_selector(selection) {
                        self.clipboard.insert(selection, data);
                    }
                }
                TermEvent::ClipboardLoad { selection } => {
                    // Reads are off unless the user asked for them. The query
                    // is just bytes on a PTY, so a `cat` of a hostile file, or
                    // anything running over ssh in that pane, can send it, and
                    // what comes back is whatever was last copied — a password
                    // as readily as a path. xterm and kitty refuse by default
                    // for the same reason.
                    //
                    // The refusal is an empty selection rather than silence:
                    // OSC 52 has no way to spell "no", a program that gets
                    // nothing back waits out its own timeout, and an empty
                    // answer is both indistinguishable from an empty clipboard
                    // and a case every reader already handles.
                    let data = if self.config.allow_clipboard_read {
                        self.clipboard.get(&selection).cloned().unwrap_or_default()
                    } else {
                        self.notifications
                            .status(format!("clipboard read refused in pane {}", id.0 + 1));
                        changed = true;
                        Vec::new()
                    };
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
        // Activity is input, and only input. A pane producing output is not a
        // person being present: a `tail -f` on a log that turns over all night
        // would hold the screen on and the lock off for as long as the machine
        // kept running, and an idle timer that a program can hold open is not
        // one. Every kind of input counts, because every kind of it is
        // somebody doing something — a key, the mouse, a paste, and the host
        // terminal saying its window has been switched to.
        self.last_activity = Instant::now();
        // The idle period ends here, so the deadlines in it are owed another
        // turn — including the one the display refused, which may have been
        // refusing for a reason that has since gone away.
        self.idle_lock_done = false;
        self.blank_refused = false;

        // A dark screen takes the event that woke it and gives it to nobody.
        // Whoever sent it could not see what they were aiming at, and the
        // panel is still off at this moment — it comes back at the end of this
        // pass — so every event that arrived while it was off was sent blind.
        // A key let through would go to whatever program has the focus, where
        // `q`, `space` and `enter` each mean something, and to the lock, where
        // it would be the first character of a password the field is not
        // showing yet. Swallowing costs a keystroke; letting it through costs
        // whatever the keystroke did.
        if self.blanked {
            return true;
        }
        // The gate is here and not in `handle_key`, which is where an overlay
        // puts its own. An overlay owns the keyboard; a lock has to own the
        // input, and the two are not the same thing: `Mouse`, `Pointer` and
        // `Paste` are routed below without ever passing through `handle_key`.
        // A lock gated one level down would let a middle click paste the
        // primary selection into a shell, let a drag select and copy what is
        // on screen, and let the host terminal's bracketed paste type for the
        // person who is not there.
        if self.lock.is_some() {
            return self.locked_input(event);
        }
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

    /// Everything that arrives while the screen is locked.
    ///
    /// One rule with no exceptions: a key goes to the lock, and nothing else
    /// goes anywhere. Focus notifications are dropped along with the rest —
    /// telling a pane it has the focus is still writing to a pane on behalf of
    /// somebody who has not proved who they are, and an exception is how a
    /// gate stops being one.
    fn locked_input(&mut self, event: InputEvent) -> bool {
        let InputEvent::Key(key) = event else {
            return false;
        };
        let Some(lock) = &mut self.lock else {
            return false;
        };
        match lock.handle_key(&key, Instant::now()) {
            LockOutcome::Consumed => false,
            LockOutcome::Changed => true,
            LockOutcome::Unlocked => {
                self.unlock();
                true
            }
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
        // The leader indicator is drawn from the keymap rather than queued as
        // a notification: arming the leader is not news, and a message that
        // says so would cost whatever is in the queue its turn on screen. The
        // keypress that disarms it still needs a frame, though, even when the
        // action it ran changed nothing else.
        let was_armed = self.keymap.is_pending();
        let changed = match self.keymap.resolve(&key) {
            Resolution::Action(action) => self.perform(action),
            Resolution::Pending => true,
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
        };
        changed || was_armed != self.keymap.is_pending()
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
            Action::RenameWorkspace => {
                // The prompt opens on the name the workspace has now, which is
                // both the value to edit and the only place it is written
                // down; clearing the line is how the number is asked back.
                let name = self.session.active().name.clone();
                self.open_overlay(
                    OverlayKind::RenameWorkspace,
                    Overlay::prompt("rename workspace", name),
                );
                true
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
                    self.notifications.status("copied");
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
            Action::ShowNotifications => {
                let items = self.notifications.open_history(Instant::now());
                let overlay = Overlay::new("notifications", items);
                self.open_overlay(OverlayKind::Notifications, overlay);
                true
            }
            Action::ShowBindings => {
                self.open_overlay(OverlayKind::Bindings, self.binding_sheet());
                true
            }
            Action::Refresh => {
                self.needs_full_redraw = true;
                true
            }
            Action::Lock => self.lock_session(),
            Action::PowerMenu => {
                self.open_overlay(OverlayKind::Power, power::menu());
                true
            }
            Action::Quit => {
                self.running = false;
                true
            }
        }
    }

    // ---- the lock -------------------------------------------------------

    /// Put the lock screen up, or say why there is no lock to put up.
    ///
    /// Public because the binding is not the only way in: an idle deadline
    /// reaches the same state machine, and a test reaches it without
    /// synthesising a keypress.
    ///
    /// The credential is read here rather than when a password is offered, so
    /// that a machine with no password never gets a locked screen at all. That
    /// one rule is the whole of what makes the live ISO behave: nothing in the
    /// compositor knows what live media is, only that this machine was never
    /// given a password to unlock with.
    pub fn lock_session(&mut self) -> bool {
        if self.lock.is_some() {
            return false;
        }
        match lock::read_credential(&self.config.credential) {
            Ok(hash) => {
                self.engage_lock(hash);
                true
            }
            Err(why) => {
                self.notifications.status(format!("cannot lock: {why}"));
                true
            }
        }
    }

    /// Put the lock up because a deadline came due rather than because
    /// somebody asked.
    ///
    /// Identical to the binding except on a machine with no password, and
    /// that difference is the point. The binding says so, because a person
    /// pressed a key and is owed an answer. This says nothing: nobody asked,
    /// nothing is wrong, and a live ISO would otherwise find "cannot lock"
    /// waiting on the status bar every time its user walked away from it.
    /// Such a session blanks and stays unlocked, which is "no credential, no
    /// lock" arriving by the other road.
    fn lock_on_idle(&mut self) -> bool {
        match lock::read_credential(&self.config.credential) {
            Ok(hash) => {
                self.engage_lock(hash);
                true
            }
            Err(_) => false,
        }
    }

    fn engage_lock(&mut self, hash: String) {
        // A half-pressed leader and a drag in progress both belong to the
        // person who was here before; neither should still be going when the
        // session comes back.
        self.keymap.cancel_pending();
        self.release_grab();
        self.lock = Some(LockScreen::new(hash));
        // An open overlay is left exactly as it was, under the lock rather
        // than closed by it. Nothing of it is drawn while the lock is up, and
        // the person who gets it back is the person who left it there.
        self.needs_full_redraw = true;
    }

    /// Whether the session is locked, which the DRM loop asks before it agrees
    /// to a VT switch and before it lets go of the display.
    pub fn is_locked(&self) -> bool {
        self.lock.is_some()
    }

    pub fn lock_screen(&self) -> Option<&LockScreen> {
        self.lock.as_ref()
    }

    /// The one way out, and there is no other.
    fn unlock(&mut self) {
        self.lock = None;
        // Nothing under the lock was drawn while it was up, and the damage
        // that would have said what to repaint was thrown away with each
        // locked frame. The whole screen is the only honest answer.
        self.needs_full_redraw = true;
        // A pane that died while the screen was locked could not be allowed to
        // end the session then. It ends it now.
        if self.session_ended_while_locked {
            self.running = false;
        }
    }

    // ---- idle -----------------------------------------------------------

    /// Whether the screen is dark because nobody has been here.
    pub fn is_blanked(&self) -> bool {
        self.blanked
    }

    /// Act on the idle deadlines, and bring the screen back the moment
    /// somebody is here again. Returns true when a frame is needed.
    ///
    /// Nothing here can fail in a way that ends a session. A display that
    /// will not blank leaves one lit, which is a thing to say rather than a
    /// thing to stop for.
    ///
    /// The time arrives as an argument for the reason the lock's does: five
    /// minutes from now has to be somewhere a test can stand without spending
    /// five minutes getting there.
    pub fn apply_idle(&mut self, now: Instant, display: &mut dyn Display) -> bool {
        let idle = now.saturating_duration_since(self.last_activity);
        let mut changed = false;

        // The lock is looked at before the blank, and the order is load
        // bearing on the pass where both are due — because they were given the
        // same interval, or because the loop was away while another VT had the
        // screen. Unblanking puts back the last frame that was drawn, so the
        // last frame drawn before the screen goes dark must never be the
        // session of somebody who is not here.
        if !self.idle_lock_done
            && self.lock.is_none()
            && self.config.idle_lock.is_some_and(|after| idle >= after)
        {
            self.idle_lock_done = true;
            changed |= self.lock_on_idle();
        }

        let dark = !self.blank_refused && self.config.idle_blank.is_some_and(|after| idle >= after);
        if dark != self.blanked {
            match display.blank(dark) {
                // Remembered only once the display agrees. A screen that
                // could not be put to sleep is still lit, and one remembered
                // as dark would swallow the next keystroke to wake a panel
                // that was never off.
                Ok(()) => {
                    self.blanked = dark;
                    if !dark {
                        // A backend that put the panel to sleep decides for
                        // itself what is on it when it wakes, and painting all
                        // of it is the only thing the session can do about
                        // that.
                        self.needs_full_redraw = true;
                        // The cursor comes back lit. Where the caret is, is
                        // the first thing anyone looks for on a screen they
                        // have just woken.
                        self.blink_visible = true;
                        self.last_blink = now;
                        changed = true;
                    }
                }
                Err(e) => {
                    // A screen that will not go out is not a reason to end
                    // somebody's session, which is what letting this error
                    // reach the loop would do. Say so, and stop asking: a
                    // display that has refused once will refuse again, and
                    // asking it on every pass is how a machine nobody is
                    // using becomes a machine that is busy all night.
                    self.report_error("blanking the display", e);
                    self.blank_refused = true;
                }
            }
        }
        changed
    }

    /// How long until the next idle deadline, when one is still to come.
    ///
    /// A deadline that has already been acted on is not waited for. The blank
    /// is level triggered off `blanked`, so it drops out once the screen is
    /// dark; the lock fires once per idle period, so a machine with no
    /// password does not spend the night rediscovering that it has none.
    fn next_idle_deadline(&self, now: Instant) -> Option<Duration> {
        let idle = now.saturating_duration_since(self.last_activity);
        let blank = self
            .config
            .idle_blank
            .filter(|_| !self.blanked && !self.blank_refused);
        let lock = self
            .config
            .idle_lock
            .filter(|_| self.lock.is_none() && !self.idle_lock_done);
        [blank, lock]
            .into_iter()
            .flatten()
            .map(|after| after.saturating_sub(idle))
            .min()
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
                self.choose(kind, Some(index), &label);
                true
            }
            OverlayOutcome::Accepted => {
                let text = overlay.query().to_string();
                self.close_overlay();
                self.choose(kind, None, &text);
                true
            }
        }
    }

    /// Act on what an overlay reported: the row that was chosen, or the line
    /// a prompt accepted. One arm per menu.
    ///
    /// Both the row's position and its text are passed, because a menu built
    /// out of names is answered by name and a menu built out of a list the
    /// compositor already holds is answered by position. A prompt has no row,
    /// which is what `None` means.
    fn choose(&mut self, kind: OverlayKind, row: Option<usize>, label: &str) {
        match kind {
            OverlayKind::Launcher => self.launch(label),
            OverlayKind::Notifications => {
                let Some(index) = row else { return };
                match self.notifications.choose(index) {
                    Chosen::Clear => self.notifications.clear_history(),
                    // Where a notification came from is the useful thing to do
                    // with it: a build that finished is a pane to go and look
                    // at, wherever that pane has ended up.
                    Chosen::Pane(pane) => {
                        if self.session.set_focus(pane) {
                            self.sync_layout();
                            self.needs_full_redraw = true;
                        }
                    }
                    Chosen::Nowhere => {}
                }
            }
            // Closing the overlay has already asked for the frame that puts
            // the new name in the status bar.
            OverlayKind::RenameWorkspace => self.session.rename_active(label),
            // Nothing to choose: the sheet is there to be read, so enter
            // closes it the way escape does.
            OverlayKind::Bindings => {}
            OverlayKind::Power => {
                // A row that names nothing is a menu that has been rebuilt
                // wrong; doing nothing is the only safe answer on this menu.
                let Some(action) = power::action_named(label) else {
                    return;
                };
                if power::needs_confirming(action) {
                    self.open_overlay(
                        OverlayKind::ConfirmPower(action),
                        power::confirmation(action),
                    );
                    return;
                }
                self.request_power(action);
            }
            // Only the row that names the action goes ahead; every other
            // answer, including escape and the enter that opened this, leaves
            // the session alone. See [`crate::power::confirmation`].
            OverlayKind::ConfirmPower(action) => {
                if power::confirmed(action, label) {
                    self.request_power(action);
                }
            }
        }
    }

    /// The cheat sheet, built from the keymap that is resolving these keys.
    ///
    /// Not from the `--help` text, and not from a copy of the defaults: a
    /// sheet that is a second telling of the bindings is one that will
    /// eventually be telling you about a key that no longer does that. This
    /// one cannot be wrong, and a keymap that was customised at startup
    /// describes itself here without anything being taught about it.
    fn binding_sheet(&self) -> Overlay {
        let title = match describe::leader_name(&self.keymap) {
            Some(leader) => format!("key bindings (leader {leader})"),
            None => "key bindings".to_string(),
        };
        // The description is the label, so that typing "split" finds the key
        // rather than only the other way round: what you have forgotten is
        // the key, and what you can still name is what you wanted to do.
        let items = describe::cheat_sheet(&self.keymap)
            .into_iter()
            .map(|row| OverlayItem::with_detail(row.action, row.keys))
            .collect();
        Overlay::new(title, items)
    }

    /// Open a pane running `program`, using the same path a split does.
    fn launch(&mut self, program: &str) {
        if tos_pty::which(program).is_none() {
            // The list came from $PATH, so this means it went away in between;
            // spawning would leave a pane that dies on its own.
            self.notifications.status(format!("not found: {program}"));
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
            self.notifications.status("no room to split");
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
            // Quitting is a way out of a locked screen, and a program exiting
            // is a way to quit that does not go through a binding: a shell
            // that reaches its end of file while nobody is there would
            // otherwise hand the machine back. The session is over, but it
            // does not end until somebody says who they are.
            if self.lock.is_some() {
                self.session_ended_while_locked = true;
            } else {
                self.running = false;
            }
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
        self.notifications.status(format!("{what} failed: {error}"));
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
        // The blink phase stands still while the screen is dark, for the
        // reason the queue below does: a cursor nobody can see does not need
        // to be somewhere in particular, and flipping it would repaint the
        // whole session behind the blank twice a second.
        if !self.blanked && self.last_blink.elapsed() >= BLINK_INTERVAL {
            self.blink_visible = !self.blink_visible;
            self.last_blink = Instant::now();
            changed = true;
        }
        // A notification only spends its time on screen while it is on screen:
        // the leader indicator has the slot while the leader is armed, and one
        // shown there instead would be one nobody read.
        // A locked screen draws no status bar and no banner, so a message
        // that spent its time there would have spent it unread. The queue
        // stands still until the session comes back, which is the same reason
        // the leader indicator holds it: time on screen means on screen. A
        // blanked screen is the same case again — there is nothing on it, and
        // it does not matter whose decision that was.
        if self.lock.is_none()
            && !self.blanked
            && !self.keymap.is_pending()
            && self.notifications.advance(now)
        {
            // Without a status bar the notification is a banner over the panes,
            // and the cells it covered are only repainted on damage they have
            // not got. Retiring it has to uncover them.
            self.needs_full_redraw |= !self.config.status_bar;
            changed = true;
        }
        // The machine moves without anybody touching the session: a battery
        // drains, a charger comes out, a link goes down. Nothing in the panes
        // is damaged by any of it, so this poll is the only thing that would
        // ever ask for the frame those changes belong on.
        if self.machine.poll(now, self.blanked) {
            changed = true;
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
        if self.lock.is_some() {
            self.render_locked(surface);
            return;
        }
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
        } else if let Some(text) = self.notifications.status_line() {
            // With no bar there is nowhere for a message to live, and going
            // quiet is the one thing it must not do: this used to be why
            // `--no-status-bar` made a failed split look like a dead key.
            let over = PixelRect::new(0, 0, area.width * cw, ch);
            notify::draw_banner(surface, &mut self.fonts, over, &self.chrome, &text);
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

    /// Paint a locked frame: the lock, and nothing else at all.
    ///
    /// The panes, the dividers, the status bar, the notification banner and
    /// any open menu are all skipped rather than painted over. Painting over
    /// is not erasing: a frame that is not a full redraw only repaints the
    /// cells a pane marked as damaged, so a box drawn on top of a session
    /// leaves every undamaged cell of that session exactly where it was, and
    /// the status bar underneath it goes on saying what the panes are called.
    ///
    /// The clear happens on every locked frame and not only the first, because
    /// a display with two buffers hands out the other one next time and a
    /// clear that ran once cleared one of them.
    fn render_locked(&mut self, surface: &mut Surface<'_>) {
        surface.clear(self.chrome.background);
        let area = PixelRect::new(0, 0, self.size.0, self.size.1);
        if let Some(lock) = &self.lock {
            lock.draw(surface, &mut self.fonts, area, &self.chrome, Instant::now());
        }
        // The panes are still running and still marking damage nobody is
        // drawing. Dropping it is what lets the loop idle instead of finding
        // work outstanding on every pass, and nothing is lost by it: unlocking
        // asks for a full redraw, which repaints every cell whatever the
        // damage says.
        for pane in self.panes.values_mut() {
            pane.terminal.clear_damage();
        }
        self.needs_full_redraw = false;
    }

    /// What the left of the status bar says: every workspace's name, in
    /// position order, with the active one marked.
    ///
    /// Separate from the drawing so that a test can read the bar's own words
    /// rather than infer them from pixels.
    fn status_items(&self) -> Vec<StatusItem> {
        let active = self.session.active_index();
        self.session
            .workspaces()
            .iter()
            .enumerate()
            .map(|(i, workspace)| StatusItem::new(workspace.name.clone(), i == active))
            .collect()
    }

    fn draw_status(&mut self, surface: &mut Surface<'_>, area: Rect, cell_height: u32) {
        let items = self.status_items();

        let focus = self.session.focus();
        // The leader indicator comes first: it is the state of the keyboard
        // right now and it lasts only until the next key. Then the queue, and
        // when it is empty, what the focused pane is.
        let right = if self.keymap.is_pending() {
            "leader".to_string()
        } else if let Some(line) = self.notifications.status_line() {
            line
        } else {
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
    /// keeps its own pace instead of the poll timer's, and the idle deadlines
    /// pull it in the same way, so that a session a minute from locking locks
    /// on the minute rather than up to a tenth of a second later.
    fn frame_timeout_ms(&self) -> i32 {
        self.frame_timeout_ms_at(Instant::now())
    }

    fn frame_timeout_ms_at(&self, now: Instant) -> i32 {
        // The wait when nothing at all is due, which is longer while the
        // screen is dark: a blanked session has no blink phase to flip, no
        // notification counting down its time on screen, and no animation
        // frame anybody could see, so the only reason left to come back is a
        // deadline, and those are folded in below.
        let longest = if self.blanked {
            BLANKED_TIMEOUT_MS
        } else {
            IDLE_TIMEOUT_MS
        };
        let animation = self
            .panes
            .values()
            .filter_map(|pane| pane.terminal.next_animation_delay(now))
            .min()
            .filter(|_| !self.blanked);
        // The poll is a deadline like the others: folded into the wait rather
        // than given a timer, so a clock turns over on the second instead of
        // up to a tenth of one after it. Skipped while the screen is dark,
        // because a dark screen is not polled either.
        let poll = self.machine.next_poll(now).filter(|_| !self.blanked);
        let soonest = [animation, poll, self.next_idle_deadline(now)]
            .into_iter()
            .flatten()
            .min();
        match soonest {
            Some(delay) => delay.as_millis().clamp(1, longest as u128) as i32,
            None => longest,
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
        // After the input, so that the event which arrived at a dark screen is
        // swallowed by the screen still being dark, and before the frame, so
        // that the frame this pass paints is the one a blanked display shows
        // when it comes back.
        dirty |= self.apply_idle(Instant::now(), display);

        if dirty || self.needs_render() {
            let retained = display.retains_contents();
            display.frame(&mut |surface| self.render_frame(surface, retained))?;
        }
        Ok(())
    }

    // ---- power ----------------------------------------------------------

    /// Act on a power action that has been asked for and, where it needed one,
    /// agreed to.
    ///
    /// Public because the menu is not the only way in: a test reaches it
    /// without driving two overlays, and a lid switch or a power button would
    /// arrive here too. It is also the one place that decides which of the
    /// three the session is expected to survive, which is the difference
    /// between the two fields it sets.
    pub fn request_power(&mut self, action: PowerAction) -> bool {
        match action {
            PowerAction::Suspend => {
                // Locked before the machine sleeps rather than after it wakes.
                // Somebody who suspends a laptop is shutting the lid and
                // walking away from it, and the session has to be behind the
                // password by the time anything can be on the screen again; a
                // lock applied on the way back is one that races whoever
                // pressed the key to wake it. This is `lock_on_idle` and not
                // `lock_session` because a machine with no password has no
                // lock to offer, and "cannot lock" is not an answer to
                // somebody who asked for a suspend.
                self.lock_on_idle();
                self.suspend_requested = true;
                true
            }
            ending => {
                self.shutdown = Some(ending);
                // Ending the loop is what gets the console and the display
                // handed back before [`Compositor::shut_down`] stops the
                // machine. The panes are not asked anything: there is nothing
                // to ask them with, which is what the confirmation said.
                self.running = false;
                true
            }
        }
    }

    /// Whether a suspend has been asked for since this was last called.
    ///
    /// Edge triggered, like [`tos_platform::take_switch_away`] and for the
    /// same reason: the loop acts on it in a place where the display and the
    /// input devices can safely be given up, and a flag that stayed set would
    /// put the machine to sleep again the moment it woke.
    pub fn take_suspend_request(&mut self) -> bool {
        std::mem::take(&mut self.suspend_requested)
    }

    /// How this session is ending, when it is ending by request. `None` for a
    /// session that stopped for any of the other reasons.
    pub fn shutdown_request(&self) -> Option<PowerAction> {
        self.shutdown
    }

    /// The machine is awake again.
    ///
    /// Everything the sleep invalidated, put back in one place: the loop has
    /// already reclaimed the screen and the keyboard by the time this is
    /// called, and this is the session's half of the same resume.
    pub fn resumed(&mut self, outcome: &power::Outcome) {
        // The CRTC has been set again from nothing, so there is no previous
        // frame on the screen for a partial repaint to build on — and the
        // damage that would have said which cells to repaint was collected
        // against a screen that no longer exists.
        self.needs_full_redraw = true;
        let now = Instant::now();
        // Where the caret is, is the first thing anyone looks for on a screen
        // they have just brought back. The same reason unblanking does it.
        self.blink_visible = true;
        self.last_blink = now;
        // Waking a machine is somebody being there, so the idle period starts
        // again from here. Nothing was missed while it slept: `Instant` is
        // `CLOCK_MONOTONIC`, which does not count time spent suspended, so the
        // deadlines stood still along with everything else.
        self.last_activity = now;
        self.idle_lock_done = false;
        self.blank_refused = false;
        // Read the machine now rather than within the second the poll would
        // take: a laptop that went to sleep on its charger and woke off it
        // would otherwise still be showing last night's battery.
        self.machine.refresh(now);
        // A suspend that did not happen, or a screen that came back wrong, is
        // a thing to say rather than a thing to end a session over — the same
        // rule the display that will not blank follows.
        for problem in &outcome.problems {
            self.notifications
                .status(format!("{}: {}", problem.step.what(), problem.message));
        }
    }

    /// Carry out the ending this session was given, if it was given one.
    ///
    /// Called by the loop after it has stopped and handed the console and the
    /// display back, which is the whole reason it is separate from asking for
    /// it: `reboot(2)` does not return on success, so anything that has to
    /// happen before the machine stops has to have happened before this call.
    /// An error means the syscall was refused — tOS without `CAP_SYS_BOOT`, a
    /// container — and by then there is no status bar left to say so on, which
    /// is why this is an error to print rather than a notification to queue.
    pub fn shut_down(&mut self) -> io::Result<()> {
        let Some(action) = self.shutdown.take() else {
            return Ok(());
        };
        // `request` is what puts the sync in front of it. There is no init
        // here to flush the filesystems on the way down, so an unsynced
        // poweroff loses whatever the panes were writing.
        tos_system::power::request(self.machine.power(), action)
    }
}

/// The selectors OSC 52 defines: the clipboard, primary, secondary, select,
/// and the eight cut buffers. The selector arrives as a raw character, so
/// without this a pane could invent a new one per store and grow the clipboard
/// map for as long as it liked; with it the map holds at most thirteen
/// selections of [`MAX_CLIPBOARD_BYTES`] each.
fn is_clipboard_selector(selection: char) -> bool {
    matches!(selection, 'c' | 'p' | 'q' | 's' | '0'..='7')
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
    let cell = stack.metrics();
    for path in &config.font_fallback {
        // A fallback that will not load is worth saying so about: the person
        // named this file, unlike the ones found by searching.
        match tos_font::TtfFont::from_path(path, pixel_size) {
            Ok(font) => stack.push_fallback(Box::new(font.fit_wide_cell(cell))),
            Err(e) => eprintln!("tos: font fallback {}: {e}", path.display()),
        }
    }
    // Without kanji somewhere in the stack Japanese is a row of hollow boxes,
    // so a face is looked for even when nothing asked for one. The ISO's own
    // face is monospace and covers kana, so this usually finds nothing to do.
    if !stack.covers(tos_font::ttf::FULL_WIDTH_PROBE) {
        if let Some(font) = tos_font::TtfFont::system_cjk(pixel_size) {
            stack.push_fallback(Box::new(font.fit_wide_cell(cell)));
        }
    }
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
        compositor_with(Config::default())
    }

    /// Build a compositor on top of a config, filling in the parts every test
    /// wants: a child that only sleeps, and the built-in bitmap font.
    fn compositor_with(config: Config) -> Compositor {
        let config = Config {
            command: Some(vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()]),
            bitmap_scale: Some(1),
            font: Some("/nonexistent-so-the-bitmap-font-is-used".into()),
            // A machine with no battery, no card, no link and no adapter, so
            // that what a test asserts about the session is not the
            // developer's laptop showing through — and so that nothing here
            // can reach a real sound card.
            system_root: "/nonexistent-so-this-machine-has-no-hardware".into(),
            ..config
        };
        Compositor::new(config, (640, 360), None).expect("compositor")
    }

    /// A font stack built from `config`, at a size the tests can reason about.
    fn fonts(config: Config) -> FontStack {
        build_fonts(&config, (640, 360), None)
    }

    /// Nothing to assert about CJK on a machine with no CJK face.
    fn a_cjk_face() -> Option<std::path::PathBuf> {
        tos_font::TtfFont::find_system_cjk_font()
    }

    #[test]
    fn kanji_are_found_without_being_configured() {
        if a_cjk_face().is_none() {
            return;
        }
        let fonts = fonts(Config::default());
        assert!(fonts.covers('漢'), "Japanese would render as boxes");
        assert!(fonts.covers('あ') && fonts.covers('ア'));
    }

    #[test]
    fn a_configured_fallback_supplies_the_glyphs() {
        let Some(path) = a_cjk_face() else { return };
        let fonts = fonts(Config {
            // A primary with no kanji in it, so only the fallback can answer.
            font: Some("/nonexistent-so-the-bitmap-font-is-used".into()),
            bitmap_scale: Some(1),
            font_fallback: vec![path],
            ..Config::default()
        });
        assert!(fonts.covers('漢'));
    }

    #[test]
    fn a_fallback_that_will_not_load_is_skipped() {
        let fonts = fonts(Config {
            font_fallback: vec!["/nonexistent.ttf".into()],
            bitmap_scale: Some(1),
            ..Config::default()
        });
        // ASCII still works, which is the whole point of not giving up here.
        assert!(fonts.covers('A'));
    }

    #[test]
    fn a_wide_glyph_fills_two_cells() {
        if a_cjk_face().is_none() {
            return;
        }
        let mut fonts = fonts(Config::default());
        let cell = fonts.metrics();
        let glyph = fonts.glyph('漢', tos_font::RasterStyle::REGULAR).clone();
        assert!(
            glyph.width > cell.cell_width,
            "a kanji narrower than two cells is the missing box"
        );
        assert!(
            glyph.left + glyph.width as i32 <= (cell.cell_width * 2) as i32,
            "a kanji wider than two cells would overwrite its neighbour"
        );
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

    /// Send an OSC 52 query and return what went back down the PTY.
    fn query_clipboard(compositor: &mut Compositor) -> String {
        compositor.inject(b"\x1b]52;c;?\x07");
        let focus = compositor.session.focus();
        compositor.handle_terminal_events(focus);
        let response = compositor.pane_mut(focus).unwrap().terminal.take_output();
        String::from_utf8(response).unwrap()
    }

    #[test]
    fn a_clipboard_query_is_refused_by_default() {
        let mut compositor = compositor();
        compositor.clipboard.insert('c', b"a password".to_vec());
        let text = query_clipboard(&mut compositor);
        // An empty selection, well formed: the program gets an answer instead
        // of waiting out a timeout, and none of the real one leaks.
        assert_eq!(text, "\x1b]52;c;\x1b\\", "got {text:?}");
        assert!(!text.contains(&tos_term::graphics::encode_base64(b"a password")));
    }

    #[test]
    fn a_clipboard_query_is_answered_once_the_user_allows_it() {
        let mut compositor = compositor_with(Config {
            allow_clipboard_read: true,
            ..Config::default()
        });
        compositor.clipboard.insert('c', b"stored".to_vec());
        let text = query_clipboard(&mut compositor);
        assert!(text.starts_with("\x1b]52;c;"), "got {text:?}");
        assert!(text.contains(&tos_term::graphics::encode_base64(b"stored")));
    }

    #[test]
    fn an_oversized_clipboard_write_is_refused() {
        let mut compositor = compositor();
        compositor.clipboard.insert('c', b"kept".to_vec());
        let payload = tos_term::graphics::encode_base64(&vec![b'x'; MAX_CLIPBOARD_BYTES + 1]);
        compositor.inject(format!("\x1b]52;c;{payload}\x07").as_bytes());
        let focus = compositor.session.focus();
        compositor.handle_terminal_events(focus);
        // Dropped whole, so what the user had is still what they have.
        assert_eq!(compositor.clipboard('c'), Some(&b"kept"[..]));
    }

    #[test]
    fn a_clipboard_write_at_the_cap_still_lands() {
        let mut compositor = compositor();
        let data = vec![b'x'; MAX_CLIPBOARD_BYTES];
        let payload = tos_term::graphics::encode_base64(&data);
        compositor.inject(format!("\x1b]52;c;{payload}\x07").as_bytes());
        let focus = compositor.session.focus();
        compositor.handle_terminal_events(focus);
        assert_eq!(compositor.clipboard('c'), Some(&data[..]));
    }

    #[test]
    fn a_made_up_selector_never_reaches_the_clipboard() {
        let mut compositor = compositor();
        let payload = tos_term::graphics::encode_base64(b"junk");
        compositor.inject(format!("\x1b]52;Z;{payload}\x07").as_bytes());
        let focus = compositor.session.focus();
        compositor.handle_terminal_events(focus);
        assert_eq!(compositor.clipboard('Z'), None);
    }

    #[test]
    fn keys_reach_the_focused_pane_and_bindings_do_not() {
        let mut compositor = compositor();
        let typing = KeyEvent::new(KeyCode::Char('x'), tos_input::Modifiers::NONE);
        compositor.handle_input(InputEvent::Key(typing));
        // A binding is consumed by the compositor instead.
        let split = KeyEvent::new(KeyCode::Char('d'), tos_input::Modifiers::SUPER);
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
        assert_eq!(
            compositor.panes.len(),
            1,
            "a binding fired under the overlay"
        );
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
        assert_eq!(
            compositor.notifications.status_line().as_deref(),
            Some("not found: definitely-not-a-program-1a2b3c")
        );
    }

    // ---- notifications --------------------------------------------------

    /// Raise an application notification in a pane, the way OSC 9 does.
    fn notify_from(compositor: &mut Compositor, id: PaneId, body: &str) {
        compositor
            .pane_mut(id)
            .unwrap()
            .terminal
            .advance(format!("\x1b]9;{body}\x07").as_bytes());
        compositor.handle_terminal_events(id);
    }

    #[test]
    fn an_application_notification_is_attributed_to_its_pane() {
        let mut compositor = compositor();
        let focus = compositor.session.focus();
        notify_from(&mut compositor, focus, "build finished");
        assert_eq!(
            compositor.notifications.status_line().as_deref(),
            Some("pane 1: build finished")
        );
    }

    #[test]
    fn a_bell_says_which_pane_rang() {
        let mut compositor = compositor();
        let focus = compositor.session.focus();
        compositor
            .pane_mut(focus)
            .unwrap()
            .terminal
            .advance(b"\x07");
        compositor.handle_terminal_events(focus);
        assert_eq!(
            compositor.notifications.status_line().as_deref(),
            Some("pane 1: bell")
        );
    }

    #[test]
    fn a_second_notification_waits_instead_of_replacing_the_first() {
        let mut compositor = compositor();
        let focus = compositor.session.focus();
        notify_from(&mut compositor, focus, "first");
        notify_from(&mut compositor, focus, "second");
        // The first is still the one on screen, and the bar says something is
        // behind it rather than the first having never existed.
        assert_eq!(
            compositor.notifications.status_line().as_deref(),
            Some("pane 1: first (+1)")
        );
    }

    #[test]
    fn the_leader_key_does_not_wipe_a_notification() {
        let mut compositor = compositor();
        let focus = compositor.session.focus();
        notify_from(&mut compositor, focus, "still here");
        compositor.handle_input(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('a'),
            tos_input::Modifiers::CTRL,
        )));
        assert!(compositor.keymap.is_pending());
        // The indicator is drawn from the keymap rather than queued, so the
        // notification is untouched and has not spent its time behind it.
        compositor.tick();
        assert_eq!(
            compositor.notifications.status_line().as_deref(),
            Some("pane 1: still here")
        );
    }

    #[test]
    fn the_history_list_holds_what_went_past() {
        let mut compositor = compositor();
        let focus = compositor.session.focus();
        notify_from(&mut compositor, focus, "one");
        notify_from(&mut compositor, focus, "two");
        assert!(compositor.perform(Action::ShowNotifications));
        let overlay = compositor.overlay().expect("the list should be open");
        let labels: Vec<&str> = overlay.items().iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, ["clear", "pane 1: two", "pane 1: one"]);
        // Reading the list is seeing them, so the bar goes quiet.
        assert!(compositor.notifications.status_line().is_none());
    }

    #[test]
    fn choosing_a_notification_goes_to_the_pane_that_raised_it() {
        let mut compositor = compositor();
        let first = compositor.session.focus();
        compositor.perform(Action::Split(Axis::Columns));
        let second = compositor.session.focus();
        assert_ne!(first, second);
        notify_from(&mut compositor, first, "over here");

        compositor.perform(Action::ShowNotifications);
        // The first row clears the list; the second is the notification.
        for code in [KeyCode::Down, KeyCode::Enter] {
            compositor.handle_input(InputEvent::Key(KeyEvent::new(
                code,
                tos_input::Modifiers::NONE,
            )));
        }
        assert!(compositor.overlay().is_none());
        assert_eq!(compositor.session.focus(), first);
    }

    #[test]
    fn the_first_row_of_the_list_clears_it() {
        let mut compositor = compositor();
        let focus = compositor.session.focus();
        notify_from(&mut compositor, focus, "gone soon");
        compositor.perform(Action::ShowNotifications);
        compositor.handle_input(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            tos_input::Modifiers::NONE,
        )));
        assert_eq!(compositor.notifications.history().count(), 0);
    }

    #[test]
    fn without_a_status_bar_a_notification_is_drawn_over_the_panes() {
        // The bar is where messages live, so with no bar they have to live
        // somewhere else: a failure nobody sees looks like a dead key.
        let mut compositor = compositor_with(Config {
            status_bar: false,
            ..Config::default()
        });
        let mut framebuffer = tos_render::OwnedFramebuffer::new(640, 360);
        let accent = compositor.chrome.accent.pack();
        {
            let mut surface = framebuffer.surface();
            compositor.render_frame(&mut surface, false);
        }
        assert!(
            !framebuffer.pixels().contains(&accent),
            "nothing should be in the accent colour yet"
        );

        let focus = compositor.session.focus();
        notify_from(&mut compositor, focus, "split failed");
        {
            let mut surface = framebuffer.surface();
            compositor.render_frame(&mut surface, false);
        }
        assert!(
            framebuffer.pixels().contains(&accent),
            "the notification was not drawn"
        );
    }

    // ---- workspace rename -----------------------------------------------

    fn press_key(
        compositor: &mut Compositor,
        code: KeyCode,
        modifiers: tos_input::Modifiers,
    ) -> bool {
        compositor.handle_input(InputEvent::Key(KeyEvent::new(code, modifiers)))
    }

    /// Open the rename prompt, clear the name it starts on and type `name`.
    fn rename_to(compositor: &mut Compositor, name: &str) {
        press_key(compositor, KeyCode::Char(','), tos_input::Modifiers::SUPER);
        while !compositor.overlay().expect("the prompt").query().is_empty() {
            press_key(compositor, KeyCode::Backspace, tos_input::Modifiers::NONE);
        }
        type_into_overlay(compositor, name);
        press_key(compositor, KeyCode::Enter, tos_input::Modifiers::NONE);
    }

    #[test]
    fn the_rename_binding_opens_a_prompt_holding_the_current_name() {
        let mut compositor = compositor();
        assert!(press_key(
            &mut compositor,
            KeyCode::Char(','),
            tos_input::Modifiers::SUPER
        ));
        let overlay = compositor.overlay().expect("the prompt should be open");
        assert_eq!(overlay.query(), "1");
        assert!(overlay.items().is_empty(), "a prompt has no list");
    }

    #[test]
    fn a_typed_name_reaches_the_status_bar() {
        let mut compositor = compositor();
        rename_to(&mut compositor, "build");
        assert!(compositor.overlay().is_none(), "accepting should close it");
        assert_eq!(compositor.session.active().name, "build");
        let items = compositor.status_items();
        assert_eq!(items[0].text, "build");
        assert!(items[0].highlighted, "the active workspace is marked");
    }

    #[test]
    fn a_renamed_workspace_is_wider_in_the_drawn_status_bar() {
        // The bar is the one thing the name is for, so this asks the pixels
        // rather than the items: a longer name highlights more of the row.
        fn accent_in_the_bar(compositor: &mut Compositor) -> usize {
            let mut framebuffer = tos_render::OwnedFramebuffer::new(640, 360);
            {
                let mut surface = framebuffer.surface();
                compositor.render_frame(&mut surface, false);
            }
            let (_, ch) = compositor.cell_size();
            let accent = compositor.chrome.accent.pack();
            let above = (compositor.grid_area().height * ch * 640) as usize;
            framebuffer
                .pixels()
                .iter()
                .skip(above)
                .filter(|&&px| px == accent)
                .count()
        }

        let mut compositor = compositor();
        let before = accent_in_the_bar(&mut compositor);
        rename_to(&mut compositor, "development");
        let after = accent_in_the_bar(&mut compositor);
        assert!(after > before, "{after} should be more than {before}");
    }

    #[test]
    fn cancelling_the_prompt_leaves_the_name_alone() {
        let mut compositor = compositor();
        compositor.perform(Action::RenameWorkspace);
        type_into_overlay(&mut compositor, "half typed");
        assert!(press_key(
            &mut compositor,
            KeyCode::Escape,
            tos_input::Modifiers::NONE
        ));
        assert!(compositor.overlay().is_none());
        assert_eq!(compositor.session.active().name, "1");
    }

    #[test]
    fn an_emptied_prompt_gives_the_workspace_its_number_back() {
        let mut compositor = compositor();
        rename_to(&mut compositor, "build");
        rename_to(&mut compositor, "");
        assert_eq!(compositor.session.active().name, "1");
        // And with the name forgotten the number follows the position again.
        compositor.perform(Action::NewWorkspace);
        compositor.perform(Action::SelectWorkspace(1));
        compositor.perform(Action::ClosePane);
        assert_eq!(compositor.status_items()[0].text, "1");
    }

    #[test]
    fn the_prompt_takes_the_keys_the_pane_would_have_had() {
        let mut compositor = compositor();
        compositor.perform(Action::RenameWorkspace);
        type_into_overlay(&mut compositor, "build");
        press_key(
            &mut compositor,
            KeyCode::Char('d'),
            tos_input::Modifiers::SUPER,
        );
        assert_eq!(
            compositor.panes.len(),
            1,
            "a binding fired under the prompt"
        );
        let focus = compositor.session.focus();
        assert_eq!(compositor.pane(focus).unwrap().pending_input(), 0);
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

    // ---- the lock -------------------------------------------------------

    /// A credential file of this test's own making, so that nothing here
    /// depends on whether the machine running it has a password of its own.
    /// The password is always "tos"; what varies is whether the file is there.
    fn credential(name: &str, password: Option<&str>) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("tos-lock-compositor-{}-{name}", std::process::id()));
        match password {
            Some(password) => {
                let hash = tos_crypt::sha512crypt::hash(password.as_bytes(), b"tOScompositor");
                std::fs::write(&path, format!("{hash}\n")).expect("credential file");
            }
            None => {
                let _ = std::fs::remove_file(&path);
            }
        }
        path
    }

    fn compositor_with_password(name: &str) -> Compositor {
        compositor_with(Config {
            credential: credential(name, Some("tos")),
            ..Config::default()
        })
    }

    /// Type a password into the lock and press enter.
    fn answer(compositor: &mut Compositor, password: &str) {
        type_into_overlay(compositor, password);
        press_key(compositor, KeyCode::Enter, tos_input::Modifiers::NONE);
    }

    fn lock_binding(compositor: &mut Compositor) -> bool {
        press_key(
            compositor,
            KeyCode::Char('l'),
            tos_input::Modifiers::SUPER.union(tos_input::Modifiers::SHIFT),
        )
    }

    #[test]
    fn the_binding_locks_and_the_password_unlocks() {
        let mut compositor = compositor_with_password("roundtrip");
        assert!(lock_binding(&mut compositor));
        assert!(compositor.is_locked());
        answer(&mut compositor, "tos");
        assert!(!compositor.is_locked());
        assert!(compositor.is_running(), "unlocking is not quitting");
    }

    #[test]
    fn a_wrong_password_leaves_the_screen_locked() {
        let mut compositor = compositor_with_password("wrong");
        compositor.lock_session();
        answer(&mut compositor, "not it");
        assert!(compositor.is_locked());
        assert_eq!(compositor.lock_screen().unwrap().attempts(), 1);
        // And the field is empty again, ready to be retyped.
        assert_eq!(compositor.lock_screen().unwrap().typed_len(), 0);
    }

    #[test]
    fn with_no_credential_the_lock_refuses_and_says_why() {
        // The live ISO, and an installed machine whose owner declined a
        // password. The compositor never asks what kind of machine it is on.
        let mut compositor = compositor_with(Config {
            credential: credential("none", None),
            ..Config::default()
        });
        assert!(lock_binding(&mut compositor));
        assert!(
            !compositor.is_locked(),
            "locked with nothing to unlock with"
        );
        let said = compositor.notifications.status_line().unwrap_or_default();
        assert!(
            said.starts_with("cannot lock: no password is set"),
            "{said:?}"
        );
    }

    #[test]
    fn a_credential_that_does_not_parse_is_not_a_wrong_password() {
        // It is a refusal to engage. Treating it as a wrong password would
        // put up a screen that could never be opened.
        let path = credential("yescrypt", None);
        std::fs::write(&path, "$y$j9T$salt$digest\n").expect("credential file");
        let mut compositor = compositor_with(Config {
            credential: path,
            ..Config::default()
        });
        compositor.lock_session();
        assert!(!compositor.is_locked());
        let said = compositor.notifications.status_line().unwrap_or_default();
        assert!(said.contains("not a password tOS can check"), "{said:?}");
    }

    #[test]
    fn no_binding_fires_while_the_screen_is_locked() {
        let mut compositor = compositor_with_password("bindings");
        compositor.lock_session();
        for (code, modifiers) in [
            (KeyCode::Char('d'), tos_input::Modifiers::SUPER),
            (KeyCode::Char('q'), tos_input::Modifiers::SUPER),
            (KeyCode::Char('x'), tos_input::Modifiers::SUPER),
            (
                KeyCode::Enter,
                tos_input::Modifiers::CTRL.union(tos_input::Modifiers::SHIFT),
            ),
        ] {
            press_key(&mut compositor, code, modifiers);
        }
        assert_eq!(compositor.panes.len(), 1, "a binding fired under the lock");
        assert!(compositor.is_running(), "super+q quit a locked session");
        assert!(compositor.is_locked());
    }

    #[test]
    fn the_leader_cannot_reach_a_binding_while_locked() {
        let mut compositor = compositor_with_password("leader");
        compositor.lock_session();
        press_key(
            &mut compositor,
            KeyCode::Char('a'),
            tos_input::Modifiers::CTRL,
        );
        assert!(
            !compositor.keymap.is_pending(),
            "the leader armed under the lock"
        );
        press_key(
            &mut compositor,
            KeyCode::Char('q'),
            tos_input::Modifiers::NONE,
        );
        assert!(compositor.is_running());
    }

    #[test]
    fn nothing_typed_at_the_lock_reaches_a_pane() {
        let mut compositor = compositor_with_password("typing");
        compositor.lock_session();
        type_into_overlay(&mut compositor, "tos");
        let focus = compositor.session.focus();
        assert_eq!(compositor.pane(focus).unwrap().pending_input(), 0);
    }

    /// The events `handle_key` would never have seen: routed straight through
    /// `handle_input`, which is why the gate has to be there and not one level
    /// further in.
    #[test]
    fn the_lock_gates_the_mouse_the_pointer_and_the_paste() {
        let mut compositor = compositor_with_password("input");
        compositor
            .clipboard
            .insert(PRIMARY, b"a command\n".to_vec());
        compositor
            .clipboard
            .insert(CLIPBOARD, b"another command\n".to_vec());
        compositor.lock_session();
        let focus = compositor.session.focus();

        // A middle click pastes primary into the focused pane when the screen
        // is not locked, and it never passes through `handle_key` at all.
        compositor.handle_input(InputEvent::Mouse(MouseEvent {
            button: Some(MouseButton::Middle),
            action: MouseAction::Press,
            col: 2,
            row: 2,
            modifiers: tos_input::Modifiers::NONE,
        }));
        // A pointer press and drag, which is how the mouse selects and copies
        // whatever is on the screen it is not supposed to be able to read.
        for (action, y) in [(MouseAction::Press, 20.0), (MouseAction::Drag, 60.0)] {
            compositor.handle_input(InputEvent::Pointer(tos_input::PointerEvent {
                x: 20.0,
                y,
                button: Some(MouseButton::Left),
                action,
                modifiers: tos_input::Modifiers::NONE,
            }));
        }
        // And a bracketed paste, which is how a host terminal types.
        compositor.handle_input(InputEvent::Paste("a pasted command\n".into()));
        // Focus notifications are input too.
        compositor.handle_input(InputEvent::FocusGained);

        assert!(compositor.is_locked());
        let pane = compositor.pane(focus).unwrap();
        assert_eq!(
            pane.pending_input(),
            0,
            "input reached the pane under the lock"
        );
        assert!(
            pane.selection.is_none(),
            "the mouse selected under the lock"
        );
        assert!(!pane.selecting);
        assert!(compositor.mouse_grab.is_none());
    }

    #[test]
    fn the_last_pane_dying_while_locked_does_not_end_the_session() {
        // A shell reaching its end of file is a way out of a locked screen
        // that does not go through a binding at all.
        let mut compositor = compositor_with_password("lastpane");
        compositor.lock_session();
        let focus = compositor.session.focus();
        compositor.close_pane(focus);
        assert!(compositor.is_running(), "the session ended under the lock");
        assert!(compositor.is_locked());
        // It ends when somebody has said who they are, and not before.
        answer(&mut compositor, "tos");
        assert!(!compositor.is_locked());
        assert!(!compositor.is_running());
    }

    #[test]
    fn resizing_the_display_while_locked_keeps_the_lock() {
        let mut compositor = compositor_with_password("resize");
        compositor.lock_session();
        compositor.resize((320, 240));
        assert!(compositor.is_locked());
        // Even down to a display with no room to draw the box in, which is
        // the one direction the lock is allowed to fail in: it stops being
        // legible, and it does not stop being a lock.
        compositor.resize((32, 24));
        let mut framebuffer = tos_render::OwnedFramebuffer::new(32, 24);
        {
            let mut surface = framebuffer.surface();
            compositor.render_frame(&mut surface, false);
        }
        assert!(compositor.is_locked());
        answer(&mut compositor, "tos");
        assert!(!compositor.is_locked());
    }

    #[test]
    fn a_notification_raised_while_locked_waits_instead_of_being_shown() {
        let mut compositor = compositor_with_password("notify");
        compositor.lock_session();
        let focus = compositor.session.focus();
        notify_from(&mut compositor, focus, "the build finished");
        // Nothing is drawn, so nothing spends its time on screen: the queue
        // stands still, and the message is still there afterwards.
        for _ in 0..4 {
            compositor.tick();
        }
        assert!(compositor.is_locked());
        answer(&mut compositor, "tos");
        assert_eq!(
            compositor.notifications.status_line().as_deref(),
            Some("pane 1: the build finished")
        );
    }

    #[test]
    fn unlocking_asks_for_the_whole_screen_back() {
        let mut compositor = compositor_with_password("redraw");
        compositor.lock_session();
        let mut framebuffer = tos_render::OwnedFramebuffer::new(640, 360);
        {
            let mut surface = framebuffer.surface();
            compositor.render_frame(&mut surface, true);
        }
        assert!(
            !compositor.needs_full_redraw,
            "a locked frame leaves nothing outstanding"
        );
        answer(&mut compositor, "tos");
        assert!(
            compositor.needs_full_redraw,
            "the session came back on damage that was thrown away"
        );
    }

    // ---- idle -----------------------------------------------------------

    /// A display that remembers what it was told about blanking.
    ///
    /// The DRM path cannot run here and the headless one has nothing to put
    /// to sleep, so what a test can check is the one thing the compositor is
    /// responsible for: that the display is told, once each way, at the right
    /// moment.
    struct Panel {
        framebuffer: tos_render::OwnedFramebuffer,
        told: Vec<bool>,
        /// A display that cannot do what it is asked, which is a thing
        /// hardware does.
        refuses: bool,
    }

    impl Panel {
        fn new() -> Panel {
            Panel {
                framebuffer: tos_render::OwnedFramebuffer::new(640, 360),
                told: Vec::new(),
                refuses: false,
            }
        }

        fn refusing() -> Panel {
            Panel {
                refuses: true,
                ..Panel::new()
            }
        }
    }

    impl Display for Panel {
        fn size(&self) -> (u32, u32) {
            (640, 360)
        }

        fn frame(&mut self, draw: &mut dyn FnMut(&mut Surface<'_>)) -> io::Result<()> {
            draw(&mut self.framebuffer.surface());
            Ok(())
        }

        fn blank(&mut self, blank: bool) -> io::Result<()> {
            self.told.push(blank);
            if self.refuses {
                return Err(io::Error::other("the panel is welded on"));
            }
            Ok(())
        }
    }

    /// A compositor whose deadlines are near enough to walk to, and the panel
    /// it sits in front of.
    fn idling(config: Config, lock_after: Option<u64>, blank_after: Option<u64>) -> Compositor {
        compositor_with(Config {
            idle_lock: lock_after.map(Duration::from_secs),
            idle_blank: blank_after.map(Duration::from_secs),
            ..config
        })
    }

    /// Where the session's idle time is measured from.
    fn started(compositor: &Compositor) -> Instant {
        compositor.last_activity
    }

    #[test]
    fn an_untouched_session_blanks_and_a_key_brings_it_back() {
        let mut compositor = idling(
            Config {
                credential: credential("idle-blank", None),
                ..Config::default()
            },
            None,
            Some(60),
        );
        let start = started(&compositor);
        let mut panel = Panel::new();

        assert!(!compositor.apply_idle(start + Duration::from_secs(59), &mut panel));
        assert!(!compositor.is_blanked(), "blanked a second early");

        compositor.apply_idle(start + Duration::from_secs(60), &mut panel);
        assert!(compositor.is_blanked());
        assert_eq!(panel.told, vec![true]);
        // Staying idle does not tell it again.
        compositor.apply_idle(start + Duration::from_secs(600), &mut panel);
        assert_eq!(panel.told, vec![true]);

        compositor.handle_input(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('a'),
            tos_input::Modifiers::NONE,
        )));
        assert!(compositor.apply_idle(Instant::now(), &mut panel));
        assert!(!compositor.is_blanked());
        assert_eq!(panel.told, vec![true, false]);
        assert!(
            compositor.needs_full_redraw,
            "the screen came back without being repainted"
        );
    }

    #[test]
    fn the_key_that_wakes_the_screen_is_swallowed() {
        // super+space opens the launcher, which is a visible thing for a key
        // to have done. Typed at a dark screen it must do nothing at all.
        let mut compositor = idling(
            Config {
                credential: credential("idle-swallow", None),
                ..Config::default()
            },
            None,
            Some(60),
        );
        let start = started(&compositor);
        let mut panel = Panel::new();
        compositor.apply_idle(start + Duration::from_secs(60), &mut panel);

        assert!(compositor.handle_input(InputEvent::Key(KeyEvent::new(
            KeyCode::Char(' '),
            tos_input::Modifiers::SUPER,
        ))));
        assert!(
            compositor.overlay().is_none(),
            "the key that woke the screen also ran a binding"
        );
    }

    #[test]
    fn a_key_at_a_dark_locked_screen_is_not_the_first_of_the_password() {
        let mut compositor = idling(
            Config {
                credential: credential("idle-locked-dark", Some("tos")),
                ..Config::default()
            },
            Some(60),
            Some(120),
        );
        let start = started(&compositor);
        let mut panel = Panel::new();
        compositor.apply_idle(start + Duration::from_secs(120), &mut panel);
        assert!(compositor.is_locked() && compositor.is_blanked());

        compositor.handle_input(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('t'),
            tos_input::Modifiers::NONE,
        )));
        assert_eq!(
            compositor.lock_screen().unwrap().typed_len(),
            0,
            "the key that woke the screen went into the password"
        );
        // The screen comes back, and the password typed at it works.
        compositor.apply_idle(Instant::now(), &mut panel);
        assert!(!compositor.is_blanked());
        answer(&mut compositor, "tos");
        assert!(!compositor.is_locked());
    }

    #[test]
    fn the_lock_comes_before_the_blank_when_both_are_due_at_once() {
        // Both deadlines land on the same pass, which is what happens when
        // they are given the same interval and when the loop was away. The
        // lock has to go up first: unblanking shows the last frame drawn, and
        // that frame must not be the session.
        let mut compositor = idling(
            Config {
                credential: credential("idle-together", Some("tos")),
                ..Config::default()
            },
            Some(30),
            Some(30),
        );
        let start = started(&compositor);
        let mut panel = Panel::new();
        compositor.apply_idle(start + Duration::from_secs(30), &mut panel);
        assert!(compositor.is_locked(), "the screen went dark unlocked");
        assert!(compositor.is_blanked());
    }

    #[test]
    fn a_session_with_no_credential_blanks_and_does_not_lock() {
        // The live ISO. The failure this guards against is the one the design
        // refuses VT_LOCKSWITCH for: a machine that goes dark and then locks
        // with nothing able to open it.
        let mut compositor = idling(
            Config {
                credential: credential("idle-no-credential", None),
                ..Config::default()
            },
            Some(30),
            Some(60),
        );
        let start = started(&compositor);
        let mut panel = Panel::new();
        for seconds in [30, 60, 90, 600] {
            compositor.apply_idle(start + Duration::from_secs(seconds), &mut panel);
        }
        assert!(!compositor.is_locked(), "locked with no way to unlock");
        assert!(
            compositor.is_blanked(),
            "a session with no password is not a session nobody left"
        );
        // And it says nothing about it. The binding answers the person who
        // pressed it; a deadline has nobody to answer.
        assert_eq!(compositor.notifications.status_line(), None);
        assert_eq!(panel.told, vec![true]);
    }

    #[test]
    fn a_deadline_that_is_never_never_comes() {
        let mut compositor = idling(
            Config {
                credential: credential("idle-off", Some("tos")),
                ..Config::default()
            },
            None,
            None,
        );
        let start = started(&compositor);
        let mut panel = Panel::new();
        compositor.apply_idle(start + Duration::from_secs(86_400), &mut panel);
        assert!(!compositor.is_locked());
        assert!(!compositor.is_blanked());
        assert!(panel.told.is_empty());
        assert_eq!(compositor.next_idle_deadline(start), None);
    }

    #[test]
    fn the_wait_is_pulled_in_to_the_next_deadline() {
        let mut compositor = idling(
            Config {
                credential: credential("idle-timeout", None),
                ..Config::default()
            },
            None,
            Some(10),
        );
        let start = started(&compositor);
        // Far from the deadline the wait is what it always was: the blink
        // phase still needs looking at.
        assert_eq!(compositor.frame_timeout_ms_at(start), IDLE_TIMEOUT_MS);
        // Near it, the deadline wins, so the screen goes dark on the ten
        // seconds rather than up to a tenth of a second afterwards.
        assert_eq!(
            compositor.frame_timeout_ms_at(start + Duration::from_millis(9_960)),
            40
        );
        // A deadline already past is not a wait of zero, which would be a
        // spin.
        assert_eq!(
            compositor.frame_timeout_ms_at(start + Duration::from_secs(11)),
            1
        );
        let mut panel = Panel::new();
        compositor.apply_idle(start + Duration::from_secs(10), &mut panel);
        // With the screen dark and nothing else to come, the loop stops
        // waking up to look at a screen nobody can see.
        assert_eq!(
            compositor.frame_timeout_ms_at(start + Duration::from_secs(10)),
            BLANKED_TIMEOUT_MS
        );
    }

    #[test]
    fn a_dark_screen_still_waits_for_the_lock_that_is_coming() {
        let mut compositor = idling(
            Config {
                credential: credential("idle-dark-wait", Some("tos")),
                ..Config::default()
            },
            Some(60),
            Some(30),
        );
        let start = started(&compositor);
        let mut panel = Panel::new();
        compositor.apply_idle(start + Duration::from_secs(30), &mut panel);
        assert!(compositor.is_blanked() && !compositor.is_locked());
        assert_eq!(
            compositor.frame_timeout_ms_at(start + Duration::from_secs(30)),
            30_000,
            "a blanked session forgot the lock it still owes"
        );
        compositor.apply_idle(start + Duration::from_secs(60), &mut panel);
        assert!(
            compositor.is_locked(),
            "the lock deadline passed in the dark"
        );
        assert_eq!(panel.told, vec![true], "locking woke the screen up");
    }

    #[test]
    fn the_loop_itself_acts_on_the_deadlines() {
        // Every test above hands the clock in. This is the one that checks the
        // loop is wired to the deadlines at all, and it needs no clock of its
        // own: a millisecond has always gone by, because starting a pane takes
        // longer than that.
        let mut compositor = compositor_with(Config {
            credential: credential("idle-loop", Some("tos")),
            idle_lock: Some(Duration::from_millis(1)),
            idle_blank: Some(Duration::from_millis(1)),
            ..Config::default()
        });
        let mut panel = Panel::new();
        compositor
            .run_once(&mut panel, &[], |_| Vec::new())
            .expect("a pass of the loop");
        assert!(compositor.is_locked(), "the loop ignored the lock deadline");
        assert!(
            compositor.is_blanked(),
            "the loop ignored the blank deadline"
        );
    }

    #[test]
    fn a_display_that_will_not_go_dark_is_said_and_not_died_of() {
        let mut compositor = idling(
            Config {
                credential: credential("idle-refused", None),
                ..Config::default()
            },
            None,
            Some(60),
        );
        let start = started(&compositor);
        let mut panel = Panel::refusing();
        for seconds in [60, 61, 62, 600] {
            compositor.apply_idle(start + Duration::from_secs(seconds), &mut panel);
        }
        assert!(!compositor.is_blanked(), "a lit screen remembered as dark");
        assert!(compositor.is_running(), "a refused blank ended the session");
        assert_eq!(panel.told, vec![true], "asked a display that said no again");
        assert!(compositor
            .notifications
            .status_line()
            .unwrap_or_default()
            .contains("blanking the display"));
        // And the loop goes back to its ordinary wait rather than spinning on
        // a deadline that has passed and can never be met.
        assert_eq!(
            compositor.frame_timeout_ms_at(start + Duration::from_secs(600)),
            IDLE_TIMEOUT_MS
        );
    }

    #[test]
    fn output_from_a_pane_is_not_somebody_being_there() {
        // The `tail -f` case. A pane that goes on writing must not hold the
        // screen on, so nothing in the pump touches the idle clock.
        let mut compositor = idling(
            Config {
                credential: credential("idle-output", None),
                ..Config::default()
            },
            None,
            Some(60),
        );
        let start = started(&compositor);
        compositor.inject(b"still here\r\n");
        compositor.pump_panes();
        compositor.tick();
        assert_eq!(started(&compositor), start, "a pane moved the idle clock");

        let mut panel = Panel::new();
        compositor.apply_idle(start + Duration::from_secs(60), &mut panel);
        assert!(compositor.is_blanked());
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
