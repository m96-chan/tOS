//! The mouse pointer: where it is, whether anybody should be able to see it,
//! and the arrow itself.
//!
//! Drawn in software, into the composited frame, after everything else. A DRM
//! hardware cursor plane is cheaper on the machines that have one — the scanout
//! engine moves it and no pixel of the frame is touched — but it is a plane the
//! nested and headless backends do not have, so a pointer built on it would
//! exist only on real hardware and no test could ever look at it. One software
//! arrow is the same code and the same picture on all three backends, and that
//! is what makes every behaviour here answerable by driving
//! [`crate::Compositor::handle_input`] and reading pixels back out. The plane is
//! an optimisation that can be added underneath this later.

use tos_render::{Rect, Surface};
use tos_term::Rgb;

/// The arrow, one character per pixel: `#` is the outline, `*` is the fill,
/// and `.` is nothing at all.
///
/// Hardcoded geometry rather than a character out of the font, the same way
/// `tos_font::boxdraw` draws U+2500 from the cell metrics rather than trusting
/// a face to have it: there is no codepoint for a pointer that every font
/// carries, and one that changed shape with the configured font would vanish
/// entirely into a face that has no arrow in it.
///
/// Two values rather than one, because what is underneath the arrow belongs to
/// whatever is being pointed at. A single-coloured arrow is invisible over text
/// that happens to be that colour, which for a pointer is not a cosmetic
/// failure: it is the one thing on screen whose whole job is to be findable.
/// The other obvious answer — inverting the cell under the pointer — needs no
/// geometry at all, but a cell is 8x16 at the smallest and several times that
/// on a HiDPI panel, so it points at a whole word rather than at a character
/// and gives no indication of which corner of itself is the hotspot.
#[rustfmt::skip]
const ARROW: [&str; 17] = [
    "#..........",
    "##.........",
    "#*#........",
    "#**#.......",
    "#***#......",
    "#****#.....",
    "#*****#....",
    "#******#...",
    "#*******#..",
    "#********#.",
    "#*****#####",
    "#**#**#....",
    "#*#.#**#...",
    "##..#**#...",
    "#....#**#..",
    ".....#**#..",
    ".....####..",
];

const ARROW_WIDTH: u32 = 11;
const ARROW_HEIGHT: u32 = ARROW.len() as u32;

/// How many display pixels one pixel of [`ARROW`] becomes.
///
/// Tied to the cell height and nothing else. The arrow has to keep its
/// proportion to the text it is being pointed at — an eleven pixel arrow on a
/// panel with 40 pixel cells is a speck — and the cell height is the one number
/// that already tracks both the panel's DPI and the configured font size, so
/// anchoring to it means a pointer that never needs its own setting. Rounded to
/// nearest rather than truncated so that the common 32 pixel cell gets a
/// doubled arrow instead of falling just short of one, and floored at 1 so that
/// a tiny font still leaves an arrow rather than nothing.
fn scale(cell_height: u32) -> u32 {
    ((cell_height + ARROW_HEIGHT / 2) / ARROW_HEIGHT).max(1)
}

/// The arrow's size in display pixels, for cells of this size.
pub fn size(cell: (u32, u32)) -> (u32, u32) {
    let scale = scale(cell.1);
    (ARROW_WIDTH * scale, ARROW_HEIGHT * scale)
}

/// Paint the arrow with its hotspot — the tip, the top left pixel of
/// [`ARROW`] — at `rect`'s corner.
///
/// `rect` is only read for that corner and for the scale; everything outside
/// the surface's clip is dropped by [`Surface::fill`], which is what lets the
/// arrow hang off the right and bottom edges the way a real one does instead of
/// being pushed back on screen where it would stop tracking the hand.
pub fn draw(surface: &mut Surface<'_>, rect: Rect, fill: Rgb, outline: Rgb) {
    let scale = (rect.height / ARROW_HEIGHT).max(1) as i32;
    for (row, line) in ARROW.iter().enumerate() {
        for (col, ch) in line.bytes().enumerate() {
            let color = match ch {
                b'*' => fill,
                b'#' => outline,
                _ => continue,
            };
            let x = rect.x + col as i32 * scale;
            let y = rect.y + row as i32 * scale;
            surface.fill(Rect::new(x, y, scale as u32, scale as u32), color);
        }
    }
}

