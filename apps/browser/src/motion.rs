//! Which format a frame arrives in, and which of two frames is on screen.
//!
//! # JPEG while it moves, PNG when it stops
//!
//! `Page.startScreencast` is bounded by the engine's own single-threaded
//! encode of each frame, which is why the format is the frame rate. Measured
//! on a 1280x770 pane, two cores, a scrolling ja.wikipedia page, and
//! unchanged from two cores to eight:
//!
//! ```text
//! format     fps     kB/frame    gap p50 / p95 / max
//! png       33.8      320        28 / 37 / 39 ms
//! jpeg q70  60.0      139        17 / 18 / 20 ms
//! jpeg q85  57.8      185        about 17 ms
//! jpeg q95  40.0      268
//! jpeg q100 27.8      387
//! ```
//!
//! `Page.captureScreenshot` in a loop is 10 to 12 frames a second in every
//! format including `png` with `optimizeForSpeed` and lossless webp, so there
//! is no fast lossless path in CDP to prefer: it is JPEG or it is half the
//! frame rate.
//!
//! **Chromium's screencast JPEG is 4:2:0 at every quality** — the encoder
//! hard-codes a sampling factor of 2x2,1x1,1x1 — so coloured text smears a
//! little whatever quality is asked for, and quality only decides how much is
//! left of the luma underneath it. At q70 the halo around blue link text is
//! visible at 1:1. At q85 the difference from PNG needs 3x zoom to find. The
//! person looked at both against the PNG and chose 85, and that is what
//! [`QUALITY`] is.
//!
//! **So: JPEG while the page is moving, PNG the moment it stops.** Text that
//! is being read is always lossless; the lossy frames are only ever the ones
//! scrolling past, which is exactly the trade VNC and RDP make and for the
//! same reason. A page that never moves costs one still and then nothing at
//! all — no screencast frames, no polling, no repainting.
//!
//! # Which frame wins
//!
//! Two sources now paint the same pane, and they can arrive out of order. A
//! screencast frame captured *before* the still was asked for can turn up
//! after it, because the still is a round trip to the engine and the frame
//! was already in the mailbox; and a still can come back after the page
//! started moving again, because it takes tens of milliseconds to encode.
//! Painting either of those over the other would put a stale picture on
//! screen and leave it there — at rest nothing arrives to correct it.
//!
//! The rule is **the newer capture wins**, and the clock both are measured
//! against is the wall clock: `Page.screencastFrame` carries
//! `metadata.timestamp`, which CDP defines as seconds since the epoch, and
//! the engine is a child process on this machine, so it is the same epoch
//! this program reads with [`std::time::SystemTime`]. A still has no
//! timestamp of its own, so it is credited with the moment it was *asked
//! for* — the earliest instant it could possibly depict.
//!
//! Crediting it with the request rather than the reply is what decides the
//! one case the timestamps cannot order: a frame captured while the
//! screenshot was being taken. That frame counts as newer and is painted over
//! the still. **Motion wins a tie**, because a page that is producing frames
//! will produce another in seventeen milliseconds and correct any mistake,
//! while a still wrongly dropped leaves the old picture up for as long as the
//! page stays still.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The JPEG quality the screencast runs at. See the table above.
pub const QUALITY: u32 = 85;

/// How long without a screencast frame counts as the page having stopped.
///
/// Long enough that it is not tripped between two frames of a scroll — the
/// gap at q85 is about 17 ms and its worst case 20 — and short enough that
/// letting go of the wheel and the text sharpening feel like one event rather
/// than two. About nine frames' worth.
pub const REST_AFTER: Duration = Duration::from_millis(150);

/// Wall-clock seconds, on the clock CDP's `TimeSinceEpoch` uses.
pub fn now_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs_f64())
        .unwrap_or(0.0)
}

