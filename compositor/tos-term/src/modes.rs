//! Terminal modes: the DEC private modes and ANSI modes tOS honours, plus the
//! mouse and keyboard reporting state that input encoding depends on.

/// Boolean terminal modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Modes {
    /// DECAWM: wrap at the right margin.
    pub wraparound: bool,
    /// DECOM: cursor addressing is relative to the scroll region.
    pub origin: bool,
    /// IRM: printing shifts the rest of the line right.
    pub insert: bool,
    /// LNM: line feed also carries a carriage return.
    pub linefeed_newline: bool,
    /// DECSCNM: swap foreground and background across the whole screen.
    pub reverse_video: bool,
    /// DECTCEM: show the cursor.
    pub cursor_visible: bool,
    /// DECCKM: cursor keys send SS3 instead of CSI.
    pub application_cursor_keys: bool,
    /// DECNKM: keypad sends application sequences.
    pub application_keypad: bool,
    /// The alternate screen buffer is active.
    pub alt_screen: bool,
    /// Bracketed paste (2004).
    pub bracketed_paste: bool,
    /// Focus in/out reporting (1004).
    pub focus_events: bool,
    /// Synchronized output (2026): hold painting until the app is done.
    pub synchronized_output: bool,
    /// Auto-repeat (DECARM); tracked so queries answer truthfully.
    pub autorepeat: bool,
    /// Column mode (DECCOLM) is accepted but never resizes the pane.
    pub allow_column_mode: bool,
}

impl Default for Modes {
    fn default() -> Self {
        Modes {
            wraparound: true,
            origin: false,
            insert: false,
            linefeed_newline: false,
            reverse_video: false,
            cursor_visible: true,
            application_cursor_keys: false,
            application_keypad: false,
            alt_screen: false,
            bracketed_paste: false,
            focus_events: false,
            synchronized_output: false,
            autorepeat: true,
            allow_column_mode: false,
        }
    }
}

/// Which mouse events an application asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MouseTracking {
    #[default]
    None,
    /// 9: press only.
    X10,
    /// 1000: press and release.
    Normal,
    /// 1002: press, release and drag while a button is held.
    ButtonEvent,
    /// 1003: every motion event.
    AnyEvent,
}

impl MouseTracking {
    pub fn is_enabled(self) -> bool {
        !matches!(self, MouseTracking::None)
    }

    pub fn reports_motion(self) -> bool {
        matches!(self, MouseTracking::ButtonEvent | MouseTracking::AnyEvent)
    }

    pub fn reports_all_motion(self) -> bool {
        matches!(self, MouseTracking::AnyEvent)
    }

    pub fn reports_release(self) -> bool {
        !matches!(self, MouseTracking::None | MouseTracking::X10)
    }
}

/// How mouse reports are encoded on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MouseEncoding {
    /// The original, byte-limited encoding.
    #[default]
    X10,
    /// 1005: UTF-8 coordinates.
    Utf8,
    /// 1006: SGR, the only encoding that handles large screens correctly.
    Sgr,
    /// 1015: urxvt decimal.
    Urxvt,
    /// 1016: SGR again, with the position in pixels rather than cells. A cell
    /// is as much as a TUI can aim at; a program drawing its own picture in
    /// the pane — a browser with a link, a scrollbar or a caret in it — needs
    /// to know where in the cell the pointer landed.
    SgrPixels,
}

impl MouseEncoding {
    /// Whether a release report names the button that was let go of.
    ///
    /// The legacy encodings have one release code for every button and lose
    /// it; both SGR encodings say which button it was and end the report with
    /// `m` instead of `M`, which is the ambiguity they were invented to fix.
    pub fn reports_the_released_button(self) -> bool {
        matches!(self, MouseEncoding::Sgr | MouseEncoding::SgrPixels)
    }
}

/// Mouse reporting state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MouseState {
    pub tracking: MouseTracking,
    pub encoding: MouseEncoding,
    /// 1007: wheel events become arrow keys on the alternate screen.
    pub alternate_scroll: bool,
}

impl MouseState {
    pub fn is_enabled(self) -> bool {
        self.tracking.is_enabled()
    }
}

/// Kitty keyboard protocol progressive enhancement flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KeyboardFlags(pub u8);

