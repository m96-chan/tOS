//! The standalone APK drives the same compositor as the PC build. JNI and
//! ANativeWindow ownership live in bridge.c; every call is serialized on the
//! Java session's single worker, never on the Android UI thread.

use std::ffi::{c_char, CStr};
use tos_compositor::{Compositor, Config};
use tos_input::{
    InputEvent, KeyCode, KeyEvent, KeyState, Modifiers, MouseAction, MouseButton, PointerEvent,
};
use tos_render::OwnedFramebuffer;
use tos_session::{Action, Axis};

pub struct Engine {
    compositor: Compositor,
    frame: OwnedFramebuffer,
    dirty: bool,
}

fn rgba(pixel: u32) -> u32 {
    0xff00_0000 | ((pixel & 0xff) << 16) | (pixel & 0xff00) | ((pixel >> 16) & 0xff)
}

fn dimensions(width: u32, height: u32) -> bool {
    width > 0
        && height > 0
        && width <= 8192
        && height <= 8192
        && u64::from(width) * u64::from(height) <= 16_777_216
}

fn key(code: u32, unicode: u32) -> Option<KeyCode> {
    Some(match code {
        66 => KeyCode::Enter,
        61 => KeyCode::Tab,
        67 => KeyCode::Backspace,
        111 => KeyCode::Escape,
        19 => KeyCode::Up,
        20 => KeyCode::Down,
        21 => KeyCode::Left,
        22 => KeyCode::Right,
        122 => KeyCode::Home,
        123 => KeyCode::End,
        92 => KeyCode::PageUp,
        93 => KeyCode::PageDown,
        112 => KeyCode::Delete,
        124 => KeyCode::Insert,
        131..=142 => KeyCode::Function((code - 130) as u8),
        _ => KeyCode::Char(
            char::from_u32(unicode)
                .filter(|c| !c.is_control())?
                .to_ascii_lowercase(),
        ),
    })
}

// All exported functions are private to the APK's JNI bridge. Its worker owns
// each Engine pointer, passes only live handles, and destroys each exactly once.
// The C bridge checks bitmap/window dimensions and passes valid slices. These
// contracts are unsafe at the FFI boundary, not assumptions of the renderer.

/// # Safety
/// `home` must be a live NUL-terminated UTF-8 path for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn tos_android_create(
    home: *const c_char,
    width: u32,
    height: u32,
    font: f32,
) -> *mut Engine {
    if home.is_null() || !dimensions(width, height) {
        return std::ptr::null_mut();
    }
    let Ok(home) = CStr::from_ptr(home).to_str() else {
        return std::ptr::null_mut();
    };
    let config = Config {
        command: Some(vec!["/system/bin/sh".into(), "-c".into(),
            "cd \"$1\" || exit; export HOME=\"$1\" TMPDIR=\"$1/tmp\" SHELL=/system/bin/sh PATH=/system/bin:/system/xbin ENV=/dev/null PS1='$ '; exec /system/bin/sh -i".into(),
            "tos".into(), home.into()]),
        font_size: Some(font.clamp(6.0, 128.0)),
        status_bar: false,
        idle_lock: None,
        idle_blank: None,
        system_root: std::path::Path::new(home).join("no-system-controls"),
        ..Config::default()
    };
    match Compositor::new(config, (width, height), None) {
        Ok(mut compositor) => {
            compositor.inject(b"\x1b[38;2;145;180;135mtOS\x1b[0m / Android\r\n\r\n");
            Box::into_raw(Box::new(Engine {
                compositor,
                frame: OwnedFramebuffer::new(width, height),
                dirty: true,
            }))
        }
        Err(error) => {
            eprintln!("tOS Android: {error}");
            std::ptr::null_mut()
        }
    }
}

/// # Safety
/// `engine` must be a live handle, exclusively accessed by the session worker.
#[no_mangle]
pub unsafe extern "C" fn tos_android_tick(engine: *mut Engine) -> i32 {
    let e = &mut *engine;
    e.dirty |= e.compositor.pump_panes();
    e.dirty |= e.compositor.tick();
    if !e.compositor.is_running() {
        return -1;
    }
    i32::from(e.dirty || e.compositor.needs_render())
}

/// # Safety
/// Live exclusive handle; `out` must cover `stride * height` writable u32s.
#[no_mangle]
pub unsafe extern "C" fn tos_android_render(
    engine: *mut Engine,
    out: *mut u32,
    width: u32,
    height: u32,
    stride: u32,
) -> bool {
    if !dimensions(width, height) || stride < width || stride > 16384 || out.is_null() {
        return false;
    }
    let e = &mut *engine;
    e.frame.resize(width, height);
    e.compositor.resize((width, height));
    e.compositor.render_frame(&mut e.frame.surface(), true);
    let destination = std::slice::from_raw_parts_mut(out, stride as usize * height as usize);
    for (source, target) in e
        .frame
        .pixels()
        .chunks_exact(width as usize)
        .zip(destination.chunks_exact_mut(stride as usize))
    {
        for (pixel, output) in source.iter().zip(target) {
            *output = rgba(*pixel);
        }
    }
    e.dirty = false;
    true
}

