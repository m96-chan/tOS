//! Show an image in a tOS pane.
//!
//! The Kitty graphics protocol has worked in `tos-term` for a while and PNG
//! payloads have decoded for a while, but until now nothing in a tOS session
//! sent either of them: the ISO shipped `tos` and `tos-install` and no program
//! that could put a picture on the screen. So the decoder was verified by unit
//! tests and by nothing else. This crate is the other end of that wire — an
//! ordinary program in a pane, writing the same escape sequences any file
//! manager would, which is what makes the graphics path something a person can
//! watch work rather than something a test asserts about.
//!
//! # The file goes over the wire, not the pixels
//!
//! A PNG is transmitted as a PNG (`f=100`) and decoded by the terminal, rather
//! than decoded here and sent as raw RGBA (`f=32`). Two reasons, in order of
//! weight. The first is that `f=100` is the path yazi, ranger and every other
//! TUI file manager uses, so running `tos-preview` exercises the code an
//! outside program will hit; a tool that took the `f=32` path would prove the
//! protocol works for `tos-preview` and leave the interesting half untested.
//! The second is that it is smaller: a photograph is several times its own
//! area in RGBA, and base64 adds a third on top of whichever one is sent.
//!
//! The file is still decoded here first, and the pixels then thrown away. That
//! is not waste — it is the only way to learn the image's real size, which
//! [`fit`] needs before it can ask for a number of cells, and it means a file
//! that cannot be decoded is refused with a sentence on stderr instead of
//! becoming a payload the terminal rejects with a protocol error that nothing
//! is listening for. `--rgba` transmits the decoded pixels instead, which is
//! the escape hatch for a terminal that speaks the protocol but cannot decode
//! the format, and the reason the decode is kept rather than reduced to a
//! header parse.
//!
//! # JPEG
//!
//! It exists now, in [`tos_term::jpeg`], and this paragraph used to say it
//! never would.
//!
//! What it said was that a baseline decoder is Huffman decoding,
//! dequantisation, an inverse DCT, chroma upsampling and a YCbCr conversion —
//! six hundred to nine hundred lines before progressive JPEG is considered at
//! all — for a second format, in a repository whose one dependency is `libc`,
//! and that nothing in the tree wanted it badly enough to pay for that.
//! Every clause of that is still true, and the estimate was accurate: the
//! decoder is a little over nine hundred lines of code, and it refuses
//! progressive files by name rather than growing a second entropy decoder
//! for them.
//!
//! What changed is the other side of the ledger, and it changed because
//! somebody measured it rather than argued about it. `docs/design/browser.md`
//! has the numbers: Chromium's screencast is bounded by its own encode of
//! each frame, and a 1280x770 pane goes from 33.8 frames a second as PNG to
//! 57.8 as JPEG at quality 85 — which is the difference between a page that
//! scrolls and a page that stutters, and it is not buyable anywhere else in
//! the path. A format nothing wanted turned out to be the only thing standing
//! between a browser and sixty frames a second, which is a better reason than
//! "an image viewer should probably read JPEGs".
//!
//! **`tos-preview` still does not show them, and that is a follow-up rather
//! than an oversight.** Everything above about `f=100` and about this program
//! existing to exercise the path a file manager takes is unaffected: the
//! graphics protocol names PNG as its payload format and names no other, so a
//! JPEG shown here would have to be decoded here and transmitted as `f=24`,
//! which is the `--rgba` path with a second decoder in front of it. That is a
//! small change and a real one — `fit` needs the size, `transmit` needs a
//! third payload shape, and somebody has to decide what happens to a
//! progressive file the person double-clicked. It belongs in its own commit
//! with its own tests.

pub mod fit;
pub mod transmit;

pub use fit::{Cells, Metrics};
pub use transmit::Payload;
