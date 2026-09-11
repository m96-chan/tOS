//! Drawing surfaces.
//!
//! A surface is a borrowed rectangle of XRGB8888 pixels. Borrowing rather than
//! owning is what lets the renderer draw straight into a DRM dumb buffer with
//! no intermediate copy, while tests and the nested backend hand it an
//! ordinary `Vec`.

use tos_term::Rgb;

/// An axis aligned rectangle in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    pub fn right(&self) -> i32 {
        self.x + self.width as i32
    }

    pub fn bottom(&self) -> i32 {
        self.y + self.height as i32
    }

    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && y >= self.y && x < self.right() && y < self.bottom()
    }

    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// The overlap of two rectangles, empty when they do not touch.
    pub fn intersect(&self, other: &Rect) -> Rect {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        Rect {
            x,
            y,
            width: (right - x).max(0) as u32,
            height: (bottom - y).max(0) as u32,
        }
    }
}

/// A mutable view of a pixel buffer.
pub struct Surface<'a> {
    pixels: &'a mut [u32],
    width: u32,
    height: u32,
    /// Pixels per row, which can exceed `width` on hardware framebuffers.
    stride: u32,
    /// Drawing outside this rectangle is discarded.
    clip: Rect,
}

impl<'a> Surface<'a> {
    /// Wrap a pixel buffer. `stride` is in pixels, not bytes.
    pub fn new(pixels: &'a mut [u32], width: u32, height: u32, stride: u32) -> Self {
        assert!(stride >= width, "stride must cover the visible width");
        assert!(
            pixels.len() >= (stride * height) as usize,
            "buffer is smaller than {stride}x{height}"
        );
        Surface {
            pixels,
            width,
            height,
            stride,
            clip: Rect::new(0, 0, width, height),
        }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn stride(&self) -> u32 {
        self.stride
    }

    pub fn clip(&self) -> Rect {
        self.clip
    }

    /// Restrict drawing to `rect`, intersected with the current clip.
    pub fn set_clip(&mut self, rect: Rect) {
        self.clip = rect.intersect(&Rect::new(0, 0, self.width, self.height));
    }

    pub fn reset_clip(&mut self) {
        self.clip = Rect::new(0, 0, self.width, self.height);
    }

    /// Run `f` with a temporary clip, restoring the previous one afterwards.
    pub fn with_clip<R>(&mut self, rect: Rect, f: impl FnOnce(&mut Surface<'a>) -> R) -> R {
        let saved = self.clip;
        self.set_clip(rect.intersect(&saved));
        let result = f(self);
        self.clip = saved;
        result
    }

    pub fn pixels(&self) -> &[u32] {
        self.pixels
    }

    #[inline]
    pub fn put(&mut self, x: i32, y: i32, color: u32) {
        if !self.clip.contains(x, y) {
            return;
        }
        self.pixels[(y as u32 * self.stride + x as u32) as usize] = color;
    }

    #[inline]
    pub fn get(&self, x: i32, y: i32) -> u32 {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return 0;
        }
        self.pixels[(y as u32 * self.stride + x as u32) as usize]
    }

    pub fn fill(&mut self, rect: Rect, color: Rgb) {
        let rect = rect.intersect(&self.clip);
        if rect.is_empty() {
            return;
        }
        let packed = color.pack();
        for y in rect.y..rect.bottom() {
            let start = (y as u32 * self.stride + rect.x as u32) as usize;
            let end = start + rect.width as usize;
            self.pixels[start..end].fill(packed);
        }
    }

    pub fn clear(&mut self, color: Rgb) {
        let all = Rect::new(0, 0, self.width, self.height);
        self.fill(all, color);
    }

    /// Blend `color` over the surface with a coverage mask.
    ///
    /// `coverage` is `width * height` alpha values placed at `(x, y)`.
    pub fn blend_mask(
        &mut self,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        coverage: &[u8],
        color: Rgb,
    ) {
        if width == 0 || height == 0 {
            return;
        }
        for row in 0..height {
            let py = y + row as i32;
            if py < self.clip.y || py >= self.clip.bottom() {
                continue;
            }
            for col in 0..width {
                let alpha = coverage[(row * width + col) as usize];
                if alpha == 0 {
                    continue;
                }
                let px = x + col as i32;
                if px < self.clip.x || px >= self.clip.right() {
                    continue;
                }
                let index = (py as u32 * self.stride + px as u32) as usize;
                self.pixels[index] = if alpha == 0xff {
                    color.pack()
                } else {
                    blend_packed(self.pixels[index], color, alpha)
                };
            }
        }
    }

    /// Composite an RGBA image, scaling it into `dest` with nearest sampling.
    pub fn blit_rgba(&mut self, dest: Rect, src: &[u8], src_width: u32, src_height: u32) {
        let whole = Rect::new(0, 0, src_width, src_height);
        self.blit_rgba_region(dest, src, src_width, src_height, whole);
    }

    /// Composite part of an RGBA image, scaling `region` into `dest`.
    pub fn blit_rgba_region(
        &mut self,
        dest: Rect,
        src: &[u8],
        src_width: u32,
        src_height: u32,
        region: Rect,
    ) {
        let clipped = dest.intersect(&self.clip);
        let region = region.intersect(&Rect::new(0, 0, src_width, src_height));
        if clipped.is_empty() || region.is_empty() {
            return;
        }
        for py in clipped.y..clipped.bottom() {
            // Map the destination pixel back to a source pixel.
            let v = ((py - dest.y) as u32 * region.height) / dest.height.max(1);
            let v = (region.y as u32 + v).min(region.bottom() as u32 - 1);
            for px in clipped.x..clipped.right() {
                let u = ((px - dest.x) as u32 * region.width) / dest.width.max(1);
                let u = (region.x as u32 + u).min(region.right() as u32 - 1);
                let offset = ((v * src_width + u) * 4) as usize;
                let (r, g, b, a) = (
                    src[offset],
                    src[offset + 1],
                    src[offset + 2],
                    src[offset + 3],
                );
                if a == 0 {
                    continue;
                }
                let index = (py as u32 * self.stride + px as u32) as usize;
                let color = Rgb::new(r, g, b);
                self.pixels[index] = if a == 0xff {
                    color.pack()
                } else {
                    blend_packed(self.pixels[index], color, a)
                };
            }
        }
    }
}

/// Blend `color` over a packed XRGB pixel.
#[inline]
fn blend_packed(dest: u32, color: Rgb, alpha: u8) -> u32 {
    let dr = (dest >> 16) & 0xff;
    let dg = (dest >> 8) & 0xff;
    let db = dest & 0xff;
    let a = alpha as u32;
    let inv = 255 - a;
    let r = (color.r as u32 * a + dr * inv) / 255;
    let g = (color.g as u32 * a + dg * inv) / 255;
    let b = (color.b as u32 * a + db * inv) / 255;
    (r << 16) | (g << 8) | b
}

/// A framebuffer that owns its pixels, for tests, screenshots and backends
/// that need to compose before presenting.
pub struct OwnedFramebuffer {
    pixels: Vec<u32>,
    width: u32,
    height: u32,
}

impl OwnedFramebuffer {
    pub fn new(width: u32, height: u32) -> Self {
        OwnedFramebuffer {
            pixels: vec![0; (width * height) as usize],
            width,
            height,
        }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == self.width && height == self.height {
            return;
        }
        self.width = width;
        self.height = height;
        self.pixels.clear();
        self.pixels.resize((width * height) as usize, 0);
    }

