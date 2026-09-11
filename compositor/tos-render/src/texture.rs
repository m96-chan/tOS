//! A cache of already-scaled image textures.
//!
//! Scaling is the expensive half of drawing a graphics placement: every
//! destination pixel costs an integer divide to find the source pixel it
//! samples, and a placement is usually the same size on every frame of its
//! life. Keeping the scaled result means a repeat frame is a straight copy
//! and blend.
//!
//! This is a CPU cache. tOS composites into DRM dumb buffers and has no GPU
//! pipeline, so there is no texture upload here and nothing to hand to a
//! driver; the name says what it is.

use std::collections::HashMap;

use crate::surface::Rect;

/// An image region resampled to a destination size.
///
/// Pixels are packed ARGB so that blitting needs no byte shuffling: the alpha
/// is tested and the low 24 bits are the colour the surface stores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Texture {
    width: u32,
    height: u32,
    pixels: Vec<u32>,
}

impl Texture {
    /// Resample `region` of an RGBA8 image to `width` x `height`.
    ///
    /// The sampling arithmetic is deliberately identical to
    /// [`crate::Surface::blit_rgba_region`], which is the uncached path, so
    /// that a cached frame and an uncached one cannot disagree by a pixel.
    pub fn scale(
        src: &[u8],
        src_width: u32,
        src_height: u32,
        region: Rect,
        width: u32,
        height: u32,
    ) -> Texture {
        let region = region.intersect(&Rect::new(0, 0, src_width, src_height));
        if region.is_empty() || width == 0 || height == 0 {
            return Texture {
                width: 0,
                height: 0,
                pixels: Vec::new(),
            };
        }
        let mut pixels = Vec::with_capacity((width as usize) * (height as usize));
        for row in 0..height {
            let v = (row * region.height) / height;
            let v = (region.y as u32 + v).min(region.bottom() as u32 - 1);
            for col in 0..width {
                let u = (col * region.width) / width;
                let u = (region.x as u32 + u).min(region.right() as u32 - 1);
                let offset = ((v * src_width + u) * 4) as usize;
                pixels.push(
                    ((src[offset + 3] as u32) << 24)
                        | ((src[offset] as u32) << 16)
                        | ((src[offset + 1] as u32) << 8)
                        | src[offset + 2] as u32,
                );
            }
        }
        Texture {
            width,
            height,
            pixels,
        }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// The packed ARGB pixels, row major.
    pub fn pixels(&self) -> &[u32] {
        &self.pixels
    }

    /// How much of the cache budget this texture occupies.
    pub fn bytes(&self) -> usize {
        self.pixels.len() * std::mem::size_of::<u32>()
    }
}

/// Everything that decides what a texture's pixels are.
///
/// `version` is the point of this type. An application may re-transmit an
/// image under an id it has already used, which changes the pixels while
/// leaving every other field the same; without it the cache would happily
/// serve the old picture forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TextureKey {
    /// Graphics protocol image id.
    pub image_id: u32,
    /// Generation of that id's pixel data, from `Image::version`.
    pub version: u64,
    /// Source rectangle within the image.
    pub src: Rect,
    pub dest_width: u32,
    pub dest_height: u32,
}

impl TextureKey {
    pub fn new(image_id: u32, version: u64, src: Rect, dest_width: u32, dest_height: u32) -> Self {
        TextureKey {
            image_id,
            version,
            src,
            dest_width,
            dest_height,
        }
    }

    /// Clamp the source rectangle to the image, so that two placements that
    /// only differ in how far they overhang share one entry.
    fn normalized(mut self, src_width: u32, src_height: u32) -> Self {
        self.src = self.src.intersect(&Rect::new(0, 0, src_width, src_height));
        self
    }
}

struct Entry {
    texture: Texture,
    /// Value of the cache clock when this entry was last handed out.
    used: u64,
}

/// Default budget for one pane's scaled textures.
///
/// Small next to the 256 MiB the image store is allowed, because these are
/// derived pixels: losing one costs a rescale, not a re-transmission.
pub const DEFAULT_BUDGET: usize = 64 * 1024 * 1024;

