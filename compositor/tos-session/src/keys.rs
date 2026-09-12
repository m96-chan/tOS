//! Compositor key bindings.
//!
//! tOS owns the whole keyboard, so bindings are direct combinations rather
//! than a prefix key. A leader key is still supported, because a nested
//! development session cannot rely on the super key reaching the compositor.
//! The two bindings that grow a session, splitting a pane and opening a
//! workspace, are also on their familiar ctrl+shift combinations.

use std::collections::HashMap;

use tos_input::{ImeKey, KeyCode, KeyEvent, KeyState, MediaKey, Modifiers};

use crate::layout::{Axis, Direction};

/// Something the compositor does in response to a binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Open a new pane beside the focused one.
    Split(Axis),
    /// Close the focused pane.
    ClosePane,
    /// Move focus.
    Focus(Direction),
    /// Move the divider next to the focused pane.
    Resize(Direction, i32),
    /// Make the focused pane fill the workspace, or restore it.
    ToggleZoom,
    /// Even out every split.
    Balance,
    NewWorkspace,
    NextWorkspace,
    PreviousWorkspace,
    /// Switch to a workspace by number, 1 based.
    SelectWorkspace(usize),
    /// Move the focused pane to a workspace by number.
    MovePaneToWorkspace(usize),
    /// Ask for a name for the active workspace.
    RenameWorkspace,
    /// Scroll the focused pane's viewport, in lines. Negative is back in time.
    Scroll(i32),
    /// Scroll by a screenful.
    ScrollPage(i32),
    /// Jump back to the live screen.
    ScrollToBottom,
    Copy,
    Paste,
    /// Open the launcher: a filtered list of programs, one of which starts in
    /// a new pane.
    OpenLauncher,
    /// Open the notification history: everything that has been on the status
    /// bar, including whatever went past while the screen was not being read.
    ShowNotifications,
    /// Show what the bindings are, read out of this keymap.
    ShowBindings,
    /// Lock the screen: a password prompt that owns the session until it is
    /// answered. Refuses to engage when the machine has no password set.
    Lock,
    /// Leave the compositor.
    Quit,
    /// Redraw everything.
    Refresh,
    /// Take the keyboard and drive a selection with it: motions move a copy
    /// cursor through the pane and its history, `v` fixes one end of the
    /// selection, `y` copies it and leaves.
    ///
    /// This replaced a `BeginSelection` that made a selection of exactly one
    /// cell and had no way to make it any bigger, because there was no action
    /// that could move either end of it. A mode is what that was missing: the
    /// leader disarms after one key, so extending a selection through the
    /// keymap would be one leader press per cell.
    CopyMode,
    /// Open the Bluetooth controls: the adapter, what it is doing, and the
    /// devices a scan found. Powering on, blocking and scanning all happen
    /// from inside it rather than each getting a key of its own.
    ShowBluetooth,
    /// Open the power menu: power off, reboot or suspend. The menu is the
    /// binding rather than the three actions being bound separately, because
    /// two of the three cannot be taken back and neither should be one
    /// keystroke away — the menu is where the confirmation lives.
    PowerMenu,
    /// Turn the default card up by one step.
    VolumeUp,
    /// Turn it down by one step.
    VolumeDown,
    /// Flip the default card between muted and not.
    ToggleMute,
    /// Show or hide the status bar.
    ///
    /// `--no-status-bar` decides what a session starts as, which is the wrong
    /// granularity for the thing it decides: whether a row of the display is
    /// worth spending is a question that has a different answer while reading
    /// a long file than it does the rest of the time, and restarting the
    /// compositor to change your mind means losing every pane.
    ToggleStatusBar,
    /// Open the network menu: the machine's interfaces, what state each is
    /// in, and what can be done to one.
    ShowNetworks,
    /// Turn Japanese input on or off for the focused pane.
    ///
    /// A binding rather than a key, and that is not a preference: the key a
    /// Japanese user actually presses is 半角/全角, which on a USB JIS
    /// keyboard is HID usage 0x35 — the position HID calls "Grave Accent and
    /// Tilde" — and `hid-input` maps 0x35 to `KEY_GRAVE` without asking what
    /// layout the keyboard claims to be. So on the hardware that most needs
    /// the toggle, the toggle is indistinguishable from a backtick, and a
    /// default bound to it would be a default that types `\``.
    ImeToggle,
}

