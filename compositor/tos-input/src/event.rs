//! The input event model.
//!
//! The compositor routes these; the encoders in [`crate::encode`] turn them
//! into the bytes an application expects. Keeping the model separate from the
//! wire format is what lets the same event drive a compositor keybinding, a
//! legacy xterm sequence or the Kitty keyboard protocol.

/// Modifier keys held when an event happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Modifiers(pub u8);

impl Modifiers {
    pub const NONE: Modifiers = Modifiers(0);
    pub const SHIFT: Modifiers = Modifiers(1 << 0);
    pub const ALT: Modifiers = Modifiers(1 << 1);
    pub const CTRL: Modifiers = Modifiers(1 << 2);
    pub const SUPER: Modifiers = Modifiers(1 << 3);
    pub const HYPER: Modifiers = Modifiers(1 << 4);
    pub const META: Modifiers = Modifiers(1 << 5);
    pub const CAPS_LOCK: Modifiers = Modifiers(1 << 6);
    pub const NUM_LOCK: Modifiers = Modifiers(1 << 7);

    pub const fn contains(self, other: Modifiers) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn union(self, other: Modifiers) -> Modifiers {
        Modifiers(self.0 | other.0)
    }

    pub const fn without(self, other: Modifiers) -> Modifiers {
        Modifiers(self.0 & !other.0)
    }

    pub fn insert(&mut self, other: Modifiers) {
        self.0 |= other.0;
    }

    pub fn remove(&mut self, other: Modifiers) {
        self.0 &= !other.0;
    }

    pub fn set(&mut self, other: Modifiers, on: bool) {
        if on {
            self.insert(other)
        } else {
            self.remove(other)
        }
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn shift(self) -> bool {
        self.contains(Modifiers::SHIFT)
    }

    pub fn alt(self) -> bool {
        self.contains(Modifiers::ALT)
    }

    pub fn ctrl(self) -> bool {
        self.contains(Modifiers::CTRL)
    }

    /// Modifiers that affect what an application receives, ignoring the lock
    /// keys, which only matter to the Kitty protocol.
    pub fn effective(self) -> Modifiers {
        self.without(Modifiers::CAPS_LOCK.union(Modifiers::NUM_LOCK))
    }

    /// The xterm style modifier parameter: 1 plus a bitmask.
    pub fn xterm_param(self) -> u32 {
        let mut value = 0;
        if self.contains(Modifiers::SHIFT) {
            value |= 1;
        }
        if self.contains(Modifiers::ALT) {
            value |= 2;
        }
        if self.contains(Modifiers::CTRL) {
            value |= 4;
        }
        if self.contains(Modifiers::SUPER) {
            value |= 8;
        }
        if self.contains(Modifiers::HYPER) {
            value |= 16;
        }
        if self.contains(Modifiers::META) {
            value |= 32;
        }
        if self.contains(Modifiers::CAPS_LOCK) {
            value |= 64;
        }
        if self.contains(Modifiers::NUM_LOCK) {
            value |= 128;
        }
        value + 1
    }
}

/// Keys on the numeric keypad, which send different sequences in application
/// keypad mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Keypad {
    Digit(u8),
    Decimal,
    Divide,
    Multiply,
    Subtract,
    Add,
    Enter,
    Equal,
    Separator,
    Begin,
}

/// A logical key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyCode {
    /// A key that produces text; the char is the unshifted base key.
    Char(char),
    Enter,
    Tab,
    Backspace,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    /// Function keys, 1 based.
    Function(u8),
    Keypad(Keypad),
    CapsLock,
    NumLock,
    ScrollLock,
    PrintScreen,
    Pause,
    Menu,
    /// A modifier pressed on its own.
    ModifierKey(ModifierKey),
    /// A conversion key from a Japanese keyboard.
    Ime(ImeKey),
    /// Recognised by the driver but not mapped to anything.
    Unknown(u32),
}

