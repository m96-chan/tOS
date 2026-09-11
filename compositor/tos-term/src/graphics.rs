//! Kitty graphics protocol.
//!
//! Commands arrive as APC strings: `ESC _ G <key=value,...> ; <base64> ESC \`.
//! This module parses them and owns the image/placement store that cells refer
//! to through [`crate::cell::GraphicsRef`].
//!
//! Raw RGB and RGBA data, PNG (`f=100`) and zlib-compressed payloads (`o=z`)
//! all transmit; PNG and zlib are decoded by [`crate::png`] and
//! [`crate::inflate`]. Anything that cannot be decoded is answered with the
//! protocol's error response rather than being silently dropped, so
//! applications can fall back instead of hanging.

use std::collections::HashMap;

use crate::inflate::{self, InflateError};
use crate::png::{self, PngError};

/// What a command asks the terminal to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// `a=t` transmit only.
    Transmit,
    /// `a=T` transmit and display.
    TransmitAndDisplay,
    /// `a=p` display a previously transmitted image.
    Put,
    /// `a=d` delete images or placements.
    Delete,
    /// `a=q` query support without storing anything.
    Query,
    /// `a=f` / `a=a` animation control, not yet implemented.
    Animate,
}

/// Pixel layout of a transmitted image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Rgb,
    Rgba,
    Png,
}

/// How the payload reaches the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Medium {
    Direct,
    File,
    TempFile,
    SharedMemory,
}

/// A parsed graphics command.
#[derive(Debug, Clone)]
pub struct GraphicsCommand {
    pub action: Action,
    pub format: Format,
    pub medium: Medium,
    pub image_id: u32,
    pub image_number: u32,
    pub placement_id: u32,
    /// Source image dimensions in pixels.
    pub width: u32,
    pub height: u32,
    /// Source rectangle within the image.
    pub src_x: u32,
    pub src_y: u32,
    pub src_w: u32,
    pub src_h: u32,
    /// Target size in cells; 0 means "derive from pixels".
    pub cols: u32,
    pub rows: u32,
    /// Pixel offset within the first cell.
    pub cell_x: u32,
    pub cell_y: u32,
    pub z_index: i32,
    /// `m=1` means more chunks follow.
    pub more: bool,
    /// `C=1` means do not move the cursor after placing.
    pub cursor_stays: bool,
    /// `q` suppresses responses: 1 = ok only, 2 = all.
    pub quiet: u8,
    /// `d` target for delete commands.
    pub delete: u8,
    /// `o=z` zlib compression.
    pub compressed: bool,
    pub payload: Vec<u8>,
}

impl Default for GraphicsCommand {
    fn default() -> Self {
        GraphicsCommand {
            action: Action::Transmit,
            format: Format::Rgba,
            medium: Medium::Direct,
            image_id: 0,
            image_number: 0,
            placement_id: 0,
            width: 0,
            height: 0,
            src_x: 0,
            src_y: 0,
            src_w: 0,
            src_h: 0,
            cols: 0,
            rows: 0,
            cell_x: 0,
            cell_y: 0,
            z_index: 0,
            more: false,
            cursor_stays: false,
            quiet: 0,
            delete: b'a',
            compressed: false,
            payload: Vec::new(),
        }
    }
}