/// Scaled textures, bounded in bytes and evicted least-recently-used first.
pub struct TextureCache {
    entries: HashMap<TextureKey, Entry>,
    bytes: usize,
    budget: usize,
    clock: u64,
    hits: u64,
    misses: u64,
}

impl Default for TextureCache {
    fn default() -> Self {
        TextureCache::new(DEFAULT_BUDGET)
    }
}

impl TextureCache {
    pub fn new(budget_bytes: usize) -> Self {
        TextureCache {
            entries: HashMap::new(),
            bytes: 0,
            budget: budget_bytes,
            clock: 0,
            hits: 0,
            misses: 0,
        }
    }

    /// The scaled texture for `key`, resampling `src` only on a miss.
    ///
    /// Returns `None` when there is nothing to draw, or when one texture
    /// would not fit the budget on its own; the caller falls back to scaling
    /// straight into the surface rather than thrashing the cache.
    pub fn get_or_scale(
        &mut self,
        key: TextureKey,
        src: &[u8],
        src_width: u32,
        src_height: u32,
    ) -> Option<&Texture> {
        let key = key.normalized(src_width, src_height);
        if key.src.is_empty() || key.dest_width == 0 || key.dest_height == 0 {
            return None;
        }
        self.clock += 1;
        let clock = self.clock;

        if self.entries.contains_key(&key) {
            self.hits += 1;
        } else {
            self.misses += 1;
            let texture = Texture::scale(
                src,
                src_width,
                src_height,
                key.src,
                key.dest_width,
                key.dest_height,
            );
            if texture.is_empty() {
                return None;
            }
            let bytes = texture.bytes();
            if bytes > self.budget {
                return None;
            }
            self.evict_until_free(bytes);
            self.bytes += bytes;
            self.entries.insert(key, Entry { texture, used: clock });
        }

        // Touching the entry on a hit as well as an insert is what makes the
        // eviction order least-recently-*used* rather than least recently
        // added, so a placement that is on screen every frame stays resident.
        let entry = self.entries.get_mut(&key)?;
        entry.used = clock;
        Some(&entry.texture)
    }

    /// Drop the least recently used entries until `bytes` more will fit.
    fn evict_until_free(&mut self, bytes: usize) {
        while self.bytes + bytes > self.budget {
            let victim = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.used)
                .map(|(key, _)| *key);
            match victim {
                Some(key) => {
                    if let Some(entry) = self.entries.remove(&key) {
                        self.bytes -= entry.texture.bytes();
                    }
                }
                None => break,
            }
        }
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Bytes of texture data currently held.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn budget(&self) -> usize {
        self.budget
    }

    /// Lookups that found a texture, and lookups that had to build one.
    pub fn hits(&self) -> u64 {
        self.hits
    }

