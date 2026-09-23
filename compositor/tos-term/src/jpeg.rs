//! Baseline JPEG decoding (ITU-T T.81 / ISO 10918-1) to RGB8.
//!
//! Written from the specification, the way [`crate::png`] and
//! [`crate::inflate`] were: T.81's Annex A for the DCT and the sample
//! layout, Annex B for the marker segments, Annex F for the entropy coder,
//! and T.871 (JFIF) for the colour transform. No other decoder's source was
//! read or reproduced, which is the same rule the rest of this crate is
//! written under and the reason a second image format costs a file rather
//! than a dependency.
//!
//! # Why this exists at all
//!
//! `apps/preview/src/lib.rs` refused JPEG on the grounds that nothing in the
//! tree wanted it. `docs/design/browser.md` now has the measurement that
//! wants it: Chromium's `Page.startScreencast` is bounded by its own
//! single-threaded encode of each frame, and asking it for JPEG instead of
//! PNG takes a 1280x770 pane from 33.8 to 57.8 frames a second — the engine
//! spends 28 ms on a PNG frame and 17 ms on a JPEG one, on the same two
//! cores. That is the difference between a page that scrolls and a page that
//! stutters, and it cannot be bought anywhere else in the path: it is the
//! encoder, not the wire and not the terminal.
//!
//! # What it decodes, and what it refuses
//!
//! Exactly what that encoder emits, and nothing beyond it:
//!
//! ```text
//! SOI                    the file starts, or it is not one
//! APPn, COM              skipped by length; APP14 is read far enough to
//!                        refuse an Adobe colour transform
//! DQT                    8-bit tables, and 16-bit ones if they come
//! SOF0                   baseline, 8-bit, one or three components,
//!                        sampling factors of 1 or 2 in each axis
//! DHT                    DC and AC tables, four of each
//! DRI + RSTn             restart intervals, resynchronised at the marker
//! SOS                    one scan over every component
//! EOI
//! ```
//!
//! Everything else is refused with a [`JpegError`] that says which thing it
//! was: progressive (SOF2) above all, because it is what a camera and every
//! web image pipeline emit and it is a second entropy decoder rather than a
//! branch, but also extended sequential and lossless, arithmetic coding,
//! 12-bit precision, CMYK, and a file that simply stops. Refusing loudly is
//! the point — a decoder that returns a grey rectangle for a progressive
//! file is worse than one that says it cannot read progressive files.
//!
//! # The arithmetic, and why it is the shape it is
//!
//! **The IDCT is separable, fixed point, 13 bits of constant.** The 1-D
//! transform of A.3.3 is split into its even and odd halves the way the
//! symmetry of the cosine invites: with `c(k) = cos(k*pi/16)` scaled by
//! 2^13, the even coefficients need six multiplications and the odd ones
//! sixteen, and `s(7-x) = even(x) - odd(x)` gives the second half of the
//! output for nothing — twenty-two multiplications a row against the
//! sixty-four a matrix multiply would take. Rows first, keeping four
//! fractional bits (`PASS1_BITS`), then columns, then one shift that undoes
//! the constants, those four bits and the 1/4 in front of the
//! two-dimensional sum together. Against the floating-point definition the
//! error is within one count a sample, which a unit test asserts; against
//! libjpeg's own answer for the same file, three, which is the tolerance the
//! fixtures are compared at.
//!
//! Every accumulation is `i64`. A hostile file can pair a 16-bit quantisation
//! table with a 15-bit coefficient and reach 2^31 before the first
//! multiplication, so `i32` here would be a panic in a debug build and
//! nonsense pixels in a release one. On a 64-bit machine the wider
//! accumulator costs nothing measurable, and it means no input needs
//! clamping to keep the transform honest.
//!
//! **Chroma is upsampled with a triangle filter, not by replication.** JFIF
//! sites a subsampled chroma sample at the centre of the luma samples it
//! covers — for 2x subsampling, half a luma sample off the grid — so linear
//! interpolation at that phase has weights of 3/4 and 1/4, and in two axes
//! 9/16, 3/16, 3/16, 1/16. Replication would be cheaper by about a shift per
//! pixel and would put a visible step on every coloured edge, which is the
//! one artefact this format is already being asked to tolerate: Chromium's
//! screencast is 4:2:0 at every quality, so a blue link is already half
//! resolution in chroma and does not need a second error on top. The filter
//! runs a row at a time into a scratch buffer of the chroma's own width, so
//! the vertical pass costs half a row per output row rather than a second
//! full-size plane.
//!
//! **No allocation per pixel, and no bounds check in the inner loops.** The
//! component planes are allocated once from the frame header, the entropy
//! decoder writes 8x8 blocks through a slice of known length, and the colour
//! conversion walks three scratch rows and one output row as chunks. A block
//! whose only coefficient is its DC is one value written sixty-four times
//! rather than two transforms, and a row of the first pass with no AC
//! coefficient in it is the same shortcut one dimension down; in a page of
//! text at this quality most blocks are one or the other.
//!
//! What that comes to: **8.1 ms** for a 1280x770 4:2:0 frame at quality 85,
//! in release, on the machine the numbers in `docs/design/browser.md` were
//! taken on — against the 17 ms the engine takes to encode it and the 17 ms
//! between two frames at the rate it then sustains. `examples/jpeg_decode.rs`
//! is how that is measured; it wants a real frame rather than a gradient,
//! because a gradient is flat blocks and decodes in a third of the time.

/// Why a JPEG could not be decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JpegError {
    /// The file does not start with SOI.
    NotJpeg,
    /// A segment, or the entropy-coded data, ended early.
    Truncated,
    /// A marker segment's length does not fit what is in it.
    BadSegment,
    /// SOF2: progressive. A different entropy decoder, not a flag.
    Progressive,
    /// SOF1, SOF3, SOF5..SOF7: extended sequential, lossless, hierarchical.
    UnsupportedProcess(u8),
    /// SOF9 and above: arithmetic coding rather than Huffman.
    Arithmetic,
    /// Sample precision other than 8 bits.
    UnsupportedPrecision(u8),
    /// A component count this decoder has no colour transform for.
    UnsupportedComponents(u8),
    /// A sampling factor outside 1 or 2, or a chroma plane larger than luma.
    UnsupportedSampling,
    /// An Adobe APP14 colour transform that is not YCbCr.
    AdobeTransform(u8),
    /// A quantisation table that is missing, the wrong size, or has an
    /// identifier outside 0..=3.
    BadQuantTable,
    /// A Huffman table that is missing, over-subscribed, or has an
    /// identifier outside 0..=3.
    BadHuffmanTable,
    /// A bit pattern in the entropy-coded data that is no code in its table.
    BadHuffmanCode,
    /// SOS before SOF, or a scan naming a component the frame does not have.
    BadScan,
    /// A frame with a zero dimension, or one too large to address.
    BadDimensions,
    /// The pixels would not fit the caller's budget.
    TooLarge,
}

