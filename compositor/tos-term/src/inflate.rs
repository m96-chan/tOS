//! DEFLATE (RFC 1951) and zlib (RFC 1950) decompression.
//!
//! Image previews arrive through the graphics protocol as PNG, and PNG pixel
//! data is always zlib-compressed; `o=z` payloads are compressed a second
//! time. Both paths land here. The whole terminal is written without
//! dependencies, so this is a from-scratch decoder rather than a binding to
//! the system zlib.
//!
//! Everything here parses bytes an application chose, so the rules are: never
//! index without a bound, never trust a length, and never allocate more than
//! the caller's limit. The limit is checked *before* each append rather than
//! after the stream finishes, because the point of a decompression bomb is
//! that the output is enormous long before the input runs out.

/// Why a stream could not be decompressed.
///
/// The variants exist so the caller can tell "this is too big" — worth a
/// distinct message to the application — from "this is corrupt".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InflateError {
    /// The stream ended in the middle of something.
    Truncated,
    /// Block type 3, which RFC 1951 reserves.
    BadBlockType,
    /// A stored block's length did not match its one's complement.
    BadStoredLength,
    /// An over-subscribed code table, or a code that decodes to nothing.
    BadHuffmanCode,
    /// A back-reference pointing before the start of the output.
    BadDistance,
    /// The output grew past the caller's limit.
    TooLarge,
    /// Not a zlib stream, or one this decoder cannot read.
    BadZlibHeader,
    /// A preset dictionary, which nothing in the graphics path ever uses.
    PresetDictionary,
    /// The Adler-32 trailer did not match the data.
    BadChecksum,
}

impl std::fmt::Display for InflateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            InflateError::Truncated => "truncated deflate stream",
            InflateError::BadBlockType => "reserved deflate block type",
            InflateError::BadStoredLength => "corrupt stored block length",
            InflateError::BadHuffmanCode => "corrupt huffman code",
            InflateError::BadDistance => "back-reference before start of output",
            InflateError::TooLarge => "decompressed data exceeds the limit",
            InflateError::BadZlibHeader => "not a zlib stream",
            InflateError::PresetDictionary => "zlib preset dictionary not supported",
            InflateError::BadChecksum => "zlib checksum mismatch",
        };
        f.write_str(text)
    }
}

impl std::error::Error for InflateError {}

/// Decompress a zlib stream (RFC 1950): two header bytes, a deflate stream,
/// and an Adler-32 of the uncompressed data.
///
/// `limit` is the largest output accepted; anything beyond it is
/// [`InflateError::TooLarge`] rather than an allocation.
pub fn zlib_decompress(data: &[u8], limit: usize) -> Result<Vec<u8>, InflateError> {
    let (&cmf, &flg) = match (data.first(), data.get(1)) {
        (Some(a), Some(b)) => (a, b),
        _ => return Err(InflateError::Truncated),
    };
    // CM must be 8 (deflate) and CINFO at most 7 (a 32 KiB window). The two
    // header bytes read as a big-endian number are a multiple of 31.
    if cmf & 0x0f != 8 || cmf >> 4 > 7 {
        return Err(InflateError::BadZlibHeader);
    }
    if (((cmf as u32) << 8) | flg as u32) % 31 != 0 {
        return Err(InflateError::BadZlibHeader);
    }
    if flg & 0x20 != 0 {
        return Err(InflateError::PresetDictionary);
    }

    let mut reader = BitReader::new(&data[2..]);
    let mut out = Vec::new();
    inflate_into(&mut reader, &mut out, limit)?;

    // The trailer starts at the next byte boundary after the final block.
    reader.align();
    let end = 2 + reader.byte_position();
    let trailer = data.get(end..end + 4).ok_or(InflateError::Truncated)?;
    let expected = u32::from_be_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]);
    if adler32(&out) != expected {
        return Err(InflateError::BadChecksum);
    }
    Ok(out)
}

/// Decompress a bare deflate stream, with no zlib wrapper or checksum.
pub fn inflate(data: &[u8], limit: usize) -> Result<Vec<u8>, InflateError> {
    let mut reader = BitReader::new(data);
    let mut out = Vec::new();
    inflate_into(&mut reader, &mut out, limit)?;
    Ok(out)
}