impl GraphicsCommand {
    /// Parse an APC body. The leading `G` has already been matched by the
    /// caller; `body` is everything after it.
    pub fn parse(body: &[u8]) -> Option<GraphicsCommand> {
        let (control, payload) = match body.iter().position(|&b| b == b';') {
            Some(i) => (&body[..i], &body[i + 1..]),
            None => (body, &body[body.len()..]),
        };

        let mut cmd = GraphicsCommand::default();
        for pair in control.split(|&b| b == b',') {
            if pair.is_empty() {
                continue;
            }
            let eq = pair.iter().position(|&b| b == b'=')?;
            let key = pair[..eq].first().copied()?;
            let value = &pair[eq + 1..];
            let text = std::str::from_utf8(value).ok()?;

            match key {
                b'a' => {
                    cmd.action = match value.first()? {
                        b't' => Action::Transmit,
                        b'T' => Action::TransmitAndDisplay,
                        b'p' => Action::Put,
                        b'd' => Action::Delete,
                        b'q' => Action::Query,
                        b'f' | b'a' | b'c' => Action::Animate,
                        _ => return None,
                    }
                }
                b'f' => {
                    cmd.format = match text.parse::<u32>().ok()? {
                        24 => Format::Rgb,
                        32 => Format::Rgba,
                        100 => Format::Png,
                        _ => return None,
                    }
                }
                b't' => {
                    cmd.medium = match value.first()? {
                        b'd' => Medium::Direct,
                        b'f' => Medium::File,
                        b't' => Medium::TempFile,
                        b's' => Medium::SharedMemory,
                        _ => return None,
                    }
                }
                b'i' => cmd.image_id = text.parse().ok()?,
                b'I' => cmd.image_number = text.parse().ok()?,
                b'p' => cmd.placement_id = text.parse().ok()?,
                b's' => cmd.width = text.parse().ok()?,
                b'v' => cmd.height = text.parse().ok()?,
                b'x' => cmd.src_x = text.parse().ok()?,
                b'y' => cmd.src_y = text.parse().ok()?,
                b'w' => cmd.src_w = text.parse().ok()?,
                b'h' => cmd.src_h = text.parse().ok()?,
                b'c' => cmd.cols = text.parse().ok()?,
                b'r' => cmd.rows = text.parse().ok()?,
                b'X' => cmd.cell_x = text.parse().ok()?,
                b'Y' => cmd.cell_y = text.parse().ok()?,
                b'z' => cmd.z_index = text.parse().ok()?,
                b'm' => cmd.more = text.parse::<u32>().ok()? != 0,
                b'C' => cmd.cursor_stays = text.parse::<u32>().ok()? != 0,
                b'q' => cmd.quiet = text.parse().ok()?,
                b'd' => cmd.delete = *value.first().unwrap_or(&b'a'),
                b'o' => cmd.compressed = value.first() == Some(&b'z'),
                // Unknown keys are ignored, as the protocol requires.
                _ => {}
            }
        }

        cmd.payload = decode_base64(payload);
        Some(cmd)
    }

    /// Identifier used in responses: `i=` if given, otherwise `I=`.
    pub fn response_id(&self) -> String {
        if self.image_id != 0 {
            format!("i={}", self.image_id)
        } else {
            format!("I={}", self.image_number)
        }
    }
}

/// A stored image.
#[derive(Debug, Clone)]
pub struct Image {
    pub id: u32,
    pub width: u32,
    pub height: u32,
    /// Always RGBA8, converted at transmission time.
    pub data: Vec<u8>,
}

/// An image placed on the grid.
#[derive(Debug, Clone)]
pub struct Placement {
    pub id: u32,
    pub image_id: u32,
    pub placement_id: u32,
    /// Anchor cell on the screen.
    pub col: u16,
    pub row: u16,
    pub cols: u16,
    pub rows: u16,
    pub src_x: u32,
    pub src_y: u32,
    pub src_w: u32,
    pub src_h: u32,
    pub z_index: i32,
}

/// Image and placement store for one terminal.
#[derive(Debug, Default)]
pub struct GraphicsStore {
    images: HashMap<u32, Image>,
    placements: HashMap<u32, Placement>,
    next_placement: u32,
    next_auto_id: u32,
    /// Partially received image, keyed by image id.
    pending: Option<(u32, GraphicsCommand, Vec<u8>)>,
    /// Total bytes of pixel data held, used to enforce the budget.
    bytes: usize,
    budget: usize,
}

impl GraphicsStore {
    pub fn new(budget_bytes: usize) -> Self {
        GraphicsStore {
            budget: budget_bytes,
            next_placement: 1,
            next_auto_id: 1 << 24,
            ..Default::default()
        }
    }

    pub fn image(&self, id: u32) -> Option<&Image> {
        self.images.get(&id)
    }

    pub fn placement(&self, id: u32) -> Option<&Placement> {
        self.placements.get(&id)
    }

    pub fn placements(&self) -> impl Iterator<Item = &Placement> {
        self.placements.values()
    }

    pub fn is_empty(&self) -> bool {
        self.images.is_empty() && self.placements.is_empty()
    }

    pub fn clear(&mut self) {
        self.images.clear();
        self.placements.clear();
        self.pending = None;
        self.bytes = 0;
    }