/// A key combination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Binding {
    pub code: KeyCode,
    pub modifiers: Modifiers,
}

impl Binding {
    pub fn new(code: KeyCode, modifiers: Modifiers) -> Self {
        Binding { code, modifiers }
    }

    pub fn matches(&self, event: &KeyEvent) -> bool {
        // Lock keys must not stop a binding from firing.
        self.code == event.code && self.modifiers == event.modifiers.effective()
    }
}

/// What happened when a key was offered to the keymap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// The compositor should perform this action.
    Action(Action),
    /// The leader key was pressed; the next key is a binding.
    Pending,
    /// Not a binding; the key belongs to the focused pane.
    Passthrough,
}

/// The binding table.
#[derive(Debug, Clone)]
pub struct Keymap {
    /// Bindings that fire directly.
    ///
    /// Visible to the crate so that [`crate::describe`] can tell the user what
    /// is bound without the table having to be restated anywhere.
    pub(crate) direct: HashMap<Binding, Action>,
    /// Bindings that fire only after the leader key.
    pub(crate) after_leader: HashMap<Binding, Action>,
    pub leader: Option<Binding>,
    leader_armed: bool,
}

impl Keymap {
    pub fn empty() -> Self {
        Keymap {
            direct: HashMap::new(),
            after_leader: HashMap::new(),
            leader: None,
            leader_armed: false,
        }
    }