/// Adler-32, the checksum zlib puts after the compressed data.
pub fn adler32(data: &[u8]) -> u32 {
    const MODULUS: u32 = 65521;
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    // 5552 is the most bytes that can be summed before `b` could overflow.
    for chunk in data.chunks(5552) {
        for &byte in chunk {
            a += byte as u32;
            b += a;
        }
        a %= MODULUS;
        b %= MODULUS;
    }
    (b << 16) | a
}

// ---------------------------------------------------------------------------
// Bit reader
// ---------------------------------------------------------------------------

/// Reads deflate's least-significant-bit-first bit stream.
struct BitReader<'a> {
    data: &'a [u8],
    /// Index of the byte holding the next bit.
    pos: usize,
    /// Bits already consumed from that byte, 0..8.
    bit: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        BitReader {
            data,
            pos: 0,
            bit: 0,
        }
    }

    /// Bytes consumed so far. Only meaningful just after [`BitReader::align`].
    fn byte_position(&self) -> usize {
        self.pos
    }

    fn bit(&mut self) -> Result<u32, InflateError> {
        let byte = *self.data.get(self.pos).ok_or(InflateError::Truncated)?;
        let value = (byte >> self.bit) & 1;
        self.bit += 1;
        if self.bit == 8 {
            self.bit = 0;
            self.pos += 1;
        }
        Ok(value as u32)
    }

    /// Read `n` bits, for `n` up to 24.
    fn bits(&mut self, n: u32) -> Result<u32, InflateError> {
        if n == 0 {
            return Ok(0);
        }
        // Four bytes always cover up to 7 leftover bits plus 24 more.
        if let Some(window) = self.data.get(self.pos..self.pos + 4) {
            let word = u32::from_le_bytes([window[0], window[1], window[2], window[3]]);
            let value = (word >> self.bit) & ((1u32 << n) - 1);
            let total = self.bit + n;
            self.pos += (total / 8) as usize;
            self.bit = total % 8;
            return Ok(value);
        }
        // Near the end of the input there may be no four bytes left to read.
        let mut value = 0;
        for i in 0..n {
            value |= self.bit()? << i;
        }
        Ok(value)
    }

    /// Skip to the next byte boundary, as stored blocks and the zlib trailer
    /// require.
    fn align(&mut self) {
        if self.bit != 0 {
            self.bit = 0;
            self.pos += 1;
        }
    }

    /// Take `n` whole bytes, for stored blocks.
    fn bytes(&mut self, n: usize) -> Result<&'a [u8], InflateError> {
        let taken = self
            .data
            .get(self.pos..self.pos + n)
            .ok_or(InflateError::Truncated)?;
        self.pos += n;
        Ok(taken)
    }
}

// ---------------------------------------------------------------------------
// Huffman decoding
// ---------------------------------------------------------------------------

/// Deflate never uses a code longer than 15 bits.
const MAX_BITS: usize = 15;

/// A canonical Huffman table, held as the number of codes of each length plus
/// the symbols in canonical order.
///
/// Decoding walks one bit at a time and compares against the first code of
/// each length. That is slower than a lookup table but needs no allocation
/// beyond the symbol list and has no table into which a malformed stream can
/// index, which matters more here than throughput.
struct Huffman {
    counts: [u16; MAX_BITS + 1],
    symbols: Vec<u16>,
}

impl Huffman {
    /// Build a table from a list of code lengths, one per symbol.
    fn new(lengths: &[u8]) -> Result<Huffman, InflateError> {
        let mut counts = [0u16; MAX_BITS + 1];
        for &length in lengths {
            if length as usize > MAX_BITS {
                return Err(InflateError::BadHuffmanCode);
            }
            counts[length as usize] += 1;
        }
        counts[0] = 0;

        // An over-subscribed table describes codes that overlap, so it can
        // never be decoded unambiguously. An *under*-subscribed one is legal
        // — a distance tree with a single code is the common case — and any
        // hole in it simply fails to decode if the stream reaches for it.
        let mut left: i32 = 1;
        for &count in counts.iter().skip(1) {
            left <<= 1;
            left -= count as i32;
            if left < 0 {
                return Err(InflateError::BadHuffmanCode);
            }
        }

        let mut offsets = [0u16; MAX_BITS + 2];
        for length in 1..=MAX_BITS {
            offsets[length + 1] = offsets[length] + counts[length];
        }
        let mut symbols = vec![0u16; lengths.iter().filter(|&&l| l != 0).count()];
        for (symbol, &length) in lengths.iter().enumerate() {
            if length != 0 {
                let slot = offsets[length as usize] as usize;
                symbols[slot] = symbol as u16;
                offsets[length as usize] += 1;
            }
        }
        Ok(Huffman { counts, symbols })
    }