    /// Store an image, converting the payload to RGBA. Returns an error string
    /// suitable for the protocol response.
    pub fn store(&mut self, cmd: &GraphicsCommand, payload: &[u8]) -> Result<u32, &'static str> {
        if cmd.medium != Medium::Direct {
            return Err("EINVAL:only direct transmission supported");
        }
        let (w, h, data) = decode_payload(cmd, payload, self.budget)?;

        let id = if cmd.image_id != 0 {
            cmd.image_id
        } else {
            self.next_auto_id += 1;
            self.next_auto_id
        };

        if let Some(old) = self.images.remove(&id) {
            self.bytes -= old.data.len();
        }
        self.bytes += data.len();
        self.images.insert(
            id,
            Image {
                id,
                width: w,
                height: h,
                data,
            },
        );
        self.evict_to_budget(id);
        Ok(id)
    }

    /// Drop least-recently-added images until the byte budget is met. The
    /// image just stored is never evicted.
    fn evict_to_budget(&mut self, keep: u32) {
        while self.bytes > self.budget {
            let victim = self
                .images
                .keys()
                .copied()
                .filter(|&id| id != keep)
                .min_by_key(|id| *id);
            match victim {
                Some(id) => {
                    if let Some(img) = self.images.remove(&id) {
                        self.bytes -= img.data.len();
                    }
                    self.placements.retain(|_, p| p.image_id != id);
                }
                None => break,
            }
        }
    }

    /// Accumulate a chunked transmission. Returns the complete payload when
    /// the final chunk (`m=0`) arrives.
    pub fn accumulate(&mut self, cmd: &GraphicsCommand) -> Option<(GraphicsCommand, Vec<u8>)> {
        let key = if cmd.image_id != 0 {
            cmd.image_id
        } else {
            cmd.image_number
        };
        match self.pending.take() {
            // A continuation either names the same image or, as the protocol
            // allows, names nothing at all. A command that names a *different*
            // image starts a new transmission and supersedes this one.
            Some((pending_key, first, mut buf)) if key == 0 || pending_key == key => {
                buf.extend_from_slice(&cmd.payload);
                if cmd.more {
                    self.pending = Some((pending_key, first, buf));
                    None
                } else {
                    Some((first, buf))
                }
            }
            other => {
                // A new transmission supersedes any abandoned one.
                drop(other);
                if cmd.more {
                    self.pending = Some((key, cmd.clone(), cmd.payload.clone()));
                    None
                } else {
                    Some((cmd.clone(), cmd.payload.clone()))
                }
            }
        }
    }

    /// Create a placement. `cell_w`/`cell_h` give the pixel size of one cell so
    /// the default size can be derived.
    pub fn place(
        &mut self,
        cmd: &GraphicsCommand,
        image_id: u32,
        col: u16,
        row: u16,
        cell_w: u32,
        cell_h: u32,
    ) -> Option<u32> {
        let image = self.images.get(&image_id)?;
        let src_w = if cmd.src_w == 0 {
            image.width.saturating_sub(cmd.src_x)
        } else {
            cmd.src_w.min(image.width.saturating_sub(cmd.src_x))
        };
        let src_h = if cmd.src_h == 0 {
            image.height.saturating_sub(cmd.src_y)
        } else {
            cmd.src_h.min(image.height.saturating_sub(cmd.src_y))
        };
        if src_w == 0 || src_h == 0 {
            return None;
        }

        let cols = if cmd.cols != 0 {
            cmd.cols
        } else {
            src_w.div_ceil(cell_w.max(1))
        };
        let rows = if cmd.rows != 0 {
            cmd.rows
        } else {
            src_h.div_ceil(cell_h.max(1))
        };

        // A placement id supplied by the application replaces the previous
        // placement with the same (image, placement) pair.
        if cmd.placement_id != 0 {
            self.placements
                .retain(|_, p| !(p.image_id == image_id && p.placement_id == cmd.placement_id));
        }

        let id = self.next_placement;
        self.next_placement += 1;
        self.placements.insert(
            id,
            Placement {
                id,
                image_id,
                placement_id: cmd.placement_id,
                col,
                row,
                cols: cols.min(u16::MAX as u32) as u16,
                rows: rows.min(u16::MAX as u32) as u16,
                src_x: cmd.src_x,
                src_y: cmd.src_y,
                src_w,
                src_h,
                z_index: cmd.z_index,
            },
        );
        Some(id)
    }

    /// Handle `a=d`. Returns true when the screen needs repainting.
    pub fn delete(&mut self, cmd: &GraphicsCommand) -> bool {
        let free_data = cmd.delete.is_ascii_uppercase();
        let what = cmd.delete.to_ascii_lowercase();
        let before = self.placements.len();

        match what {
            b'a' => self.placements.clear(),
            b'i' => {
                let image_id = cmd.image_id;
                self.placements.retain(|_, p| {
                    p.image_id != image_id
                        || (cmd.placement_id != 0 && p.placement_id != cmd.placement_id)
                });
                if free_data {
                    if let Some(img) = self.images.remove(&image_id) {
                        self.bytes -= img.data.len();
                    }
                }
            }
            b'z' => {
                let z = cmd.z_index;
                self.placements.retain(|_, p| p.z_index != z);
            }
            _ => {}
        }

        if free_data && what == b'a' {
            self.images.clear();
            self.bytes = 0;
        }
        self.placements.len() != before
    }

    /// Drop placements that scrolled off or were overwritten.
    pub fn retain_rows(&mut self, rows: u16) {
        self.placements.retain(|_, p| p.row < rows);
    }

    /// Shift placements up by `n` rows, dropping those that leave the screen.
    pub fn scroll(&mut self, n: u16) {
        self.placements.retain(|_, p| {
            let bottom = p.row as i32 + p.rows as i32;
            bottom - n as i32 > 0
        });
        for p in self.placements.values_mut() {
            p.row = p.row.saturating_sub(n);
        }
    }
}

