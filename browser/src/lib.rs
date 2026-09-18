//! Browse the web in a tOS pane.
//!
//! tOS owns the display: there is no X11 and no Wayland, and there never will
//! be, so no browser can be ported to it in the ordinary sense — every engine
//! worth having assumes a window system underneath. What tOS does have is a
//! terminal that speaks the Kitty graphics protocol, SGR mouse reporting and
//! the Kitty keyboard protocol, and those three together are enough to be a
//! screen, a mouse and a keyboard. So `tos-browser` runs a headless Chromium
//! as a child process, drives it over the Chrome DevTools Protocol on a
//! hand-rolled WebSocket, takes its screencast as PNG frames and hands them to
//! the terminal as graphics commands, and turns the terminal's own reports of
//! keys and mouse back into CDP input events. The engine renders; the
//! compositor displays; this crate is the wire between them and nothing else.
//!
//! The numbers it was built against: 60 frames a second at 640x360, about
//! 58 kB per PNG frame on the engine's side. What that costs on the terminal's
//! side is the question this crate exists to answer, which is why the frames
//! go through `/dev/shm` (`t=s`) rather than as base64 in the escape sequence
//! wherever the terminal will read them — see [`graphics`] for the id and the
//! transport, both of which are decisions rather than defaults.
//!
//! # What it deliberately does not do
//!
//! **No JPEG.** The screencast can produce it and [`tos_term::png`] cannot
//! read it; adding a baseline decoder would be more code than the PNG and
//! inflate decoders put together, for a second format, in a workspace whose
//! only dependency is `libc`. `tos-preview` made the same call for the same
//! reason.
//!
//! **No text as cells.** A page is not re-rendered as characters in the grid.
//! That is a different program — a text browser — and it would throw away the
//! layout, the images and the video that are the reason for wanting a browser
//! at all. The page arrives as pixels because it *is* pixels.
//!
//! **No place on the ISO.** `docs/design/applications.md` lists what every tOS
//! image carries and says why: thirteen Debian packages, one upstream binary,
//! and the rule that everything else is the person's. A Chromium is 150 MB of
//! squashfs and a policy about which browser somebody uses. This crate builds
//! into the tree, and the engine it drives is installed by the person who
//! wants one — `$TOS_BROWSER_ENGINE`, or whichever of `chromium-shell`,
//! `chromium`, `chromium-browser` or `google-chrome` is on the path.
//!
//! **No dependencies.** The HTTP GET, the WebSocket client, the JSON, the
//! base64 and the SHA-1 are all in this crate, each in its own module with its
//! own tests, for the reason the rest of the workspace hand-rolled PNG and
//! inflate: a browser is a large enough thing to want a crate for every part
//! of it, and that is exactly how a one-dependency workspace stops being one.

pub mod app;
pub mod base64;
pub mod cdp;
pub mod engine;
pub mod graphics;
pub mod http;
pub mod input;
pub mod json;
pub mod keys;
pub mod screen;
pub mod sha1;
pub mod ws;

pub use app::Options;
pub use json::Json;