    fn decode(&self, reader: &mut BitReader) -> Result<u16, InflateError> {
        // `code` is the bits read so far, `first` the first code of the
        // current length, and `index` where that length's symbols begin.
        let mut code: i32 = 0;
        let mut first: i32 = 0;
        let mut index: i32 = 0;
        for length in 1..=MAX_BITS {
            code |= reader.bit()? as i32;
            let count = self.counts[length] as i32;
            if code - count < first {
                let slot = (index + (code - first)) as usize;
                // A hole in an under-subscribed table lands outside the list.
                return self
                    .symbols
                    .get(slot)
                    .copied()
                    .ok_or(InflateError::BadHuffmanCode);
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err(InflateError::BadHuffmanCode)
    }
}

/// Length codes 257..=285: the base length each one stands for.
const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
/// Extra bits read after each length code.
const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
/// Distance codes 0..=29.
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049,
    3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
/// Extra bits read after each distance code.
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13,
];
/// The order in which a dynamic block writes the code-length code lengths.
const CODE_LENGTH_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

// ---------------------------------------------------------------------------
// Block decoding
// ---------------------------------------------------------------------------

fn inflate_into(reader: &mut BitReader, out: &mut Vec<u8>, limit: usize) -> Result<(), InflateError> {
    let (fixed_literals, fixed_distances) = fixed_tables()?;
    loop {
        let last = reader.bits(1)? == 1;
        match reader.bits(2)? {
            0 => stored_block(reader, out, limit)?,
            1 => compressed_block(reader, out, limit, &fixed_literals, &fixed_distances)?,
            2 => {
                let (literals, distances) = dynamic_tables(reader)?;
                compressed_block(reader, out, limit, &literals, &distances)?;
            }
            _ => return Err(InflateError::BadBlockType),
        }
        if last {
            return Ok(());
        }
    }
}

/// The fixed tables of RFC 1951 section 3.2.6. They are built once per stream
/// rather than kept in a static, which costs under a kilobyte and keeps the
/// table construction in one place.
fn fixed_tables() -> Result<(Huffman, Huffman), InflateError> {
    let mut literal_lengths = [0u8; 288];
    for (symbol, length) in literal_lengths.iter_mut().enumerate() {
        *length = match symbol {
            0..=143 => 8,
            144..=255 => 9,
            256..=279 => 7,
            _ => 8,
        };
    }
    let distance_lengths = [5u8; 30];
    Ok((
        Huffman::new(&literal_lengths)?,
        Huffman::new(&distance_lengths)?,
    ))
}

fn stored_block(reader: &mut BitReader, out: &mut Vec<u8>, limit: usize) -> Result<(), InflateError> {
    reader.align();
    let header = reader.bytes(4)?;
    let len = u16::from_le_bytes([header[0], header[1]]) as usize;
    let nlen = u16::from_le_bytes([header[2], header[3]]) as usize;
    if len != !nlen & 0xffff {
        return Err(InflateError::BadStoredLength);
    }
    if out.len() + len > limit {
        return Err(InflateError::TooLarge);
    }
    let data = reader.bytes(len)?;
    out.extend_from_slice(data);
    Ok(())
}

fn compressed_block(
    reader: &mut BitReader,
    out: &mut Vec<u8>,
    limit: usize,
    literals: &Huffman,
    distances: &Huffman,
) -> Result<(), InflateError> {
    loop {
        let symbol = literals.decode(reader)? as usize;
        if symbol < 256 {
            if out.len() + 1 > limit {
                return Err(InflateError::TooLarge);
            }
            out.push(symbol as u8);
            continue;
        }
        if symbol == 256 {
            return Ok(());
        }

        // Symbols 286 and 287 exist in the fixed table but stand for nothing.
        let length_code = symbol - 257;
        let base = *LENGTH_BASE
            .get(length_code)
            .ok_or(InflateError::BadHuffmanCode)?;
        let length = base as usize + reader.bits(LENGTH_EXTRA[length_code] as u32)? as usize;

        let distance_code = distances.decode(reader)? as usize;
        let base = *DIST_BASE
            .get(distance_code)
            .ok_or(InflateError::BadHuffmanCode)?;
        let distance = base as usize + reader.bits(DIST_EXTRA[distance_code] as u32)? as usize;

        // The window is the output itself, so a distance larger than what has
        // been produced would read from before the buffer.
        if distance == 0 || distance > out.len() {
            return Err(InflateError::BadDistance);
        }
        if out.len() + length > limit {
            return Err(InflateError::TooLarge);
        }
        let start = out.len() - distance;
        for i in 0..length {
            // Byte at a time on purpose: an overlapping reference is how
            // deflate encodes a run, so the copy reads bytes it just wrote.
            let byte = out[start + i];
            out.push(byte);
        }
    }
}

/// Read the two code tables a dynamic block carries in front of its data.
fn dynamic_tables(reader: &mut BitReader) -> Result<(Huffman, Huffman), InflateError> {
    let hlit = reader.bits(5)? as usize + 257;
    let hdist = reader.bits(5)? as usize + 1;
    let hclen = reader.bits(4)? as usize + 4;

    let mut code_lengths = [0u8; 19];
    for &slot in CODE_LENGTH_ORDER.iter().take(hclen) {
        code_lengths[slot] = reader.bits(3)? as u8;
    }
    let code_table = Huffman::new(&code_lengths)?;

    // The literal and distance lengths are written as one run, which a repeat
    // code is allowed to span.
    let total = hlit + hdist;
    let mut lengths = vec![0u8; total];
    let mut written = 0;
    while written < total {
        let symbol = code_table.decode(reader)?;
        let (value, repeat) = match symbol {
            0..=15 => {
                lengths[written] = symbol as u8;
                written += 1;
                continue;
            }
            16 => {
                // Repeat the previous length; there has to be one.
                if written == 0 {
                    return Err(InflateError::BadHuffmanCode);
                }
                (lengths[written - 1], 3 + reader.bits(2)? as usize)
            }
            17 => (0, 3 + reader.bits(3)? as usize),
            18 => (0, 11 + reader.bits(7)? as usize),
            _ => return Err(InflateError::BadHuffmanCode),
        };
        if written + repeat > total {
            return Err(InflateError::BadHuffmanCode);
        }
        for slot in &mut lengths[written..written + repeat] {
            *slot = value;
        }
        written += repeat;
    }

    // Without an end-of-block code the block could never finish, so the
    // stream would only stop at the limit or the end of the input.
    if lengths[256] == 0 {
        return Err(InflateError::BadHuffmanCode);
    }
    let literals = Huffman::new(&lengths[..hlit])?;
    let distances = Huffman::new(&lengths[hlit..])?;
    Ok((literals, distances))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Wrap raw bytes in a zlib stream of stored deflate blocks, which is the
    /// one compressed form that can be written without a Huffman encoder.
    /// `png` builds its fixtures with this too.
    pub(crate) fn zlib_stored(data: &[u8]) -> Vec<u8> {
        let mut out = vec![0x78, 0x01];
        let mut chunks: Vec<&[u8]> = data.chunks(0xffff).collect();
        if chunks.is_empty() {
            chunks.push(&[]);
        }
        let last = chunks.len() - 1;
        for (i, chunk) in chunks.iter().enumerate() {
            out.push(if i == last { 1 } else { 0 });
            let len = chunk.len() as u16;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&(!len).to_le_bytes());
            out.extend_from_slice(chunk);
        }
        out.extend_from_slice(&adler32(data).to_be_bytes());
        out
    }