impl std::fmt::Display for JpegError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JpegError::NotJpeg => f.write_str("not a JPEG file"),
            JpegError::Truncated => f.write_str("truncated JPEG"),
            JpegError::BadSegment => f.write_str("corrupt JPEG marker segment"),
            JpegError::Progressive => {
                f.write_str("progressive JPEG (SOF2): this decoder is baseline only")
            }
            JpegError::UnsupportedProcess(marker) => write!(
                f,
                "JPEG coding process SOF{}: this decoder is baseline (SOF0) only",
                marker & 0x0f
            ),
            JpegError::Arithmetic => {
                f.write_str("arithmetic-coded JPEG: this decoder is Huffman only")
            }
            JpegError::UnsupportedPrecision(bits) => {
                write!(f, "{bits}-bit JPEG samples: this decoder is 8-bit only")
            }
            JpegError::UnsupportedComponents(count) => write!(
                f,
                "{count}-component JPEG: this decoder reads grayscale and YCbCr only"
            ),
            JpegError::UnsupportedSampling => {
                f.write_str("JPEG sampling factors outside 4:4:4, 4:2:2, 4:4:0 and 4:2:0")
            }
            JpegError::AdobeTransform(code) => write!(
                f,
                "Adobe colour transform {code}: this decoder reads YCbCr only"
            ),
            JpegError::BadQuantTable => f.write_str("corrupt JPEG quantisation table"),
            JpegError::BadHuffmanTable => f.write_str("corrupt JPEG Huffman table"),
            JpegError::BadHuffmanCode => f.write_str("corrupt JPEG entropy-coded data"),
            JpegError::BadScan => f.write_str("JPEG scan header does not match the frame"),
            JpegError::BadDimensions => f.write_str("corrupt JPEG frame header"),
            JpegError::TooLarge => f.write_str("JPEG exceeds the image budget"),
        }
    }
}

impl std::error::Error for JpegError {}

/// A decoded image: RGB8, three bytes per pixel, top row first.
///
/// Three and not four, unlike [`crate::png::PngImage`]: a JPEG has no alpha
/// to carry and the graphics protocol has a format for each — `f=24` takes
/// exactly this, and widening here would make every caller that sends one
/// pay a third more memory and a third more bytes for a channel of 255s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
}

/// How big a JPEG is, without decoding it.
///
/// Reads marker segments until the frame header and stops there, so a caller
/// that only needs the shape of a picture pays for the headers rather than
/// for the pixels. A file this refuses is one [`decode`] would refuse too.
pub fn dimensions(data: &[u8]) -> Result<(u32, u32), JpegError> {
    let frame = Reader::new(data).frame_header()?;
    Ok((frame.width, frame.height))
}

/// Decode a baseline JPEG into RGB8.
///
/// `limit` is the largest RGB output accepted, in bytes; an image whose frame
/// header asks for more is refused before anything is allocated.
pub fn decode(data: &[u8], limit: usize) -> Result<Image, JpegError> {
    Reader::new(data).decode(limit)
}

// ---------------------------------------------------------------------------
// The constants of the transform
// ---------------------------------------------------------------------------

/// Fractional bits in the cosine constants. Thirteen leaves a 1-count error
/// against an exact transform and keeps every product inside a machine word
/// with room for the sum of eight of them.
const CONST_BITS: u32 = 13;

/// Fractional bits kept between the row pass and the column pass.
///
/// Four rather than two: the column pass multiplies whatever the row pass
/// rounded off by up to five, so two bits let the first rounding show in the
/// second and four do not. It costs nothing — the workspace is `i64` for
/// other reasons — and it halves the number of samples that come out one
/// count away from an exact transform.
const PASS1_BITS: u32 = 4;

/// `cos(k*pi/16) * 2^13`, which is every constant the 1-D transform needs.
const C1: i64 = 8035;
const C2: i64 = 7568;
const C3: i64 = 6811;
const C4: i64 = 5793;
const C5: i64 = 4551;
const C6: i64 = 3135;
const C7: i64 = 1598;

/// T.81 Figure A.6: where the k-th coefficient of the zig-zag sequence lives
/// in an 8x8 block read in raster order.
const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, //
    17, 24, 32, 25, 18, 11, 4, 5, //
    12, 19, 26, 33, 40, 48, 41, 34, //
    27, 20, 13, 6, 7, 14, 21, 28, //
    35, 42, 49, 56, 57, 50, 43, 36, //
    29, 22, 15, 23, 30, 37, 44, 51, //
    58, 59, 52, 45, 38, 31, 39, 46, //
    53, 60, 61, 54, 47, 55, 62, 63,
];

// ---------------------------------------------------------------------------
// Huffman tables
// ---------------------------------------------------------------------------

/// How many bits the flat lookup covers. Nine is what a Chromium-sized table
/// puts most of its mass under: the codes for a run-length of zero and a
/// small magnitude, which is nearly every symbol in a photograph.
const FAST_BITS: u32 = 9;

/// A Huffman table, in the two shapes decoding wants.
///
/// The flat array answers a code of nine bits or fewer in one indexed read.
/// Anything longer falls through to the procedure of T.81 F.16, which walks
/// the lengths comparing against `maxcode` — the canonical codes make that a
/// comparison rather than a search.
struct HuffTable {
    /// `(length << 8) | value` for every 9-bit prefix, or 0 for "longer".
    fast: Box<[u16; 1 << FAST_BITS]>,
    maxcode: [i32; 18],
    mincode: [i32; 17],
    valptr: [usize; 17],
    values: Vec<u8>,
}

impl HuffTable {
    /// Build from a DHT segment's sixteen counts and its list of values.
    fn build(counts: &[u8; 16], values: Vec<u8>) -> Result<HuffTable, JpegError> {
        let mut table = HuffTable {
            fast: Box::new([0u16; 1 << FAST_BITS]),
            maxcode: [-1; 18],
            mincode: [0; 17],
            valptr: [0; 17],
            values,
        };
        // T.81 F.15: the canonical code of each length, assigned in order.
        let mut code: u32 = 0;
        let mut k: usize = 0;
        for length in 1..=16usize {
            table.mincode[length] = code as i32;
            table.valptr[length] = k;
            let count = counts[length - 1] as usize;
            if k + count > table.values.len() {
                return Err(JpegError::BadHuffmanTable);
            }
            for _ in 0..count {
                if code >= (1 << length) {
                    // More codes of this length than the prefix property
                    // allows: the table is over-subscribed.
                    return Err(JpegError::BadHuffmanTable);
                }
                if length <= FAST_BITS as usize {
                    let shift = FAST_BITS as usize - length;
                    let base = (code as usize) << shift;
                    let entry = ((length as u16) << 8) | table.values[k] as u16;
                    for slot in &mut table.fast[base..base + (1 << shift)] {
                        *slot = entry;
                    }
                }
                code += 1;
                k += 1;
            }
            table.maxcode[length] = if count == 0 { -1 } else { code as i32 - 1 };
            code <<= 1;
        }
        // The walk in `decode` stops here rather than running off the end.
        table.maxcode[17] = i32::MAX;
        Ok(table)
    }
}

