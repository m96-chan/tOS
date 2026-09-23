//! The JPEG decoder against files a JPEG encoder wrote.
//!
//! The unit tests in `src/jpeg.rs` check the pieces against the specification
//! — the zig-zag is a permutation, the magnitude bits extend the way F.12
//! says, the fixed-point transform tracks the floating-point definition. What
//! they cannot check is whether the whole thing agrees with the rest of the
//! world about what a particular file means, and that is what these do.
//!
//! **Where the fixtures come from.** They were made on the host with
//! ImageMagick and their expected pixels with Pillow, at generation time, and
//! both are committed:
//!
//! ```text
//! magick src.png -sampling-factor 2x2 -quality 85 420.jpg
//! python3 -c "from PIL import Image; \
//!   open('420.rgb','wb').write(Image.open('420.jpg').convert('RGB').tobytes())"
//! ```
//!
//! Generating them here instead would make the test depend on an encoder
//! being installed, which is the thing a test of a decoder must not do; and
//! decoding them here with a second decoder would make it depend on one. So
//! the answer is a file: `<name>.jpg` beside `<name>.rgb`, raw RGB8, and the
//! test is a comparison.
//!
//! **Why a tolerance and not equality.** Every decoder's inverse DCT is an
//! approximation — T.81 does not specify one, and Annex A only bounds the
//! error — and its chroma upsampling is a filter choice. Three counts per
//! channel is what two honest baseline decoders differ by; it is small enough
//! that a wrong coefficient, a wrong table or a wrong edge fails loudly, and
//! large enough that the test is not a test of which rounding was used. The
//! dimensions are asserted exactly, because there is nothing to approximate
//! about how big a picture is.

use tos_term::jpeg::{self, JpegError};

/// How far a sample may be from the reference decoder's.
const TOLERANCE: i32 = 3;

fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/jpeg/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// Decode `<name>.jpg` and compare it with `<name>.rgb`.
fn agrees(name: &str, width: u32, height: u32) {
    let data = fixture(&format!("{name}.jpg"));
    let want = fixture(&format!("{name}.rgb"));
    let image = jpeg::decode(&data, usize::MAX).unwrap_or_else(|e| panic!("{name}: {e}"));

    assert_eq!((image.width, image.height), (width, height), "{name}");
    assert_eq!(
        image.rgb.len(),
        want.len(),
        "{name}: three bytes a pixel and no padding"
    );
    assert_eq!(
        jpeg::dimensions(&data),
        Ok((width, height)),
        "{name}: the header says a different size from the pixels"
    );

    let mut worst = (0i32, 0usize);
    for (index, (&got, &reference)) in image.rgb.iter().zip(want.iter()).enumerate() {
        let off = (got as i32 - reference as i32).abs();
        if off > worst.0 {
            worst = (off, index);
        }
    }
    let (off, index) = worst;
    assert!(
        off <= TOLERANCE,
        "{name}: pixel ({}, {}) channel {} is {} against {}, which is {} out",
        (index / 3) % width as usize,
        (index / 3) / width as usize,
        index % 3,
        image.rgb[index],
        want[index],
        off
    );
}

/// Every sampling the encoder in front of this decoder can choose, at a size
/// that is a whole number of no MCU at all: 33 by 17 leaves a one-pixel
/// column and a one-pixel row of the last MCU to be the picture and the rest
/// of it to be thrown away, in both axes, at every subsampling.
#[test]
fn every_sampling_decodes_at_a_size_that_fits_no_mcu() {
    agrees("444", 33, 17);
    agrees("422", 33, 17);
    agrees("440", 33, 17);
    agrees("420", 33, 17);
}

/// One component is a grayscale picture, and comes out as three equal
/// channels rather than as a plane the caller has to know about.
#[test]
fn a_grayscale_file_decodes_to_grey_pixels() {
    agrees("gray", 33, 17);
    let image = jpeg::decode(&fixture("gray.jpg"), usize::MAX).expect("decodes");
    for pixel in image.rgb.chunks_exact(3) {
        assert_eq!(pixel[0], pixel[1], "grey is three of the same");
        assert_eq!(pixel[1], pixel[2]);
    }
}

/// Restart markers: the DC predictors reset and the reader finds the marker
/// again, eight times in this file.
#[test]
fn a_file_with_restart_intervals_stays_in_step() {
    agrees("restart", 48, 32);
    // The fixture is only worth anything if it really carries DRI.
    let data = fixture("restart.jpg");
    assert!(
        data.windows(2).any(|w| w == [0xFF, 0xDD]),
        "the restart fixture has no DRI marker in it"
    );
}

/// One pixel is one MCU of which one sample is the picture.
#[test]
fn a_one_pixel_file_is_one_pixel() {
    agrees("one", 1, 1);
}

