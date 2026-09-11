//! PNG decoding (RFC 2083) to RGBA8.
//!
//! File managers send previews through the graphics protocol as PNG, so this
//! is what makes `f=100` work. The output is always RGBA8 because that is the
//! single layout [`crate::graphics::Image`] stores; everything narrower is
//! widened here rather than at paint time.
//!
//! The input is a file the terminal was handed by an arbitrary program, so
//! every length, index and palette reference is checked. The decoder allocates
//! two buffers whose sizes follow from IHDR and nothing else: the RGBA output,
//! refused up front if it would not fit the caller's budget, and the
//! decompressed scanlines, whose exact size is known from the same header and
//! is handed to the inflater as its limit. A bomb in IDAT therefore stops at
//! the size the header promised instead of at the size it claims.

use crate::inflate::{self, InflateError};

/// Why a PNG could not be decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PngError {
    /// The eight-byte signature is missing or wrong.
    BadSignature,
    /// A chunk, or the file, ended early.
    Truncated,
    /// A chunk's CRC-32 did not match its contents.
    BadCrc,
    /// IHDR is missing, malformed, or describes a zero-sized image.
    BadHeader,
    /// A legal PNG this decoder does not implement.
    Unsupported,
    /// PLTE or tRNS is the wrong size, or a palette image has no PLTE.
    BadPalette,
    /// A palette index with no entry behind it.
    BadPaletteIndex,
    /// A scanline filter byte outside 0..=4.
    BadFilter,
    /// No IDAT data at all.
    NoImageData,
    /// The pixel data would not fit the caller's budget.
    TooLarge,
    /// The compressed pixel data could not be inflated.
    Deflate(InflateError),
}

impl std::fmt::Display for PngError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PngError::BadSignature => f.write_str("not a PNG file"),
            PngError::Truncated => f.write_str("truncated PNG"),
            PngError::BadCrc => f.write_str("PNG chunk checksum mismatch"),
            PngError::BadHeader => f.write_str("corrupt PNG header"),
            PngError::Unsupported => f.write_str("unsupported PNG variant"),
            PngError::BadPalette => f.write_str("corrupt PNG palette"),
            PngError::BadPaletteIndex => f.write_str("PNG palette index out of range"),
            PngError::BadFilter => f.write_str("unknown PNG scanline filter"),
            PngError::NoImageData => f.write_str("PNG has no image data"),
            PngError::TooLarge => f.write_str("PNG exceeds the image budget"),
            PngError::Deflate(err) => write!(f, "PNG pixel data: {err}"),
        }
    }
}

impl std::error::Error for PngError {}

/// A decoded image: RGBA8, four bytes per pixel, top row first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PngImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

/// Decode a PNG into RGBA8.
///
/// `limit` is the largest RGBA output accepted, in bytes; an image whose
/// header asks for more is refused before anything is allocated.
pub fn decode(data: &[u8], limit: usize) -> Result<PngImage, PngError> {
    if data.get(..8) != Some(&SIGNATURE[..]) {
        return Err(PngError::BadSignature);
    }

    let mut header: Option<Header> = None;
    let mut palette: Vec<[u8; 4]> = Vec::new();
    let mut transparency: Option<Vec<u8>> = None;
    let mut compressed: Vec<u8> = Vec::new();
    let mut ended = false;
    let mut pos = 8;

    while pos < data.len() {
        let chunk = Chunk::read(data, pos)?;
        pos = chunk.end;
        match &chunk.kind {
            b"IHDR" => {
                if header.is_some() {
                    return Err(PngError::BadHeader);
                }
                header = Some(Header::parse(chunk.body, limit)?);
            }
            b"PLTE" => {
                // A palette after the pixel data has started cannot apply to
                // it, and an oversized one is simply corrupt.
                if !compressed.is_empty() || !palette.is_empty() {
                    return Err(PngError::BadPalette);
                }
                if chunk.body.len() % 3 != 0 || chunk.body.len() > 256 * 3 {
                    return Err(PngError::BadPalette);
                }
                palette = chunk
                    .body
                    .chunks_exact(3)
                    .map(|entry| [entry[0], entry[1], entry[2], 0xff])
                    .collect();
            }
            b"tRNS" => {
                if !compressed.is_empty() {
                    return Err(PngError::BadPalette);
                }
                transparency = Some(chunk.body.to_vec());
            }
            b"IDAT" => compressed.extend_from_slice(chunk.body),
            b"IEND" => {
                ended = true;
                break;
            }
            // Ancillary chunks (gAMA, pHYs, text, ...) are safe to ignore. An
            // unknown *critical* chunk changes the meaning of the pixels, so
            // the spec says a decoder that does not know it must give up.
            kind if kind[0].is_ascii_uppercase() => return Err(PngError::Unsupported),
            _ => {}
        }
    }

    // A file that stops before IEND is truncated, even if every chunk that
    // did arrive was whole: there is no way to know the pixels are all here.
    if !ended {
        return Err(PngError::Truncated);
    }
    let header = header.ok_or(PngError::BadHeader)?;
    if compressed.is_empty() {
        return Err(PngError::NoImageData);
    }
    apply_transparency(&header, &mut palette, transparency.as_deref())?;
    if header.colour == 3 && palette.is_empty() {
        return Err(PngError::BadPalette);
    }
    let transparent = transparent_sample(&header, transparency.as_deref());

    render(&header, &palette, transparent, &compressed, limit)
}