/// Whether the tab in front is moving, and what is on its screen.
///
/// One of these, not one per tab: only the tab in front has a screencast and
/// only the tab in front is painted, so a switch resets this rather than
/// keeping a second copy that would be wrong by the time it was used.
#[derive(Debug)]
pub struct Motion {
    /// When the last screencast frame arrived, on the monotonic clock.
    last_frame: Instant,
    /// The capture time of what is on screen, in wall-clock seconds.
    painted_at: f64,
    /// A still has been painted and nothing has moved since.
    at_rest: bool,
}

impl Motion {
    /// A tab that has just come to the front: nothing painted, nothing at
    /// rest, and the clock started so that a page which never paints gets its
    /// still one rest interval from now.
    pub fn new(now: Instant) -> Motion {
        Motion {
            last_frame: now,
            painted_at: 0.0,
            at_rest: false,
        }
    }

    /// The same, for a tab switch or a resize.
    pub fn reset(&mut self, now: Instant) {
        *self = Motion::new(now);
    }

    /// A screencast frame arrived. `timestamp` is its
    /// `metadata.timestamp`. Returns whether it is worth decoding and
    /// painting.
    ///
    /// Either way the page is moving again, so the rest timer restarts and
    /// the next still will not be asked for until it has run out.
    pub fn motion_frame(&mut self, timestamp: Option<f64>, now: Instant) -> bool {
        self.last_frame = now;
        self.at_rest = false;
        match timestamp {
            // Older than what is on screen: this is the frame that was in the
            // mailbox while the still was being taken.
            Some(when) if when < self.painted_at => false,
            Some(when) => {
                self.painted_at = when;
                true
            }
            // A frame with no timestamp is a frame the engine described
            // oddly, and a frame in hand beats no frame.
            None => true,
        }
    }

    /// Whether the page has been still long enough to be worth a lossless
    /// picture.
    pub fn wants_still(&self, now: Instant) -> bool {
        !self.at_rest && now.duration_since(self.last_frame) >= REST_AFTER
    }

    /// The still came back, having been asked for at wall-clock
    /// `requested_at`. Returns whether it should be painted.
    ///
    /// Nothing is recorded when the request goes out, only when it comes
    /// back: a screenshot that fails must not leave this saying a still is on
    /// screen. `false` means a screencast frame captured after the request
    /// was painted while the engine was drawing this one, so the page is
    /// moving and the still is already out of date.
    pub fn still_arrived(&mut self, requested_at: f64) -> bool {
        if self.painted_at > requested_at {
            return false;
        }
        self.painted_at = requested_at;
        self.at_rest = true;
        true
    }

    /// The still could not be taken. The tab is marked at rest anyway, so
    /// that a page whose screenshots fail is asked once rather than fifty
    /// times a second until it moves again.
    pub fn still_failed(&mut self) {
        self.at_rest = true;
    }