    /// The bindings tOS ships with.
    ///
    /// Direct bindings use super, which only a compositor that owns the
    /// keyboard can claim. The same set is available after the leader key for
    /// nested sessions, where super never arrives. Splitting and opening a
    /// workspace are additionally bound to ctrl+shift+enter and ctrl+shift+t,
    /// which is where most people expect them.
    pub fn default_bindings() -> Self {
        let mut keymap = Keymap::empty();
        keymap.leader = Some(Binding::new(KeyCode::Char('a'), Modifiers::CTRL));

        let bindings: &[(KeyCode, Modifiers, Action)] = &[
            (
                KeyCode::Char('d'),
                Modifiers::NONE,
                Action::Split(Axis::Columns),
            ),
            (
                KeyCode::Char('s'),
                Modifiers::NONE,
                Action::Split(Axis::Rows),
            ),
            (KeyCode::Char('x'), Modifiers::NONE, Action::ClosePane),
            (
                KeyCode::Left,
                Modifiers::NONE,
                Action::Focus(Direction::Left),
            ),
            (
                KeyCode::Right,
                Modifiers::NONE,
                Action::Focus(Direction::Right),
            ),
            (KeyCode::Up, Modifiers::NONE, Action::Focus(Direction::Up)),
            (
                KeyCode::Down,
                Modifiers::NONE,
                Action::Focus(Direction::Down),
            ),
            (
                KeyCode::Char('h'),
                Modifiers::NONE,
                Action::Focus(Direction::Left),
            ),
            (
                KeyCode::Char('l'),
                Modifiers::NONE,
                Action::Focus(Direction::Right),
            ),
            (
                KeyCode::Char('k'),
                Modifiers::NONE,
                Action::Focus(Direction::Up),
            ),
            (
                KeyCode::Char('j'),
                Modifiers::NONE,
                Action::Focus(Direction::Down),
            ),
            (
                KeyCode::Left,
                Modifiers::SHIFT,
                Action::Resize(Direction::Left, 2),
            ),
            (
                KeyCode::Right,
                Modifiers::SHIFT,
                Action::Resize(Direction::Right, 2),
            ),
            (
                KeyCode::Up,
                Modifiers::SHIFT,
                Action::Resize(Direction::Up, 1),
            ),
            (
                KeyCode::Down,
                Modifiers::SHIFT,
                Action::Resize(Direction::Down, 1),
            ),
            (KeyCode::Char('z'), Modifiers::NONE, Action::ToggleZoom),
            (KeyCode::Char('='), Modifiers::NONE, Action::Balance),
            (KeyCode::Char('c'), Modifiers::NONE, Action::NewWorkspace),
            (KeyCode::Char('n'), Modifiers::NONE, Action::NextWorkspace),
            (
                KeyCode::Char('p'),
                Modifiers::NONE,
                Action::PreviousWorkspace,
            ),
            // Comma is where tmux renames a window, and nothing else here
            // wants the key.
            (KeyCode::Char(','), Modifiers::NONE, Action::RenameWorkspace),
            (KeyCode::PageUp, Modifiers::NONE, Action::ScrollPage(-1)),
            (KeyCode::PageDown, Modifiers::NONE, Action::ScrollPage(1)),
            (KeyCode::Char(']'), Modifiers::NONE, Action::Paste),
            (KeyCode::Char('y'), Modifiers::NONE, Action::Copy),
            (KeyCode::Char('r'), Modifiers::NONE, Action::Refresh),
            // Space is the one key nothing else wants, and super+space is
            // where a launcher lives on every other desktop.
            (KeyCode::Char(' '), Modifiers::NONE, Action::OpenLauncher),
            // m for messages: the notifications that have been and gone.
            (
                KeyCode::Char('m'),
                Modifiers::NONE,
                Action::ShowNotifications,
            ),
            // Shift and the slash key is the question mark, which is where
            // every other program with a leader key keeps its own help.
            (KeyCode::Char('/'), Modifiers::SHIFT, Action::ShowBindings),
            // Shift and the l key, because l on its own moves focus right the
            // way vim does, and because every other desktop locks with an L.
            (KeyCode::Char('l'), Modifiers::SHIFT, Action::Lock),
            (KeyCode::Char('q'), Modifiers::NONE, Action::Quit),
            // Where tmux keeps copy mode, and the key that used to start a
            // selection nothing could extend.
            (KeyCode::Char('['), Modifiers::NONE, Action::CopyMode),
            // b for Bluetooth, which nothing else wants and which is what the
            // radio is called everywhere a user has seen it before.
            (KeyCode::Char('b'), Modifiers::NONE, Action::ShowBluetooth),
            // Delete, so that the leader and super aliases are the same key as
            // the ctrl+alt+delete bound below: one key to remember for this,
            // rather than one for the console gesture and another for tOS.
            (KeyCode::Delete, Modifiers::NONE, Action::PowerMenu),
            // The angle brackets, because they point the way the volume goes
            // and because both keys are free once shifted: plain comma
            // renames a workspace and plain period is bound to nothing, so
            // neither loses anything it was already doing. Mute takes shift
            // and the m key rather than a plain m, which is the notification
            // history — one letter, the two things you want from a machine
            // that has just started making a noise at you.
            (KeyCode::Char('.'), Modifiers::SHIFT, Action::VolumeUp),
            (KeyCode::Char(','), Modifiers::SHIFT, Action::VolumeDown),
            (KeyCode::Char('m'), Modifiers::SHIFT, Action::ToggleMute),
            // Shift and the s key: s on its own splits into rows, and the
            // shifted key is free. b would have been the letter the bar is
            // named after, but b is the radio — a person looking for
            // Bluetooth has one word for it and a person looking for the bar
            // has several, so the unambiguous name wins the letter.
            (
                KeyCode::Char('s'),
                Modifiers::SHIFT,
                Action::ToggleStatusBar,
            ),
            // n is the next workspace and w is not taken, but neither reads
            // as "network"; shift and the n key does, and shift is where the
            // bindings that open a system menu have started to live.
            (KeyCode::Char('n'), Modifiers::SHIFT, Action::ShowNetworks),
            // i for input method. It is the letter the thing is named after
            // in every language it has a name in, and it is one of the few
            // still free — the obvious alternative, ctrl+space, is NUL to a
            // terminal and set-mark to emacs, so binding it would take a key
            // away from the program the IME exists to type into.
            (KeyCode::Char('i'), Modifiers::NONE, Action::ImeToggle),
        ];
        for (code, modifiers, action) in bindings {
            keymap.bind_after_leader(Binding::new(*code, *modifiers), action.clone());
            // The same key with super pressed works without the leader.
            keymap.bind(
                Binding::new(*code, modifiers.union(Modifiers::SUPER)),
                action.clone(),
            );
        }

        for n in 1..=9usize {
            let digit = KeyCode::Char((b'0' + n as u8) as char);
            keymap.bind_after_leader(
                Binding::new(digit, Modifiers::NONE),
                Action::SelectWorkspace(n),
            );
            keymap.bind(
                Binding::new(digit, Modifiers::SUPER),
                Action::SelectWorkspace(n),
            );
            keymap.bind_after_leader(
                Binding::new(digit, Modifiers::SHIFT),
                Action::MovePaneToWorkspace(n),
            );
            keymap.bind(
                Binding::new(digit, Modifiers::SUPER.union(Modifiers::SHIFT)),
                Action::MovePaneToWorkspace(n),
            );
        }

        // The combinations people arrive with from browsers, editors and other
        // terminal emulators. tOS owns the keyboard, so these can be direct
        // defaults rather than something the leader has to reach.
        keymap.bind(
            Binding::new(KeyCode::Enter, Modifiers::CTRL.union(Modifiers::SHIFT)),
            Action::Split(Axis::Columns),
        );
        keymap.bind(
            Binding::new(KeyCode::Char('t'), Modifiers::CTRL.union(Modifiers::SHIFT)),
            Action::NewWorkspace,
        );

        // Scrolling is useful without any prefix at all.
        keymap.bind(
            Binding::new(KeyCode::PageUp, Modifiers::SHIFT),
            Action::ScrollPage(-1),
        );
        keymap.bind(
            Binding::new(KeyCode::PageDown, Modifiers::SHIFT),
            Action::ScrollPage(1),
        );
        keymap.bind(
            Binding::new(KeyCode::End, Modifiers::SHIFT),
            Action::ScrollToBottom,
        );

        // The one gesture every PC user already knows for "I want this machine
        // to stop". tOS can claim it because it owns the keyboard: the console
        // keyboard is in `K_OFF` while a session is up, so the kernel's own
        // ctrl+alt+delete — which signals init — never sees the key. It opens
        // the menu rather than doing anything, which is the whole difference
        // between this and the reboot the BIOS does with the same fingers.
        keymap.bind(
            Binding::new(KeyCode::Delete, Modifiers::CTRL.union(Modifiers::ALT)),
            Action::PowerMenu,
        );
        // The keys on the keyboard that are already labelled with what they
        // do. They take no modifier and go nowhere near the leader, because a
        // key that exists to change the volume has nothing else it could
        // mean: there is no program in a pane that is owed a volume key, and
        // a laptop whose volume keys do nothing under tOS while they work
        // under every other system reads as tOS being broken.
        for (code, action) in [
            (MediaKey::VolumeUp, Action::VolumeUp),
            (MediaKey::VolumeDown, Action::VolumeDown),
            (MediaKey::Mute, Action::ToggleMute),
        ] {
            keymap.bind(Binding::new(KeyCode::Media(code), Modifiers::NONE), action);
        }
        // The かな key, for the same reason and with the same caveat: a key
        // labelled カタカナひらがな has nothing else it could mean, and
        // `encode_key` gives it no bytes, so no program is owed it. It is the
        // one of the three JIS conversion keys whose meaning is not in
        // question — 半角/全角 never arrives at all, and what 変換 and 無変換
        // do belongs to the preedit rather than to the keymap.
        keymap.bind(
            Binding::new(KeyCode::Ime(ImeKey::KanaMode), Modifiers::NONE),
            Action::ImeToggle,
        );
        keymap
    }

