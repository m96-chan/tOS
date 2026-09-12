//! tOS input.

pub mod encode;
#[cfg(target_os = "linux")]
pub mod evdev;
pub mod event;
pub mod host;
// The layout table is plain data, so it builds and is tested everywhere even
// though only the evdev backend consumes it.
pub mod keymap;

pub use encode::{encode_focus, encode_key, encode_mouse, encode_paste, EncodeContext};
pub use event::{
    ImeKey, InputEvent, KeyCode, KeyEvent, KeyState, Keypad, MediaKey, ModifierKey, Modifiers,
    MouseAction, MouseButton, MouseEvent, PointerEvent,
};