/// Turn a transmitted payload into RGBA8 and the dimensions that belong to it.
///
/// `budget` is the store's whole byte allowance, used as the ceiling on
/// anything that has to be decompressed. A payload that would expand past it
/// could never be kept, so it is refused while it is still being decoded
/// instead of after it has been materialised — which is the difference
/// between rejecting a decompression bomb and being flattened by one.
fn decode_payload(
    cmd: &GraphicsCommand,
    payload: &[u8],
    budget: usize,
) -> Result<(u32, u32, Vec<u8>), &'static str> {
    match cmd.format {
        Format::Png => {
            // A PNG carries its own dimensions, so `s=`/`v=` are advisory and
            // the file wins if they disagree.
            let decompressed;
            let bytes = if cmd.compressed {
                decompressed = inflate::zlib_decompress(payload, budget).map_err(zlib_error)?;
                &decompressed[..]
            } else {
                payload
            };
            let image = png::decode(bytes, budget).map_err(png_error)?;
            Ok((image.width, image.height, image.rgba))
        }
        Format::Rgb | Format::Rgba => {
            let stride = if cmd.format == Format::Rgb { 3 } else { 4 };
            let (w, h) = (cmd.width, cmd.height);
            if w == 0 || h == 0 {
                return Err("EINVAL:missing dimensions");
            }
            let expected = (w as usize)
                .checked_mul(h as usize)
                .and_then(|pixels| pixels.checked_mul(stride))
                .ok_or("EINVAL:image too large")?;

            let decompressed;
            let bytes = if cmd.compressed {
                // Raw pixels decompress to exactly this many bytes, so the
                // inflater can be told the answer in advance; an image that
                // would not fit the budget is refused at the budget instead.
                decompressed =
                    inflate::zlib_decompress(payload, expected.min(budget)).map_err(zlib_error)?;
                &decompressed[..]
            } else {
                payload
            };
            if bytes.len() < expected {
                return Err("EINVAL:truncated payload");
            }

            let mut data = Vec::with_capacity((w as usize) * (h as usize) * 4);
            for px in bytes[..expected].chunks_exact(stride) {
                data.extend_from_slice(&[px[0], px[1], px[2]]);
                data.push(if stride == 4 { px[3] } else { 0xff });
            }
            Ok((w, h, data))
        }
    }
}

/// Map a decompression failure onto a protocol error response. The one
/// distinction worth keeping is "too big to hold" against "corrupt", because
/// an application can do something useful about the first.
fn zlib_error(err: InflateError) -> &'static str {
    match err {
        InflateError::TooLarge => "EINVAL:compressed payload exceeds the image budget",
        _ => "EINVAL:corrupt zlib payload",
    }
}

fn png_error(err: PngError) -> &'static str {
    match err {
        PngError::TooLarge | PngError::Deflate(InflateError::TooLarge) => {
            "EINVAL:PNG exceeds the image budget"
        }
        PngError::BadSignature => "EINVAL:not a PNG",
        PngError::Unsupported => "EINVAL:unsupported PNG variant",
        _ => "EINVAL:corrupt PNG",
    }
}

