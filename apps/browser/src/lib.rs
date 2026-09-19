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
//! # Tabs, and why they are not panes
//!
//! A tab here is one CDP page target: a page with its own history, its own
//! renderer and its own WebSocket, listed on the one row this program already
//! owns. Tabs exist because the first thing anybody meets on a real site is a
//! link with `target=_blank` — the engine makes a target for it whatever this
//! program does, and a target nothing attaches to is a click that did nothing
//! at all.
//!
//! The tOS-shaped answer would be a pane each: the compositor has workspaces
//! and panes and a tree to arrange them in, and a page per pane would put a
//! browser's tabs under the same keys as everything else on the machine. It is
//! not what this does, for two reasons. A pane program has no way to ask for
//! another pane — there is no compositor API, no socket, and `apps/browser`
//! having one would be this crate's second dependency after `libc` and the
//! first that is on tOS itself. And the engine's own model *is* tabs: targets
//! are opened, closed and raised by a browser-level connection that knows
//! nothing about panes, so a pane per page would be a second list to keep in
//! step with the engine's. So the tabs live in the browser, on the row that
//! was already there, and the pane stays one pane. See [`tabs`] for what that
//! costs per tab, which is one socket and no frames.
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
//! and the rule that everything else is the person's. A Chromium is 482 MB
//! installed — twice the ISO, measured in `docs/design/browser.md` — and a
//! policy about which browser somebody uses. This crate builds
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
pub mod tabs;
pub mod ws;

pub use app::Options;
pub use json::Json;