    pub fn misses(&self) -> u64 {
        self.misses
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 2x2 image: red, green / blue, white.
    fn checker() -> Vec<u8> {
        vec![
            255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
        ]
    }

    fn key(version: u64, width: u32, height: u32) -> TextureKey {
        TextureKey::new(1, version, Rect::new(0, 0, 2, 2), width, height)
    }

    #[test]
    fn scaling_matches_nearest_sampling() {
        let texture = Texture::scale(&checker(), 2, 2, Rect::new(0, 0, 2, 2), 4, 4);
        assert_eq!(texture.width(), 4);
        assert_eq!(texture.pixels()[0], 0xffff_0000);
        assert_eq!(texture.pixels()[3], 0xff00_ff00);
        assert_eq!(texture.pixels()[12], 0xff00_00ff);
        assert_eq!(texture.pixels()[15], 0xffff_ffff);
    }

    #[test]
    fn repeat_frames_do_not_rescale() {
        let mut cache = TextureCache::default();
        let src = checker();
        for _ in 0..4 {
            assert!(cache.get_or_scale(key(1, 8, 8), &src, 2, 2).is_some());
        }
        assert_eq!(cache.misses(), 1);
        assert_eq!(cache.hits(), 3);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn retransmitting_an_image_invalidates_its_texture() {
        let mut cache = TextureCache::default();
        let first = checker();
        let scaled = cache.get_or_scale(key(1, 2, 2), &first, 2, 2).unwrap();
        assert_eq!(scaled.pixels()[0], 0xffff_0000);

        // Same id, same region, same destination: only the version differs.
        let mut second = checker();
        second[0..4].copy_from_slice(&[0, 0, 0, 255]);
        let scaled = cache.get_or_scale(key(2, 2, 2), &second, 2, 2).unwrap();
        assert_eq!(scaled.pixels()[0], 0xff00_0000);
        assert_eq!(cache.misses(), 2);
    }

    #[test]
    fn a_different_destination_size_is_a_different_texture() {
        let mut cache = TextureCache::default();
        let src = checker();
        cache.get_or_scale(key(1, 2, 2), &src, 2, 2);
        cache.get_or_scale(key(1, 4, 4), &src, 2, 2);
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.misses(), 2);
    }

    #[test]
    fn a_different_source_region_is_a_different_texture() {
        let mut cache = TextureCache::default();
        let src = checker();
        let whole = TextureKey::new(1, 1, Rect::new(0, 0, 2, 2), 2, 2);
        let corner = TextureKey::new(1, 1, Rect::new(1, 1, 1, 1), 2, 2);
        assert_eq!(
            cache.get_or_scale(whole, &src, 2, 2).unwrap().pixels()[0],
            0xffff_0000
        );
        assert_eq!(
            cache.get_or_scale(corner, &src, 2, 2).unwrap().pixels()[0],
            0xffff_ffff
        );
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn an_overhanging_region_keys_the_same_as_its_clamped_self() {
        let mut cache = TextureCache::default();
        let src = checker();
        let exact = TextureKey::new(1, 1, Rect::new(0, 0, 2, 2), 2, 2);
        let overhanging = TextureKey::new(1, 1, Rect::new(0, 0, 9, 9), 2, 2);
        cache.get_or_scale(exact, &src, 2, 2);
        cache.get_or_scale(overhanging, &src, 2, 2);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.hits(), 1);
    }

    #[test]
    fn the_budget_evicts_the_least_recently_used_entry() {
        // Room for two 2x2 textures and no more.
        let mut cache = TextureCache::new(2 * 2 * 2 * 4);
        let src = checker();
        cache.get_or_scale(key(1, 2, 2), &src, 2, 2);
        cache.get_or_scale(key(2, 2, 2), &src, 2, 2);
        // Touch the older one so the newer becomes the eviction candidate.
        cache.get_or_scale(key(1, 2, 2), &src, 2, 2);
        cache.get_or_scale(key(3, 2, 2), &src, 2, 2);

        assert_eq!(cache.len(), 2);
        assert!(cache.bytes() <= cache.budget());
        assert_eq!(cache.hits(), 1);
        // Version 2 was the least recently used, so it is the one that went.
        assert_eq!(cache.misses(), 3);
        cache.get_or_scale(key(2, 2, 2), &src, 2, 2);
        assert_eq!(cache.misses(), 4);
    }

    #[test]
    fn a_texture_larger_than_the_budget_is_not_cached() {
        let mut cache = TextureCache::new(16);
        let src = checker();
        assert!(cache.get_or_scale(key(1, 64, 64), &src, 2, 2).is_none());
        assert!(cache.is_empty());
        assert_eq!(cache.bytes(), 0);
    }

    #[test]
    fn an_empty_region_has_no_texture() {
        let mut cache = TextureCache::default();
        let src = checker();
        let outside = TextureKey::new(1, 1, Rect::new(8, 8, 2, 2), 4, 4);
        assert!(cache.get_or_scale(outside, &src, 2, 2).is_none());
        assert!(cache.is_empty());
    }

    #[test]
    fn clearing_releases_the_bytes() {
        let mut cache = TextureCache::default();
        let src = checker();
        cache.get_or_scale(key(1, 4, 4), &src, 2, 2);
        assert!(cache.bytes() > 0);
        cache.clear();
        assert_eq!(cache.bytes(), 0);
        assert!(cache.is_empty());
    }
}