/// Where the pointer is, whether it is being shown, and where the last frame
/// drew it.
#[derive(Debug, Default)]
pub struct Pointer {
    /// The hotspot, in display pixels.
    at: (u32, u32),
    /// A pointing device has been heard from.
    ///
    /// This is the whole of "does this machine have a mouse", and it is
    /// deliberately not the other answer. `tos_input::evdev` already probes a
    /// `Capabilities` per device and could be asked, but what it knows is
    /// whether some device node claims `BTN_LEFT` — which a dead trackpad, a
    /// receiver with nothing paired to it and a VM's emulated tablet that never
    /// reports all do. It also has nothing to say on the nested and headless
    /// backends, where there is no evdev to ask at all and whether a mouse
    /// exists depends on what the host terminal is willing to forward, which
    /// changes while the session is running. An event that has arrived is proof
    /// rather than a claim, it is the same rule on every backend, and it costs
    /// somebody with a mouse one twitch of it.
    seen: bool,
    /// Put away because somebody is typing.
    hidden: bool,
    /// The rectangle the last frame painted the arrow into, already clipped to
    /// the display.
    ///
    /// Pixels, not rows — unlike [`crate::ime::ImeContext::painted`], which is
    /// rows because a preedit belongs to a pane's grid and moves when that grid
    /// is resized under it. The pointer belongs to no grid: it is in screen
    /// space, it can sit over a divider or the status bar or the gap at the
    /// edge where the cells do not quite divide the panel, and a row of some
    /// pane is not a thing every position it can reach even has.
    painted: Option<Rect>,
}

impl Pointer {
    /// A pointing device reported a position. Returns whether the picture owes
    /// a frame because of it.
    pub fn moved_to(&mut self, x: u32, y: u32) -> bool {
        let changed = self.at != (x, y) || !self.seen || self.hidden;
        self.at = (x, y);
        self.seen = true;
        self.hidden = false;
        changed
    }

    /// Put the arrow away. Returns whether that was a change worth a frame.
    pub fn hide(&mut self) -> bool {
        let was_shown = self.seen && !self.hidden;
        self.hidden = true;
        was_shown
    }

    /// Where the arrow belongs this frame, or `None` when there is none to
    /// draw.
    pub fn rect(&self, cell: (u32, u32)) -> Option<Rect> {
        if !self.seen || self.hidden {
            return None;
        }
        let (width, height) = size(cell);
        Some(Rect::new(self.at.0 as i32, self.at.1 as i32, width, height))
    }

    /// Where the last frame drew it.
    pub fn painted(&self) -> Option<Rect> {
        self.painted
    }

    pub fn set_painted(&mut self, rect: Option<Rect>) {
        self.painted = rect;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_arrow_is_rectangular_and_has_an_outline_all_the_way_round_its_fill() {
        // The mask is written out as text, so a row one character short is a
        // typo that would silently shear the arrow rather than fail to
        // compile. And every filled pixel wants an outline pixel between it
        // and whatever is underneath, or the arrow disappears into a
        // background of its own colour, which is the entire reason there are
        // two values.
        for line in ARROW {
            assert_eq!(
                line.len() as u32,
                ARROW_WIDTH,
                "{line:?} is the wrong width"
            );
        }
        let at = |col: i32, row: i32| -> u8 {
            if col < 0 || row < 0 || col >= ARROW_WIDTH as i32 || row >= ARROW_HEIGHT as i32 {
                return b'.';
            }
            ARROW[row as usize].as_bytes()[col as usize]
        };
        for row in 0..ARROW_HEIGHT as i32 {
            for col in 0..ARROW_WIDTH as i32 {
                if at(col, row) != b'*' {
                    continue;
                }
                for (dx, dy) in [(-1, 0), (1, 0), (0, -1), (0, 1)] {
                    assert_ne!(
                        at(col + dx, row + dy),
                        b'.',
                        "the fill at {col},{row} touches the background"
                    );
                }
            }
        }
    }

    #[test]
    fn the_arrow_grows_with_the_cell_but_never_shrinks_below_its_own_pixels() {
        assert_eq!(scale(16), 1);
        assert_eq!(scale(32), 2);
        assert_eq!(scale(64), 4);
        // A cell shorter than the arrow still gets a whole arrow: half a
        // pointer is not a smaller pointer.
        assert_eq!(scale(8), 1);
        assert_eq!(scale(0), 1);
    }

    #[test]
    fn a_pointer_nothing_has_ever_moved_has_no_rectangle_at_all() {
        let mut pointer = Pointer::default();
        assert_eq!(pointer.rect((8, 16)), None);
        assert!(pointer.moved_to(40, 40));
        assert_eq!(pointer.rect((8, 16)), Some(Rect::new(40, 40, 11, 17)));
        assert!(pointer.hide());
        assert_eq!(pointer.rect((8, 16)), None);
        // Hiding twice is not news; a key repeat must not ask for a frame per
        // keystroke on top of the one the typing already costs.
        assert!(!pointer.hide());
        assert!(pointer.moved_to(40, 40));
    }
}