    pub fn bind(&mut self, binding: Binding, action: Action) {
        self.direct.insert(binding, action);
    }

    pub fn bind_after_leader(&mut self, binding: Binding, action: Action) {
        self.after_leader.insert(binding, action);
    }

    pub fn unbind(&mut self, binding: &Binding) {
        self.direct.remove(binding);
        self.after_leader.remove(binding);
    }

    /// Whether the leader key has been pressed and is waiting for a second key.
    pub fn is_pending(&self) -> bool {
        self.leader_armed
    }

    pub fn cancel_pending(&mut self) {
        self.leader_armed = false;
    }

    /// Offer a key event to the keymap.
    pub fn resolve(&mut self, event: &KeyEvent) -> Resolution {
        if !event.is_press() {
            return Resolution::Passthrough;
        }
        // Modifier and lock keys are not commands: pressing one must neither
        // trigger a binding nor consume a leader that is waiting.
        if matches!(
            event.code,
            KeyCode::ModifierKey(_) | KeyCode::CapsLock | KeyCode::NumLock | KeyCode::ScrollLock
        ) {
            return Resolution::Passthrough;
        }
        // Holding the leader down repeats it; that is one keypress, not two,
        // so a repeat must not disarm what the first press armed.
        if event.state == KeyState::Repeat && self.leader_armed {
            if let Some(leader) = self.leader {
                if leader.matches(event) {
                    return Resolution::Pending;
                }
            }
        }

        let lookup = |table: &HashMap<Binding, Action>| {
            table
                .iter()
                .find(|(binding, _)| binding.matches(event))
                .map(|(_, action)| action.clone())
        };

        if self.leader_armed {
            self.leader_armed = false;
            // Pressing the leader twice sends the leader itself to the pane.
            if let Some(leader) = self.leader {
                if leader.matches(event) {
                    return Resolution::Passthrough;
                }
            }
            return match lookup(&self.after_leader) {
                Some(action) => Resolution::Action(action),
                // An unbound key after the leader is swallowed rather than
                // being sent to the pane, so a mistyped binding cannot run a
                // command by accident.
                None => Resolution::Action(Action::Refresh),
            };
        }

        if let Some(action) = lookup(&self.direct) {
            return Resolution::Action(action);
        }
        if let Some(leader) = self.leader {
            if leader.matches(event) {
                self.leader_armed = true;
                return Resolution::Pending;
            }
        }
        Resolution::Passthrough
    }
}

