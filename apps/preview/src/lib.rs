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
//! Not here, and deliberately. A baseline JPEG decoder is Huffman decoding,
//! dequantisation, an inverse DCT, upsampling for subsampled chroma and a
//! YCbCr conversion — somewhere around 600 to 900 lines before progressive
//! JPEG, which most cameras and every web image pipeline emit, is considered
//! at all. That is larger than [`tos_term::png`] and [`tos_term::inflate`]
//! put together, for a second format, in a repository whose one dependency is
//! `libc`. "Cheap" was the condition in the issue and JPEG does not meet it.
//! PNG first is the right call because it is what the graphics protocol
//! already names as its own payload format, so the decoder earns its keep
//! twice: once for this program and once for every file manager that sends
//! `f=100`. JPEG deserves its own issue, its own test corpus and its own
//! decision about progressive.

pub mod fit;
pub mod transmit;

pub use fit::{Cells, Metrics};
pub use transmit::Payload;
