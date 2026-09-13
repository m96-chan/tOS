//! What a frame touched.
//!
//! The renderer already knows how to draw only what changed — that is what
//! `retains_contents` and the per-row dirty flags in the terminal grid are
//! for. What nothing knew until now is *where* it drew, in pixels, after the
//! fact. A backend that composites into its own memory and then has to move
//! those pixels somewhere else needs exactly that: a description of the part
//! of the surface worth moving.
//!
//! The shape here is one horizontal span per row rather than a single
//! bounding box, and that choice is the whole point. A frame that moves the
//! pointer one pixel and turns the clock over damages a dozen rows near the
//! arrow and a dozen along the status bar, with eight hundred untouched rows
//! in between; their bounding box is the panel, and a backend that believed
//! it would copy four megabytes to move a mouse. Per-row spans cost one
//! `Vec` of two `u32`s per scanline — 6 KB for a 1280x800 panel — and answer
//! the question the copy actually asks, which is per row anyway because that
//! is how a framebuffer is laid out.
//!
//! Marking is per rectangle, never per pixel. Every drawing operation on a
//! [`Surface`](crate::Surface) knows the rectangle it is about to work inside
//! before it starts, so the bookkeeping is a short loop over that rectangle's
//! rows and nothing at all in the inner loop over its pixels. Over-reporting
//! is allowed and happens: a glyph marks its whole cell box although the
//! coverage mask leaves most of it alone. Under-reporting is not, because a
//! pixel that was drawn and not marked is a pixel that never reaches the
//! screen.

use crate::surface::Rect;

/// The horizontal extent of one row's damage, `start..end` in pixels.
///
/// `end <= start` means the row was not touched. There is no `Option` here
/// because there is one of these per scanline and they are walked over in
/// bulk; the empty case is a comparison rather than a discriminant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Span {
    start: u32,
    end: u32,
}

impl Span {
    const EMPTY: Span = Span { start: 0, end: 0 };

    fn is_empty(&self) -> bool {
        self.end <= self.start
    }

    fn add(&mut self, start: u32, end: u32) {
        if self.is_empty() {
            self.start = start;
            self.end = end;
            return;
        }
        self.start = self.start.min(start);
        self.end = self.end.max(end);
    }
}

/// The pixels of a surface that have been drawn on since this was last
/// cleared.
#[derive(Debug, Clone)]
pub struct Damage {
    width: u32,
    height: u32,
    spans: Vec<Span>,
    /// The range of rows with a non-empty span, so that clearing and walking
    /// are proportional to what was drawn rather than to the height of the
    /// panel. Without it, a frame that changed one cell would still pay eight
    /// hundred iterations to find it and eight hundred more to forget it —
    /// small, but this is the bookkeeping that is supposed to cost nothing.
    first: u32,
    last: u32,
}

impl Damage {
    /// A tracker for a `width` by `height` surface, with nothing damaged.
    pub fn new(width: u32, height: u32) -> Damage {
        Damage {
            width,
            height,
            spans: vec![Span::EMPTY; height as usize],
            first: height,
            last: 0,
        }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn is_empty(&self) -> bool {
        self.first >= self.last
    }

    /// Record that every pixel of `rect` may have changed.
    ///
    /// Clamped to the surface here rather than trusted from the caller. The
    /// callers are the drawing operations, which have already clipped, so the
    /// intersection is normally a no-op — but the result of this is used to
    /// index two buffers, and a rectangle that escaped its clip should cost a
    /// wasted comparison rather than a copy that runs off the end of a
    /// mapping.
    pub fn mark(&mut self, rect: Rect) {
        let rect = rect.intersect(&Rect::new(0, 0, self.width, self.height));
        if rect.is_empty() {
            return;
        }
        let (top, bottom) = (rect.y as u32, rect.bottom() as u32);
        let (left, right) = (rect.x as u32, rect.right() as u32);
        for span in &mut self.spans[top as usize..bottom as usize] {
            span.add(left, right);
        }
        self.first = self.first.min(top);
        self.last = self.last.max(bottom);
    }

    /// Record that the whole surface may have changed.
    pub fn mark_all(&mut self) {
        self.mark(Rect::new(0, 0, self.width, self.height));
    }

    /// Add everything `other` has recorded.
    ///
    /// This is how a buffer that is not written every frame catches up: it is
    /// owed the union of the frames it missed, and the union is taken a frame
    /// at a time as they go past. Rows are merged into one span rather than
    /// kept apart, so two narrow marks at opposite ends of a row become one
    /// wide one — the same over-reporting the rectangle granularity already
    /// allows, on the axis where a copy is sequential anyway.
    pub fn union(&mut self, other: &Damage) {
        debug_assert_eq!(self.height, other.height, "damage for a different size");
        for (y, start, end) in other.spans() {
            self.spans[y as usize].add(start, end);
        }
        if !other.is_empty() {
            self.first = self.first.min(other.first);
            self.last = self.last.max(other.last);
        }
    }

    /// Forget everything, without giving up the allocation.
    pub fn clear(&mut self) {
        if self.is_empty() {
            // `first` is above `last` while nothing is damaged, which is not a
            // slice range.
            return;
        }
        for span in &mut self.spans[self.first as usize..self.last as usize] {
            *span = Span::EMPTY;
        }
        self.first = self.height;
        self.last = 0;
    }

    /// The damaged rows, as `(y, start, end)` with `end` exclusive.
    ///
    /// Rows that were not touched are not yielded at all, which is what makes
    /// a copy driven by this proportional to what changed.
    pub fn spans(&self) -> impl Iterator<Item = (u32, u32, u32)> + '_ {
        (self.first..self.last).filter_map(move |y| {
            let span = self.spans[y as usize];
            (!span.is_empty()).then_some((y, span.start, span.end))
        })
    }

