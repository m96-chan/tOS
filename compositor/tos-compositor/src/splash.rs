//! The pictures on the login and lock screens.
//!
//! A machine that has been turned on shows the login screen before it shows
//! anything else, and until #132 that screen was a box on an empty field.
//! Everything tOS had to say about itself it said afterwards: `.motd_art` is
//! printed by a shell profile, so it arrives once a pane is running, which on
//! a gated machine is after somebody has already logged in.
//!
//! This is the part of tOS that is drawn in pixels rather than in cells, and
//! it is the only such part the compositor draws for itself. The two halves of
//! it already existed for the graphics protocol: [`tos_term::png`] turns a
//! file into RGBA8 against a budget, and [`Surface::blit_rgba`] composites it
//! with alpha and nearest sampling.
//!
//! ## Whole pixels
//!
//! Nearest sampling at a fractional ratio is what makes scaled pixel art look
//! melted: one source pixel lands on two screen pixels and its neighbour on
//! one, so an edge that was straight comes out ragged, and it comes out
//! ragged differently on every panel. So the picture is only ever drawn at a
//! whole number of screen pixels per picture pixel — `n` of them going up, one
//! in `n` coming down — and a display with no room for even the smallest of
//! those gets no picture rather than a smeared one. [`Splash::fit`] is that
//! rule and nothing else. What differs between the two screens is only the
//! room handed to it: [`Splash::room_across`] for the frontispiece over a
//! login, [`Splash::room_in_corner`] for the picture beside a lock.
//!
//! ## Nothing here draws to a display it is not given
//!
//! [`crate::lock`] reads no file, holds no picture and asks for no clock:
//! everything it draws is handed to it, which is what lets its tests drive the
//! whole state machine with no display. A picture is a file, so the loading is
//! here and the compositor hands the result in — the same shape the clock
//! already has.

use std::path::Path;

use tos_render::{Rect, Surface};

/// The login screen's picture as it ships, compiled into the compositor.
///
/// Compiled in rather than read from the filesystem because the login screen
/// is the first thing on the display: a picture that lived only in `/etc`
/// would be missing from exactly the machines that are hardest to look at —
/// the initramfs rescue session, and any disk whose `/etc` did not come from
/// the installer.
const BUILT_IN: &[u8] = include_bytes!("../assets/splash.png");

/// The lock screen's picture, likewise, and a different file.
///
/// The two screens are one type asking one account for one password, and this
/// is the one thing about them that is genuinely not shared: a login screen is
/// the machine opening and wants a frontispiece, where a lock is somebody's
/// own session waiting behind it and wants a corner. One file each, so
/// replacing either does not silently change the other.
const BUILT_IN_LOCK: &[u8] = include_bytes!("../assets/lock.png");

/// Where a machine keeps a login picture of its own.
///
/// The same door `/etc/tos/motd_art` opens for the banner, for the same
/// reason: a machine should be able to say it is somebody's without being
/// rebuilt. A file that is not there, or is not a picture this can decode, is
/// not an error — it is the built-in picture, which is what the screen would
/// have shown anyway.
pub const SPLASH_PATH: &str = "/etc/tos/splash.png";

/// And where it keeps a lock picture of its own, on the same terms.
pub const LOCK_PATH: &str = "/etc/tos/lock.png";

/// The largest decoded picture accepted, in bytes.
///
/// The file is named by a path the compositor was pointed at rather than by a
/// program on a PTY, so this is not the adversarial budget the graphics
/// protocol needs; it is the line past which somebody has pointed the login
/// screen at a photograph by mistake and would rather see the built-in
/// picture than wait. Sixteen mebibytes is a little over a 2048x2048 picture,
/// which is more than any display tOS drives can show of one.
const BUDGET: usize = 16 << 20;

/// How much of the display's width the picture may take: four fifths, leaving
/// a tenth of it either side.
///
/// A frontispiece that reached the edges would be a background, and the thing
/// under it is a password box that has to look like the one thing on the
/// screen to type into.
const WIDTH_NUMERATOR: u32 = 4;
const WIDTH_DENOMINATOR: u32 = 5;

/// How much of the display a picture in a corner may take: a third each way.
///
/// Wider than that and it stops being something the eye finds after the box
/// and starts being the thing on the screen — which over a lock is the wrong
/// way round, because the box is what the person in front of it came for.
const CORNER_DENOMINATOR: u32 = 3;