// ---------------------------------------------------------------------------
// The bit reader
// ---------------------------------------------------------------------------

/// The entropy-coded segment, read a bit at a time out of a 64-bit window.
///
/// Two things make this more than a shift register. A 0xFF byte in the data
/// is written as 0xFF 0x00 (T.81 B.1.1.5), so the zero has to be swallowed;
/// and a 0xFF followed by anything else is a marker, which ends the segment
/// and must *not* be consumed, because the restart logic and the caller both
/// need to find it again. Past a marker the window is fed zero bits, so a
/// decode that has run off the end terminates with nonsense rather than
/// looping — and `ran_out`, which is set only when the bytes themselves ran
/// out with no marker in sight, is what turns that into [`JpegError::Truncated`].
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    window: u64,
    bits: u32,
    /// A marker was reached; `pos` points at its 0xFF.
    at_marker: bool,
    /// The data ended with no marker at all.
    ran_out: bool,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8], pos: usize) -> Bits<'a> {
        Bits {
            data,
            pos,
            window: 0,
            bits: 0,
            at_marker: false,
            ran_out: false,
        }
    }

    /// The next byte of entropy data, with stuffing undone.
    #[inline]
    fn next_byte(&mut self) -> u8 {
        let Some(&byte) = self.data.get(self.pos) else {
            self.at_marker = true;
            self.ran_out = true;
            return 0;
        };
        if byte != 0xFF {
            self.pos += 1;
            return byte;
        }
        // A run of 0xFF may be fill bytes before a marker (T.81 B.1.1.2);
        // what follows the run decides which this is.
        let mut next = self.pos + 1;
        while self.data.get(next) == Some(&0xFF) {
            next += 1;
        }
        match self.data.get(next) {
            Some(0x00) => {
                self.pos = next + 1;
                0xFF
            }
            Some(_) => {
                self.at_marker = true;
                0
            }
            None => {
                self.at_marker = true;
                self.ran_out = true;
                0
            }
        }
    }

    /// Top the window up to at least 57 bits, which is more than any code and
    /// its magnitude bits together can ask for.
    #[inline]
    fn fill(&mut self) {
        while self.bits <= 56 {
            let byte = if self.at_marker { 0 } else { self.next_byte() };
            self.window = (self.window << 8) | byte as u64;
            self.bits += 8;
        }
    }

    /// The next `n` bits, most significant first. `n` is at most 16.
    #[inline]
    fn take(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        if self.bits < n {
            self.fill();
        }
        self.bits -= n;
        ((self.window >> self.bits) as u32) & ((1u32 << n) - 1)
    }

    /// Look at the next `FAST_BITS` bits without consuming them.
    #[inline]
    fn peek(&mut self) -> u32 {
        if self.bits < FAST_BITS {
            self.fill();
        }
        ((self.window >> (self.bits - FAST_BITS)) as u32) & ((1 << FAST_BITS) - 1)
    }

    #[inline]
    fn skip(&mut self, n: u32) {
        self.bits -= n;
    }

    /// One Huffman symbol: T.81 F.16, with the short codes answered flat.
    #[inline]
    fn huffman(&mut self, table: &HuffTable) -> Result<u8, JpegError> {
        let entry = table.fast[self.peek() as usize];
        if entry != 0 {
            self.skip((entry >> 8) as u32);
            return Ok(entry as u8);
        }
        // Longer than the lookup: walk the lengths. The first FAST_BITS are
        // still in the window, so this re-reads them one at a time.
        let mut code: i32 = 0;
        let mut length = 0usize;
        while length < 16 {
            code = (code << 1) | self.take(1) as i32;
            length += 1;
            if length >= FAST_BITS as usize && code <= table.maxcode[length] {
                let index = table.valptr[length] + (code - table.mincode[length]) as usize;
                return table
                    .values
                    .get(index)
                    .copied()
                    .ok_or(JpegError::BadHuffmanCode);
            }
        }
        Err(JpegError::BadHuffmanCode)
    }

    /// T.81 F.12: `n` magnitude bits, extended into the signed range the
    /// difference or coefficient they encode actually covers.
    #[inline]
    fn receive_extend(&mut self, n: u32) -> i32 {
        let raw = self.take(n) as i32;
        // The low half of the range is negative, and the offset that makes it
        // so is one less than twice the half: -(2^n - 1) .. -(2^(n-1)).
        if raw < (1 << (n - 1)) {
            raw - (1 << n) + 1
        } else {
            raw
        }
    }

    /// Byte-align and step over the restart marker that should be here.
    ///
    /// Scanning forward rather than trusting `pos`: the window reads up to
    /// eight bytes ahead of the bit that was last needed, so the marker may
    /// be anywhere from here to a few bytes on. Anything that is not an RSTn
    /// ends the scan, which is what an EOI arriving early means.
    fn restart(&mut self) -> bool {
        self.window = 0;
        self.bits = 0;
        self.at_marker = false;
        while self.pos + 1 < self.data.len() {
            if self.data[self.pos] != 0xFF {
                self.pos += 1;
                continue;
            }
            match self.data[self.pos + 1] {
                0x00 => self.pos += 2,
                0xFF => self.pos += 1,
                0xD0..=0xD7 => {
                    self.pos += 2;
                    return true;
                }
                _ => return false,
            }
        }
        self.ran_out = true;
        false
    }
}

// ---------------------------------------------------------------------------
// Headers
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Component {
    id: u8,
    h: u32,
    v: u32,
    quant: usize,
    /// Blocks across and down this component's own plane, whole MCUs included.
    blocks_w: usize,
    blocks_h: usize,
    /// The samples that are really the picture, before MCU padding.
    width: usize,
    height: usize,
}

struct Frame {
    width: u32,
    height: u32,
    components: Vec<Component>,
    h_max: u32,
    v_max: u32,
    mcus_w: usize,
    mcus_h: usize,
}

