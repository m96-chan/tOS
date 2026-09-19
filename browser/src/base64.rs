//! Base64, strictly.
//!
//! Two callers with opposite needs. The WebSocket handshake needs an encoder
//! for its sixteen random bytes and for the SHA-1 the server is checked
//! against; the screencast needs a decoder for a PNG that arrived inside a
//! JSON string.
//!
//! [`tos_term::graphics::decode_base64`] already decodes, and this is
//! deliberately not it. That one is a terminal reading what an application
//! sent: it skips whitespace and stops at the first byte outside the
//! alphabet, because a terminal that rejected a payload would have to answer
//! with a protocol error and the kinder failure is a short image. Here the
//! payload is a video frame arriving sixty times a second down a socket this
//! program owns both ends of, and a decode that silently returns half a PNG
//! would show a torn picture and give no hint why. So anything that is not
//! four-character-aligned, in-alphabet and padded exactly once is an error
//! with a name, and the frame is dropped rather than drawn wrong.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Why a string was not base64.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Base64Error {
    /// The length is not a multiple of four, so a group is incomplete.
    Length,
    /// A byte outside the alphabet, or padding somewhere other than the end.
    Byte(u8),
}

impl std::fmt::Display for Base64Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Base64Error::Length => write!(f, "base64 length is not a multiple of four"),
            Base64Error::Byte(b) => write!(f, "base64 contains {:?}", *b as char),
        }
    }
}

/// Encode, with padding.
pub fn encode(input: &[u8]) -> String {
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

/// Decode, refusing anything that is not exactly base64.
pub fn decode(input: &[u8]) -> Result<Vec<u8>, Base64Error> {
    if input.len() % 4 != 0 {
        return Err(Base64Error::Length);
    }
    let mut table = [0xffu8; 256];
    for (value, &c) in ALPHABET.iter().enumerate() {
        table[c as usize] = value as u8;
    }

    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let last = input.len() / 4;
    for (index, group) in input.chunks(4).enumerate() {
        // Padding is only ever the last one or two characters of the last
        // group. Anywhere else it is a payload that was cut and rejoined, and
        // the useful failure for that is a refusal rather than a picture with
        // its middle replaced.
        let pad = group.iter().filter(|&&b| b == b'=').count();
        if pad > 0 {
            let padded_tail = group[4 - pad..].iter().all(|&b| b == b'=');
            if !padded_tail || pad > 2 || index + 1 != last {
                return Err(Base64Error::Byte(b'='));
            }
        }
        let mut n: u32 = 0;
        for &byte in &group[..4 - pad] {
            let v = table[byte as usize];
            if v == 0xff {
                return Err(Base64Error::Byte(byte));
            }
            n = (n << 6) | v as u32;
        }
        n <<= 6 * pad as u32;
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rfc_vectors_round_trip() {
        let cases = [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ];
        for (plain, encoded) in cases {
            assert_eq!(encode(plain.as_bytes()), encoded);
            assert_eq!(decode(encoded.as_bytes()).unwrap(), plain.as_bytes());
        }
    }

    #[test]
    fn every_byte_survives_the_trip() {
        let data: Vec<u8> = (0..=255u8).collect();
        assert_eq!(decode(encode(&data).as_bytes()).unwrap(), data);
    }

    #[test]
    fn the_same_bytes_the_terminals_decoder_would_produce() {
        // The encoder here and the one in the graphics protocol have to agree,
        // because a frame sent inline is read back by that one.
        let data: Vec<u8> = (0..300u32).map(|i| (i * 7) as u8).collect();
        assert_eq!(encode(&data), tos_term::graphics::encode_base64(&data));
    }

    #[test]
    fn a_payload_cut_in_the_middle_of_a_group_is_refused() {
        let encoded = encode(b"a whole PNG, pretend");
        for cut in 1..4 {
            let ragged = &encoded.as_bytes()[..encoded.len() - cut];
            assert_eq!(decode(ragged), Err(Base64Error::Length), "cut {cut}");
        }
    }

    #[test]
    fn whitespace_is_not_quietly_skipped() {
        assert_eq!(decode(b"Zm9v Zm9"), Err(Base64Error::Byte(b' ')));
        assert_eq!(decode(b"Zm9v YmFy"), Err(Base64Error::Length));
        assert_eq!(decode(b"Zm9vYmFy\n"), Err(Base64Error::Length));
    }

    #[test]
    fn padding_in_the_middle_is_refused() {
        assert_eq!(decode(b"Zg==Zg=="), Err(Base64Error::Byte(b'=')));
        assert_eq!(decode(b"Z=g="), Err(Base64Error::Byte(b'=')));
    }
}