impl KeyboardFlags {
    pub const DISAMBIGUATE: KeyboardFlags = KeyboardFlags(0b0_0001);
    pub const REPORT_EVENT_TYPES: KeyboardFlags = KeyboardFlags(0b0_0010);
    pub const REPORT_ALTERNATE_KEYS: KeyboardFlags = KeyboardFlags(0b0_0100);
    pub const REPORT_ALL_KEYS_AS_ESCAPE: KeyboardFlags = KeyboardFlags(0b0_1000);
    pub const REPORT_ASSOCIATED_TEXT: KeyboardFlags = KeyboardFlags(0b1_0000);

    pub const fn contains(self, other: KeyboardFlags) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

/// Stack of keyboard flags, as required by CSI > u / CSI < u.
#[derive(Debug, Clone, Default)]
pub struct KeyboardStack {
    current: KeyboardFlags,
    stack: Vec<KeyboardFlags>,
}

const MAX_KEYBOARD_STACK: usize = 16;

impl KeyboardStack {
    pub fn current(&self) -> KeyboardFlags {
        self.current
    }

    /// CSI = flags ; mode u
    pub fn set(&mut self, flags: KeyboardFlags, mode: u16) {
        self.current = match mode {
            1 => flags,
            2 => KeyboardFlags(self.current.0 | flags.0),
            3 => KeyboardFlags(self.current.0 & !flags.0),
            _ => flags,
        };
    }

    /// CSI > flags u
    pub fn push(&mut self, flags: KeyboardFlags) {
        if self.stack.len() == MAX_KEYBOARD_STACK {
            self.stack.remove(0);
        }
        self.stack.push(self.current);
        self.current = flags;
    }

    /// CSI < n u
    pub fn pop(&mut self, count: usize) {
        for _ in 0..count.max(1) {
            self.current = self.stack.pop().unwrap_or_default();
        }
    }

    pub fn reset(&mut self) {
        self.current = KeyboardFlags::default();
        self.stack.clear();
    }
}

/// Cursor shapes selectable with DECSCUSR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CursorShape {
    #[default]
    Block,
    Underline,
    Beam,
    /// Drawn as an outline; used for unfocused panes.
    Hollow,
}

/// Cursor style: shape plus whether it blinks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorStyle {
    pub shape: CursorShape,
    pub blinking: bool,
}

impl Default for CursorStyle {
    fn default() -> Self {
        CursorStyle {
            shape: CursorShape::Block,
            blinking: true,
        }
    }
}

impl CursorStyle {
    /// DECSCUSR parameter to style.
    pub fn from_decscusr(param: u16) -> CursorStyle {
        match param {
            0 | 1 => CursorStyle {
                shape: CursorShape::Block,
                blinking: true,
            },
            2 => CursorStyle {
                shape: CursorShape::Block,
                blinking: false,
            },
            3 => CursorStyle {
                shape: CursorShape::Underline,
                blinking: true,
            },
            4 => CursorStyle {
                shape: CursorShape::Underline,
                blinking: false,
            },
            5 => CursorStyle {
                shape: CursorShape::Beam,
                blinking: true,
            },
            6 => CursorStyle {
                shape: CursorShape::Beam,
                blinking: false,
            },
            _ => CursorStyle::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyboard_stack_push_pop() {
        let mut kb = KeyboardStack::default();
        kb.set(KeyboardFlags::DISAMBIGUATE, 1);
        kb.push(KeyboardFlags::REPORT_EVENT_TYPES);
        assert_eq!(kb.current(), KeyboardFlags::REPORT_EVENT_TYPES);
        kb.pop(1);
        assert_eq!(kb.current(), KeyboardFlags::DISAMBIGUATE);
        kb.pop(1);
        assert!(kb.current().is_empty());
    }

    #[test]
    fn keyboard_set_modes_combine() {
        let mut kb = KeyboardStack::default();
        kb.set(KeyboardFlags::DISAMBIGUATE, 1);
        kb.set(KeyboardFlags::REPORT_EVENT_TYPES, 2);
        assert!(kb.current().contains(KeyboardFlags::DISAMBIGUATE));
        assert!(kb.current().contains(KeyboardFlags::REPORT_EVENT_TYPES));
        kb.set(KeyboardFlags::DISAMBIGUATE, 3);
        assert!(!kb.current().contains(KeyboardFlags::DISAMBIGUATE));
    }

    #[test]
    fn decscusr_shapes() {
        assert_eq!(CursorStyle::from_decscusr(4).shape, CursorShape::Underline);
        assert!(!CursorStyle::from_decscusr(4).blinking);
        assert_eq!(CursorStyle::from_decscusr(5).shape, CursorShape::Beam);
        assert!(CursorStyle::from_decscusr(5).blinking);
    }
}
