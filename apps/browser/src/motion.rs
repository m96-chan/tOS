//! Which format a frame arrives in, and when the lossless one is worth asking
//! for.
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
//! # When a still is worth asking for
//!
//! The first version of this asked for a still 150 ms after the last frame,
//! and on an installed tOS in VirtualBox — two vCPUs, no GPU, a 1280x770
//! pane — that made a scroll flash. The measurements that say why were taken
//! on that machine's own engine:
//!
//! ```text
//! Page.captureScreenshot png, pane size    66 to 98 ms
//! Page.captureScreenshot jpeg, pane size   42 to 51 ms
//! jpeg q85 screencast while scrolling      about 42 fps, 24 to 27 ms gaps
//! ```
//!
//! A mouse-wheel notch makes the engine animate for about 100 ms and then
//! stop. A hand on a wheel produces notches 150 to 300 ms apart. At 150 ms of
//! frame quiet, *every notch* therefore ended in a lossless still, and what
//! was on the screen was JPEG frames, PNG, JPEG frames, PNG, several times a
//! second. On a page with any colour in it — a gradient, a picture, coloured
//! text, all of which 4:2:0 chroma treats differently from PNG — that
//! difference is visible at 1:1, and a scroll looks like it is flashing.
//!
//! So a still now waits for two kinds of quiet rather than one: no screencast
//! frame for [`REST_AFTER`], **and** no wheel notch or key for
//! [`INPUT_QUIET`]. A hand on the wheel produces JPEG frames and nothing else;
//! the PNG arrives once the hand stops. The two together are what make the
//! format change once per scroll instead of once per notch.
//!
//! # Which frame wins
//!
//! Two sources paint the same pane, and they can arrive out of order. A
//! screencast frame captured *before* the still was asked for can turn up
//! after it, because the still is a round trip to the engine and the frame was
//! already in the mailbox; and a still can come back after the page started
//! moving again, because it takes tens of milliseconds to encode — 66 to 98 of
//! them on the machine above.
//!
//! The rule is **a still counts only if the page stayed still for the whole of
//! it** — with one frame forgiven, because the still photographs itself. See
//! [`SHUTTER_FRAMES`]: `Page.captureScreenshot` forces a surface capture and
//! the screencast is watching that same surface, so every screenshot is
//! followed by exactly one screencast frame of the picture it just took. Two
//! or more frames in the window are the page moving; one is the shutter.
//!
//! That replaces an earlier rule, and the earlier one is worth recording
//! because of what it did with that shutter frame. It credited the still with
//! the wall-clock moment it was *asked for* — the earliest instant it could
//! depict — and compared that against each frame's `metadata.timestamp`. The
//! shutter frame is stamped about four milliseconds after the request, so it
//! counted as newer, and a JPEG of the page was painted over the PNG that had
//! just replaced it. Which cleared the tab's rest, which asked for another
//! still 150 ms later, which produced another shutter frame: **a loop, about
//! four times a second, on every page including one that nothing was
//! happening to at all.** That is the flashing, and the wheel only made it
//! worse by making the stills more frequent still.
//!
//! So a still is credited with the moment its **reply** arrived — the latest
//! instant it could depict — and a frame older than what is on screen is
//! dropped *and is not motion*, because it shows a moment that has already
//! been drawn. The shutter frame, stamped 35 to 48 ms before the reply, is
//! exactly such a frame. The loop has nothing to stand on.
//!
//! The clock all of that is measured on is the wall clock:
//! `Page.screencastFrame` carries `metadata.timestamp`, which CDP defines as
//! seconds since the epoch, and the engine is a child process on this machine,
//! so it is the same epoch this program reads with [`std::time::SystemTime`]
//! (checked on the VM: the two agree).

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The JPEG quality the screencast runs at. See the table above.
pub const QUALITY: u32 = 85;

