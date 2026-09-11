//! A display that draws into memory.
//!
//! Used by tests and by `tos --screenshot`, which is how the rendering path
//! can be inspected on a machine with no DRM device.

use std::io;
use std::path::{Path, PathBuf};

use tos_render::{OwnedFramebuffer, Surface};

use crate::display::Display;

/// An off screen display.
pub struct HeadlessDisplay {
    framebuffer: OwnedFramebuffer,
    /// Where each frame is written, if anywhere.
    output: Option<PathBuf>,
    frames: u64,
}

impl HeadlessDisplay {
    pub fn new(width: u32, height: u32) -> Self {
        HeadlessDisplay {
            framebuffer: OwnedFramebuffer::new(width, height),
            output: None,
            frames: 0,
        }
    }

    /// Write every frame to this path as a PPM image.
    pub fn writing_to(mut self, path: impl Into<PathBuf>) -> Self {
        self.output = Some(path.into());
        self
    }

    pub fn frames_drawn(&self) -> u64 {
        self.frames
    }

    pub fn framebuffer(&self) -> &OwnedFramebuffer {
        &self.framebuffer
    }

    /// Save the current contents as a PPM image.
    pub fn save(&self, path: impl AsRef<Path>) -> io::Result<()> {
        std::fs::write(path, self.framebuffer.to_ppm())
    }
}

impl Display for HeadlessDisplay {
    fn size(&self) -> (u32, u32) {
        (self.framebuffer.width(), self.framebuffer.height())
    }

    fn frame(&mut self, draw: &mut dyn FnMut(&mut Surface<'_>)) -> io::Result<()> {
        {
            let mut surface = self.framebuffer.surface();
            draw(&mut surface);
        }
        self.frames += 1;
        if let Some(path) = self.output.clone() {
            self.save(path)?;
        }
        Ok(())
    }

    fn retains_contents(&self) -> bool {
        true
    }

    fn name(&self) -> String {
        let (w, h) = self.size();
        format!("headless {w}x{h}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tos_render::Rect;
    use tos_term::Rgb;

    #[test]
    fn frames_are_drawn_into_the_buffer() {
        let mut display = HeadlessDisplay::new(4, 4);
        display
            .frame(&mut |surface| surface.clear(Rgb::new(1, 2, 3)))
            .unwrap();
        assert_eq!(display.framebuffer().pixel(0, 0), 0x010203);
        assert_eq!(display.frames_drawn(), 1);
    }

    #[test]
    fn contents_persist_between_frames() {
        let mut display = HeadlessDisplay::new(4, 4);
        display
            .frame(&mut |surface| surface.clear(Rgb::WHITE))
            .unwrap();
        display
            .frame(&mut |surface| surface.fill(Rect::new(0, 0, 1, 1), Rgb::BLACK))
            .unwrap();
        assert_eq!(display.framebuffer().pixel(0, 0), 0);
        assert_eq!(display.framebuffer().pixel(3, 3), 0xffffff);
    }

    #[test]
    fn saving_writes_a_ppm() {
        let mut display = HeadlessDisplay::new(2, 2);
        display
            .frame(&mut |surface| surface.clear(Rgb::WHITE))
            .unwrap();
        let path = std::env::temp_dir().join("tos-headless-test.ppm");
        display.save(&path).unwrap();
        let data = std::fs::read(&path).unwrap();
        assert!(data.starts_with(b"P6\n2 2\n255\n"));
        let _ = std::fs::remove_file(path);
    }
}