/// Decode base64, skipping whitespace. Invalid characters end the decode.
pub fn decode_base64(input: &[u8]) -> Vec<u8> {
    const INVALID: u8 = 0xff;
    let mut table = [INVALID; 256];
    for (value, &c) in b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
        .iter()
        .enumerate()
    {
        table[c as usize] = value as u8;
    }

    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for &byte in input {
        if byte == b'=' {
            break;
        }
        if byte.is_ascii_whitespace() {
            continue;
        }
        let v = table[byte as usize];
        if v == INVALID {
            break;
        }
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

/// Encode base64, used when tOS answers queries with payloads.
pub fn encode_base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_roundtrip() {
        for case in ["", "f", "fo", "foo", "foob", "fooba", "foobar"] {
            let encoded = encode_base64(case.as_bytes());
            assert_eq!(decode_base64(encoded.as_bytes()), case.as_bytes());
        }
    }

    #[test]
    fn base64_known_vector() {
        assert_eq!(encode_base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(decode_base64(b"Zm9vYmFy"), b"foobar");
    }

    #[test]
    fn parses_transmit_and_display() {
        let cmd = GraphicsCommand::parse(b"a=T,f=24,s=2,v=1,i=7;AAAA").unwrap();
        assert_eq!(cmd.action, Action::TransmitAndDisplay);
        assert_eq!(cmd.format, Format::Rgb);
        assert_eq!(cmd.width, 2);
        assert_eq!(cmd.height, 1);
        assert_eq!(cmd.image_id, 7);
    }

    #[test]
    fn stores_rgb_as_rgba() {
        let mut store = GraphicsStore::new(1 << 20);
        let payload = encode_base64(&[1, 2, 3, 4, 5, 6]);
        let cmd = GraphicsCommand::parse(
            format!("a=t,f=24,s=2,v=1,i=1;{payload}").as_bytes(),
        )
        .unwrap();
        let id = store.store(&cmd, &cmd.payload).unwrap();
        let img = store.image(id).unwrap();
        assert_eq!(img.data, vec![1, 2, 3, 255, 4, 5, 6, 255]);
    }

    /// Build a transmit command carrying `data` as its base64 payload.
    fn transmit(control: &str, data: &[u8]) -> GraphicsCommand {
        let payload = encode_base64(data);
        GraphicsCommand::parse(format!("{control};{payload}").as_bytes()).unwrap()
    }

    #[test]
    fn a_png_payload_is_decoded_to_rgba() {
        let mut store = GraphicsStore::new(1 << 20);
        let pixels: Vec<u8> = (0..2 * 2 * 4).map(|i| i as u8).collect();
        let cmd = transmit("a=t,f=100,i=1", &crate::png::tests::rgba_png(2, 2, &pixels));
        let id = store.store(&cmd, &cmd.payload).unwrap();
        let img = store.image(id).unwrap();
        // The file's own dimensions are used, not the ones on the command.
        assert_eq!((img.width, img.height), (2, 2));
        assert_eq!(img.data, pixels);
    }

    #[test]
    fn a_zlib_compressed_rgba_payload_is_decoded() {
        let mut store = GraphicsStore::new(1 << 20);
        let pixels = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let cmd = transmit(
            "a=t,f=32,o=z,s=2,v=1,i=1",
            &crate::inflate::tests::zlib_stored(&pixels),
        );
        let id = store.store(&cmd, &cmd.payload).unwrap();
        assert_eq!(store.image(id).unwrap().data, pixels);
    }

    #[test]
    fn a_zlib_compressed_png_payload_is_decoded() {
        let mut store = GraphicsStore::new(1 << 20);
        let pixels: Vec<u8> = (0..2 * 4).map(|i| (i * 3) as u8).collect();
        let file = crate::png::tests::rgba_png(1, 2, &pixels);
        let cmd = transmit("a=t,f=100,o=z,i=1", &crate::inflate::tests::zlib_stored(&file));
        let id = store.store(&cmd, &cmd.payload).unwrap();
        assert_eq!(store.image(id).unwrap().data, pixels);
    }

    #[test]
    fn a_corrupt_png_is_an_error_not_a_panic() {
        let mut store = GraphicsStore::new(1 << 20);
        let full = crate::png::tests::rgba_png(2, 2, &[0x40; 2 * 2 * 4]);
        for cut in 0..full.len() {
            let cmd = transmit("a=t,f=100,i=1", &full[..cut]);
            assert!(store.store(&cmd, &cmd.payload).is_err());
        }
        // And a payload that is not a PNG at all.
        let cmd = transmit("a=t,f=100,i=1", b"not an image");
        assert_eq!(store.store(&cmd, &cmd.payload), Err("EINVAL:not a PNG"));
    }

    #[test]
    fn a_compressed_payload_that_would_burst_the_budget_is_refused() {
        // 4 MiB of zeroes offered to a store that can only hold 1 KiB.
        let mut store = GraphicsStore::new(1024);
        let bomb = crate::inflate::tests::zlib_stored(&vec![0u8; 4 << 20]);
        let cmd = transmit("a=t,f=32,o=z,s=1024,v=1024,i=1", &bomb);
        assert_eq!(
            store.store(&cmd, &cmd.payload),
            Err("EINVAL:compressed payload exceeds the image budget")
        );
    }

    #[test]
    fn a_png_larger_than_the_budget_is_refused_from_its_header() {
        let mut store = GraphicsStore::new(1024);
        let file = crate::png::tests::rgba_png(64, 64, &[0; 64 * 64 * 4]);
        let cmd = transmit("a=t,f=100,i=1", &file);
        assert_eq!(
            store.store(&cmd, &cmd.payload),
            Err("EINVAL:PNG exceeds the image budget")
        );
    }

    #[test]
    fn chunked_transmission_reassembles() {
        let mut store = GraphicsStore::new(1 << 20);
        let first = GraphicsCommand::parse(b"a=t,f=32,s=1,v=1,i=3,m=1;AAAA").unwrap();
        assert!(store.accumulate(&first).is_none());
        let last = GraphicsCommand::parse(b"m=0;BBBB").unwrap();
        let (cmd, payload) = store.accumulate(&last).unwrap();
        assert_eq!(cmd.image_id, 3);
        assert_eq!(payload.len(), 6);
    }

    #[test]
    fn a_differently_identified_command_does_not_join_a_pending_transfer() {
        let mut store = GraphicsStore::new(1 << 20);
        let abandoned = GraphicsCommand::parse(b"a=t,f=32,s=1,v=1,i=1,m=1;AAAA").unwrap();
        assert!(store.accumulate(&abandoned).is_none());

        // A complete command that identifies itself by image number must be
        // stored as itself, not appended to the stale buffer.
        let fresh = GraphicsCommand::parse(b"a=t,f=32,s=1,v=1,I=7;BBBBBB").unwrap();
        let (cmd, payload) = store.accumulate(&fresh).unwrap();
        assert_eq!(cmd.image_number, 7);
        assert_eq!(cmd.image_id, 0);
        assert_eq!(payload, fresh.payload);
    }

    #[test]
    fn placement_size_derived_from_cell_size() {
        let mut store = GraphicsStore::new(1 << 20);
        let data = vec![0u8; 20 * 32 * 4];
        let payload = encode_base64(&data);
        let cmd = GraphicsCommand::parse(
            format!("a=T,f=32,s=20,v=32,i=1;{payload}").as_bytes(),
        )
        .unwrap();
        let id = store.store(&cmd, &cmd.payload).unwrap();
        let placement = store.place(&cmd, id, 0, 0, 8, 16).unwrap();
        let p = store.placement(placement).unwrap();
        assert_eq!(p.cols, 3); // 20px over 8px cells, rounded up
        assert_eq!(p.rows, 2); // 32px over 16px cells
    }

    #[test]
    fn scrolling_drops_offscreen_placements() {
        let mut store = GraphicsStore::new(1 << 20);
        let data = vec![0u8; 8 * 16 * 4];
        let payload = encode_base64(&data);
        let cmd =
            GraphicsCommand::parse(format!("a=T,f=32,s=8,v=16,i=1;{payload}").as_bytes()).unwrap();
        let id = store.store(&cmd, &cmd.payload).unwrap();
        store.place(&cmd, id, 0, 0, 8, 16);
        assert_eq!(store.placements().count(), 1);
        store.scroll(1);
        assert_eq!(store.placements().count(), 0);
    }
}