/// How long without a screencast frame counts as the page having stopped.
///
/// Long enough that it is never tripped between two frames of a scroll: the
/// gap at q85 is about 17 ms on the host the format was chosen on and 24 to
/// 27 ms in the VirtualBox machine, so this is ten frames' worth of the slow
/// one. Short enough that letting go of the wheel and the text sharpening
/// still feel like one event rather than two.
///
/// It was 150 ms, which is longer than a wheel notch's hundred milliseconds of
/// animation and shorter than the 150 to 300 ms between two notches of a hand
/// on a wheel — so it fired in the gaps between them, once per notch. This is
/// the frame half of the fix; [`INPUT_QUIET`] is the half that settles it,
/// because no frame-quiet interval on its own can tell the gap between two
/// notches from the end of a scroll.
pub const REST_AFTER: Duration = Duration::from_millis(250);

/// How long after the last wheel notch or key the page is left alone.
///
/// A hand turning a wheel produces notches 150 to 300 ms apart, and each one
/// makes the engine animate for about 100 ms and then stop. Nothing about the
/// frames says whether a gap is the end of a scroll or the moment before the
/// next notch; the wheel does. So a still is not worth asking for until the
/// wheel has been quiet for longer than the longest gap a hand leaves —
/// 400 ms is 300 with room — and a scroll then costs one still at the end of
/// it rather than one per notch.
///
/// A notch is now an `Input.synthesizeScrollGesture` rather than a dispatched
/// wheel event, so the animation is 232 ms rather than about 100 and it
/// outlives the notch that asked for it. [`Motion::input`] is called at both
/// ends of one — when it is issued and when its reply says it has finished —
/// so this interval is counted from the end of the *animation* rather than
/// from the end of the hand. See `docs/design/browser.md`.
pub const INPUT_QUIET: Duration = Duration::from_millis(400);

/// How many screencast frames a still produces just by being taken.
///
/// `Page.captureScreenshot` forces a capture of the page's surface, and the
/// screencast is watching that same surface, so the screenshot shows up in it.
/// Probed against `chromium-shell` on a page nothing was happening to: eight
/// screenshots in a row, eight screencast frames, one each, every one stamped
/// about 4 ms after the request went out and 35 to 48 ms before the reply came
/// back — and not one frame in the three seconds either side of them.
/// `fromSurface=false` makes no difference.
///
/// So one frame inside a still's window is the still photographing itself, and
/// anything more is the page moving. `apps/browser/tests/engine.rs` asserts
/// the one, because it is the number this rule is built on and an engine that
/// changed it would otherwise change the policy quietly.
pub const SHUTTER_FRAMES: u32 = 1;

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
    /// When the person last turned the wheel or pressed a key, if they have.
    last_input: Option<Instant>,
    /// The capture time of what is on screen, in wall-clock seconds.
    painted_at: f64,
    /// A still has been painted and nothing has moved since.
    at_rest: bool,
    /// A still has been asked for and its reply has not come back.
    in_flight: bool,
    /// How many screencast frames have arrived since it was asked for. See
    /// [`SHUTTER_FRAMES`]: this is the whole of the test that decides whether
    /// the still may be painted.
    frames_in_flight: u32,
}

impl Motion {
    /// A tab that has just come to the front: nothing painted, nothing at
    /// rest, and the clock started so that a page which never paints gets its
    /// still one rest interval from now.
    pub fn new(now: Instant) -> Motion {
        Motion {
            last_frame: now,
            last_input: None,
            painted_at: 0.0,
            at_rest: false,
            in_flight: false,
            frames_in_flight: 0,
        }
    }

    /// The same, for a tab switch or a resize.
    ///
    /// A still that was in flight is forgotten along with everything else: its
    /// reply describes a page that is no longer the one in front, or one that
    /// is no longer the size it was.
    pub fn reset(&mut self, now: Instant) {
        *self = Motion::new(now);
    }