// ---------------------------------------------------------------------------
// Chunks
// ---------------------------------------------------------------------------

/// One `length / type / data / CRC` record.
struct Chunk<'a> {
    kind: [u8; 4],
    body: &'a [u8],
    /// Offset of the next chunk.
    end: usize,
}

impl<'a> Chunk<'a> {
    fn read(data: &'a [u8], pos: usize) -> Result<Chunk<'a>, PngError> {
        let header = data.get(pos..pos + 8).ok_or(PngError::Truncated)?;
        let length = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
        // PNG caps a chunk at 2^31-1, which also keeps the arithmetic below
        // from wrapping on a 32-bit host.
        if length > 0x7fff_ffff {
            return Err(PngError::Truncated);
        }
        let kind = [header[4], header[5], header[6], header[7]];
        let start = pos + 8;
        let end = start.checked_add(length).ok_or(PngError::Truncated)?;
        let body = data.get(start..end).ok_or(PngError::Truncated)?;
        let trailer = data.get(end..end + 4).ok_or(PngError::Truncated)?;
        let expected = u32::from_be_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]);

        let mut crc = Crc::new();
        crc.update(&kind);
        crc.update(body);
        if crc.finish() != expected {
            return Err(PngError::BadCrc);
        }
        Ok(Chunk {
            kind,
            body,
            end: end + 4,
        })
    }
}

/// CRC-32 as PNG defines it: the same polynomial as zlib, over the chunk type
/// and data.
struct Crc(u32);

const CRC_TABLE: [u32; 256] = crc_table();

const fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut value = i as u32;
        let mut bit = 0;
        while bit < 8 {
            value = if value & 1 != 0 {
                0xedb8_8320 ^ (value >> 1)
            } else {
                value >> 1
            };
            bit += 1;
        }
        table[i] = value;
        i += 1;
    }
    table
}

impl Crc {
    fn new() -> Crc {
        Crc(0xffff_ffff)
    }

    fn update(&mut self, data: &[u8]) {
        for &byte in data {
            self.0 = CRC_TABLE[((self.0 ^ byte as u32) & 0xff) as usize] ^ (self.0 >> 8);
        }
    }

    fn finish(&self) -> u32 {
        self.0 ^ 0xffff_ffff
    }
}

// ---------------------------------------------------------------------------
// Header
// ---------------------------------------------------------------------------

/// The contents of IHDR, once they are known to be a combination that exists.
struct Header {
    width: usize,
    height: usize,
    depth: u8,
    colour: u8,
    interlaced: bool,
}

impl Header {
    fn parse(body: &[u8], limit: usize) -> Result<Header, PngError> {
        if body.len() != 13 {
            return Err(PngError::BadHeader);
        }
        let width = u32::from_be_bytes([body[0], body[1], body[2], body[3]]) as usize;
        let height = u32::from_be_bytes([body[4], body[5], body[6], body[7]]) as usize;
        let depth = body[8];
        let colour = body[9];
        let compression = body[10];
        let filter = body[11];
        let interlace = body[12];

        if width == 0 || height == 0 {
            return Err(PngError::BadHeader);
        }
        // Deflate and adaptive filtering are the only methods ever defined;
        // anything else is either corrupt or from a format that is not PNG.
        if compression != 0 || filter != 0 {
            return Err(PngError::Unsupported);
        }
        if interlace > 1 {
            return Err(PngError::Unsupported);
        }
        // Which depths each colour type allows, from RFC 2083 table 11.1.
        let allowed: &[u8] = match colour {
            0 => &[1, 2, 4, 8, 16],
            2 | 4 | 6 => &[8, 16],
            3 => &[1, 2, 4, 8],
            _ => return Err(PngError::BadHeader),
        };
        if !allowed.contains(&depth) {
            return Err(PngError::Unsupported);
        }

        // Refuse an image too big to keep before any buffer is sized from it.
        // Everything allocated later is derived from these two numbers, so
        // this one check bounds the whole decode.
        let pixels = width.checked_mul(height).ok_or(PngError::TooLarge)?;
        if pixels.checked_mul(4).ok_or(PngError::TooLarge)? > limit {
            return Err(PngError::TooLarge);
        }

        Ok(Header {
            width,
            height,
            depth,
            colour,
            interlaced: interlace == 1,
        })
    }

    fn channels(&self) -> usize {
        match self.colour {
            0 | 3 => 1,
            4 => 2,
            2 => 3,
            _ => 4,
        }
    }

    fn bits_per_pixel(&self) -> usize {
        self.channels() * self.depth as usize
    }

    /// The filter offset: one pixel in bytes, rounded up, never below one.
    fn filter_step(&self) -> usize {
        self.bits_per_pixel().div_ceil(8).max(1)
    }