impl Default for Keymap {
    fn default() -> Self {
        Keymap::default_bindings()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode, modifiers: Modifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn ordinary_keys_pass_through() {
        let mut keymap = Keymap::default_bindings();
        assert_eq!(
            keymap.resolve(&press(KeyCode::Char('a'), Modifiers::NONE)),
            Resolution::Passthrough
        );
    }

    #[test]
    fn the_leader_arms_and_then_fires() {
        let mut keymap = Keymap::default_bindings();
        assert_eq!(
            keymap.resolve(&press(KeyCode::Char('a'), Modifiers::CTRL)),
            Resolution::Pending
        );
        assert!(keymap.is_pending());
        assert_eq!(
            keymap.resolve(&press(KeyCode::Char('d'), Modifiers::NONE)),
            Resolution::Action(Action::Split(Axis::Columns))
        );
        assert!(!keymap.is_pending());
    }

    #[test]
    fn pressing_the_leader_twice_sends_it_to_the_pane() {
        let mut keymap = Keymap::default_bindings();
        keymap.resolve(&press(KeyCode::Char('a'), Modifiers::CTRL));
        assert_eq!(
            keymap.resolve(&press(KeyCode::Char('a'), Modifiers::CTRL)),
            Resolution::Passthrough
        );
    }

    #[test]
    fn the_ime_toggle_is_a_binding_and_not_the_key_a_jis_keyboard_sends_as_a_backtick() {
        let mut keymap = Keymap::default_bindings();
        // 半角/全角 arrives as `KEY_GRAVE`, so a backtick has to stay a
        // backtick: a default bound to that key would be a default that types
        // one every time somebody tried to turn the IME on.
        assert_eq!(
            keymap.resolve(&press(KeyCode::Char('`'), Modifiers::NONE)),
            Resolution::Passthrough
        );
        assert_eq!(
            keymap.resolve(&press(KeyCode::Char('i'), Modifiers::SUPER)),
            Resolution::Action(Action::ImeToggle)
        );
        // And the かな key, which has nothing else it could mean.
        assert_eq!(
            keymap.resolve(&press(KeyCode::Ime(ImeKey::KanaMode), Modifiers::NONE)),
            Resolution::Action(Action::ImeToggle)
        );
        // The other two conversion keys belong to the preedit rather than to
        // the keymap, so they go on through to the compositor's IME arm.
        for code in [ImeKey::Convert, ImeKey::NonConvert] {
            assert_eq!(
                keymap.resolve(&press(KeyCode::Ime(code), Modifiers::NONE)),
                Resolution::Passthrough
            );
        }
    }

    #[test]
    fn super_bindings_fire_without_the_leader() {
        let mut keymap = Keymap::default_bindings();
        assert_eq!(
            keymap.resolve(&press(KeyCode::Char('s'), Modifiers::SUPER)),
            Resolution::Action(Action::Split(Axis::Rows))
        );
    }

    #[test]
    fn an_unbound_key_after_the_leader_is_swallowed() {
        // Otherwise a mistyped binding would run whatever the pane made of it.
        let mut keymap = Keymap::default_bindings();
        keymap.resolve(&press(KeyCode::Char('a'), Modifiers::CTRL));
        let resolution = keymap.resolve(&press(KeyCode::Char('@'), Modifiers::NONE));
        assert_ne!(resolution, Resolution::Passthrough);
    }

    #[test]
    fn releases_never_trigger_bindings() {
        let mut keymap = Keymap::default_bindings();
        let event = press(KeyCode::Char('s'), Modifiers::SUPER).with_state(KeyState::Release);
        assert_eq!(keymap.resolve(&event), Resolution::Passthrough);
    }

    #[test]
    fn modifier_presses_do_not_disarm_the_leader() {
        let mut keymap = Keymap::default_bindings();
        keymap.resolve(&press(KeyCode::Char('a'), Modifiers::CTRL));
        let shift = press(
            KeyCode::ModifierKey(tos_input::ModifierKey::LeftShift),
            Modifiers::SHIFT,
        );
        assert_eq!(keymap.resolve(&shift), Resolution::Passthrough);
        assert!(keymap.is_pending(), "leader should still be armed");
    }

    #[test]
    fn holding_the_leader_does_not_disarm_it() {
        // Autorepeat is one keypress held down, not a second press.
        let mut keymap = Keymap::default_bindings();
        assert_eq!(
            keymap.resolve(&press(KeyCode::Char('a'), Modifiers::CTRL)),
            Resolution::Pending
        );
        for _ in 0..5 {
            let repeat = press(KeyCode::Char('a'), Modifiers::CTRL).with_state(KeyState::Repeat);
            assert_eq!(keymap.resolve(&repeat), Resolution::Pending);
            assert!(keymap.is_pending());
        }
        assert_eq!(
            keymap.resolve(&press(KeyCode::Char('d'), Modifiers::NONE)),
            Resolution::Action(Action::Split(Axis::Columns))
        );
    }

    #[test]
    fn lock_keys_do_not_consume_an_armed_leader() {
        let mut keymap = Keymap::default_bindings();
        keymap.resolve(&press(KeyCode::Char('a'), Modifiers::CTRL));
        for lock in [KeyCode::CapsLock, KeyCode::NumLock, KeyCode::ScrollLock] {
            assert_eq!(
                keymap.resolve(&press(lock, Modifiers::NONE)),
                Resolution::Passthrough
            );
            assert!(keymap.is_pending(), "{lock:?} ate the leader");
        }
        assert_eq!(
            keymap.resolve(&press(KeyCode::Char('s'), Modifiers::NONE)),
            Resolution::Action(Action::Split(Axis::Rows))
        );
    }

    #[test]
    fn lock_keys_do_not_block_bindings() {
        let mut keymap = Keymap::default_bindings();
        let event = press(
            KeyCode::Char('s'),
            Modifiers::SUPER.union(Modifiers::CAPS_LOCK),
        );
        assert_eq!(
            keymap.resolve(&event),
            Resolution::Action(Action::Split(Axis::Rows))
        );
    }

    #[test]
    fn workspace_digits_are_bound() {
        let mut keymap = Keymap::default_bindings();
        assert_eq!(
            keymap.resolve(&press(KeyCode::Char('3'), Modifiers::SUPER)),
            Resolution::Action(Action::SelectWorkspace(3))
        );
        assert_eq!(
            keymap.resolve(&press(
                KeyCode::Char('3'),
                Modifiers::SUPER.union(Modifiers::SHIFT)
            )),
            Resolution::Action(Action::MovePaneToWorkspace(3))
        );
    }

    #[test]
    fn ctrl_shift_enter_splits_and_ctrl_shift_t_opens_a_workspace() {
        let mut keymap = Keymap::default_bindings();
        assert_eq!(
            keymap.resolve(&press(
                KeyCode::Enter,
                Modifiers::CTRL.union(Modifiers::SHIFT)
            )),
            Resolution::Action(Action::Split(Axis::Columns))
        );
        assert_eq!(
            keymap.resolve(&press(
                KeyCode::Char('t'),
                Modifiers::CTRL.union(Modifiers::SHIFT)
            )),
            Resolution::Action(Action::NewWorkspace)
        );
    }

    #[test]
    fn the_ctrl_shift_bindings_need_both_modifiers() {
        // Plain enter and a lone ctrl+t belong to the program in the pane.
        let mut keymap = Keymap::default_bindings();
        for (code, modifiers) in [
            (KeyCode::Enter, Modifiers::NONE),
            (KeyCode::Enter, Modifiers::CTRL),
            (KeyCode::Enter, Modifiers::SHIFT),
            (KeyCode::Char('t'), Modifiers::NONE),
            (KeyCode::Char('t'), Modifiers::CTRL),
            (KeyCode::Char('t'), Modifiers::SHIFT),
        ] {
            assert_eq!(
                keymap.resolve(&press(code, modifiers)),
                Resolution::Passthrough,
                "{code:?} with {modifiers:?} should reach the pane"
            );
        }
    }

    #[test]
    fn space_opens_the_launcher_with_or_without_the_leader() {
        let mut keymap = Keymap::default_bindings();
        assert_eq!(
            keymap.resolve(&press(KeyCode::Char(' '), Modifiers::SUPER)),
            Resolution::Action(Action::OpenLauncher)
        );
        keymap.resolve(&press(KeyCode::Char('a'), Modifiers::CTRL));
        assert_eq!(
            keymap.resolve(&press(KeyCode::Char(' '), Modifiers::NONE)),
            Resolution::Action(Action::OpenLauncher)
        );
        // A plain space is still a space, which is most of what a pane gets.
        assert_eq!(
            keymap.resolve(&press(KeyCode::Char(' '), Modifiers::NONE)),
            Resolution::Passthrough
        );
    }

    #[test]
    fn comma_renames_the_workspace_with_or_without_the_leader() {
        let mut keymap = Keymap::default_bindings();
        assert_eq!(
            keymap.resolve(&press(KeyCode::Char(','), Modifiers::SUPER)),
            Resolution::Action(Action::RenameWorkspace)
        );
        keymap.resolve(&press(KeyCode::Char('a'), Modifiers::CTRL));
        assert_eq!(
            keymap.resolve(&press(KeyCode::Char(','), Modifiers::NONE)),
            Resolution::Action(Action::RenameWorkspace)
        );
        // A plain comma is a comma, which the pane is owed.
        assert_eq!(
            keymap.resolve(&press(KeyCode::Char(','), Modifiers::NONE)),
            Resolution::Passthrough
        );
    }

    #[test]
    fn a_question_mark_asks_what_the_bindings_are() {
        // The question mark arrives as the slash key with shift, because a
        // `KeyCode::Char` is the unshifted key.
        let mut keymap = Keymap::default_bindings();
        keymap.resolve(&press(KeyCode::Char('a'), Modifiers::CTRL));
        assert_eq!(
            keymap.resolve(&press(KeyCode::Char('/'), Modifiers::SHIFT)),
            Resolution::Action(Action::ShowBindings)
        );
        assert_eq!(
            keymap.resolve(&press(
                KeyCode::Char('/'),
                Modifiers::SUPER.union(Modifiers::SHIFT)
            )),
            Resolution::Action(Action::ShowBindings)
        );
        // An unshifted slash is still a slash, which panes need for paths.
        assert_eq!(
            keymap.resolve(&press(KeyCode::Char('/'), Modifiers::NONE)),
            Resolution::Passthrough
        );
    }

    #[test]
    fn the_status_bar_can_be_hidden_from_the_keyboard() {
        // `--no-status-bar` is a decision taken before there is a session;
        // this is the same decision taken while looking at one.
        let mut keymap = Keymap::default_bindings();
        let shift_s = || press(KeyCode::Char('s'), Modifiers::SHIFT);
        assert_eq!(
            keymap.resolve(&press(
                KeyCode::Char('s'),
                Modifiers::SUPER.union(Modifiers::SHIFT)
            )),
            Resolution::Action(Action::ToggleStatusBar)
        );
        keymap.resolve(&press(KeyCode::Char('a'), Modifiers::CTRL));
        assert_eq!(
            keymap.resolve(&shift_s()),
            Resolution::Action(Action::ToggleStatusBar)
        );
        // A shifted s is an S, which is most of what a pane gets it for, and
        // the unshifted key still splits.
        assert_eq!(keymap.resolve(&shift_s()), Resolution::Passthrough);
        keymap.resolve(&press(KeyCode::Char('a'), Modifiers::CTRL));
        assert_eq!(
            keymap.resolve(&press(KeyCode::Char('s'), Modifiers::NONE)),
            Resolution::Action(Action::Split(Axis::Rows))
        );
    }

    #[test]
    fn scrollback_keys_need_no_prefix() {
        let mut keymap = Keymap::default_bindings();
        assert_eq!(
            keymap.resolve(&press(KeyCode::PageUp, Modifiers::SHIFT)),
            Resolution::Action(Action::ScrollPage(-1))
        );
    }

    #[test]
    fn a_keyboards_own_volume_keys_need_no_modifier_and_no_leader() {
        let mut keymap = Keymap::default_bindings();
        for (media, action) in [
            (MediaKey::VolumeUp, Action::VolumeUp),
            (MediaKey::VolumeDown, Action::VolumeDown),
            (MediaKey::Mute, Action::ToggleMute),
        ] {
            assert_eq!(
                keymap.resolve(&press(KeyCode::Media(media), Modifiers::NONE)),
                Resolution::Action(action)
            );
        }
    }

    #[test]
    fn the_volume_combination_does_not_take_the_keys_under_it() {
        // Shift is what tells the three of them apart from a plain comma,
        // which renames a workspace, and from a plain m, which opens the
        // notification list. Losing either to a mistake here would be a
        // binding silently stolen rather than a volume key that does nothing.
        let mut keymap = Keymap::default_bindings();
        keymap.resolve(&press(KeyCode::Char('a'), Modifiers::CTRL));
        assert_eq!(
            keymap.resolve(&press(KeyCode::Char('.'), Modifiers::SHIFT)),
            Resolution::Action(Action::VolumeUp)
        );
        keymap.resolve(&press(KeyCode::Char('a'), Modifiers::CTRL));
        assert_eq!(
            keymap.resolve(&press(KeyCode::Char(','), Modifiers::NONE)),
            Resolution::Action(Action::RenameWorkspace)
        );
        keymap.resolve(&press(KeyCode::Char('a'), Modifiers::CTRL));
        assert_eq!(
            keymap.resolve(&press(KeyCode::Char('m'), Modifiers::NONE)),
            Resolution::Action(Action::ShowNotifications)
        );
        assert_eq!(
            keymap.resolve(&press(
                KeyCode::Char('m'),
                Modifiers::SUPER.union(Modifiers::SHIFT)
            )),
            Resolution::Action(Action::ToggleMute)
        );
    }

    #[test]
    fn custom_bindings_replace_defaults() {
        let mut keymap = Keymap::empty();
        keymap.bind(
            Binding::new(KeyCode::Function(1), Modifiers::NONE),
            Action::Quit,
        );
        assert_eq!(
            keymap.resolve(&press(KeyCode::Function(1), Modifiers::NONE)),
            Resolution::Action(Action::Quit)
        );
        keymap.unbind(&Binding::new(KeyCode::Function(1), Modifiers::NONE));
        assert_eq!(
            keymap.resolve(&press(KeyCode::Function(1), Modifiers::NONE)),
            Resolution::Passthrough
        );
    }

    #[test]
    fn b_opens_the_bluetooth_controls_with_or_without_the_leader() {
        let mut keymap = Keymap::default_bindings();
        assert_eq!(
            keymap.resolve(&press(KeyCode::Char('b'), Modifiers::SUPER)),
            Resolution::Action(Action::ShowBluetooth)
        );
        keymap.resolve(&press(KeyCode::Char('a'), Modifiers::CTRL));
        assert_eq!(
            keymap.resolve(&press(KeyCode::Char('b'), Modifiers::NONE)),
            Resolution::Action(Action::ShowBluetooth)
        );
        // A plain b is a b, which is most of what a pane is typed.
        assert_eq!(
            keymap.resolve(&press(KeyCode::Char('b'), Modifiers::NONE)),
            Resolution::Passthrough
        );
    }

    #[test]
    fn the_power_menu_answers_to_the_gesture_people_already_have() {
        let mut keymap = Keymap::default_bindings();
        assert_eq!(
            keymap.resolve(&press(
                KeyCode::Delete,
                Modifiers::CTRL.union(Modifiers::ALT)
            )),
            Resolution::Action(Action::PowerMenu)
        );
        assert_eq!(
            keymap.resolve(&press(KeyCode::Delete, Modifiers::SUPER)),
            Resolution::Action(Action::PowerMenu)
        );
        keymap.resolve(&press(KeyCode::Char('a'), Modifiers::CTRL));
        assert_eq!(
            keymap.resolve(&press(KeyCode::Delete, Modifiers::NONE)),
            Resolution::Action(Action::PowerMenu)
        );
        // A bare delete is the key that deletes a character, which is most of
        // what a pane gets it for.
        assert_eq!(
            keymap.resolve(&press(KeyCode::Delete, Modifiers::NONE)),
            Resolution::Passthrough
        );
    }
}