/// The conversion keys a JIS keyboard has and a US one does not.
///
/// These are neither characters nor modifiers: nothing about them makes sense
/// as text, and holding one changes no other key. They get their own variant
/// rather than [`KeyCode::Unknown`] because an input method has to match on
/// them by meaning, and a scancode would tie that match to the driver. The
/// names are the W3C UI Events ones rather than the labels printed on the
/// keycaps, because that is the vocabulary an input method and a keybinding
/// file are already written against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ImeKey {
    /// 変換 (henkan): convert what has been typed so far.
    Convert,
    /// 無変換 (muhenkan): take what has been typed so far unconverted.
    NonConvert,
    /// かな / カタカナひらがな (katakana-hiragana): switch the input mode.
    KanaMode,
}

/// Which physical modifier key was pressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModifierKey {
    LeftShift,
    RightShift,
    LeftCtrl,
    RightCtrl,
    LeftAlt,
    RightAlt,
    LeftSuper,
    RightSuper,
}

impl ModifierKey {
    pub fn modifier(self) -> Modifiers {
        match self {
            ModifierKey::LeftShift | ModifierKey::RightShift => Modifiers::SHIFT,
            ModifierKey::LeftCtrl | ModifierKey::RightCtrl => Modifiers::CTRL,
            ModifierKey::LeftAlt => Modifiers::ALT,
            // The right alt key is AltGr on many layouts, but tOS treats it as
            // alt until layout support arrives.
            ModifierKey::RightAlt => Modifiers::ALT,
            ModifierKey::LeftSuper | ModifierKey::RightSuper => Modifiers::SUPER,
        }
    }
}

/// Whether a key went down, repeated, or came up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyState {
    Press,
    Repeat,
    Release,
}

impl KeyState {
    /// The event type number used by the Kitty keyboard protocol.
    pub fn kitty_event_type(self) -> u32 {
        match self {
            KeyState::Press => 1,
            KeyState::Repeat => 2,
            KeyState::Release => 3,
        }
    }
}

/// A key event.
#[derive(Debug, Clone, PartialEq)]
pub struct KeyEvent {
    pub code: KeyCode,
    pub modifiers: Modifiers,
    pub state: KeyState,
    /// The text this keypress produces, after the layout and shift state have
    /// been applied. Empty for keys that produce no text.
    pub text: Option<char>,
    /// The key as it would be without shift, for the Kitty alternate key
    /// reporting.
    pub base: Option<char>,
}

impl KeyEvent {
    pub fn new(code: KeyCode, modifiers: Modifiers) -> Self {
        let text = match code {
            KeyCode::Char(c) => Some(c),
            _ => None,
        };
        KeyEvent {
            code,
            modifiers,
            state: KeyState::Press,
            text,
            base: text,
        }
    }

    pub fn with_state(mut self, state: KeyState) -> Self {
        self.state = state;
        self
    }

    pub fn with_text(mut self, text: Option<char>) -> Self {
        self.text = text;
        self
    }

    pub fn is_press(&self) -> bool {
        matches!(self.state, KeyState::Press | KeyState::Repeat)
    }
}

/// Mouse buttons, including the wheel, which terminals report as buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
    WheelLeft,
    WheelRight,
    /// Buttons 8 and above.
    Other(u8),
}

impl MouseButton {
    /// The button number used in terminal mouse reports.
    ///
    /// Buttons 8 and up occupy 128..131; the variant already carries the
    /// button number, so the base has to be subtracted, not added to.
    pub fn report_code(self) -> u32 {
        match self {
            MouseButton::Left => 0,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
            MouseButton::WheelUp => 64,
            MouseButton::WheelDown => 65,
            MouseButton::WheelLeft => 66,
            MouseButton::WheelRight => 67,
            MouseButton::Other(n) => 128 + (n as u32).saturating_sub(8),
        }
    }

    /// The button a report code names, if any. `3` means "no button", which
    /// the legacy encodings use for a release.
    pub fn from_report_code(code: u32) -> Option<MouseButton> {
        Some(match code {
            0 => MouseButton::Left,
            1 => MouseButton::Middle,
            2 => MouseButton::Right,
            64 => MouseButton::WheelUp,
            65 => MouseButton::WheelDown,
            66 => MouseButton::WheelLeft,
            67 => MouseButton::WheelRight,
            128..=131 => MouseButton::Other((code - 128 + 8) as u8),
            _ => return None,
        })
    }