    /// Bytes in a scanline of `pixels` pixels, with the sub-byte depths packed.
    fn row_bytes(&self, pixels: usize) -> usize {
        (pixels * self.bits_per_pixel()).div_ceil(8)
    }

    /// The passes to walk: seven for Adam7, one covering everything for a
    /// plain image. Each is `(x0, y0, dx, dy)`.
    fn passes(&self) -> &'static [(usize, usize, usize, usize)] {
        const ADAM7: [(usize, usize, usize, usize); 7] = [
            (0, 0, 8, 8),
            (4, 0, 8, 8),
            (0, 4, 4, 8),
            (2, 0, 4, 4),
            (0, 2, 2, 4),
            (1, 0, 2, 2),
            (0, 1, 1, 2),
        ];
        const WHOLE: [(usize, usize, usize, usize); 1] = [(0, 0, 1, 1)];
        if self.interlaced {
            &ADAM7
        } else {
            &WHOLE
        }
    }

    /// Size of one pass in pixels, which is zero for the passes an image is
    /// too small to have.
    fn pass_size(&self, pass: (usize, usize, usize, usize)) -> (usize, usize) {
        let (x0, y0, dx, dy) = pass;
        let w = self.width.saturating_sub(x0).div_ceil(dx);
        let h = self.height.saturating_sub(y0).div_ceil(dy);
        (w, h)
    }

    /// Exactly how many bytes IDAT must decompress to: for every pass, one
    /// filter byte plus a packed scanline per row.
    /// How many bytes the decoded picture occupies as RGBA8.
    fn rgba_size(&self) -> Result<usize, PngError> {
        self.width
            .checked_mul(self.height)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or(PngError::TooLarge)
    }

    fn decompressed_size(&self) -> Result<usize, PngError> {
        let mut total = 0usize;
        for &pass in self.passes() {
            let (w, h) = self.pass_size(pass);
            if w == 0 || h == 0 {
                continue;
            }
            let row = self.row_bytes(w).checked_add(1).ok_or(PngError::TooLarge)?;
            let size = row.checked_mul(h).ok_or(PngError::TooLarge)?;
            total = total.checked_add(size).ok_or(PngError::TooLarge)?;
        }
        Ok(total)
    }
}

/// Fold tRNS into the palette, or keep it as the one colour that is see
/// through. Colour types 4 and 6 carry their own alpha, so tRNS is not
/// allowed to accompany them and is ignored if it does.
fn apply_transparency(
    header: &Header,
    palette: &mut [[u8; 4]],
    transparency: Option<&[u8]>,
) -> Result<(), PngError> {
    let Some(data) = transparency else {
        return Ok(());
    };
    if header.colour == 3 {
        if data.len() > palette.len() {
            return Err(PngError::BadPalette);
        }
        for (entry, &alpha) in palette.iter_mut().zip(data) {
            entry[3] = alpha;
        }
    }
    Ok(())
}