/// Walks the marker segments of one file.
struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
    quant: [Option<[u16; 64]>; 4],
    dc: [Option<HuffTable>; 4],
    ac: [Option<HuffTable>; 4],
    restart_interval: usize,
    frame: Option<Frame>,
    adobe: Option<u8>,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Reader<'a> {
        Reader {
            data,
            pos: 0,
            quant: [None, None, None, None],
            dc: [None, None, None, None],
            ac: [None, None, None, None],
            restart_interval: 0,
            frame: None,
            adobe: None,
        }
    }

    fn u16_at(&self, at: usize) -> Result<u16, JpegError> {
        match (self.data.get(at), self.data.get(at + 1)) {
            (Some(&hi), Some(&lo)) => Ok(((hi as u16) << 8) | lo as u16),
            _ => Err(JpegError::Truncated),
        }
    }

    /// Step to the next marker byte, skipping the fill bytes that may precede
    /// it. Returns the marker and leaves `pos` at its first payload byte.
    fn next_marker(&mut self) -> Result<u8, JpegError> {
        // Between segments there is nothing but markers and 0xFF fill.
        while self.data.get(self.pos) == Some(&0xFF) {
            self.pos += 1;
        }
        let marker = *self.data.get(self.pos).ok_or(JpegError::Truncated)?;
        self.pos += 1;
        Ok(marker)
    }

    /// The body of the segment starting at `pos`, and `pos` moved past it.
    fn segment(&mut self) -> Result<&'a [u8], JpegError> {
        let length = self.u16_at(self.pos)? as usize;
        if length < 2 {
            return Err(JpegError::BadSegment);
        }
        let start = self.pos + 2;
        let end = self.pos + length;
        let body = self.data.get(start..end).ok_or(JpegError::Truncated)?;
        self.pos = end;
        Ok(body)
    }

    /// Read segments until SOI has been seen and a frame header parsed.
    fn frame_header(mut self) -> Result<Frame, JpegError> {
        self.read_until_scan(false)?;
        self.frame.ok_or(JpegError::Truncated)
    }

    /// The marker loop. With `want_scan` it stops at SOS, with `pos` on the
    /// scan header's length; without it, at the frame header.
    fn read_until_scan(&mut self, want_scan: bool) -> Result<(), JpegError> {
        if self.data.len() < 2 || self.data[0] != 0xFF || self.data[1] != 0xD8 {
            return Err(JpegError::NotJpeg);
        }
        self.pos = 2;
        loop {
            // Markers are introduced by 0xFF; anything else here is corrupt.
            match self.data.get(self.pos) {
                Some(&0xFF) => {}
                Some(_) => return Err(JpegError::BadSegment),
                None => return Err(JpegError::Truncated),
            }
            let marker = self.next_marker()?;
            match marker {
                // Standalone markers: no length, nothing to read.
                0x01 | 0xD0..=0xD7 => {}
                0xD9 => return Err(JpegError::Truncated), // EOI before any scan
                0xC0 => {
                    let body = self.segment()?;
                    self.read_frame(body)?;
                    if !want_scan {
                        return Ok(());
                    }
                }
                0xC2 => return Err(JpegError::Progressive),
                0xC1 | 0xC3 | 0xC5 | 0xC6 | 0xC7 | 0xC8 => {
                    return Err(JpegError::UnsupportedProcess(marker))
                }
                0xC9..=0xCB | 0xCD..=0xCF => return Err(JpegError::Arithmetic),
                0xC4 => {
                    let body = self.segment()?;
                    self.read_huffman(body)?;
                }
                0xCC => return Err(JpegError::Arithmetic), // DAC
                0xDB => {
                    let body = self.segment()?;
                    self.read_quant(body)?;
                }
                0xDD => {
                    let body = self.segment()?;
                    if body.len() < 2 {
                        return Err(JpegError::BadSegment);
                    }
                    self.restart_interval = ((body[0] as usize) << 8) | body[1] as usize;
                }
                0xEE => {
                    let body = self.segment()?;
                    // APP14, and an "Adobe" one carries the colour transform
                    // in its last byte. Anything but 1 (YCbCr) is a file this
                    // decoder would render in the wrong colours.
                    if body.len() >= 12 && body.starts_with(b"Adobe") {
                        self.adobe = body.last().copied();
                    }
                }
                0xDA => {
                    if !want_scan {
                        return Err(JpegError::Truncated);
                    }
                    return Ok(());
                }
                // APPn, COM, DNL and every other segment that carries a
                // length and nothing this decoder needs.
                _ => {
                    self.segment()?;
                }
            }
        }
    }

    /// DQT: one or more tables, each 8-bit or 16-bit, in zig-zag order.
    fn read_quant(&mut self, mut body: &[u8]) -> Result<(), JpegError> {
        while !body.is_empty() {
            let spec = body[0];
            let precision = spec >> 4;
            let id = (spec & 0x0f) as usize;
            if id >= 4 || precision > 1 {
                return Err(JpegError::BadQuantTable);
            }
            let width = if precision == 0 { 1 } else { 2 };
            let need = 1 + 64 * width;
            if body.len() < need {
                return Err(JpegError::BadQuantTable);
            }
            let mut table = [0u16; 64];
            for k in 0..64 {
                let value = if precision == 0 {
                    body[1 + k] as u16
                } else {
                    ((body[1 + 2 * k] as u16) << 8) | body[2 + 2 * k] as u16
                };
                if value == 0 {
                    // A zero divisor is a table no encoder writes and a
                    // decoder cannot honour.
                    return Err(JpegError::BadQuantTable);
                }
                table[ZIGZAG[k]] = value;
            }
            self.quant[id] = Some(table);
            body = &body[need..];
        }
        Ok(())
    }

    /// DHT: one or more tables, each sixteen counts and then the values.
    fn read_huffman(&mut self, mut body: &[u8]) -> Result<(), JpegError> {
        while !body.is_empty() {
            if body.len() < 17 {
                return Err(JpegError::BadHuffmanTable);
            }
            let class = body[0] >> 4;
            let id = (body[0] & 0x0f) as usize;
            if id >= 4 || class > 1 {
                return Err(JpegError::BadHuffmanTable);
            }
            let mut counts = [0u8; 16];
            counts.copy_from_slice(&body[1..17]);
            let total: usize = counts.iter().map(|&c| c as usize).sum();
            if body.len() < 17 + total {
                return Err(JpegError::BadHuffmanTable);
            }
            let table = HuffTable::build(&counts, body[17..17 + total].to_vec())?;
            if class == 0 {
                self.dc[id] = Some(table);
            } else {
                self.ac[id] = Some(table);
            }
            body = &body[17 + total..];
        }
        Ok(())
    }

    /// SOF0, and the shape of everything that follows it.
    fn read_frame(&mut self, body: &[u8]) -> Result<(), JpegError> {
        if self.frame.is_some() {
            return Err(JpegError::BadSegment);
        }
        if body.len() < 6 {
            return Err(JpegError::BadSegment);
        }
        let precision = body[0];
        if precision != 8 {
            return Err(JpegError::UnsupportedPrecision(precision));
        }
        let height = ((body[1] as u32) << 8) | body[2] as u32;
        let width = ((body[3] as u32) << 8) | body[4] as u32;
        let count = body[5];
        if width == 0 || height == 0 {
            return Err(JpegError::BadDimensions);
        }
        if count != 1 && count != 3 {
            return Err(JpegError::UnsupportedComponents(count));
        }
        if let Some(transform) = self.adobe {
            // 0 is RGB or CMYK, 2 is YCCK; only 1 is the YCbCr below.
            if transform != 1 {
                return Err(JpegError::AdobeTransform(transform));
            }
        }
        if body.len() < 6 + 3 * count as usize {
            return Err(JpegError::BadSegment);
        }

        let mut components = Vec::with_capacity(count as usize);
        let (mut h_max, mut v_max) = (1u32, 1u32);
        for index in 0..count as usize {
            let at = 6 + 3 * index;
            let h = (body[at + 1] >> 4) as u32;
            let v = (body[at + 1] & 0x0f) as u32;
            let quant = (body[at + 2] & 0x0f) as usize;
            if h == 0 || h > 2 || v == 0 || v > 2 || body[at + 2] > 3 {
                return Err(JpegError::UnsupportedSampling);
            }
            h_max = h_max.max(h);
            v_max = v_max.max(v);
            components.push(Component {
                id: body[at],
                h,
                v,
                quant,
                blocks_w: 0,
                blocks_h: 0,
                width: 0,
                height: 0,
            });
        }
        // A single-component frame is grayscale whatever its factors claim,
        // and a three-component one with the chroma larger than the luma is
        // a layout nothing emits and this decoder will not guess at.
        if count == 3 && (components[1].h > components[0].h || components[2].h > components[0].h) {
            return Err(JpegError::UnsupportedSampling);
        }

        let mcu_w = (8 * h_max) as usize;
        let mcu_h = (8 * v_max) as usize;
        let mcus_w = (width as usize).div_ceil(mcu_w);
        let mcus_h = (height as usize).div_ceil(mcu_h);
        for component in &mut components {
            component.blocks_w = mcus_w * component.h as usize;
            component.blocks_h = mcus_h * component.v as usize;
            component.width = (width as usize * component.h as usize).div_ceil(h_max as usize);
            component.height = (height as usize * component.v as usize).div_ceil(v_max as usize);
        }

        self.frame = Some(Frame {
            width,
            height,
            components,
            h_max,
            v_max,
            mcus_w,
            mcus_h,
        });
        Ok(())
    }

    // -----------------------------------------------------------------------
    // The scan
    // -----------------------------------------------------------------------

    fn decode(mut self, limit: usize) -> Result<Image, JpegError> {
        self.read_until_scan(true)?;
        let frame = self.frame.take().ok_or(JpegError::BadScan)?;

        let pixels = (frame.width as usize)
            .checked_mul(frame.height as usize)
            .and_then(|area| area.checked_mul(3))
            .ok_or(JpegError::BadDimensions)?;
        if pixels > limit {
            return Err(JpegError::TooLarge);
        }

        // SOS: which table each component reads from, in the order the MCUs
        // interleave them.
        let header = self.segment()?;
        if header.is_empty() {
            return Err(JpegError::BadScan);
        }
        let count = header[0] as usize;
        if count != frame.components.len() || header.len() < 1 + 2 * count + 3 {
            return Err(JpegError::BadScan);
        }
        let mut order = Vec::with_capacity(count);
        for index in 0..count {
            let id = header[1 + 2 * index];
            let tables = header[2 + 2 * index];
            let which = frame
                .components
                .iter()
                .position(|component| component.id == id)
                .ok_or(JpegError::BadScan)?;
            order.push((which, (tables >> 4) as usize, (tables & 0x0f) as usize));
        }
        // Ss, Se, Ah, Al: a baseline scan is the whole spectrum at full
        // precision, and anything else is a progressive file wearing a SOF0.
        let tail = &header[1 + 2 * count..];
        if tail[0] != 0 || tail[1] != 63 || tail[2] != 0 {
            return Err(JpegError::Progressive);
        }

        // One plane per component, whole MCUs wide so that the entropy
        // decoder never has to think about the edge.
        let mut planes: Vec<Vec<u8>> = Vec::with_capacity(frame.components.len());
        for component in &frame.components {
            let size = (component.blocks_w * 8)
                .checked_mul(component.blocks_h * 8)
                .ok_or(JpegError::BadDimensions)?;
            if size > limit.saturating_mul(4).max(1 << 20) {
                return Err(JpegError::TooLarge);
            }
            planes.push(vec![128u8; size]);
        }

        self.scan(&frame, &order, &mut planes)?;
        Ok(to_rgb(&frame, &planes))
    }

    /// The entropy-coded segment: MCU after MCU, restart after restart.
    fn scan(
        &self,
        frame: &Frame,
        order: &[(usize, usize, usize)],
        planes: &mut [Vec<u8>],
    ) -> Result<(), JpegError> {
        // Collecting the table references up front turns a per-block option
        // check and bounds check into a slice index.
        let mut tables = Vec::with_capacity(order.len());
        for &(which, dc, ac) in order {
            if dc >= 4 || ac >= 4 {
                return Err(JpegError::BadScan);
            }
            let dc = self.dc[dc].as_ref().ok_or(JpegError::BadHuffmanTable)?;
            let ac = self.ac[ac].as_ref().ok_or(JpegError::BadHuffmanTable)?;
            let quant = frame.components[which].quant;
            let quant = self.quant[quant].as_ref().ok_or(JpegError::BadQuantTable)?;
            tables.push((which, dc, ac, quant));
        }

        let mut bits = Bits::new(self.data, self.pos);
        let mut predictors = vec![0i32; frame.components.len()];
        let mut block = [0i32; 64];
        let mut since_restart = 0usize;

        for mcu_y in 0..frame.mcus_h {
            for mcu_x in 0..frame.mcus_w {
                if self.restart_interval != 0 && since_restart == self.restart_interval {
                    if !bits.restart() {
                        return Err(JpegError::Truncated);
                    }
                    predictors.iter_mut().for_each(|p| *p = 0);
                    since_restart = 0;
                }
                since_restart += 1;

                for (slot, &(which, dc, ac, quant)) in tables.iter().enumerate() {
                    let component = frame.components[which];
                    let stride = component.blocks_w * 8;
                    for by in 0..component.v as usize {
                        for bx in 0..component.h as usize {
                            let nonzero = decode_block(
                                &mut bits,
                                dc,
                                ac,
                                quant,
                                &mut predictors[slot],
                                &mut block,
                            )?;
                            let x = (mcu_x * component.h as usize + bx) * 8;
                            let y = (mcu_y * component.v as usize + by) * 8;
                            idct_into(&block, nonzero, &mut planes[which], stride, x, y);
                        }
                    }
                }
            }
        }

        if bits.ran_out {
            return Err(JpegError::Truncated);
        }
        Ok(())
    }
}

