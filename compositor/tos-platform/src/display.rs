//! The display backend interface.
//!
//! Everything above this line works in terms of a pixel surface and knows
//! nothing about how it reaches a screen. That is what lets the same
//! compositor run on DRM/KMS, nested in another terminal during development,
//! or headless in a test.

use std::io;

use tos_render::Surface;

/// A display tOS can draw on.
pub trait Display {
    /// Size of the visible area in pixels.
    fn size(&self) -> (u32, u32);

    /// Physical size in millimetres, when the hardware reports it. Used to
    /// pick a sensible font size.
    fn physical_size(&self) -> Option<(u32, u32)> {
        None
    }

    /// Draw one frame and put it on screen.
    ///
    /// The callback receives the surface for the frame; the backend decides
    /// whether that is a hardware buffer, a scratch buffer, or something that
    /// gets translated into escape sequences.
    fn frame(&mut self, draw: &mut dyn FnMut(&mut Surface<'_>)) -> io::Result<()>;

    /// Whether consecutive frames share a buffer, so that only damaged rows
    /// need to be repainted.
    fn retains_contents(&self) -> bool {
        false
    }

    /// Release the display, for example when switching away from the VT.
    fn release(&mut self) -> io::Result<()> {
        Ok(())
    }

    /// Reacquire it.
    fn restore(&mut self) -> io::Result<()> {
        Ok(())
    }

    /// A human readable name, for logs.
    fn name(&self) -> String {
        "display".to_string()
    }
}

/// A hint for how large text should be on this display.
pub fn suggested_font_size(width: u32, height: u32, physical_mm: Option<(u32, u32)>) -> f32 {
    // Without physical dimensions, guess from resolution alone.
    let Some((mm_width, _)) = physical_mm.filter(|(w, h)| *w > 0 && *h > 0) else {
        return if width >= 2560 {
            24.0
        } else if width >= 1600 {
            18.0
        } else if width >= 1024 {
            15.0
        } else {
            13.0
        };
    };
    let dpi = width as f32 * 25.4 / mm_width as f32;
    // Roughly an 11 point font, clamped to something usable.
    let _ = height;
    (dpi * 11.0 / 72.0).clamp(11.0, 48.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_size_grows_with_resolution() {
        let small = suggested_font_size(1280, 720, None);
        let large = suggested_font_size(3840, 2160, None);
        assert!(large > small);
    }

    #[test]
    fn font_size_uses_physical_size_when_known() {
        // A 4K panel in a 13 inch laptop lid is a high DPI display.
        let dense = suggested_font_size(3840, 2160, Some((294, 165)));
        // The same resolution on a 32 inch monitor is not.
        let sparse = suggested_font_size(3840, 2160, Some((708, 399)));
        assert!(dense > sparse, "{dense} should exceed {sparse}");
    }

    #[test]
    fn font_size_stays_in_a_usable_range() {
        for (w, h, mm) in [
            (640u32, 480u32, None),
            (1920, 1080, Some((530u32, 300u32))),
            (7680, 4320, Some((1200, 700))),
        ] {
            let size = suggested_font_size(w, h, mm);
            assert!((11.0..=48.0).contains(&size), "unusable size {size}");
        }
    }
}