/// # Safety
/// `engine` must be a live, exclusively owned handle.
#[no_mangle]
pub unsafe extern "C" fn tos_android_resize(
    engine: *mut Engine,
    width: u32,
    height: u32,
    font: f32,
) {
    let e = &mut *engine;
    if dimensions(width, height) {
        e.compositor.resize((width, height));
    }
    e.compositor.set_font_size(font);
    e.compositor.perform(Action::Refresh);
    e.dirty = true;
}

/// # Safety
/// Live exclusive handle; `text` must cover `len` readable UTF-16 code units.
#[no_mangle]
pub unsafe extern "C" fn tos_android_text(
    engine: *mut Engine,
    text: *const u16,
    len: usize,
    modifiers: u8,
    paste: bool,
) {
    if len == 0 || len > 1_048_576 || text.is_null() {
        return;
    }
    let e = &mut *engine;
    let text = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
    if paste {
        e.compositor.handle_input(InputEvent::Paste(text));
    } else {
        for c in text.chars() {
            let code = match c {
                '\n' | '\r' => KeyCode::Enter,
                '\t' => KeyCode::Tab,
                _ => KeyCode::Char(c.to_ascii_lowercase()),
            };
            let event = KeyEvent::new(code, Modifiers(modifiers)).with_text(Some(c));
            e.compositor.handle_input(InputEvent::Key(event));
        }
    }
    e.dirty = true;
}

/// # Safety
/// `engine` must be a live, exclusively owned handle.
#[no_mangle]
pub unsafe extern "C" fn tos_android_key(
    engine: *mut Engine,
    code: u32,
    unicode: u32,
    modifiers: u8,
    release: bool,
) {
    if let Some(code) = key(code, unicode) {
        let event = KeyEvent::new(code, Modifiers(modifiers))
            .with_state(if release {
                KeyState::Release
            } else {
                KeyState::Press
            })
            .with_text(char::from_u32(unicode).filter(|c| *c != '\0'));
        (*engine).compositor.handle_input(InputEvent::Key(event));
        (*engine).dirty = true;
    }
}

/// # Safety
/// `engine` must be a live, exclusively owned handle.
#[no_mangle]
pub unsafe extern "C" fn tos_android_pointer(engine: *mut Engine, x: f64, y: f64, wheel: i32) {
    let e = &mut *engine;
    let button = if wheel > 0 {
        MouseButton::WheelUp
    } else if wheel < 0 {
        MouseButton::WheelDown
    } else {
        MouseButton::Left
    };
    for action in [MouseAction::Press, MouseAction::Release] {
        e.compositor.handle_input(InputEvent::Pointer(PointerEvent {
            x,
            y,
            button: Some(button),
            action,
            modifiers: Modifiers::NONE,
        }));
    }
    e.dirty = true;
}

/// # Safety
/// `engine` must be a live, exclusively owned handle.
#[no_mangle]
pub unsafe extern "C" fn tos_android_action(engine: *mut Engine, action: u32) {
    let action = match action {
        0 => Action::Split(Axis::Columns),
        1 => Action::Split(Axis::Rows),
        2 => Action::ClosePane,
        3 => Action::ToggleZoom,
        4 => Action::CopyMode,
        5 => Action::Copy,
        6 => Action::NewWorkspace,
        7 => Action::NextWorkspace,
        _ => return,
    };
    (*engine).compositor.perform(action);
    (*engine).dirty = true;
}

/// # Safety
/// Live exclusive handle. `len` must point to writable storage. The returned
/// pointer is borrowed until the next call that mutates this engine.
#[no_mangle]
pub unsafe extern "C" fn tos_android_clipboard(engine: *mut Engine, len: *mut usize) -> *const u8 {
    let bytes = (*engine).compositor.clipboard('c').unwrap_or_default();
    *len = bytes.len();
    bytes.as_ptr()
}

/// # Safety
/// `engine` must be a live, exclusively owned handle.
#[no_mangle]
pub unsafe extern "C" fn tos_android_grid(engine: *mut Engine) -> u64 {
    let area = (*engine).compositor.grid_area();
    (u64::from(area.width) << 32) | u64::from(area.height)
}

/// # Safety
/// The handle must have been returned by create, and must not be used again.
#[no_mangle]
pub unsafe extern "C" fn tos_android_destroy(engine: *mut Engine) {
    if !engine.is_null() {
        drop(Box::from_raw(engine));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn android_pixels_are_opaque_and_have_the_right_channel_order() {
        assert_eq!(rgba(0x123456).to_le_bytes(), [0x12, 0x34, 0x56, 0xff]);
    }
    #[test]
    fn hardware_special_keys_and_unicode_are_not_confused() {
        assert_eq!(key(66, 0), Some(KeyCode::Enter));
        assert_eq!(key(0, 'あ' as u32), Some(KeyCode::Char('あ')));
        assert_eq!(key(0, 'A' as u32), Some(KeyCode::Char('a')));
        assert_eq!(key(0, 0), None);
        assert_eq!(key(0, 0xd800), None);
    }
    #[test]
    fn reject_unbounded_surface_allocations() {
        assert!(dimensions(1080, 2400));
        assert!(!dimensions(0, 2400));
        assert!(!dimensions(u32::MAX, 2400));
        assert!(!dimensions(8192, 8192));
    }
}