/// One 8x8 block: Huffman symbols in, dequantised coefficients out.
///
/// Returns whether anything but the DC coefficient survived, which is what
/// lets the transform take the shortcut that a flat block deserves — and in a
/// photograph at this quality most blocks are flat.
#[inline]
fn decode_block(
    bits: &mut Bits<'_>,
    dc: &HuffTable,
    ac: &HuffTable,
    quant: &[u16; 64],
    predictor: &mut i32,
    block: &mut [i32; 64],
) -> Result<bool, JpegError> {
    block.fill(0);

    // T.81 F.2.2.1: the DC coefficient is a difference from the last block of
    // the same component.
    let size = bits.huffman(dc)? as u32;
    if size > 15 {
        return Err(JpegError::BadHuffmanCode);
    }
    let diff = if size == 0 {
        0
    } else {
        bits.receive_extend(size)
    };
    *predictor = predictor.wrapping_add(diff);
    // Wrapping, not checked: a corrupt stream can drive the predictor
    // anywhere, and the transform below is `i64` and clamps its output, so a
    // nonsense block is nonsense pixels rather than a panic in a debug build.
    block[0] = predictor.wrapping_mul(quant[0] as i32);

    // T.81 F.2.2.2: run-length of zeros and magnitude, to the end of the
    // block or to an end-of-block symbol.
    let mut k = 1usize;
    let mut nonzero = false;
    while k < 64 {
        let symbol = bits.huffman(ac)?;
        let run = (symbol >> 4) as usize;
        let size = (symbol & 0x0f) as u32;
        if size == 0 {
            if run != 15 {
                break; // EOB
            }
            k += 16; // ZRL: sixteen zeros
            continue;
        }
        k += run;
        if k >= 64 {
            return Err(JpegError::BadHuffmanCode);
        }
        let at = ZIGZAG[k];
        block[at] = bits.receive_extend(size).wrapping_mul(quant[at] as i32);
        nonzero = true;
        k += 1;
    }
    Ok(nonzero)
}