    /// A screencast frame arrived. `timestamp` is its `metadata.timestamp`.
    /// Returns whether it is worth decoding and painting.
    ///
    /// A frame older than what is on screen is dropped, **and it is not
    /// motion**: it shows a moment that has already been drawn, so the tab
    /// stays at rest and the rest timer is left alone. That is what the
    /// shutter frame after a still is, and letting it count as movement is
    /// what made the screen flash four times a second.
    ///
    /// Anything else is the page moving, so the rest timer restarts — and it
    /// is counted against a still that is in flight, because [`SHUTTER_FRAMES`]
    /// of them are the still itself and the rest are the page.
    pub fn motion_frame(&mut self, timestamp: Option<f64>, now: Instant) -> bool {
        if self.in_flight {
            self.frames_in_flight += 1;
        }
        if let Some(when) = timestamp {
            if when < self.painted_at {
                return false;
            }
            self.painted_at = when;
        }
        // A frame with no timestamp is a frame the engine described oddly, and
        // a frame in hand beats no frame.
        self.last_frame = now;
        self.at_rest = false;
        true
    }

    /// The person turned the wheel or pressed a key. See [`INPUT_QUIET`].
    pub fn input(&mut self, now: Instant) {
        self.last_input = Some(now);
    }

    /// Whether the page has been quiet long enough to be worth a lossless
    /// picture: no frame for [`REST_AFTER`], no input for [`INPUT_QUIET`],
    /// nothing already on its way, and no still already up.
    pub fn wants_still(&self, now: Instant) -> bool {
        !self.at_rest
            && !self.in_flight
            && now.duration_since(self.last_frame) >= REST_AFTER
            && match self.last_input {
                Some(at) => now.duration_since(at) >= INPUT_QUIET,
                // Nobody has touched this tab, so there is nobody to wait for.
                None => true,
            }
    }

    /// A still has been asked for.
    ///
    /// Nothing about the screen changes here. What starts is the window the
    /// rule is about: the frames that arrive between now and the reply are
    /// counted, and [`SHUTTER_FRAMES`] of them are free.
    pub fn still_requested(&mut self) {
        self.in_flight = true;
        self.frames_in_flight = 0;
    }

    /// The still came back, at wall-clock `replied_at`. Returns whether it
    /// should be decoded and painted.
    ///
    /// `false` means more frames arrived than the still's own: the page moved
    /// while the engine was drawing, what is on screen is newer than this, and
    /// the tab stays in motion so that the next still waits for quiet all over
    /// again.
    ///
    /// `true` marks the tab at rest and credits the still with the moment its
    /// reply arrived — the latest instant it could depict. Crediting it with
    /// the latest rather than the earliest is what makes the shutter frame,
    /// which is stamped tens of milliseconds before this, fall on the stale
    /// side and stay off the screen.
    pub fn still_arrived(&mut self, replied_at: f64) -> bool {
        if !self.in_flight {
            return false;
        }
        self.in_flight = false;
        let frames = std::mem::take(&mut self.frames_in_flight);
        if frames > SHUTTER_FRAMES {
            return false;
        }
        self.painted_at = replied_at;
        self.at_rest = true;
        true
    }

    /// The still could not be taken, or would not decode. The tab is marked at
    /// rest anyway, so that a page whose screenshots fail is asked once rather
    /// than fifty times a second until it moves again.
    pub fn still_failed(&mut self) {
        self.in_flight = false;
        self.frames_in_flight = 0;
        self.at_rest = true;
    }

    /// Whether a still has been asked for and not yet answered.
    pub fn still_in_flight(&self) -> bool {
        self.in_flight
    }

