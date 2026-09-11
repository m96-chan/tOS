//! tOS input.

pub mod encode;
pub mod event;
#[cfg(target_os = "linux")]
pub mod evdev;
pub mod host;
// The layout table is plain data, so it builds and is tested everywhere even
// though only the evdev backend consumes it.
pub mod keymap;

pub use encode::{encode_focus, encode_key, encode_mouse, encode_paste, EncodeContext};
pub use event::{
    InputEvent, KeyCode, KeyEvent, KeyState, Keypad, ModifierKey, Modifiers, MouseAction,
    MouseButton, MouseEvent, PointerEvent,
};