    pub fn is_wheel(self) -> bool {
        matches!(
            self,
            MouseButton::WheelUp
                | MouseButton::WheelDown
                | MouseButton::WheelLeft
                | MouseButton::WheelRight
        )
    }
}

/// What the mouse did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MouseAction {
    Press,
    Release,
    /// Movement with a button held.
    Drag,
    /// Movement with no button held.
    Motion,
}

/// A mouse event, already translated into grid coordinates by the compositor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseEvent {
    pub button: Option<MouseButton>,
    pub action: MouseAction,
    /// Zero based cell coordinates within the pane.
    pub col: usize,
    pub row: usize,
    pub modifiers: Modifiers,
}

/// A pointer event in display pixels, as a device reports it.
///
/// This is deliberately a different type from [`MouseEvent`]: a device knows
/// pixels and nothing about cells, while the encoders need cells. Keeping the
/// two apart means the conversion has to be written out, rather than being
/// forgotten in one of the two input paths.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PointerEvent {
    pub button: Option<MouseButton>,
    pub action: MouseAction,
    /// Absolute position on the display, in pixels.
    pub x: f64,
    pub y: f64,
    pub modifiers: Modifiers,
}

/// Everything that can arrive from an input device.
#[derive(Debug, Clone, PartialEq)]
pub enum InputEvent {
    Key(KeyEvent),
    /// A mouse event already in cell coordinates, as a host terminal reports it.
    Mouse(MouseEvent),
    /// A pointer event in display pixels, as a device reports it.
    Pointer(PointerEvent),
    /// Text that arrived as a unit, such as a paste.
    Paste(String),
    FocusGained,
    FocusLost,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xterm_modifier_params_match_the_convention() {
        assert_eq!(Modifiers::NONE.xterm_param(), 1);
        assert_eq!(Modifiers::SHIFT.xterm_param(), 2);
        assert_eq!(Modifiers::ALT.xterm_param(), 3);
        assert_eq!(Modifiers::SHIFT.union(Modifiers::ALT).xterm_param(), 4);
        assert_eq!(Modifiers::CTRL.xterm_param(), 5);
        assert_eq!(
            Modifiers::CTRL.union(Modifiers::ALT).union(Modifiers::SHIFT).xterm_param(),
            8
        );
    }

    #[test]
    fn lock_keys_do_not_count_as_effective_modifiers() {
        let mods = Modifiers::CTRL.union(Modifiers::CAPS_LOCK);
        assert_eq!(mods.effective(), Modifiers::CTRL);
    }

    #[test]
    fn wheel_buttons_report_in_the_high_range() {
        assert_eq!(MouseButton::WheelUp.report_code(), 64);
        assert!(MouseButton::WheelDown.is_wheel());
        assert!(!MouseButton::Left.is_wheel());
    }

    #[test]
    fn extra_buttons_use_the_128_range() {
        assert_eq!(MouseButton::Other(8).report_code(), 128);
        assert_eq!(MouseButton::Other(11).report_code(), 131);
    }

    #[test]
    fn report_codes_round_trip() {
        for button in [
            MouseButton::Left,
            MouseButton::Middle,
            MouseButton::Right,
            MouseButton::WheelUp,
            MouseButton::WheelDown,
            MouseButton::Other(8),
            MouseButton::Other(11),
        ] {
            assert_eq!(
                MouseButton::from_report_code(button.report_code()),
                Some(button)
            );
        }
        // Three is the legacy "released, button unknown" code.
        assert_eq!(MouseButton::from_report_code(3), None);
    }

    #[test]
    fn modifier_keys_map_to_modifiers() {
        assert_eq!(ModifierKey::LeftCtrl.modifier(), Modifiers::CTRL);
        assert_eq!(ModifierKey::RightShift.modifier(), Modifiers::SHIFT);
    }
}