    /// Whether the last thing painted was a lossless still.
    pub fn at_rest(&self) -> bool {
        self.at_rest
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Long enough after everything that both kinds of quiet have run out.
    const QUIET: Duration = Duration::from_millis(500);

    /// The frame `Page.captureScreenshot` produces of its own accord: stamped
    /// a few milliseconds after the request and well before the reply, which
    /// is what the probe in `apps/browser/tests/engine.rs` measured.
    fn shutter(motion: &mut Motion, requested_at: f64, at: Instant) -> bool {
        motion.motion_frame(Some(requested_at + 0.004), at)
    }

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
    ///
    /// Including the frame the still takes of itself, which is the whole of
    /// what used to make this four stills a second for ever.
    #[test]
    fn a_page_that_stops_costs_one_still_and_then_nothing() {
        let start = Instant::now();
        let mut motion = Motion::new(start);
        let at = start + Duration::from_millis(17);
        assert!(motion.motion_frame(Some(1000.0), at));

        assert!(!motion.wants_still(at + Duration::from_millis(249)));
        let resting = at + REST_AFTER;
        assert!(motion.wants_still(resting));

        motion.still_requested();
        assert!(motion.still_in_flight());
        assert!(!motion.wants_still(resting), "one at a time");
        // Ninety milliseconds of engine, and the shutter frame turns up in
        // them.
        let requested_at = 1000.2;
        assert!(shutter(&mut motion, requested_at, resting));
        assert!(motion.still_arrived(requested_at + 0.09));
        assert!(motion.at_rest());

        // And an hour later it still wants nothing.
        assert!(!motion.wants_still(resting + Duration::from_secs(3600)));
    }

    /// The same, with the shutter frame arriving after the reply rather than
    /// before it. Either way it is the picture that is already on the screen.
    #[test]
    fn the_frame_a_still_takes_of_itself_is_not_the_page_moving() {
        let start = Instant::now();
        let mut motion = Motion::new(start);
        let resting = start + REST_AFTER;
        assert!(motion.wants_still(resting));
        let requested_at = 1000.0;
        motion.still_requested();
        assert!(motion.still_arrived(requested_at + 0.09));
        assert!(motion.at_rest());

        let late = resting + Duration::from_millis(95);
        assert!(
            !shutter(&mut motion, requested_at, late),
            "it shows what the still already showed"
        );
        assert!(
            motion.at_rest(),
            "and it must not clear the rest, or the next still is 250 ms away \
             and the screen flashes for ever"
        );
        assert!(!motion.wants_still(late + Duration::from_secs(3600)));
    }

    /// Two frames in the window are the page moving, and the still goes.
    #[test]
    fn frames_beyond_the_shutter_throw_the_still_away() {
        let start = Instant::now();
        let mut motion = Motion::new(start);
        let resting = start + REST_AFTER;
        assert!(motion.wants_still(resting));
        let requested_at = 1000.0;
        motion.still_requested();

        // The shutter, and then the page itself, at the VM's 25 ms gap.
        assert!(shutter(&mut motion, requested_at, resting));
        let moved = resting + Duration::from_millis(40);
        assert!(motion.motion_frame(Some(requested_at + 0.04), moved));
        assert!(
            !motion.still_arrived(requested_at + 0.09),
            "the page did not stay still for the whole of it"
        );
        assert!(!motion.at_rest(), "so the tab is still in motion");
        assert!(!motion.still_in_flight());

        // And the next still waits for quiet from that frame, not from before.
        assert!(!motion.wants_still(moved + Duration::from_millis(249)));
        assert!(motion.wants_still(moved + QUIET));
    }

    /// A reply with nothing but the shutter in between is painted, and that is
    /// the only thing that puts a tab at rest.
    #[test]
    fn a_still_with_no_frame_but_its_own_is_the_one_that_is_painted() {
        let start = Instant::now();
        let mut motion = Motion::new(start);
        motion.motion_frame(Some(1000.0), start);
        let resting = start + REST_AFTER;
        assert!(motion.wants_still(resting));
        motion.still_requested();
        assert!(motion.still_arrived(1000.4));
        assert!(motion.at_rest());
    }

    /// A hand on the wheel: notches 200 ms apart, the frames they cause dying
    /// out after each one, and not a single still until the hand stops.
    #[test]
    fn a_hand_on_the_wheel_is_not_interrupted_by_a_still() {
        let start = Instant::now();
        let mut motion = Motion::new(start);
        let mut at = start;
        for notch in 0..10u32 {
            motion.input(at);
            // About a hundred milliseconds of animation and then quiet until
            // the next notch: four frames at the VM's 25 ms gap.
            for frame in 1..=4u32 {
                let when = at + Duration::from_millis(frame as u64 * 25);
                let stamp = 1000.0 + notch as f64 * 0.2 + frame as f64 * 0.025;
                assert!(motion.motion_frame(Some(stamp), when));
            }
            // Every instant between this notch and the next is checked,
            // because one still anywhere in there is the flicker.
            for step in 0..20u64 {
                let when = at + Duration::from_millis(step * 10);
                assert!(!motion.wants_still(when), "notch {notch}, {step}0 ms in");
            }
            at += Duration::from_millis(200);
        }
        // The hand comes off, and the still arrives once the wheel has been
        // quiet for its interval.
        let last_input = at - Duration::from_millis(200);
        assert!(!motion.wants_still(last_input + Duration::from_millis(399)));
        assert!(motion.wants_still(last_input + INPUT_QUIET));
    }

    /// Frames can be quiet long before the wheel is, and that alone is not
    /// enough.
    #[test]
    fn frames_quiet_but_a_key_just_pressed_is_not_rest() {
        let start = Instant::now();
        let mut motion = Motion::new(start);
        motion.motion_frame(Some(1000.0), start);
        let quiet = start + REST_AFTER;
        assert!(motion.wants_still(quiet), "frames alone would allow it");
        motion.input(quiet);
        assert!(!motion.wants_still(quiet));
        assert!(!motion.wants_still(quiet + Duration::from_millis(399)));
        assert!(motion.wants_still(quiet + INPUT_QUIET));
    }

    /// A frame that was in the mailbox before a still that was painted,
    /// arriving after it. The timestamps still order that one, and it is not
    /// motion either.
    #[test]
    fn a_frame_from_before_a_painted_still_does_not_overwrite_it() {
        let start = Instant::now();
        let mut motion = Motion::new(start);
        motion.motion_frame(Some(1000.0), start);
        motion.still_requested();
        assert!(motion.still_arrived(1000.5));

        let late = start + Duration::from_millis(10);
        assert!(
            !motion.motion_frame(Some(1000.3), late),
            "captured before the still that is on screen, so it is stale"
        );
        assert!(
            motion.at_rest(),
            "and a moment already drawn is not movement"
        );
        assert!(!motion.wants_still(late + QUIET));
    }

    /// Motion wins a tie: a frame captured at the instant the still's reply
    /// came back is painted over it, because another is coming behind it.
    #[test]
    fn a_frame_from_the_same_instant_as_the_still_wins() {
        let start = Instant::now();
        let mut motion = Motion::new(start);
        motion.still_requested();
        assert!(motion.still_arrived(1000.0));
        assert!(
            motion.motion_frame(Some(1000.0), start),
            "not older than the still, so it goes up"
        );
        assert!(!motion.at_rest());
    }

    /// A tab switch forgets everything, including a still that was in flight:
    /// its reply is about the page that was left behind.
    #[test]
    fn switching_tabs_starts_the_policy_again() {
        let start = Instant::now();
        let mut motion = Motion::new(start);
        motion.motion_frame(Some(2000.0), start);
        motion.still_requested();
        assert!(motion.still_in_flight());

        let switched = start + Duration::from_secs(5);
        motion.reset(switched);
        assert!(!motion.at_rest());
        assert!(!motion.still_in_flight());
        assert!(!motion.still_arrived(2000.6), "nothing is owed a reply now");
        assert!(!motion.wants_still(switched));
        // A frame from the new tab's past is not compared against the old
        // tab's clock.
        assert!(motion.motion_frame(Some(1.0), switched));
        assert!(motion.wants_still(switched + QUIET));
    }

    /// A screenshot that fails is asked for once, not every pass.
    #[test]
    fn a_still_that_cannot_be_taken_is_not_asked_for_again() {
        let start = Instant::now();
        let mut motion = Motion::new(start);
        let resting = start + REST_AFTER;
        assert!(motion.wants_still(resting));
        motion.still_requested();
        motion.still_failed();
        assert!(!motion.still_in_flight());
        assert!(!motion.wants_still(resting + Duration::from_secs(10)));
        // Until the page moves, which is when it is worth trying again.
        let moved = resting + Duration::from_secs(10);
        motion.motion_frame(Some(3000.0), moved);
        assert!(motion.wants_still(moved + QUIET));
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
