//! SHA-512, as specified in FIPS 180-4.
//!
//! This exists because [`crate::sha512crypt`] needs it, not because tOS wants
//! a hash function. It is about a hundred lines of shifts and additions over
//! a fixed table of constants, it has no I/O and no state beyond the block it
//! is filling, and NIST publishes vectors for it — which is the whole reason
//! writing it by hand is a defensible thing to do. `tests/vectors.rs` runs
//! those vectors, including every message length either side of a padding
//! boundary, so a mistyped constant cannot survive a test run.
//!
//! The only subtle part is the padding: a message whose last block has more
//! than 111 bytes in it does not leave room for the sixteen-byte length, so
//! that block is flushed and the length goes into a further one. Getting that
//! wrong produces a hash that is right for almost every input, which is why
//! the vectors below are chosen at 111, 112, 119, 120, 127 and 128 bytes
//! rather than at round numbers.

/// The initial hash value: the fractional parts of the square roots of the
/// first eight primes (FIPS 180-4 section 5.3.5).
const H0: [u64; 8] = [
    0x6a09e667f3bcc908,
    0xbb67ae8584caa73b,
    0x3c6ef372fe94f82b,
    0xa54ff53a5f1d36f1,
    0x510e527fade682d1,
    0x9b05688c2b3e6c1f,
    0x1f83d9abfb41bd6b,
    0x5be0cd19137e2179,
];

/// The round constants: the fractional parts of the cube roots of the first
/// eighty primes (FIPS 180-4 section 4.2.3).
const K: [u64; 80] = [
    0x428a2f98d728ae22,
    0x7137449123ef65cd,
    0xb5c0fbcfec4d3b2f,
    0xe9b5dba58189dbbc,
    0x3956c25bf348b538,
    0x59f111f1b605d019,
    0x923f82a4af194f9b,
    0xab1c5ed5da6d8118,
    0xd807aa98a3030242,
    0x12835b0145706fbe,
    0x243185be4ee4b28c,
    0x550c7dc3d5ffb4e2,
    0x72be5d74f27b896f,
    0x80deb1fe3b1696b1,
    0x9bdc06a725c71235,
    0xc19bf174cf692694,
    0xe49b69c19ef14ad2,
    0xefbe4786384f25e3,
    0x0fc19dc68b8cd5b5,
    0x240ca1cc77ac9c65,
    0x2de92c6f592b0275,
    0x4a7484aa6ea6e483,
    0x5cb0a9dcbd41fbd4,
    0x76f988da831153b5,
    0x983e5152ee66dfab,
    0xa831c66d2db43210,
    0xb00327c898fb213f,
    0xbf597fc7beef0ee4,
    0xc6e00bf33da88fc2,
    0xd5a79147930aa725,
    0x06ca6351e003826f,
    0x142929670a0e6e70,
    0x27b70a8546d22ffc,
    0x2e1b21385c26c926,
    0x4d2c6dfc5ac42aed,
    0x53380d139d95b3df,
    0x650a73548baf63de,
    0x766a0abb3c77b2a8,
    0x81c2c92e47edaee6,
    0x92722c851482353b,
    0xa2bfe8a14cf10364,
    0xa81a664bbc423001,
    0xc24b8b70d0f89791,
    0xc76c51a30654be30,
    0xd192e819d6ef5218,
    0xd69906245565a910,
    0xf40e35855771202a,
    0x106aa07032bbd1b8,
    0x19a4c116b8d2d0c8,
    0x1e376c085141ab53,
    0x2748774cdf8eeb99,
    0x34b0bcb5e19b48a8,
    0x391c0cb3c5c95a63,
    0x4ed8aa4ae3418acb,
    0x5b9cca4f7763e373,
    0x682e6ff3d6b2b8a3,
    0x748f82ee5defb2fc,
    0x78a5636f43172f60,
    0x84c87814a1f0ab72,
    0x8cc702081a6439ec,
    0x90befffa23631e28,
    0xa4506cebde82bde9,
    0xbef9a3f7b2c67915,
    0xc67178f2e372532b,
    0xca273eceea26619c,
    0xd186b8c721c0c207,
    0xeada7dd6cde0eb1e,
    0xf57d4f7fee6ed178,
    0x06f067aa72176fba,
    0x0a637dc5a2c898a6,
    0x113f9804bef90dae,
    0x1b710b35131c471b,
    0x28db77f523047d84,
    0x32caab7b40c72493,
    0x3c9ebe0a15c9bebc,
    0x431d67c49c100d4c,
    0x4cc5d4becb3e42b6,
    0x597f299cfc657e2a,
    0x5fcb6fab3ad6faec,
    0x6c44198c4a475817,
];

/// The size of the block SHA-512 compresses, in bytes.
pub const BLOCK_LEN: usize = 128;

/// The size of a SHA-512 digest, in bytes.
pub const DIGEST_LEN: usize = 64;

/// A SHA-512 computation in progress.
///
/// The crypt scheme feeds a digest in many small pieces — a password here, a
/// salt there, the same digest twice — so this is an incremental interface
/// rather than a single function over one slice. [`sha512`] is the one-shot
/// convenience on top of it.
#[derive(Clone)]
pub struct Sha512 {
    /// The eight chaining words.
    h: [u64; 8],
    /// Bytes that have arrived but do not yet make a whole block.
    block: [u8; BLOCK_LEN],
    /// How many of them there are; always less than a full block, because a
    /// block that fills is compressed immediately.
    used: usize,
    /// The message length in bytes, which becomes the padded tail.
    len: u128,
}