/// Progressive is the refusal that matters most, because it is what the rest
/// of the web is encoded as — and the sentence has to name it.
#[test]
fn a_progressive_file_is_refused_and_says_so() {
    let data = fixture("progressive.jpg");
    let error = jpeg::decode(&data, usize::MAX).expect_err("progressive is not baseline");
    assert_eq!(error, JpegError::Progressive);
    let said = error.to_string();
    assert!(said.contains("progressive"), "{said}");
    assert!(said.contains("SOF2"), "{said}");
    assert!(said.contains("baseline"), "{said}");
    // And asking only for its size is refused the same way, rather than
    // answering for a file that cannot be decoded.
    assert_eq!(jpeg::dimensions(&data), Err(JpegError::Progressive));
}

/// A file that stops in the middle is an error at every place it could stop,
/// and never a panic, a hang or a picture.
#[test]
fn a_truncated_file_ends_in_an_error_rather_than_a_loop() {
    for name in ["420.jpg", "restart.jpg", "gray.jpg"] {
        let whole = fixture(name);
        // Every cut, not a sample of them: the interesting ones are inside a
        // marker's length, inside a table, and inside the entropy data, and
        // which byte that is differs per file.
        for cut in 0..whole.len() {
            let result = jpeg::decode(&whole[..cut], usize::MAX);
            assert!(
                result.is_err(),
                "{name} cut to {cut} of {} bytes decoded anyway",
                whole.len()
            );
        }
        // And the whole file still decodes, so the loop above was testing
        // truncation rather than a decoder that refuses everything.
        assert!(jpeg::decode(&whole, usize::MAX).is_ok(), "{name}");
    }
}

/// Corruption inside the entropy-coded data is not truncation: the decoder
/// may produce a wrong picture or an error, but it must do one of them
/// within the time a frame takes.
#[test]
fn a_corrupt_scan_is_survived() {
    let whole = fixture("restart.jpg");
    let scan = whole
        .windows(2)
        .position(|w| w == [0xFF, 0xDA])
        .expect("a scan");
    for offset in 0..256usize {
        let mut broken = whole.clone();
        let at = scan + 16 + offset * 3;
        if at >= broken.len() {
            break;
        }
        broken[at] ^= 0xA5;
        // Either answer is right; what is not allowed is a panic, a hang, or
        // a picture that is not the size the header promised.
        if let Ok(image) = jpeg::decode(&broken, usize::MAX) {
            assert_eq!((image.width, image.height), (48, 32));
            assert_eq!(image.rgb.len(), 48 * 32 * 3);
        }
    }
}

/// The budget is checked against the frame header, before the pixels exist.
#[test]
fn an_image_larger_than_the_budget_is_refused_from_its_header() {
    let data = fixture("restart.jpg");
    assert_eq!(
        jpeg::decode(&data, 48 * 32 * 3 - 1),
        Err(JpegError::TooLarge)
    );
    assert!(jpeg::decode(&data, 48 * 32 * 3).is_ok(), "exactly enough");
}

/// A 16-bit quantisation table means the same thing as the 8-bit one it was
/// widened from, which is the only way to test a precision no encoder here
/// emits.
#[test]
fn a_sixteen_bit_quantisation_table_decodes_the_same_picture() {
    let eight = fixture("gray.jpg");
    let sixteen = widen_quant_tables(&eight);
    assert_ne!(eight, sixteen, "the rewrite did nothing");
    assert_eq!(
        jpeg::decode(&sixteen, usize::MAX).expect("16-bit tables are accepted"),
        jpeg::decode(&eight, usize::MAX).expect("8-bit tables are accepted"),
        "the same divisors, written wider"
    );
}

/// Rewrite every DQT segment's tables from 8-bit to 16-bit precision.
fn widen_quant_tables(data: &[u8]) -> Vec<u8> {
    let mut out = data[..2].to_vec();
    let mut pos = 2;
    while pos + 4 <= data.len() {
        let marker = data[pos + 1];
        let length = ((data[pos + 2] as usize) << 8) | data[pos + 3] as usize;
        let body = &data[pos + 4..pos + 2 + length];
        if marker == 0xDA {
            // The scan and everything after it is the same bytes.
            out.extend_from_slice(&data[pos..]);
            return out;
        }
        if marker != 0xDB {
            out.extend_from_slice(&data[pos..pos + 2 + length]);
            pos += 2 + length;
            continue;
        }
        let mut wider = Vec::new();
        let mut rest = body;
        while !rest.is_empty() {
            wider.push(rest[0] | 0x10);
            for &value in &rest[1..65] {
                wider.extend_from_slice(&(value as u16).to_be_bytes());
            }
            rest = &rest[65..];
        }
        out.extend_from_slice(&[0xFF, 0xDB]);
        out.extend_from_slice(&((wider.len() + 2) as u16).to_be_bytes());
        out.extend_from_slice(&wider);
        pos += 2 + length;
    }
    out
}