/// The single sample value a tRNS chunk marks as transparent, at the file's
/// own bit depth so it can be compared before any scaling.
fn transparent_sample(header: &Header, transparency: Option<&[u8]>) -> Option<[u16; 3]> {
    let data = transparency?;
    match header.colour {
        0 if data.len() >= 2 => {
            let grey = u16::from_be_bytes([data[0], data[1]]);
            Some([grey, grey, grey])
        }
        2 if data.len() >= 6 => Some([
            u16::from_be_bytes([data[0], data[1]]),
            u16::from_be_bytes([data[2], data[3]]),
            u16::from_be_bytes([data[4], data[5]]),
        ]),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Pixel data
// ---------------------------------------------------------------------------

fn render(
    header: &Header,
    palette: &[[u8; 4]],
    transparent: Option<[u16; 3]>,
    compressed: &[u8],
    limit: usize,
) -> Result<PngImage, PngError> {
    let expected = header.decompressed_size()?;
    // The scanlines and the RGBA they turn into are both held at once, and
    // the scanlines can be the larger of the two: sixteen bits a sample is
    // twice the eight the output keeps. The budget is for what this costs to
    // decode, so both have to fit inside it, not just the half that is kept.
    let rgba = header.rgba_size()?;
    if expected.checked_add(rgba).ok_or(PngError::TooLarge)? > limit {
        return Err(PngError::TooLarge);
    }
    // The inflater's limit is the exact size the header promised, so a bomb
    // in IDAT stops there rather than at whatever size it wanted to reach.
    let raw = inflate::zlib_decompress(compressed, expected).map_err(PngError::Deflate)?;
    if raw.len() != expected {
        return Err(PngError::Truncated);
    }

    let pixels = Pixels {
        depth: header.depth,
        colour: header.colour,
        channels: header.channels(),
        palette,
        transparent,
    };
    let mut out = vec![0u8; header.width * header.height * 4];
    debug_assert!(out.len() <= limit);

    let step = header.filter_step();
    let mut offset = 0;
    for &pass in header.passes() {
        let (x0, y0, dx, dy) = pass;
        let (pass_w, pass_h) = header.pass_size(pass);
        if pass_w == 0 || pass_h == 0 {
            continue;
        }
        let stride = header.row_bytes(pass_w);
        // Each pass filters against its own rows only, so the "previous row"
        // starts out as zeroes again.
        let mut previous = vec![0u8; stride];
        let mut current = vec![0u8; stride];

        for row in 0..pass_h {
            let filter = *raw.get(offset).ok_or(PngError::Truncated)?;
            let line = raw
                .get(offset + 1..offset + 1 + stride)
                .ok_or(PngError::Truncated)?;
            current.copy_from_slice(line);
            unfilter(filter, &mut current, &previous, step)?;
            offset += 1 + stride;

            let y = y0 + row * dy;
            for column in 0..pass_w {
                let x = x0 + column * dx;
                let rgba = pixels.at(&current, column)?;
                let at = (y * header.width + x) * 4;
                out[at..at + 4].copy_from_slice(&rgba);
            }
            std::mem::swap(&mut previous, &mut current);
        }
    }

    Ok(PngImage {
        width: header.width as u32,
        height: header.height as u32,
        rgba: out,
    })
}

/// Undo one scanline filter in place. `previous` is the already-unfiltered
/// row above, all zeroes for the first row of a pass.
fn unfilter(filter: u8, current: &mut [u8], previous: &[u8], step: usize) -> Result<(), PngError> {
    match filter {
        // None.
        0 => {}
        // Sub: the byte one pixel to the left.
        1 => {
            for i in step..current.len() {
                current[i] = current[i].wrapping_add(current[i - step]);
            }
        }
        // Up: the byte above.
        2 => {
            for (byte, &above) in current.iter_mut().zip(previous) {
                *byte = byte.wrapping_add(above);
            }
        }
        // Average: the mean of left and above, computed without wrapping.
        3 => {
            for i in 0..current.len() {
                let left = if i >= step { current[i - step] as u16 } else { 0 };
                let above = previous[i] as u16;
                current[i] = current[i].wrapping_add(((left + above) / 2) as u8);
            }
        }
        // Paeth: whichever of left, above and above-left the predictor picks.
        4 => {
            for i in 0..current.len() {
                let left = if i >= step { current[i - step] } else { 0 };
                let above = previous[i];
                let corner = if i >= step { previous[i - step] } else { 0 };
                current[i] = current[i].wrapping_add(paeth(left, above, corner));
            }
        }
        _ => return Err(PngError::BadFilter),
    }
    Ok(())
}

/// The Paeth predictor of RFC 2083 section 6.6.
fn paeth(left: u8, above: u8, corner: u8) -> u8 {
    let estimate = left as i16 + above as i16 - corner as i16;
    let d_left = (estimate - left as i16).abs();
    let d_above = (estimate - above as i16).abs();
    let d_corner = (estimate - corner as i16).abs();
    if d_left <= d_above && d_left <= d_corner {
        left
    } else if d_above <= d_corner {
        above
    } else {
        corner
    }
}

/// Turns the samples of one unfiltered scanline into RGBA.
struct Pixels<'a> {
    depth: u8,
    colour: u8,
    channels: usize,
    /// Palette entries with any tRNS alpha already folded in.
    palette: &'a [[u8; 4]],
    /// The colour a tRNS chunk made transparent, at the file's bit depth.
    transparent: Option<[u16; 3]>,
}

impl Pixels<'_> {
    fn at(&self, row: &[u8], x: usize) -> Result<[u8; 4], PngError> {
        let base = x * self.channels;
        let depth = self.depth;
        match self.colour {
            0 => {
                let grey = sample(row, base, depth)?;
                let alpha = match self.transparent {
                    Some(key) if key[0] == grey => 0,
                    _ => 0xff,
                };
                let value = scale(grey, depth);
                Ok([value, value, value, alpha])
            }
            2 => {
                let r = sample(row, base, depth)?;
                let g = sample(row, base + 1, depth)?;
                let b = sample(row, base + 2, depth)?;
                let alpha = match self.transparent {
                    Some(key) if key == [r, g, b] => 0,
                    _ => 0xff,
                };
                Ok([scale(r, depth), scale(g, depth), scale(b, depth), alpha])
            }
            3 => {
                let index = sample(row, base, depth)? as usize;
                self.palette
                    .get(index)
                    .copied()
                    .ok_or(PngError::BadPaletteIndex)
            }
            4 => {
                let grey = scale(sample(row, base, depth)?, depth);
                let alpha = scale(sample(row, base + 1, depth)?, depth);
                Ok([grey, grey, grey, alpha])
            }
            _ => Ok([
                scale(sample(row, base, depth)?, depth),
                scale(sample(row, base + 1, depth)?, depth),
                scale(sample(row, base + 2, depth)?, depth),
                scale(sample(row, base + 3, depth)?, depth),
            ]),
        }
    }
}