impl Default for Sha512 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha512 {
    /// Begin hashing.
    pub fn new() -> Self {
        Sha512 {
            h: H0,
            block: [0; BLOCK_LEN],
            used: 0,
            len: 0,
        }
    }

    /// Add bytes to the message.
    ///
    /// Splitting a message across calls never changes the digest — that is
    /// the property the crypt scheme leans on for all of its assembling.
    pub fn update(&mut self, mut data: &[u8]) {
        self.len = self.len.wrapping_add(data.len() as u128);

        if self.used > 0 {
            let take = (BLOCK_LEN - self.used).min(data.len());
            self.block[self.used..self.used + take].copy_from_slice(&data[..take]);
            self.used += take;
            data = &data[take..];
            if self.used < BLOCK_LEN {
                // Everything handed over is queued and there is still not a
                // whole block. Returning here rather than falling through is
                // what keeps the queued bytes: the tail handling below
                // assumes it starts from an empty buffer.
                return;
            }
            Self::compress(&mut self.h, &self.block);
            self.used = 0;
        }

        let mut chunks = data.chunks_exact(BLOCK_LEN);
        for chunk in &mut chunks {
            Self::compress(&mut self.h, chunk);
        }

        let tail = chunks.remainder();
        self.block[..tail.len()].copy_from_slice(tail);
        self.used = tail.len();
    }

    /// Pad the message and produce the digest.
    ///
    /// Padding is a `0x80` byte, then zeros, then the message length in bits
    /// as a 128-bit big-endian number in the last sixteen bytes of the last
    /// block. If the `0x80` and the zeros cannot reach that far without
    /// colliding with the length, one more block is compressed first.
    pub fn finalize(mut self) -> [u8; DIGEST_LEN] {
        let bits = self.len << 3;

        self.block[self.used] = 0x80;
        self.used += 1;
        if self.used > BLOCK_LEN - 16 {
            self.block[self.used..].fill(0);
            Self::compress(&mut self.h, &self.block);
            self.used = 0;
        }
        self.block[self.used..BLOCK_LEN - 16].fill(0);
        self.block[BLOCK_LEN - 16..].copy_from_slice(&bits.to_be_bytes());
        Self::compress(&mut self.h, &self.block);

        let mut out = [0u8; DIGEST_LEN];
        for (word, slot) in self.h.iter().zip(out.chunks_exact_mut(8)) {
            slot.copy_from_slice(&word.to_be_bytes());
        }
        out
    }

    /// The compression function of FIPS 180-4 section 6.4.2.
    ///
    /// Taken as an associated function rather than a method so the caller can
    /// hand it `self.h` and `self.block` at once; they are different fields,
    /// so the borrow checker is happy and no copy is needed.
    fn compress(h: &mut [u64; 8], block: &[u8]) {
        debug_assert_eq!(block.len(), BLOCK_LEN);

        // The message schedule: sixteen words read out of the block, then
        // sixty-four more derived from them.
        let mut w = [0u64; 80];
        for (word, bytes) in w[..16].iter_mut().zip(block.chunks_exact(8)) {
            let mut be = [0u8; 8];
            be.copy_from_slice(bytes);
            *word = u64::from_be_bytes(be);
        }
        for i in 16..80 {
            let s0 = w[i - 15].rotate_right(1) ^ w[i - 15].rotate_right(8) ^ (w[i - 15] >> 7);
            let s1 = w[i - 2].rotate_right(19) ^ w[i - 2].rotate_right(61) ^ (w[i - 2] >> 6);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = *h;
        for i in 0..80 {
            let s1 = e.rotate_right(14) ^ e.rotate_right(18) ^ e.rotate_right(41);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(28) ^ a.rotate_right(34) ^ a.rotate_right(39);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }

        for (slot, value) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *slot = slot.wrapping_add(value);
        }
    }
}

/// The SHA-512 digest of one slice.
pub fn sha512(data: &[u8]) -> [u8; DIGEST_LEN] {
    let mut ctx = Sha512::new();
    ctx.update(data);
    ctx.finalize()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Splitting the message must not change the digest. The crypt scheme
    /// pushes the salt, the password and a digest into the same context in
    /// pieces of every size, so this is the property it actually depends on.
    #[test]
    fn update_is_chunk_independent() {
        let message: Vec<u8> = (0u8..=255).cycle().take(1000).collect();
        let whole = sha512(&message);
        for step in [1usize, 7, 63, 64, 65, 127, 128, 129, 333] {
            let mut ctx = Sha512::new();
            for piece in message.chunks(step) {
                ctx.update(piece);
            }
            assert_eq!(ctx.finalize(), whole, "split into {step}-byte pieces");
        }
    }

    /// An empty update in the middle of a message is a no-op, which matters
    /// because the crypt scheme hands over an empty password without
    /// checking.
    #[test]
    fn empty_updates_do_nothing() {
        let mut ctx = Sha512::new();
        ctx.update(b"");
        ctx.update(b"abc");
        ctx.update(b"");
        assert_eq!(ctx.finalize(), sha512(b"abc"));
    }
}