    /// `zlib.compress(b"hello hello hello world", 9)` as CPython produces it,
    /// kept as a literal so the dynamic-Huffman path is tested against an
    /// encoder this crate did not write.
    const CPYTHON_HELLO: [u8; 21] = [
        120, 218, 203, 72, 205, 201, 201, 87, 200, 64, 34, 203, 243, 139, 114, 82, 0, 104, 125, 8,
        197,
    ];

    #[test]
    fn a_stored_block_round_trips() {
        let data = b"the quick brown fox".to_vec();
        let stream = zlib_stored(&data);
        assert_eq!(zlib_decompress(&stream, 4096).unwrap(), data);
    }

    #[test]
    fn an_empty_stream_round_trips() {
        let stream = zlib_stored(&[]);
        assert_eq!(zlib_decompress(&stream, 4096).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn a_multi_block_stored_stream_round_trips() {
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let stream = zlib_stored(&data);
        assert_eq!(zlib_decompress(&stream, 1 << 20).unwrap(), data);
    }

    #[test]
    fn a_dynamic_huffman_stream_from_another_encoder_decodes() {
        let out = zlib_decompress(&CPYTHON_HELLO, 4096).unwrap();
        assert_eq!(out, b"hello hello hello world");
    }

    #[test]
    fn a_fixed_huffman_block_with_a_back_reference_decodes() {
        // Hand-assembled: final fixed block, literals 'a' and 'b', then a
        // length-5 distance-2 reference, then end of block.
        let mut bits = BitWriter::default();
        bits.write(1, 1); // BFINAL
        bits.write(1, 2); // BTYPE = fixed
        fixed_literal(&mut bits, b'a' as u16);
        fixed_literal(&mut bits, b'b' as u16);
        fixed_literal(&mut bits, 259); // length code for 5
        bits.write_msb(1, 5); // distance code 1 => distance 2
        fixed_literal(&mut bits, 256);
        assert_eq!(inflate(&bits.finish(), 64).unwrap(), b"abababa".to_vec());
    }

    #[test]
    fn an_overlapping_back_reference_repeats_the_window() {
        let mut bits = BitWriter::default();
        bits.write(1, 1);
        bits.write(1, 2);
        fixed_literal(&mut bits, b'x' as u16);
        fixed_literal(&mut bits, 264); // length 10, no extra bits
        bits.write_msb(0, 5); // distance 1
        fixed_literal(&mut bits, 256);
        assert_eq!(inflate(&bits.finish(), 64).unwrap(), b"xxxxxxxxxxx".to_vec());
    }

    #[test]
    fn a_distance_before_the_start_of_the_output_is_an_error() {
        let mut bits = BitWriter::default();
        bits.write(1, 1);
        bits.write(1, 2);
        fixed_literal(&mut bits, b'x' as u16);
        fixed_literal(&mut bits, 257); // length 3
        bits.write_msb(5, 5); // distance code 5, base 7, past the output
        bits.write(0, 1); // the extra bit that code carries
        fixed_literal(&mut bits, 256);
        assert_eq!(inflate(&bits.finish(), 64), Err(InflateError::BadDistance));
    }

    #[test]
    fn the_reserved_block_type_is_an_error() {
        let mut bits = BitWriter::default();
        bits.write(1, 1);
        bits.write(3, 2);
        assert_eq!(inflate(&bits.finish(), 64), Err(InflateError::BadBlockType));
    }

    #[test]
    fn a_stored_block_with_a_bad_complement_is_an_error() {
        let mut stream = zlib_stored(b"abcd");
        stream[5] ^= 0xff; // corrupt NLEN
        assert_eq!(
            zlib_decompress(&stream, 4096),
            Err(InflateError::BadStoredLength)
        );
    }

    #[test]
    fn a_bad_zlib_header_is_rejected() {
        assert_eq!(
            zlib_decompress(&[0x00, 0x00], 4096),
            Err(InflateError::BadZlibHeader)
        );
        // A window larger than 32 KiB, which no zlib stream may ask for.
        assert_eq!(
            zlib_decompress(&[0x88, 0x1d], 4096),
            Err(InflateError::BadZlibHeader)
        );
    }

    #[test]
    fn a_preset_dictionary_is_rejected() {
        // 0x78 0x3f: FDICT set, check bits still valid.
        assert_eq!(
            zlib_decompress(&[0x78, 0x3f], 4096),
            Err(InflateError::PresetDictionary)
        );
    }

    #[test]
    fn a_corrupt_checksum_is_detected() {
        let mut stream = zlib_stored(b"abcd");
        let last = stream.len() - 1;
        stream[last] ^= 0xff;
        assert_eq!(zlib_decompress(&stream, 4096), Err(InflateError::BadChecksum));
    }

    #[test]
    fn the_limit_stops_a_decompression_bomb_before_it_allocates() {
        // A megabyte of zeroes, refused when the caller can only take 1 KiB.
        let data = vec![0u8; 1 << 20];
        let stream = zlib_stored(&data);
        assert_eq!(zlib_decompress(&stream, 1024), Err(InflateError::TooLarge));
        // The same stream is fine when the caller can afford it.
        assert_eq!(zlib_decompress(&stream, 1 << 20).unwrap().len(), 1 << 20);
    }

    #[test]
    fn a_back_reference_bomb_is_also_bounded() {
        // One literal, then maximum-length runs that would expand for ever.
        let mut bits = BitWriter::default();
        bits.write(0, 1);
        bits.write(1, 2);
        fixed_literal(&mut bits, b'x' as u16);
        for _ in 0..100 {
            fixed_literal(&mut bits, 285); // length 258
            bits.write_msb(0, 5); // distance 1
        }
        fixed_literal(&mut bits, 256);
        assert_eq!(inflate(&bits.finish(), 512), Err(InflateError::TooLarge));
    }

    #[test]
    fn truncating_a_stream_anywhere_is_an_error_not_a_panic() {
        let full = zlib_stored(b"the quick brown fox jumps over the lazy dog");
        for cut in 0..full.len() {
            assert!(zlib_decompress(&full[..cut], 4096).is_err());
        }
        for cut in 0..CPYTHON_HELLO.len() {
            assert!(zlib_decompress(&CPYTHON_HELLO[..cut], 4096).is_err());
        }
    }

    #[test]
    fn corrupting_any_bit_of_a_stream_never_panics() {
        for byte in 0..CPYTHON_HELLO.len() {
            for bit in 0..8 {
                let mut damaged = CPYTHON_HELLO;
                damaged[byte] ^= 1 << bit;
                // Some flips still describe a valid stream; the point is that
                // none of them panic or run away.
                let _ = zlib_decompress(&damaged, 1 << 16);
            }
        }
    }

    #[test]
    fn adler32_matches_known_values() {
        assert_eq!(adler32(b""), 1);
        assert_eq!(adler32(b"a"), 0x0062_0062);
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }

    // -------------------------------------------------------------------
    // Test-only bit writer, enough to hand-assemble fixed-Huffman blocks.
    // -------------------------------------------------------------------

    #[derive(Default)]
    struct BitWriter {
        out: Vec<u8>,
        bit: u32,
    }

    impl BitWriter {
        /// Write `n` bits least-significant first, as deflate headers are.
        fn write(&mut self, value: u32, n: u32) {
            for i in 0..n {
                self.push_bit((value >> i) & 1);
            }
        }

        /// Write `n` bits most-significant first, as Huffman codes are.
        fn write_msb(&mut self, value: u32, n: u32) {
            for i in (0..n).rev() {
                self.push_bit((value >> i) & 1);
            }
        }

        fn push_bit(&mut self, bit: u32) {
            if self.bit == 0 {
                self.out.push(0);
            }
            let last = self.out.len() - 1;
            self.out[last] |= (bit as u8) << self.bit;
            self.bit = (self.bit + 1) % 8;
        }

        fn finish(self) -> Vec<u8> {
            self.out
        }
    }

    /// Emit one symbol using the fixed literal/length code of RFC 1951.
    fn fixed_literal(bits: &mut BitWriter, symbol: u16) {
        match symbol {
            0..=143 => bits.write_msb(0x30 + symbol as u32, 8),
            144..=255 => bits.write_msb(0x190 + symbol as u32 - 144, 9),
            256..=279 => bits.write_msb(symbol as u32 - 256, 7),
            _ => bits.write_msb(0xc0 + symbol as u32 - 280, 8),
        }
    }
}