/// Read sample number `index` out of a packed scanline.
///
/// The stride was computed to hold every sample of the row, so a miss means
/// the caller's arithmetic is wrong rather than the file's; it is still an
/// error rather than a panic.
fn sample(row: &[u8], index: usize, depth: u8) -> Result<u16, PngError> {
    match depth {
        16 => {
            let at = index * 2;
            let pair = row.get(at..at + 2).ok_or(PngError::Truncated)?;
            Ok(u16::from_be_bytes([pair[0], pair[1]]))
        }
        8 => Ok(*row.get(index).ok_or(PngError::Truncated)? as u16),
        // 1, 2 and 4 bits per sample, most significant first within a byte.
        _ => {
            let per_byte = 8 / depth as usize;
            let byte = *row.get(index / per_byte).ok_or(PngError::Truncated)?;
            let shift = 8 - depth as usize * (index % per_byte + 1);
            let mask = (1u16 << depth) - 1;
            Ok((byte as u16 >> shift) & mask)
        }
    }
}

/// Widen a sample to eight bits, spreading the value over the full range so
/// that the maximum stays the maximum (a 1-bit 1 becomes 255, not 1).
fn scale(value: u16, depth: u8) -> u8 {
    match depth {
        16 => (value >> 8) as u8,
        8 => value as u8,
        4 => (value * 17) as u8,
        2 => (value * 85) as u8,
        _ => {
            if value != 0 {
                0xff
            } else {
                0
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::inflate::tests::zlib_stored;

    /// Wrap a chunk body in its length, type and CRC.
    pub(crate) fn chunk(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        let mut crc = Crc::new();
        crc.update(kind);
        crc.update(body);
        out.extend_from_slice(&crc.finish().to_be_bytes());
        out
    }

    /// Assemble a PNG from already-filtered scanline bytes (each row must
    /// start with its filter byte). `extras` are chunks placed between IHDR
    /// and IDAT, for PLTE and tRNS.
    pub(crate) fn png(
        width: u32,
        height: u32,
        depth: u8,
        colour: u8,
        interlace: u8,
        extras: &[Vec<u8>],
        scanlines: &[u8],
    ) -> Vec<u8> {
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&width.to_be_bytes());
        ihdr.extend_from_slice(&height.to_be_bytes());
        ihdr.extend_from_slice(&[depth, colour, 0, 0, interlace]);

        let mut out = SIGNATURE.to_vec();
        out.extend_from_slice(&chunk(b"IHDR", &ihdr));
        for extra in extras {
            out.extend_from_slice(extra);
        }
        out.extend_from_slice(&chunk(b"IDAT", &zlib_stored(scanlines)));
        out.extend_from_slice(&chunk(b"IEND", &[]));
        out
    }

    /// An 8-bit RGBA image with every scanline unfiltered, which is the
    /// simplest thing that is still a real PNG.
    pub(crate) fn rgba_png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
        let stride = width as usize * 4;
        let mut scanlines = Vec::new();
        for row in rgba.chunks(stride) {
            scanlines.push(0);
            scanlines.extend_from_slice(row);
        }
        png(width, height, 8, 6, 0, &[], &scanlines)
    }

    const LIMIT: usize = 1 << 20;

    #[test]
    fn the_scanlines_count_against_the_budget_too() {
        // Sixteen bits a sample: the scanlines are twice the RGBA they become,
        // and both are held at once. A budget that only weighed the output
        // would let this through and then use three times it.
        let (w, h) = (16u32, 16u32);
        let mut scanlines = Vec::new();
        for _ in 0..h {
            scanlines.push(0);
            scanlines.extend(std::iter::repeat_n(0x40u8, w as usize * 8));
        }
        let data = png(w, h, 16, 6, 0, &[], &scanlines);

        let rgba = (w * h * 4) as usize;
        let raw = h as usize * (w as usize * 8 + 1);
        // Room for the picture but not for decoding it.
        assert!(
            decode(&data, rgba + raw - 1).is_err(),
            "a budget smaller than the decode accepted the image"
        );
        assert!(decode(&data, rgba + raw).is_ok(), "an ample budget was refused");
    }

    #[test]
    fn an_rgba_image_decodes_unchanged() {
        let rgba: Vec<u8> = (0..2 * 2 * 4).map(|i| i as u8).collect();
        let image = decode(&rgba_png(2, 2, &rgba), LIMIT).unwrap();
        assert_eq!((image.width, image.height), (2, 2));
        assert_eq!(image.rgba, rgba);
    }

    #[test]
    fn an_rgb_image_gains_an_opaque_alpha() {
        let scanlines = [0, 1, 2, 3, 4, 5, 6, 0, 7, 8, 9, 10, 11, 12];
        let image = decode(&png(2, 2, 8, 2, 0, &[], &scanlines), LIMIT).unwrap();
        assert_eq!(
            image.rgba,
            vec![1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255]
        );
    }

    #[test]
    fn an_eight_bit_greyscale_image_becomes_grey_rgba() {
        let image = decode(&png(2, 1, 8, 0, 0, &[], &[0, 0x20, 0x40]), LIMIT).unwrap();
        assert_eq!(image.rgba, vec![0x20, 0x20, 0x20, 255, 0x40, 0x40, 0x40, 255]);
    }

    #[test]
    fn a_greyscale_alpha_image_keeps_its_alpha() {
        let image = decode(&png(2, 1, 8, 4, 0, &[], &[0, 0x10, 0x80, 0x20, 0x00]), LIMIT).unwrap();
        assert_eq!(image.rgba, vec![0x10, 0x10, 0x10, 0x80, 0x20, 0x20, 0x20, 0x00]);
    }

    #[test]
    fn a_palette_image_resolves_its_entries() {
        let plte = chunk(b"PLTE", &[255, 0, 0, 0, 255, 0, 0, 0, 255]);
        // Two pixels at four bits each pack into one byte.
        let image = decode(&png(2, 1, 4, 3, 0, &[plte], &[0, 0x21]), LIMIT).unwrap();
        assert_eq!(image.rgba, vec![0, 0, 255, 255, 0, 255, 0, 255]);
    }

    #[test]
    fn a_palette_transparency_chunk_sets_per_entry_alpha() {
        let plte = chunk(b"PLTE", &[1, 2, 3, 4, 5, 6]);
        let trns = chunk(b"tRNS", &[0x00]);
        let image = decode(&png(2, 1, 8, 3, 0, &[plte, trns], &[0, 0, 1]), LIMIT).unwrap();
        assert_eq!(image.rgba, vec![1, 2, 3, 0, 4, 5, 6, 255]);
    }

    #[test]
    fn a_palette_index_past_the_end_of_the_palette_is_an_error() {
        let plte = chunk(b"PLTE", &[1, 2, 3]);
        let bytes = png(2, 1, 8, 3, 0, &[plte], &[0, 0, 9]);
        assert_eq!(decode(&bytes, LIMIT), Err(PngError::BadPaletteIndex));
    }

    #[test]
    fn a_palette_image_without_a_palette_is_an_error() {
        let bytes = png(1, 1, 8, 3, 0, &[], &[0, 0]);
        assert_eq!(decode(&bytes, LIMIT), Err(PngError::BadPalette));
    }

    #[test]
    fn one_bit_greyscale_expands_to_black_and_white() {
        // 0b1010_0000: white, black, white, black in the top four bits.
        let image = decode(&png(4, 1, 1, 0, 0, &[], &[0, 0b1010_0000]), LIMIT).unwrap();
        assert_eq!(
            image.rgba,
            vec![255, 255, 255, 255, 0, 0, 0, 255, 255, 255, 255, 255, 0, 0, 0, 255]
        );
    }

    #[test]
    fn two_bit_greyscale_spreads_over_the_full_range() {
        // 0b00_01_10_11 is the four levels in order.
        let image = decode(&png(4, 1, 2, 0, 0, &[], &[0, 0b00_01_10_11]), LIMIT).unwrap();
        let greys: Vec<u8> = image.rgba.chunks(4).map(|px| px[0]).collect();
        assert_eq!(greys, vec![0, 85, 170, 255]);
    }

    #[test]
    fn sixteen_bit_samples_are_reduced_to_eight() {
        // 0x1234 and 0xabcd keep their high bytes.
        let image = decode(&png(2, 1, 16, 0, 0, &[], &[0, 0x12, 0x34, 0xab, 0xcd]), LIMIT).unwrap();
        assert_eq!(image.rgba, vec![0x12, 0x12, 0x12, 255, 0xab, 0xab, 0xab, 255]);
    }

    #[test]
    fn a_transparency_chunk_makes_one_grey_see_through() {
        let trns = chunk(b"tRNS", &[0x00, 0x40]);
        let image = decode(&png(2, 1, 8, 0, 0, &[trns], &[0, 0x40, 0x41]), LIMIT).unwrap();
        assert_eq!(image.rgba, vec![0x40, 0x40, 0x40, 0, 0x41, 0x41, 0x41, 255]);
    }

    #[test]
    fn a_transparency_chunk_makes_one_colour_see_through() {
        let trns = chunk(b"tRNS", &[0, 10, 0, 20, 0, 30]);
        let scanlines = [0, 10, 20, 30, 10, 20, 31];
        let image = decode(&png(2, 1, 8, 2, 0, &[trns], &scanlines), LIMIT).unwrap();
        assert_eq!(image.rgba, vec![10, 20, 30, 0, 10, 20, 31, 255]);
    }

    /// Filter every row of a three-row RGB image with `filter`, then check the
    /// decoder puts back the pixels the filter was computed from.
    fn filtered_round_trip(filter: u8) {
        let width = 4usize;
        let height = 3usize;
        let step = 3usize;
        let stride = width * step;
        let source: Vec<u8> = (0..stride * height).map(|i| (i * 7 % 251) as u8).collect();

        let mut scanlines = Vec::new();
        for row in 0..height {
            scanlines.push(filter);
            for i in 0..stride {
                let raw = source[row * stride + i] as i16;
                let left = if i >= step {
                    source[row * stride + i - step]
                } else {
                    0
                };
                let above = if row > 0 { source[(row - 1) * stride + i] } else { 0 };
                let corner = if row > 0 && i >= step {
                    source[(row - 1) * stride + i - step]
                } else {
                    0
                };
                let predictor = match filter {
                    0 => 0i16,
                    1 => left as i16,
                    2 => above as i16,
                    3 => ((left as u16 + above as u16) / 2) as i16,
                    _ => paeth(left, above, corner) as i16,
                };
                scanlines.push((raw - predictor) as u8);
            }
        }

        let image = decode(
            &png(width as u32, height as u32, 8, 2, 0, &[], &scanlines),
            LIMIT,
        )
        .unwrap();
        let decoded: Vec<u8> = image
            .rgba
            .chunks(4)
            .flat_map(|px| [px[0], px[1], px[2]])
            .collect();
        assert_eq!(decoded, source, "filter {filter} did not round trip");
    }

    #[test]
    fn every_scanline_filter_reconstructs_the_original_pixels() {
        for filter in 0..=4 {
            filtered_round_trip(filter);
        }
    }

    #[test]
    fn an_unknown_scanline_filter_is_an_error() {
        let bytes = png(1, 1, 8, 2, 0, &[], &[9, 1, 2, 3]);
        assert_eq!(decode(&bytes, LIMIT), Err(PngError::BadFilter));
    }

    #[test]
    fn an_interlaced_image_is_reassembled_in_pass_order() {
        // An 8x8 greyscale image where each pixel is its own index, written
        // out pass by pass the way Adam7 orders them.
        let width = 8usize;
        let height = 8usize;
        let source: Vec<u8> = (0..width * height).map(|i| i as u8).collect();
        const ADAM7: [(usize, usize, usize, usize); 7] = [
            (0, 0, 8, 8),
            (4, 0, 8, 8),
            (0, 4, 4, 8),
            (2, 0, 4, 4),
            (0, 2, 2, 4),
            (1, 0, 2, 2),
            (0, 1, 1, 2),
        ];
        let mut scanlines = Vec::new();
        for (x0, y0, dx, dy) in ADAM7 {
            let mut y = y0;
            while y < height {
                scanlines.push(0);
                let mut x = x0;
                while x < width {
                    scanlines.push(source[y * width + x]);
                    x += dx;
                }
                y += dy;
            }
        }

        let image = decode(
            &png(width as u32, height as u32, 8, 0, 1, &[], &scanlines),
            LIMIT,
        )
        .unwrap();
        let greys: Vec<u8> = image.rgba.chunks(4).map(|px| px[0]).collect();
        assert_eq!(greys, source);
    }

    #[test]
    fn a_five_by_three_interlaced_image_handles_empty_passes() {
        // Passes 2 and 4 have no rows at this size, which is where an
        // off-by-one in the pass geometry would show up.
        let width = 5usize;
        let height = 3usize;
        let source: Vec<u8> = (0..width * height).map(|i| (i * 3) as u8).collect();
        const ADAM7: [(usize, usize, usize, usize); 7] = [
            (0, 0, 8, 8),
            (4, 0, 8, 8),
            (0, 4, 4, 8),
            (2, 0, 4, 4),
            (0, 2, 2, 4),
            (1, 0, 2, 2),
            (0, 1, 1, 2),
        ];
        let mut scanlines = Vec::new();
        for (x0, y0, dx, dy) in ADAM7 {
            let mut y = y0;
            while y < height {
                scanlines.push(0);
                let mut x = x0;
                while x < width {
                    scanlines.push(source[y * width + x]);
                    x += dx;
                }
                y += dy;
            }
        }
        let image = decode(
            &png(width as u32, height as u32, 8, 0, 1, &[], &scanlines),
            LIMIT,
        )
        .unwrap();
        let greys: Vec<u8> = image.rgba.chunks(4).map(|px| px[0]).collect();
        assert_eq!(greys, source);
    }

    #[test]
    fn a_file_that_is_not_a_png_is_rejected() {
        assert_eq!(decode(b"GIF89a", LIMIT), Err(PngError::BadSignature));
        assert_eq!(decode(&[], LIMIT), Err(PngError::BadSignature));
    }

    #[test]
    fn a_corrupt_chunk_checksum_is_an_error() {
        let mut bytes = rgba_png(1, 1, &[1, 2, 3, 4]);
        // The last byte of IHDR's CRC.
        bytes[32] ^= 0xff;
        assert_eq!(decode(&bytes, LIMIT), Err(PngError::BadCrc));
    }

    #[test]
    fn a_zero_sized_image_is_an_error() {
        let bytes = png(0, 1, 8, 6, 0, &[], &[0]);
        assert_eq!(decode(&bytes, LIMIT), Err(PngError::BadHeader));
    }

    #[test]
    fn an_impossible_depth_and_colour_combination_is_an_error() {
        // Colour type 2 has no four-bit form.
        let bytes = png(1, 1, 4, 2, 0, &[], &[0, 0]);
        assert_eq!(decode(&bytes, LIMIT), Err(PngError::Unsupported));
        // Colour type 5 does not exist at all.
        let bytes = png(1, 1, 8, 5, 0, &[], &[0, 0]);
        assert_eq!(decode(&bytes, LIMIT), Err(PngError::BadHeader));
    }

    #[test]
    fn an_unknown_critical_chunk_is_refused() {
        let mut bytes = SIGNATURE.to_vec();
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&1u32.to_be_bytes());
        ihdr.extend_from_slice(&1u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
        bytes.extend_from_slice(&chunk(b"IHDR", &ihdr));
        bytes.extend_from_slice(&chunk(b"zZZz", &[1, 2, 3])); // ancillary, skipped
        bytes.extend_from_slice(&chunk(b"QQQQ", &[1, 2, 3])); // critical, fatal
        bytes.extend_from_slice(&chunk(b"IDAT", &zlib_stored(&[0, 1, 2, 3, 4])));
        bytes.extend_from_slice(&chunk(b"IEND", &[]));
        assert_eq!(decode(&bytes, LIMIT), Err(PngError::Unsupported));
    }

    #[test]
    fn an_ancillary_chunk_is_skipped() {
        let mut bytes = SIGNATURE.to_vec();
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&1u32.to_be_bytes());
        ihdr.extend_from_slice(&1u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
        bytes.extend_from_slice(&chunk(b"IHDR", &ihdr));
        bytes.extend_from_slice(&chunk(b"gAMA", &[0, 1, 0x86, 0xa0]));
        bytes.extend_from_slice(&chunk(b"IDAT", &zlib_stored(&[0, 1, 2, 3, 4])));
        bytes.extend_from_slice(&chunk(b"IEND", &[]));
        assert_eq!(decode(&bytes, LIMIT).unwrap().rgba, vec![1, 2, 3, 4]);
    }

    #[test]
    fn image_data_split_across_several_idat_chunks_is_joined() {
        let stream = zlib_stored(&[0, 1, 2, 3, 4, 0, 5, 6, 7, 8]);
        let (first, second) = stream.split_at(stream.len() / 2);
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&1u32.to_be_bytes());
        ihdr.extend_from_slice(&2u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
        let mut bytes = SIGNATURE.to_vec();
        bytes.extend_from_slice(&chunk(b"IHDR", &ihdr));
        bytes.extend_from_slice(&chunk(b"IDAT", first));
        bytes.extend_from_slice(&chunk(b"IDAT", second));
        bytes.extend_from_slice(&chunk(b"IEND", &[]));
        assert_eq!(decode(&bytes, LIMIT).unwrap().rgba, vec![1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn a_png_with_no_pixel_data_is_an_error() {
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&1u32.to_be_bytes());
        ihdr.extend_from_slice(&1u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
        let mut bytes = SIGNATURE.to_vec();
        bytes.extend_from_slice(&chunk(b"IHDR", &ihdr));
        bytes.extend_from_slice(&chunk(b"IEND", &[]));
        assert_eq!(decode(&bytes, LIMIT), Err(PngError::NoImageData));
    }

    #[test]
    fn pixel_data_shorter_than_the_header_promises_is_an_error() {
        // Two rows declared, one row supplied.
        let bytes = png(1, 2, 8, 6, 0, &[], &[0, 1, 2, 3, 4]);
        assert_eq!(decode(&bytes, LIMIT), Err(PngError::Truncated));
    }

    #[test]
    fn an_image_larger_than_the_budget_is_refused_before_allocating() {
        // 20000x20000 RGBA is 1.6 GB; the header alone is enough to say no.
        let bytes = png(20_000, 20_000, 8, 6, 0, &[], &[0, 0, 0, 0, 0]);
        assert_eq!(decode(&bytes, 1 << 20), Err(PngError::TooLarge));
    }

    #[test]
    fn an_idat_bomb_stops_at_the_size_the_header_promised() {
        // The header says one pixel; IDAT decompresses to a megabyte.
        let bytes = png(1, 1, 8, 6, 0, &[], &vec![0u8; 1 << 20]);
        assert_eq!(
            decode(&bytes, LIMIT),
            Err(PngError::Deflate(InflateError::TooLarge))
        );
    }

    #[test]
    fn a_truncated_idat_is_an_error_not_a_panic() {
        let full = rgba_png(4, 4, &[0x55; 4 * 4 * 4]);
        for cut in 0..full.len() {
            assert!(
                decode(&full[..cut], LIMIT).is_err(),
                "truncation at {cut} decoded"
            );
        }
    }

    #[test]
    fn corrupting_any_byte_of_a_png_never_panics() {
        let full = rgba_png(3, 2, &(0..3 * 2 * 4).map(|i| i as u8).collect::<Vec<u8>>());
        for at in 0..full.len() {
            for pattern in [0x01u8, 0x80, 0xff] {
                let mut damaged = full.clone();
                damaged[at] ^= pattern;
                // Most of these fail a CRC; what matters is that none of them
                // panic, hang, or allocate without bound.
                let _ = decode(&damaged, LIMIT);
            }
        }
    }

    #[test]
    fn garbage_of_every_length_behind_a_valid_signature_never_panics() {
        for length in 0..64usize {
            let mut bytes = SIGNATURE.to_vec();
            bytes.extend((0..length).map(|i| (i * 37 % 256) as u8));
            let _ = decode(&bytes, LIMIT);
        }
    }
}