    /// How many pixels are covered by the spans.
    ///
    /// For tests and for anything wanting to know what a frame cost to move;
    /// nothing in the drawing path needs it.
    pub fn pixels(&self) -> u64 {
        self.spans()
            .map(|(_, start, end)| (end - start) as u64)
            .sum()
    }

    /// The smallest rectangle containing everything damaged, when anything is.
    pub fn bounds(&self) -> Option<Rect> {
        let mut left = u32::MAX;
        let mut right = 0;
        for (_, start, end) in self.spans() {
            left = left.min(start);
            right = right.max(end);
        }
        if left >= right {
            return None;
        }
        Some(Rect::new(
            left as i32,
            self.first as i32,
            right - left,
            self.last - self.first,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_damaged_to_begin_with() {
        let damage = Damage::new(64, 32);
        assert!(damage.is_empty());
        assert_eq!(damage.spans().count(), 0);
        assert_eq!(damage.bounds(), None);
    }

    #[test]
    fn a_rectangle_damages_its_own_rows_and_nothing_between_them() {
        let mut damage = Damage::new(64, 32);
        damage.mark(Rect::new(10, 4, 6, 3));
        assert_eq!(
            damage.spans().collect::<Vec<_>>(),
            vec![(4, 10, 16), (5, 10, 16), (6, 10, 16)]
        );
        assert_eq!(damage.pixels(), 18);
    }

    #[test]
    fn rows_nothing_touched_are_not_copied() {
        // The case a bounding box gets wrong, and the reason this is per row:
        // the arrow near the middle of the panel and the clock in the status
        // bar span almost the whole height between them and almost none of
        // the pixels.
        let mut damage = Damage::new(1280, 800);
        damage.mark(Rect::new(400, 200, 12, 20));
        damage.mark(Rect::new(1100, 780, 60, 16));
        assert_eq!(damage.spans().count(), 36);
        assert_eq!(damage.pixels(), 12 * 20 + 60 * 16);
        // Whereas the bounding box of the two is most of the screen.
        let bounds = damage.bounds().expect("something was drawn");
        assert!(
            bounds.width as u64 * bounds.height as u64 > 100 * damage.pixels(),
            "the bounding box is not much larger than the spans: {bounds:?}"
        );
    }

    #[test]
    fn overlapping_marks_on_a_row_become_one_span() {
        let mut damage = Damage::new(64, 32);
        damage.mark(Rect::new(4, 0, 4, 1));
        damage.mark(Rect::new(6, 0, 10, 1));
        assert_eq!(damage.spans().collect::<Vec<_>>(), vec![(0, 4, 16)]);
    }

    #[test]
    fn separate_marks_on_a_row_are_joined_across_the_gap() {
        // Deliberate: a row is copied with one `memcpy`, so the gap costs the
        // bytes it spans and a second span would cost a second call.
        let mut damage = Damage::new(64, 32);
        damage.mark(Rect::new(0, 0, 2, 1));
        damage.mark(Rect::new(60, 0, 4, 1));
        assert_eq!(damage.spans().collect::<Vec<_>>(), vec![(0, 0, 64)]);
    }

    #[test]
    fn marking_outside_the_surface_is_clamped() {
        let mut damage = Damage::new(8, 4);
        damage.mark(Rect::new(-4, -2, 16, 12));
        assert_eq!(
            damage.spans().collect::<Vec<_>>(),
            vec![(0, 0, 8), (1, 0, 8), (2, 0, 8), (3, 0, 8)]
        );
        let mut away = Damage::new(8, 4);
        away.mark(Rect::new(100, 100, 4, 4));
        assert!(away.is_empty());
    }

    #[test]
    fn an_empty_rectangle_damages_nothing() {
        let mut damage = Damage::new(8, 4);
        damage.mark(Rect::new(2, 2, 0, 4));
        damage.mark(Rect::new(2, 2, 4, 0));
        assert!(damage.is_empty());
    }

    #[test]
    fn clearing_forgets_everything() {
        let mut damage = Damage::new(8, 4);
        damage.mark_all();
        assert_eq!(damage.pixels(), 32);
        damage.clear();
        assert!(damage.is_empty());
        assert_eq!(damage.pixels(), 0);
        // And the tracker still works afterwards, which a stale row range
        // would break by hiding the rows outside it.
        damage.mark(Rect::new(0, 3, 8, 1));
        assert_eq!(damage.spans().collect::<Vec<_>>(), vec![(3, 0, 8)]);
    }

    #[test]
    fn a_union_owes_both_sets_of_rows() {
        let mut owed = Damage::new(64, 32);
        let mut frame = Damage::new(64, 32);
        frame.mark(Rect::new(0, 0, 4, 1));
        owed.union(&frame);
        frame.clear();
        frame.mark(Rect::new(10, 20, 4, 1));
        owed.union(&frame);
        assert_eq!(
            owed.spans().collect::<Vec<_>>(),
            vec![(0, 0, 4), (20, 10, 14)]
        );
        // And a union with nothing in it changes nothing.
        owed.union(&Damage::new(64, 32));
        assert_eq!(
            owed.spans().collect::<Vec<_>>(),
            vec![(0, 0, 4), (20, 10, 14)]
        );
    }
}