// ---------------------------------------------------------------------------
// The inverse DCT
// ---------------------------------------------------------------------------

/// One 1-D inverse transform, T.81 A.3.3 split by the symmetry of the cosine.
///
/// With `c(k) = cos(k*pi/16)`, the even coefficients of `s(x)` contribute
/// symmetrically about the middle of the row and the odd ones
/// antisymmetrically, so `s(7-x) = even(x) - odd(x)` and only four outputs
/// have to be computed. The even half collapses further:
/// `c4*(S0 +- S4)` and `c2*S2 +- c6*S6` between them cover all four of its
/// values, which is six multiplications for the even part and sixteen for the
/// odd — twenty-two against the sixty-four a matrix multiply would take.
///
/// Everything is `i64`, with the constants scaled by `2^CONST_BITS`; the
/// caller shifts the result back down.
#[inline(always)]
fn idct_1d(s: &[i64; 8]) -> [i64; 8] {
    let p = C4 * (s[0] + s[4]);
    let m = C4 * (s[0] - s[4]);
    let q = C2 * s[2] + C6 * s[6];
    let r = C6 * s[2] - C2 * s[6];
    let e0 = p + q;
    let e1 = m + r;
    let e2 = m - r;
    let e3 = p - q;

    let o0 = C1 * s[1] + C3 * s[3] + C5 * s[5] + C7 * s[7];
    let o1 = C3 * s[1] - C7 * s[3] - C1 * s[5] - C5 * s[7];
    let o2 = C5 * s[1] - C1 * s[3] + C7 * s[5] + C3 * s[7];
    let o3 = C7 * s[1] - C5 * s[3] + C3 * s[5] - C1 * s[7];

    [
        e0 + o0,
        e1 + o1,
        e2 + o2,
        e3 + o3,
        e3 - o3,
        e2 - o2,
        e1 - o1,
        e0 - o0,
    ]
}

/// Level-shift, clamp and store one sample.
#[inline(always)]
fn to_sample(value: i64) -> u8 {
    (value + 128).clamp(0, 255) as u8
}

/// Transform one block and write its 8x8 of samples into a plane.
///
/// `nonzero` false means the block is its DC coefficient and nothing else,
/// which is one value repeated sixty-four times — the common case in a
/// photograph, and the reason this is worth a branch.
fn idct_into(
    block: &[i32; 64],
    nonzero: bool,
    plane: &mut [u8],
    stride: usize,
    x: usize,
    y: usize,
) {
    let round1 = 1i64 << (CONST_BITS - PASS1_BITS - 1);
    let round2 = 1i64 << (CONST_BITS + PASS1_BITS + 2 - 1);
    let shift2 = CONST_BITS + PASS1_BITS + 2;

    if !nonzero {
        let row = (C4 * block[0] as i64 + round1) >> (CONST_BITS - PASS1_BITS);
        let value = to_sample((C4 * row + round2) >> shift2);
        for line in 0..8 {
            let at = (y + line) * stride + x;
            plane[at..at + 8].fill(value);
        }
        return;
    }

    // Rows, into a workspace that keeps PASS1_BITS fractional bits.
    let mut work = [0i64; 64];
    for row in 0..8 {
        let s = &block[row * 8..row * 8 + 8];
        if s[1] == 0 && s[2] == 0 && s[3] == 0 && s[4] == 0 && s[5] == 0 && s[6] == 0 && s[7] == 0 {
            let value = (C4 * s[0] as i64 + round1) >> (CONST_BITS - PASS1_BITS);
            work[row * 8..row * 8 + 8].fill(value);
            continue;
        }
        let input = [
            s[0] as i64,
            s[1] as i64,
            s[2] as i64,
            s[3] as i64,
            s[4] as i64,
            s[5] as i64,
            s[6] as i64,
            s[7] as i64,
        ];
        let out = idct_1d(&input);
        for (slot, value) in work[row * 8..row * 8 + 8].iter_mut().zip(out) {
            *slot = (value + round1) >> (CONST_BITS - PASS1_BITS);
        }
    }

    // Columns. The final shift undoes the constants, the fractional bits the
    // row pass kept, and the 1/4 in front of the two-dimensional sum.
    let mut column = [0i64; 8];
    for col in 0..8 {
        for (row, slot) in column.iter_mut().enumerate() {
            *slot = work[row * 8 + col];
        }
        let out = idct_1d(&column);
        for (row, value) in out.iter().enumerate() {
            plane[(y + row) * stride + x + col] = to_sample((value + round2) >> shift2);
        }
    }
}

// ---------------------------------------------------------------------------
// Upsampling and colour
// ---------------------------------------------------------------------------