    /// Whether the last thing painted was a lossless still.
    pub fn at_rest(&self) -> bool {
        self.at_rest
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A page that is scrolling: frames in order, every one painted, and no
    /// still ever asked for.
    #[test]
    fn a_moving_page_is_all_motion_frames_and_no_stills() {
        let start = Instant::now();
        let mut motion = Motion::new(start);
        let mut at = start;
        for tick in 1..60u32 {
            at += Duration::from_millis(17);
            assert!(motion.motion_frame(Some(1000.0 + tick as f64 * 0.017), at));
            assert!(!motion.wants_still(at), "frame {tick}");
            assert!(!motion.at_rest());
        }
    }

    /// A page that stops: one still, and then nothing however long it is left.
    #[test]
    fn a_page_that_stops_costs_one_still_and_then_nothing() {
        let start = Instant::now();
        let mut motion = Motion::new(start);
        let at = start + Duration::from_millis(17);
        assert!(motion.motion_frame(Some(1000.0), at));

        assert!(!motion.wants_still(at + Duration::from_millis(149)));
        let resting = at + Duration::from_millis(150);
        assert!(motion.wants_still(resting));

        let requested = 1000.2;
        assert!(motion.still_arrived(requested));
        assert!(motion.at_rest());
        // And an hour later it still wants nothing.
        assert!(!motion.wants_still(resting + Duration::from_secs(3600)));
    }

    /// A frame captured before the still was asked for, arriving after it.
    #[test]
    fn a_frame_from_before_the_still_does_not_overwrite_it() {
        let start = Instant::now();
        let mut motion = Motion::new(start);
        motion.motion_frame(Some(1000.0), start);
        assert!(motion.still_arrived(1000.5), "the still is newer");

        let late = start + Duration::from_millis(10);
        assert!(
            !motion.motion_frame(Some(1000.3), late),
            "a frame captured before the still was asked for is stale"
        );
        // But it did count as movement, so the next still waits again.
        assert!(!motion.at_rest());
        assert!(!motion.wants_still(late + Duration::from_millis(100)));
        assert!(motion.wants_still(late + REST_AFTER));
    }

    /// A still that comes back after the page has started moving again.
    #[test]
    fn a_still_does_not_land_on_top_of_a_newer_motion_frame() {
        let start = Instant::now();
        let mut motion = Motion::new(start);
        let requested = 1000.0;
        // While the engine is drawing the still, the page moves.
        assert!(motion.motion_frame(Some(1000.1), start));
        assert!(
            !motion.still_arrived(requested),
            "the still is older than what is on screen"
        );
        assert!(!motion.at_rest(), "and the page is still moving");
    }

    /// Motion wins a tie: a frame captured at the instant the still was
    /// asked for is painted over it, because another frame is coming and a
    /// still that is dropped leaves nothing to correct it.
    #[test]
    fn a_frame_from_the_same_instant_as_the_still_wins() {
        let start = Instant::now();
        let mut motion = Motion::new(start);
        let requested = 1000.0;
        assert!(motion.still_arrived(requested));
        assert!(
            motion.motion_frame(Some(requested), start),
            "not older than the still, so it goes up"
        );
    }

    /// A tab switch forgets everything: the new tab's first frame is newer
    /// than nothing, and its rest timer starts now.
    #[test]
    fn switching_tabs_starts_the_policy_again() {
        let start = Instant::now();
        let mut motion = Motion::new(start);
        motion.motion_frame(Some(2000.0), start);
        motion.still_arrived(2000.5);
        assert!(motion.at_rest());

        let switched = start + Duration::from_secs(5);
        motion.reset(switched);
        assert!(!motion.at_rest());
        assert!(!motion.wants_still(switched));
        // A frame from the new tab's past is not compared against the old
        // tab's clock.
        assert!(motion.motion_frame(Some(1.0), switched));
        assert!(motion.wants_still(switched + REST_AFTER));
    }

    /// A screenshot that fails is asked for once, not every pass.
    #[test]
    fn a_still_that_cannot_be_taken_is_not_asked_for_again() {
        let start = Instant::now();
        let mut motion = Motion::new(start);
        let resting = start + REST_AFTER;
        assert!(motion.wants_still(resting));
        motion.still_failed();
        assert!(!motion.wants_still(resting + Duration::from_secs(10)));
        // Until the page moves, which is when it is worth trying again.
        let moved = resting + Duration::from_secs(10);
        motion.motion_frame(Some(3000.0), moved);
        assert!(motion.wants_still(moved + REST_AFTER));
    }

    /// The clock the timestamps are compared on is the one CDP uses.
    #[test]
    fn the_clock_is_seconds_since_the_epoch() {
        let seconds = now_seconds();
        // Somewhere between 2020 and 2100, which is enough to catch a
        // milliseconds-or-seconds mistake and nothing else.
        assert!(seconds > 1_577_836_800.0, "{seconds}");
        assert!(seconds < 4_102_444_800.0, "{seconds}");
    }
}
