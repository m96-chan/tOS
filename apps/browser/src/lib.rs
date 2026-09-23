//! Browse the web in a tOS pane.
//!
//! tOS owns the display: there is no X11 and no Wayland, and there never will
//! be, so no browser can be ported to it in the ordinary sense — every engine
//! worth having assumes a window system underneath. What tOS does have is a
//! terminal that speaks the Kitty graphics protocol, SGR mouse reporting and
//! the Kitty keyboard protocol, and those three together are enough to be a
//! screen, a mouse and a keyboard. So `tos-browser` runs a headless Chromium
//! as a child process, drives it over the Chrome DevTools Protocol on a
//! hand-rolled WebSocket, takes its screencast, decodes each frame here, and
//! hands the terminal raw pixels as graphics commands, turning the terminal's
//! own reports of keys and mouse back into CDP input events. The engine
//! renders; the compositor displays; this crate is the wire between them and
//! nothing else.
//!
//! The numbers it was built against: 57.8 frames a second at 1280x770, about
//! 185 kB per frame on the engine's side, 8 ms to decode one here. The frames
//! go through `/dev/shm` (`t=s`) rather than as base64 in the escape sequence
//! wherever the terminal will read them, and they go as raw pixels (`f=24`,
//! `f=32`) rather than as files the terminal decodes — see [`graphics`] for
//! the id, the format and the transport, all three of which are decisions
//! rather than defaults.
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
//! # JPEG while it moves, PNG when it stops
//!
//! This paragraph used to say "No JPEG", on the grounds that a baseline
//! decoder is more code than PNG and inflate put together for a second format
//! in a workspace whose one dependency is `libc`. It was measured and it was
//! wrong — not about the code, which is nine hundred lines in
//! `tos_term::jpeg`, but about what it buys.
//!
//! `Page.startScreencast` is bounded by the engine's own single-threaded
//! encode of each frame. At 1280x770 on two cores that is 33.8 frames a
//! second as PNG and 57.8 as JPEG at quality 85, and it does not change from
//! two cores to eight. `Page.captureScreenshot` in a loop is 10 to 12 in
//! every format CDP offers, lossless webp included, so there is no third
//! option: it is JPEG or it is half the frame rate.
//!
//! So the screencast is JPEG at quality 85 while the page is moving, and the
//! moment it stops — a rest interval of 150 ms with no frame — the tab in
//! front is asked for one `Page.captureScreenshot` in PNG and that is what is
//! left on screen. Text that is being read is always lossless; the lossy
//! frames are the ones scrolling past, which nobody reads. A page that never
//! moves costs one still and then nothing. [`motion`] has the table, the
//! reason quality 85 rather than 70 or 95, and the rule that decides which of
//! two frames arriving out of order is the one to keep.
//!
//! The frames are decoded here rather than by the terminal, which is the
//! other half of the change and the reason the compositor needed none: the
//! pixels go over as `f=24` and the per-frame PNG decode on the compositor's
//! parse loop — named in `docs/design/browser.md` as the first cost to delete
//! — is gone rather than moved.
//!
//! # What it deliberately does not do
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
pub mod motion;
pub mod screen;
pub mod scroll;
pub mod sha1;
pub mod tabs;
pub mod ws;

pub use app::Options;
pub use json::Json;