    pub fn surface(&mut self) -> Surface<'_> {
        Surface::new(&mut self.pixels, self.width, self.height, self.width)
    }

    pub fn pixels(&self) -> &[u32] {
        &self.pixels
    }

    pub fn pixel(&self, x: u32, y: u32) -> u32 {
        self.pixels[(y * self.width + x) as usize]
    }

    /// Serialize as a binary PPM, which every image viewer can open.
    pub fn to_ppm(&self) -> Vec<u8> {
        let mut out = format!("P6\n{} {}\n255\n", self.width, self.height).into_bytes();
        out.reserve(self.pixels.len() * 3);
        for &px in &self.pixels {
            out.push(((px >> 16) & 0xff) as u8);
            out.push(((px >> 8) & 0xff) as u8);
            out.push((px & 0xff) as u8);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn framebuffer() -> OwnedFramebuffer {
        OwnedFramebuffer::new(8, 4)
    }

    #[test]
    fn fill_writes_the_rectangle_only() {
        let mut fb = framebuffer();
        let mut s = fb.surface();
        s.fill(Rect::new(2, 1, 3, 2), Rgb::new(0xff, 0, 0));
        assert_eq!(fb.pixel(2, 1), 0xff0000);
        assert_eq!(fb.pixel(4, 2), 0xff0000);
        assert_eq!(fb.pixel(5, 1), 0);
        assert_eq!(fb.pixel(2, 3), 0);
    }

    #[test]
    fn clipping_discards_outside_pixels() {
        let mut fb = framebuffer();
        let mut s = fb.surface();
        s.set_clip(Rect::new(0, 0, 4, 4));
        s.fill(Rect::new(0, 0, 8, 4), Rgb::WHITE);
        assert_eq!(fb.pixel(3, 0), 0xffffff);
        assert_eq!(fb.pixel(4, 0), 0);
    }

    #[test]
    fn negative_coordinates_are_clipped() {
        let mut fb = framebuffer();
        let mut s = fb.surface();
        s.fill(Rect::new(-4, -2, 8, 4), Rgb::WHITE);
        assert_eq!(fb.pixel(0, 0), 0xffffff);
        assert_eq!(fb.pixel(4, 2), 0);
    }

    #[test]
    fn blend_mask_interpolates() {
        let mut fb = framebuffer();
        {
            let mut s = fb.surface();
            s.clear(Rgb::BLACK);
            // Half coverage of white over black is mid grey.
            s.blend_mask(0, 0, 1, 1, &[128], Rgb::WHITE);
        }
        let px = fb.pixel(0, 0);
        let r = (px >> 16) & 0xff;
        assert!((120..=136).contains(&r), "unexpected blend: {r}");
    }

    #[test]
    fn full_coverage_writes_the_exact_color() {
        let mut fb = framebuffer();
        {
            let mut s = fb.surface();
            s.blend_mask(1, 1, 2, 1, &[255, 255], Rgb::new(1, 2, 3));
        }
        assert_eq!(fb.pixel(1, 1), 0x010203);
        assert_eq!(fb.pixel(2, 1), 0x010203);
    }

    #[test]
    fn blit_scales_with_nearest_sampling() {
        let mut fb = OwnedFramebuffer::new(4, 4);
        // A 2x2 image: red, green / blue, white.
        let src = vec![
            255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
        ];
        {
            let mut s = fb.surface();
            s.blit_rgba(Rect::new(0, 0, 4, 4), &src, 2, 2);
        }
        assert_eq!(fb.pixel(0, 0), 0xff0000);
        assert_eq!(fb.pixel(3, 0), 0x00ff00);
        assert_eq!(fb.pixel(0, 3), 0x0000ff);
        assert_eq!(fb.pixel(3, 3), 0xffffff);
    }

    #[test]
    fn transparent_pixels_are_skipped() {
        let mut fb = OwnedFramebuffer::new(2, 1);
        {
            let mut s = fb.surface();
            s.clear(Rgb::new(0x11, 0x22, 0x33));
            let src = vec![255, 255, 255, 0, 255, 255, 255, 255];
            s.blit_rgba(Rect::new(0, 0, 2, 1), &src, 2, 1);
        }
        assert_eq!(fb.pixel(0, 0), 0x112233);
        assert_eq!(fb.pixel(1, 0), 0xffffff);
    }

    #[test]
    fn stride_larger_than_width_is_respected() {
        let mut pixels = vec![0u32; 16];
        {
            let mut s = Surface::new(&mut pixels, 3, 4, 4);
            s.fill(Rect::new(0, 1, 3, 1), Rgb::WHITE);
        }
        // Row 1 starts at index 4 because the stride is 4.
        assert_eq!(&pixels[4..8], &[0xffffff, 0xffffff, 0xffffff, 0]);
    }

    #[test]
    fn ppm_header_and_length_are_correct() {
        let fb = OwnedFramebuffer::new(2, 2);
        let ppm = fb.to_ppm();
        assert!(ppm.starts_with(b"P6\n2 2\n255\n"));
        assert_eq!(ppm.len(), b"P6\n2 2\n255\n".len() + 12);
    }

    #[test]
    fn rect_intersection() {
        let a = Rect::new(0, 0, 10, 10);
        let b = Rect::new(5, 5, 10, 10);
        assert_eq!(a.intersect(&b), Rect::new(5, 5, 5, 5));
        let c = Rect::new(20, 20, 1, 1);
        assert!(a.intersect(&c).is_empty());
    }
}