/// The smallest fraction the picture is drawn at before it is dropped.
///
/// A quarter of the shipped picture is 128 pixels across, which is small
/// enough that one more halving would be a smudge rather than a picture.
const MIN_FRACTION: u32 = 4;

/// A decoded picture, kept as the RGBA8 the blit wants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Splash {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

impl Splash {
    /// The picture tOS ships, or `None` if it will not decode.
    ///
    /// There is no panic here even though the bytes are compiled in and a
    /// failure would mean the build is wrong: a compositor that refused to
    /// start over a decoration would be a machine bricked by its own
    /// frontispiece. The unit tests are what make sure it is never `None`.
    pub fn built_in() -> Option<Splash> {
        decode(BUILT_IN)
    }

    /// The lock screen's picture as tOS ships it, on the same terms.
    pub fn built_in_lock() -> Option<Splash> {
        decode(BUILT_IN_LOCK)
    }

    /// The login picture on this machine, falling back to the one tOS ships.
    pub fn load(path: &Path) -> Option<Splash> {
        Splash::at(path).or_else(Splash::built_in)
    }

    /// The lock picture on this machine, likewise.
    pub fn load_lock(path: &Path) -> Option<Splash> {
        Splash::at(path).or_else(Splash::built_in_lock)
    }

    /// Whatever is at `path`, or nothing — the fallback is the caller's,
    /// because it is the only half of this that differs between the two.
    fn at(path: &Path) -> Option<Splash> {
        std::fs::read(path).ok().and_then(|bytes| decode(&bytes))
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// The size to draw the picture at inside `room`, or `None` when even the
    /// smallest fraction of it will not fit.
    ///
    /// Whole pixels, as the module says: `n` screen pixels per picture pixel
    /// going up, one picture pixel in `n` coming down. The width of `room` is
    /// already the share of the display the picture is allowed; the height is
    /// whatever the box above it left.
    pub fn fit(&self, room: (u32, u32)) -> Option<(u32, u32)> {
        if self.width == 0 || self.height == 0 {
            return None;
        }
        // The larger of the two multiples is the one that would overflow the
        // other axis, so the smaller is the answer.
        let times = (room.0 / self.width).min(room.1 / self.height);
        if times >= 1 {
            return Some((self.width * times, self.height * times));
        }
        for divisor in 2..=MIN_FRACTION {
            let size = (self.width / divisor, self.height / divisor);
            if size.0 == 0 || size.1 == 0 {
                break;
            }
            if size.0 <= room.0 && size.1 <= room.1 {
                return Some(size);
            }
        }
        None
    }

    /// The share of `width` the picture may take.
    pub fn room_across(width: u32) -> u32 {
        width / WIDTH_DENOMINATOR * WIDTH_NUMERATOR
    }

    /// The room a picture tucked into a corner of a display this size gets.
    ///
    /// Both axes, unlike the frontispiece: that one is centred above the box
    /// and so is bounded across by a share and down by whatever the box left,
    /// where a corner is bounded by the corner.
    pub fn room_in_corner(size: (u32, u32)) -> (u32, u32) {
        (size.0 / CORNER_DENOMINATOR, size.1 / CORNER_DENOMINATOR)
    }

    /// Composite the picture into `dest`.
    ///
    /// The alpha is the picture's own: what tOS ships is drawn on nothing, so
    /// the field around it is the screen's background rather than a black
    /// rectangle the picture brought with it.
    pub fn draw(&self, surface: &mut Surface<'_>, dest: Rect) {
        surface.blit_rgba(dest, &self.rgba, self.width, self.height);
    }
}

/// A PNG as a picture, or `None` for anything that is not one.
fn decode(bytes: &[u8]) -> Option<Splash> {
    let image = tos_term::png::decode(bytes, BUDGET).ok()?;
    if image.width == 0 || image.height == 0 {
        return None;
    }
    Some(Splash {
        width: image.width,
        height: image.height,
        rgba: image.rgba,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn shipped() -> Splash {
        Splash::built_in().expect("the picture tOS ships should decode")
    }

    fn shipped_lock() -> Splash {
        Splash::built_in_lock().expect("the lock picture tOS ships should decode")
    }

    #[test]
    fn the_lock_picture_that_ships_decodes_and_is_not_the_login_one() {
        let lock = shipped_lock();
        assert!(lock.width > 0 && lock.height > 0);
        assert_eq!(
            lock.rgba.len(),
            (lock.width * lock.height * 4) as usize,
            "the picture should be RGBA8"
        );
        assert_ne!(lock, shipped(), "the two screens ship two pictures");
    }

    #[test]
    fn the_lock_picture_is_drawn_pixel_for_pixel_on_the_displays_that_matter() {
        // This one is not pixel art the way the frontispiece is — it is a
        // render, so a whole fraction of it is soft and a whole multiple of it
        // is blocky, and it only looks like what it is at 1:1. That is what
        // decided its size: 426 across is a third of 1280 exactly, so it lands
        // pixel for pixel on the display tOS runs headless at and on the one
        // it runs on a laptop. A picture swapped for a wider one would still
        // draw, at half size and softly, and nothing else here would notice.
        let lock = shipped_lock();
        for display in [(1280u32, 720u32), (1920, 1080)] {
            let fitted = lock
                .fit(Splash::room_in_corner(display))
                .expect("the corner should have room");
            assert_eq!(
                fitted,
                (lock.width, lock.height),
                "on {display:?} the picture is not drawn at its own size"
            );
        }
    }

    #[test]
    fn a_corner_is_a_third_of_the_display_each_way() {
        assert_eq!(Splash::room_in_corner((1920, 1080)), (640, 360));
    }

    #[test]
    fn a_bad_lock_picture_falls_back_to_the_lock_one_and_not_the_login_one() {
        // The two fallbacks are the only thing `load` and `load_lock` do
        // differently, so it is the thing worth a test: a machine with an
        // unreadable /etc/tos/lock.png should get a lock picture back.
        let path = std::env::temp_dir().join(format!("tos-lock-bad-{}.png", std::process::id()));
        std::fs::write(&path, b"not a png").expect("write");
        assert_eq!(Splash::load_lock(&path), Some(shipped_lock()));
        assert_eq!(Splash::load(&path), Some(shipped()));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_picture_that_ships_decodes() {
        let splash = shipped();
        assert!(splash.width > 0 && splash.height > 0);
        assert_eq!(
            splash.rgba.len(),
            (splash.width * splash.height * 4) as usize,
            "the picture should be RGBA8"
        );
    }

    #[test]
    fn the_picture_that_ships_fits_a_modest_display() {
        // 640x480 is the smallest mode a PC is guaranteed to have, and the
        // login screen has to have something on it there too.
        let splash = shipped();
        let room = (Splash::room_across(640), 480 - 5 * 16);
        assert!(
            splash.fit(room).is_some(),
            "a {}x{} picture found no room on a VGA screen",
            splash.width,
            splash.height
        );
    }

    #[test]
    fn the_picture_that_ships_has_something_to_see() {
        // Every pixel transparent would decode, fit and draw nothing at all,
        // which is the one way this could pass every other test and still
        // leave the screen empty.
        let splash = shipped();
        let opaque = splash.rgba.chunks_exact(4).filter(|px| px[3] > 0).count();
        assert!(
            opaque > (splash.width * splash.height / 4) as usize,
            "only {opaque} pixels of the picture are visible"
        );
    }

    #[test]
    fn the_sessions_colours_are_colours_the_picture_is_drawn_in() {
        // The accent and the attention colour are not decoration picked to go
        // with the picture; they are taken out of it — the `tOS` and the
        // prompt for one, the streak in the hair for the other. Replacing the
        // picture with something else is allowed and does not have to keep
        // this true, but replacing *this* picture without moving the colours
        // would quietly separate the two, and this is what would say so.
        let splash = shipped();
        for (name, colour) in [
            ("accent", crate::chrome::ACCENT),
            ("attention", crate::chrome::ATTENTION),
        ] {
            let nearest = splash
                .rgba
                .chunks_exact(4)
                .filter(|px| px[3] >= 250)
                .map(|px| {
                    [
                        px[0].abs_diff(colour.r),
                        px[1].abs_diff(colour.g),
                        px[2].abs_diff(colour.b),
                    ]
                    .into_iter()
                    .max()
                    .unwrap_or(u8::MAX)
                })
                .min()
                .expect("the picture has opaque pixels");
            // Eight, because the shipped picture is a resize of the drawing
            // and a resize moves a colour by a little.
            assert!(
                nearest <= 8,
                "the {name} colour is {nearest} away from anything in the picture"
            );
        }
    }

    #[test]
    fn a_picture_grows_in_whole_multiples() {
        let splash = shipped();
        let (w, h) = (splash.width, splash.height);
        // Room for three of it across and four down: three is the answer,
        // exactly, rather than something that fills the width.
        let fitted = splash.fit((w * 3 + 7, h * 4)).expect("room for three");
        assert_eq!(fitted, (w * 3, h * 3));
    }

    #[test]
    fn a_picture_shrinks_in_whole_fractions() {
        let splash = shipped();
        let (w, h) = (splash.width, splash.height);
        let fitted = splash.fit((w - 1, h)).expect("room for half");
        assert_eq!(fitted, (w / 2, h / 2));
        let fitted = splash.fit((w / 2 - 1, h)).expect("room for a third");
        assert_eq!(fitted, (w / 3, h / 3));
    }

    #[test]
    fn a_display_with_no_room_gets_no_picture() {
        let splash = shipped();
        assert_eq!(splash.fit((splash.width / 8, splash.height)), None);
        assert_eq!(splash.fit((splash.width, 0)), None);
    }

    #[test]
    fn the_height_holds_the_picture_back_as_well_as_the_width() {
        // A wide short strip of a display is the case a rule written against
        // the width alone would get wrong.
        let splash = shipped();
        let fitted = splash
            .fit((splash.width * 10, splash.height))
            .expect("room for one");
        assert_eq!(fitted, (splash.width, splash.height));
    }

    #[test]
    fn a_machine_can_bring_a_picture_of_its_own() {
        let path = std::env::temp_dir().join(format!("tos-splash-{}.png", std::process::id()));
        // A one pixel PNG: the smallest thing that is a picture and is not the
        // one tOS ships.
        let mut file = std::fs::File::create(&path).expect("picture file");
        file.write_all(&one_pixel_png()).expect("write");
        let splash = Splash::load(&path).expect("a picture");
        assert_eq!((splash.width, splash.height), (1, 1));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_file_that_is_not_a_picture_leaves_the_shipped_one() {
        let path = std::env::temp_dir().join(format!("tos-splash-bad-{}.png", std::process::id()));
        std::fs::write(&path, b"this is not a PNG").expect("write");
        let splash = Splash::load(&path).expect("the built-in picture");
        assert_eq!(splash, shipped(), "a bad file should not cost the picture");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_path_with_nothing_at_it_leaves_the_shipped_one() {
        let splash = Splash::load(Path::new("/nonexistent/tos/splash.png"));
        assert_eq!(splash, Some(shipped()));
    }

    /// A 1x1 opaque white PNG, written out by hand so the tests depend on no
    /// encoder.
    fn one_pixel_png() -> Vec<u8> {
        fn chunk(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
            let mut out = (body.len() as u32).to_be_bytes().to_vec();
            out.extend_from_slice(kind);
            out.extend_from_slice(body);
            // The CRC covers the type and the body, and nothing else.
            let mut checked = kind.to_vec();
            checked.extend_from_slice(body);
            out.extend_from_slice(&crc32(&checked).to_be_bytes());
            out
        }
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        let mut header = Vec::new();
        header.extend_from_slice(&1u32.to_be_bytes());
        header.extend_from_slice(&1u32.to_be_bytes());
        header.extend_from_slice(&[8, 6, 0, 0, 0]);
        png.extend_from_slice(&chunk(b"IHDR", &header));
        // One filter byte and four channels, stored uncompressed in a single
        // final deflate block.
        let raw = [0u8, 0xff, 0xff, 0xff, 0xff];
        let mut idat = vec![0x78, 0x01, 0x01, 0x05, 0x00, 0xfa, 0xff];
        idat.extend_from_slice(&raw);
        idat.extend_from_slice(&adler32(&raw).to_be_bytes());
        png.extend_from_slice(&chunk(b"IDAT", &idat));
        png.extend_from_slice(&chunk(b"IEND", &[]));
        png
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut value = 0xffff_ffffu32;
        for &byte in data {
            value ^= byte as u32;
            for _ in 0..8 {
                value = if value & 1 != 0 {
                    (value >> 1) ^ 0xedb8_8320
                } else {
                    value >> 1
                };
            }
        }
        value ^ 0xffff_ffff
    }

    fn adler32(data: &[u8]) -> u32 {
        let (mut a, mut b) = (1u32, 0u32);
        for &byte in data {
            a = (a + byte as u32) % 65521;
            b = (b + a) % 65521;
        }
        (b << 16) | a
    }
}
