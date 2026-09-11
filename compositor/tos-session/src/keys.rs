//! Compositor key bindings.
//!
//! tOS owns the whole keyboard, so bindings are direct combinations rather
//! than a prefix key. A leader key is still supported, because a nested
//! development session cannot rely on the super key reaching the compositor.

use std::collections::HashMap;

use tos_input::{KeyCode, KeyEvent, Modifiers};

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
    /// Scroll the focused pane's viewport, in lines. Negative is back in time.
    Scroll(i32),
    /// Scroll by a screenful.
    ScrollPage(i32),
    /// Jump back to the live screen.
    ScrollToBottom,
    Copy,
    Paste,
    /// Start a selection with the keyboard.
    BeginSelection,
    /// Leave the compositor.
    Quit,
    /// Redraw everything.
    Refresh,
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
    direct: HashMap<Binding, Action>,
    /// Bindings that fire only after the leader key.
    after_leader: HashMap<Binding, Action>,
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
    /// nested sessions, where super never arrives.
    pub fn default_bindings() -> Self {
        let mut keymap = Keymap::empty();
        keymap.leader = Some(Binding::new(KeyCode::Char('a'), Modifiers::CTRL));

        let bindings: &[(KeyCode, Modifiers, Action)] = &[
            (KeyCode::Char('d'), Modifiers::NONE, Action::Split(Axis::Columns)),
            (KeyCode::Char('s'), Modifiers::NONE, Action::Split(Axis::Rows)),
            (KeyCode::Char('x'), Modifiers::NONE, Action::ClosePane),
            (KeyCode::Left, Modifiers::NONE, Action::Focus(Direction::Left)),
            (KeyCode::Right, Modifiers::NONE, Action::Focus(Direction::Right)),
            (KeyCode::Up, Modifiers::NONE, Action::Focus(Direction::Up)),
            (KeyCode::Down, Modifiers::NONE, Action::Focus(Direction::Down)),
            (KeyCode::Char('h'), Modifiers::NONE, Action::Focus(Direction::Left)),
            (KeyCode::Char('l'), Modifiers::NONE, Action::Focus(Direction::Right)),
            (KeyCode::Char('k'), Modifiers::NONE, Action::Focus(Direction::Up)),
            (KeyCode::Char('j'), Modifiers::NONE, Action::Focus(Direction::Down)),
            (KeyCode::Left, Modifiers::SHIFT, Action::Resize(Direction::Left, 2)),
            (KeyCode::Right, Modifiers::SHIFT, Action::Resize(Direction::Right, 2)),
            (KeyCode::Up, Modifiers::SHIFT, Action::Resize(Direction::Up, 1)),
            (KeyCode::Down, Modifiers::SHIFT, Action::Resize(Direction::Down, 1)),
            (KeyCode::Char('z'), Modifiers::NONE, Action::ToggleZoom),
            (KeyCode::Char('='), Modifiers::NONE, Action::Balance),
            (KeyCode::Char('c'), Modifiers::NONE, Action::NewWorkspace),
            (KeyCode::Char('n'), Modifiers::NONE, Action::NextWorkspace),
            (KeyCode::Char('p'), Modifiers::NONE, Action::PreviousWorkspace),
            (KeyCode::PageUp, Modifiers::NONE, Action::ScrollPage(-1)),
            (KeyCode::PageDown, Modifiers::NONE, Action::ScrollPage(1)),
            (KeyCode::Char('['), Modifiers::NONE, Action::BeginSelection),
            (KeyCode::Char(']'), Modifiers::NONE, Action::Paste),
            (KeyCode::Char('y'), Modifiers::NONE, Action::Copy),
            (KeyCode::Char('r'), Modifiers::NONE, Action::Refresh),
            (KeyCode::Char('q'), Modifiers::NONE, Action::Quit),
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
        // A modifier on its own never triggers or cancels anything.
        if matches!(event.code, KeyCode::ModifierKey(_)) {
            return Resolution::Passthrough;
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
    use tos_input::KeyState;

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
    fn scrollback_keys_need_no_prefix() {
        let mut keymap = Keymap::default_bindings();
        assert_eq!(
            keymap.resolve(&press(KeyCode::PageUp, Modifiers::SHIFT)),
            Resolution::Action(Action::ScrollPage(-1))
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
}