/// The JFIF colour transform, as four tables indexed by a sample.
///
/// T.871: `R = Y + 1.402 (Cr-128)`, `G = Y - 0.344136 (Cb-128) - 0.714136
/// (Cr-128)`, `B = Y + 1.772 (Cb-128)`. Sixteen fractional bits is more than
/// the eight bits of output can tell apart, so the rounding of the constants
/// never shows. The red and blue terms round at table-build time; the two
/// green ones cannot, because they are summed before they are rounded.
struct Colour {
    r_cr: [i32; 256],
    b_cb: [i32; 256],
    g_cb: [i32; 256],
    g_cr: [i32; 256],
}

impl Colour {
    fn new() -> Colour {
        let mut colour = Colour {
            r_cr: [0; 256],
            b_cb: [0; 256],
            g_cb: [0; 256],
            g_cr: [0; 256],
        };
        for value in 0..256usize {
            let centred = value as i32 - 128;
            colour.r_cr[value] = (91881 * centred + 32768) >> 16;
            colour.b_cb[value] = (116130 * centred + 32768) >> 16;
            colour.g_cb[value] = -22554 * centred;
            colour.g_cr[value] = -46802 * centred;
        }
        colour
    }
}

/// One full-width row of one component's samples, upsampled if it is
/// subsampled.
///
/// The vertical pass runs at the chroma's own width into `scratch` with two
/// fractional bits; the horizontal pass expands that to the picture's width.
/// Both are the triangle filter the JFIF sample positions ask for: a chroma
/// sample sits at the centre of the luma samples it covers, so interpolating
/// at a luma position gives 3/4 of the nearer sample and 1/4 of the one on
/// the other side, and at the edges the two collapse onto the same sample.
#[allow(clippy::too_many_arguments)]
fn sample_row(
    plane: &[u8],
    stride: usize,
    comp_w: usize,
    comp_h: usize,
    half_x: bool,
    half_y: bool,
    y: usize,
    scratch: &mut [i32],
    out: &mut [u8],
) {
    let near_y = if half_y {
        (y / 2).min(comp_h - 1)
    } else {
        y.min(comp_h - 1)
    };
    let far_y = if !half_y {
        near_y
    } else if y % 2 == 0 {
        near_y.saturating_sub(1)
    } else {
        (near_y + 1).min(comp_h - 1)
    };

    let near = &plane[near_y * stride..near_y * stride + comp_w];
    if near_y == far_y {
        for (slot, &sample) in scratch[..comp_w].iter_mut().zip(near) {
            *slot = sample as i32 * 4;
        }
    } else {
        let far = &plane[far_y * stride..far_y * stride + comp_w];
        for ((slot, &a), &b) in scratch[..comp_w].iter_mut().zip(near).zip(far) {
            *slot = a as i32 * 3 + b as i32;
        }
    }

    if half_x {
        let last = comp_w - 1;
        for (x, slot) in out.iter_mut().enumerate() {
            let cx = (x / 2).min(last);
            let other = if x % 2 == 0 {
                cx.saturating_sub(1)
            } else {
                (cx + 1).min(last)
            };
            *slot = ((3 * scratch[cx] + scratch[other] + 8) >> 4) as u8;
        }
    } else {
        let last = comp_w - 1;
        for (x, slot) in out.iter_mut().enumerate() {
            *slot = ((scratch[x.min(last)] + 2) >> 2) as u8;
        }
    }
}

/// Planes to pixels: upsample the chroma, apply the colour transform, and cut
/// the MCU padding off the right and bottom edges.
fn to_rgb(frame: &Frame, planes: &[Vec<u8>]) -> Image {
    let width = frame.width as usize;
    let height = frame.height as usize;
    let mut rgb = vec![0u8; width * height * 3];

    if frame.components.len() == 1 {
        // Grayscale: the one plane is the picture, padding aside.
        let component = frame.components[0];
        let stride = component.blocks_w * 8;
        for (y, row) in rgb.chunks_exact_mut(width * 3).enumerate() {
            let source = &planes[0][y * stride..y * stride + width];
            for (pixel, &grey) in row.chunks_exact_mut(3).zip(source) {
                pixel[0] = grey;
                pixel[1] = grey;
                pixel[2] = grey;
            }
        }
        return Image {
            width: frame.width,
            height: frame.height,
            rgb,
        };
    }

    let colour = Colour::new();
    let mut luma = vec![0u8; width];
    let mut cb = vec![0u8; width];
    let mut cr = vec![0u8; width];
    let mut scratch = vec![0i32; width + 2];

    for (y, row) in rgb.chunks_exact_mut(width * 3).enumerate() {
        for (index, target) in [&mut luma, &mut cb, &mut cr].into_iter().enumerate() {
            let component = frame.components[index];
            sample_row(
                &planes[index],
                component.blocks_w * 8,
                component.width,
                component.height,
                component.h < frame.h_max,
                component.v < frame.v_max,
                y,
                &mut scratch,
                target,
            );
        }
        for (((pixel, &y), &cb), &cr) in row
            .chunks_exact_mut(3)
            .zip(luma.iter())
            .zip(cb.iter())
            .zip(cr.iter())
        {
            let y = y as i32;
            let green = y + ((colour.g_cb[cb as usize] + colour.g_cr[cr as usize] + 32768) >> 16);
            pixel[0] = (y + colour.r_cr[cr as usize]).clamp(0, 255) as u8;
            pixel[1] = green.clamp(0, 255) as u8;
            pixel[2] = (y + colour.b_cb[cb as usize]).clamp(0, 255) as u8;
        }
    }

    Image {
        width: frame.width,
        height: frame.height,
        rgb,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The zig-zag is a permutation of the block, and its first and last
    /// entries are the two corners.
    #[test]
    fn the_zigzag_visits_every_coefficient_once() {
        let mut seen = [false; 64];
        for &index in &ZIGZAG {
            assert!(!seen[index], "{index} twice");
            seen[index] = true;
        }
        assert!(seen.iter().all(|&s| s));
        assert_eq!(ZIGZAG[0], 0);
        assert_eq!(ZIGZAG[63], 63);
        assert_eq!(ZIGZAG[1], 1, "along the top before down the side");
        assert_eq!(ZIGZAG[2], 8);
    }

    /// A block with only a DC coefficient is a flat square, and the shortcut
    /// that says so has to agree with the transform that does not take it.
    #[test]
    fn a_flat_block_comes_out_flat_either_way() {
        for level in [-1016i32, -128, 0, 8, 127, 1016] {
            let mut block = [0i32; 64];
            block[0] = level;
            let mut shortcut = [0u8; 64];
            idct_into(&block, false, &mut shortcut, 8, 0, 0);

            // The long way round, forced by a zero coefficient that is
            // nevertheless "nonzero" as far as the flag is concerned.
            let mut long = [0u8; 64];
            idct_into(&block, true, &mut long, 8, 0, 0);
            assert_eq!(shortcut, long, "level {level}");
            assert!(shortcut.iter().all(|&s| s == shortcut[0]));
        }
        // And the level shift lands where T.81 says: a zero block is mid grey.
        let mut block = [0i32; 64];
        let mut out = [0u8; 64];
        idct_into(&block, false, &mut out, 8, 0, 0);
        assert_eq!(out[0], 128);
        // 8 * (v - 128) is the DC coefficient of a flat block of value v.
        block[0] = 8 * (200 - 128);
        idct_into(&block, false, &mut out, 8, 0, 0);
        assert_eq!(out[0], 200);
    }

    /// The transform against the definition it came from, summed in floating
    /// point: T.81 A.3.3, with no shortcuts and no fixed point.
    #[test]
    fn the_fixed_point_transform_tracks_the_real_one() {
        fn reference(block: &[i32; 64]) -> [f64; 64] {
            let mut out = [0.0; 64];
            for y in 0..8 {
                for x in 0..8 {
                    let mut sum = 0.0;
                    for v in 0..8 {
                        for u in 0..8 {
                            let cu = if u == 0 { 1.0 / 2f64.sqrt() } else { 1.0 };
                            let cv = if v == 0 { 1.0 / 2f64.sqrt() } else { 1.0 };
                            sum += cu
                                * cv
                                * block[v * 8 + u] as f64
                                * (((2 * x + 1) as f64 * u as f64 * std::f64::consts::PI) / 16.0)
                                    .cos()
                                * (((2 * y + 1) as f64 * v as f64 * std::f64::consts::PI) / 16.0)
                                    .cos();
                        }
                    }
                    out[y * 8 + x] = sum / 4.0 + 128.0;
                }
            }
            out
        }

        // A deterministic spread of coefficients, including negatives and the
        // high frequencies that the odd half of the butterfly carries.
        let mut state = 0x243f_6a88u32;
        for _ in 0..64 {
            let mut block = [0i32; 64];
            for slot in block.iter_mut() {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                // Coefficients of the size a real 8-bit block produces.
                *slot = (state >> 20) as i32 % 121 - 60;
            }
            block[0] = (state >> 8) as i32 % 2048 - 1024;

            let mut got = [0u8; 64];
            idct_into(&block, true, &mut got, 8, 0, 0);
            let want = reference(&block);
            for (index, (&got, want)) in got.iter().zip(want).enumerate() {
                let want = want.clamp(0.0, 255.0);
                assert!(
                    (got as f64 - want).abs() <= 1.0,
                    "sample {index}: {got} against {want:.3}"
                );
            }
        }
    }

    /// A file that is not one, and a file that stops.
    #[test]
    fn nonsense_is_refused_rather_than_guessed_at() {
        assert_eq!(decode(b"", usize::MAX), Err(JpegError::NotJpeg));
        assert_eq!(
            decode(b"\x89PNG\r\n\x1a\n", usize::MAX),
            Err(JpegError::NotJpeg)
        );
        assert_eq!(decode(b"\xff\xd8", usize::MAX), Err(JpegError::Truncated));
        // SOI then a frame header that says it is progressive.
        assert_eq!(
            decode(b"\xff\xd8\xff\xc2\x00\x08", usize::MAX),
            Err(JpegError::Progressive)
        );
        // SOF9: arithmetic coding.
        assert_eq!(
            decode(b"\xff\xd8\xff\xc9\x00\x08", usize::MAX),
            Err(JpegError::Arithmetic)
        );
    }

    /// An over-subscribed Huffman table is a table, not a loop.
    #[test]
    fn a_huffman_table_with_too_many_codes_is_refused() {
        // Three codes of length one: one more than a binary tree has room for.
        let mut counts = [0u8; 16];
        counts[0] = 3;
        assert_eq!(
            HuffTable::build(&counts, vec![1, 2, 3]).err(),
            Some(JpegError::BadHuffmanTable)
        );
        // Two is exactly right.
        counts[0] = 2;
        assert!(HuffTable::build(&counts, vec![1, 2]).is_ok());
    }

    /// The magnitude extension of T.81 F.12, at both ends of a few sizes.
    #[test]
    fn magnitude_bits_extend_into_the_range_they_encode() {
        fn extended(size: u32, raw: u32) -> i32 {
            // The bits as they would arrive, most significant first.
            let byte = ((raw << (8 - size)) & 0xff) as u8;
            let data = [byte, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xd9];
            let mut bits = Bits::new(&data, 0);
            bits.receive_extend(size)
        }
        assert_eq!(extended(1, 0), -1);
        assert_eq!(extended(1, 1), 1);
        assert_eq!(extended(2, 0), -3);
        assert_eq!(extended(2, 1), -2);
        assert_eq!(extended(2, 2), 2);
        assert_eq!(extended(2, 3), 3);
        assert_eq!(extended(4, 0), -15);
        assert_eq!(extended(4, 7), -8);
        assert_eq!(extended(4, 8), 8);
        assert_eq!(extended(4, 15), 15);
    }

    /// 0xFF 0x00 is a data byte; 0xFF anything else is the end of the data.
    #[test]
    fn byte_stuffing_is_undone_and_a_marker_stops_the_reader() {
        let data = [0xAA, 0xFF, 0x00, 0x55, 0xFF, 0xD9];
        let mut bits = Bits::new(&data, 0);
        assert_eq!(bits.take(8), 0xAA);
        assert_eq!(bits.take(8), 0xFF, "the stuffed zero is not a byte");
        assert_eq!(bits.take(8), 0x55);
        // Past the marker the window is zeros, and nothing was consumed past
        // the 0xFF the marker starts with.
        assert_eq!(bits.take(8), 0);
        assert!(bits.at_marker);
        assert!(!bits.ran_out, "a marker is not the end of the file");
        assert_eq!(bits.data[bits.pos], 0xFF);

        // Data that simply stops is a different thing, and is remembered.
        let data = [0xAA];
        let mut bits = Bits::new(&data, 0);
        assert_eq!(bits.take(8), 0xAA);
        assert_eq!(bits.take(8), 0);
        assert!(bits.ran_out);
    }

    /// A restart marker is found from wherever the window left the position.
    #[test]
    fn a_restart_marker_resynchronises_the_reader() {
        let data = [0x12, 0x34, 0xFF, 0xD0, 0x56, 0xFF, 0xD9];
        let mut bits = Bits::new(&data, 0);
        assert_eq!(bits.take(4), 1);
        assert!(bits.restart(), "the RST0 is there to be found");
        assert_eq!(bits.take(8), 0x56);
        // And an EOI where a restart should be is not a restart.
        let mut bits = Bits::new(&data, 4);
        assert!(!bits.restart());
    }
}
