//! The animation a wheel notch starts, and the ticks that carry it out.
//!
//! A terminal mouse reports notches: discrete, instantaneous, with no
//! acceleration and no fractional scroll. A page expects a wheel. Everything
//! between the two is here, and none of it touches a socket — this module is
//! arithmetic on a distance and a clock, so the rule can be read and tested
//! without an engine, a terminal or a pane.
//!
//! The rule is the one a browser in a window follows. A notch does not move
//! the page; it adds to a **distance still owed**, and an animation that is
//! already running simply gets a further target. Every [`TICK`] a fraction
//! [`K`] of what is owed goes out as one `mouseWheel` event, so the page eases
//! towards where the hand asked for and a notch that arrives meanwhile makes
//! the next step *larger* rather than starting a new animation. Nothing is
//! ever in flight, nothing waits for a reply, and there is no state to get
//! wrong when a second notch lands in the middle of the first.
//!
//! ## What this replaced, and why
//!
//! `Input.synthesizeScrollGesture` — the engine animating a distance itself —
//! was measured against `chromium-shell` 153 in docker on two vCPUs at
//! 1280×770 on ja.wikipedia, as the scroll offset each screencast frame
//! carried. Five notches 100 ms apart, one gesture per coalesced pile:
//!
//! ```text
//! 12 12 12 11 12 9 [0] 12 23 23 24 23 18 [0] 10 12 23 25 22 …
//! ```
//!
//! Every gesture starts from a standstill and ends at one, because a gesture
//! is an animation with its own beginning and end and the hand's notches do
//! not line up with them. The person's word for what that feels like was
//! "ドッ、ドッ" — pulsing — and ten notches 60 ms apart were worse, because the
//! piles get bigger: `… 48 70 25 44 [0] 45 46 47 … 47 [116] 12 [0] 5 5 13 17 …`
//! is two dead stops, a 116-pixel jump and a slow tail.
//!
//! No amount of tuning the gesture fixes that. The stops are not a frame rate
//! or a speed; they are the seams between one engine animation and the next,
//! and there is one seam per pile whatever the pile is worth. So the animation
//! moved to this side of the socket, where there is only ever *one* of it and
//! a notch extends it instead of queueing behind it.
//!
//! ## What the animator gives, measured the same way
//!
//! Same engine, same page, same size, read off `metadata.scrollOffsetY` frame
//! by frame — this is `apps/browser/tests/engine.rs` with `--nocapture`:
//!
//! ```text
//! one notch     36 25 18 12 9 6 4 4 6
//! five @100 ms  36 25 18 12 9 6 4 39 27 19 13 9 7 41 28 20 14 10 7 41 29 20
//!               14 10 7 5 39 28 [0] 33 9 7 5 4 7
//! ten @60 ms    36 25 18 12 45 31 22 15 47 33 23 16 47 33 23 16 47 33 [0] 23
//!               52 37 26 66 34 24 17 48 33 23 16 47 33 23 16 [0] 81 39 11 8
//!               6 4 4 5
//! ```
//!
//! One notch is nine frames over 132 to 143 ms and it is over. Five notches
//! are one animation whose step *grows* where each notch lands — 4 then 39, 7
//! then 41 — and it is done 173 to 175 ms after the hand comes off. Ten
//! notches 60 ms apart never fall below 11 pixels a frame while the hand is on
//! the wheel, reach 81 in the middle, and are done 181 to 198 ms after the
//! last one. (Two runs each; what moves between them is the engine's frame
//! cadence rather than the ticks, which are the same numbers every time.)
//!
//! The `[0]`s are not seams. They are single frames in which the engine had no
//! new delta to apply, because on that host a screencast frame is 16.7 ms and
//! a tick is 16, so twice a second a frame falls in a gap and the next one
//! carries two ticks; the page never stands still for more than 40 ms and
//! never for two frames running. On the machine this is really for — 24 to
//! 27 ms a frame — every frame holds a tick or two and they cannot happen at
//! all. The gesture's `[0]`s were one per pile, always at the same place, and
//! always followed by a step that started over from nothing.
//!
//! Nothing anywhere caps a step, which is the other half of what the person
//! reported: the scrolling had "an upper limit on speed, unlike a normal
//! browser". A tick sends [`K`] of whatever is owed, so ten notches owe more
//! and therefore move further, and a pile is paid off *faster* rather than
//! *longer* — eleven ticks are 96% of any amount at all, which is why "when I
//! stop the wheel I want it to stop" holds for a flick as well as for a notch.
//!
//! The page sees ordinary `wheel` events — nine for a notch on its own, and
//! about seven per notch when they run together — so a site that listens for
//! them, or calls `preventDefault` on them, behaves as it would in a window,
//! and a notch over an inner scroller scrolls that scroller. `mouseWheel`
//! deltas are applied by the engine on the frame after the event (measured:
//! one event, one frame, about 12 ms), so the frames follow the ticks.

use std::time::{Duration, Instant};

/// How long one step of the animation lasts.
///
/// A screencast frame is 17 ms on the host the format was chosen on and 24 to
/// 27 ms in the VirtualBox machine, and the engine turns one `mouseWheel` into
/// one frame about 12 ms later. 16 ms is a display's frame and slightly faster
/// than either, which is the right side to be on: a tick that arrives while
/// the last one is still on its way is coalesced into the same frame and costs
/// nothing, whereas a tick that arrives late is a frame in which the page did
/// not move.
pub const TICK: Duration = Duration::from_millis(16);

/// The fraction of what is still owed that one tick sends.
///
/// This is the whole of the feel, and the profiles it gives against
/// `chromium-shell` 153 on two vCPUs at 1280×770 are at the top of this file.
/// What they come to, with [`MIN_STEP`] at 4 pixels:
///
/// | `K` | one notch | five @100 ms | ten @60 ms | tail of floor steps |
/// |------|-------------|--------------|--------------|-------------------|
/// | 0.22 | 193 ms | +222 ms | +245 ms | 4 |
/// | 0.25 | 166 ms | +207 ms | +216 ms | 4 |
/// | 0.3  | 132–143 ms | +173–175 ms | +181–198 ms | 2 |
///
/// The middle two columns are how long after the last notch the page was still
/// moving, which is the number the person's second report is about: "when I
/// stop the wheel I want it to stop." The deadline is 250 ms. 0.22 misses it
/// by five milliseconds on a host with two spare cores and nothing else to do,
/// which is no margin at all; 0.25 has 34; 0.3 has 52, and it is also the one
/// that shortens the tail, because four minimum-sized steps at the end of an
/// animation is the creeping a floor was supposed to remove.
///
/// The cost of 0.3 is that one notch on its own is 140 ms rather than the 180
/// to 260 this was aimed at. That band came from a run with a 1-pixel floor,
/// where the length *was* the tail: at 0.22 with no floor a notch took
/// sixteen frames and 340 ms, the last six of them a pixel each. With a real
/// floor the animation is nine frames that move 36, 25, 18, 12, 9, 6, 4, 4 and
/// 6 pixels, which nobody could mistake for a jump — and a browser's own wheel
/// animation in a window is about the same length.
pub const K: f64 = 0.3;

/// The smallest step a tick sends, in CSS pixels.
///
/// A geometric series never arrives, so something has to end it. Without a
/// floor, one notch at [`K`] spends its last third of a second moving a pixel
/// at a time — measured at 0.22 with no floor: sixteen frames, the last six of
/// them 1 pixel, about 340 ms of visible creeping after the page had for all
/// practical purposes arrived.
///
/// Four pixels is the smallest step that still reads as movement rather than
/// as a twitch at a pane's size, and at [`K`] it turns the tail into exactly
/// two of them: the measured end of a notch is `… 6 4 4 6`. A tick that would
/// leave less than a floor behind takes the remainder instead, which is where
/// that last 6 comes from — the animation never spends a whole tick on a
/// fraction of a pixel, and the last frame is a real step rather than a
/// rounding error.
pub const MIN_STEP: f64 = 4.0;

/// How far behind its own schedule the animation may fall and still be caught
/// up on.
///
/// Ticks are counted on the wall clock rather than from whenever the loop got
/// round to the last one: a pass held up by a slow paint fires the ticks it
/// missed, so nine ticks span 128 ms whatever the loop was doing, and that is
/// what "it stops when I stop" rests on. Beyond a tenth of a second the
/// program was not running at all — a pane that was not scheduled, a still
/// that went wrong — and replaying twenty ticks at once would be the jump this
/// module exists to remove, so the schedule restarts from now instead.
const CATCH_UP: Duration = Duration::from_millis(100);

/// One step of the animation: where to send it, and how far.
///
/// The sign is the page's and not the protocol's inverse: `deltaY` of +120 on
/// a `mouseWheel` leaves `window.scrollY` at 120, which is the opposite of
/// `Input.synthesizeScrollGesture`'s `yDistance` and was checked against the
/// engine rather than read off the documentation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Step {
    /// The pointer position the event carries, in page pixels.
    pub at: (i32, i32),
    /// `deltaX` and `deltaY`, in CSS pixels.
    pub delta: (f64, f64),
}

/// One page's scrolling: what is owed, where to send it, and when the next
/// step is due.
///
/// One of these rather than one per tab, for the reason [`crate::motion`] is
/// one: only the tab in front is being scrolled, and a switch drops what the
/// tab behind was owed rather than keeping a second copy to send into a page
/// nobody is looking at.
#[derive(Debug, Default)]
pub struct Animator {
    /// What is still to be scrolled, in CSS pixels: x then y.
    owed: (f64, f64),
    /// Where the pointer was for the most recent notch.
    ///
    /// The animation is sent where the wheel was last turned rather than where
    /// it was first turned, because a pointer that has moved onto a different
    /// scroller is a person who means the one they are pointing at now.
    at: (i32, i32),
    /// When the next tick is due. `None` is nothing owed and nothing to do.
    next: Option<Instant>,
}

impl Animator {
    /// A notch, as the distance it is worth on each axis.
    ///
    /// It extends the target rather than replacing it, and it does not restart
    /// the clock: an animation that is already running keeps its tick times
    /// and simply has more to pay, so the next step is *larger* than the last
    /// instead of beginning again at the top of a new curve. That is the
    /// difference between momentum and pulsing.
    pub fn notch(&mut self, at: (i32, i32), distance: (f64, f64), now: Instant) {
        self.at = at;
        self.owed.0 += distance.0;
        self.owed.1 += distance.1;
        if self.owed == (0.0, 0.0) {
            // Two notches that cancel, which is a hand changing its mind.
            self.next = None;
            return;
        }
        if self.next.is_none() {
            // Nothing was animating, so the first step is due at once: a wheel
            // that waited a frame before moving would be a wheel with lag.
            self.next = Some(now);
        }
    }

    /// How long until the next step is due, if anything is owed.
    ///
    /// This is what the loop's `poll` waits for: a tick that is late is a
    /// frame in which the page did not move.
    pub fn until(&self, now: Instant) -> Option<Duration> {
        self.next.map(|next| next.saturating_duration_since(now))
    }

    /// The step to send now, if one is due.
    ///
    /// Called until it says `None`, which is both "not yet" and "nothing left"
    /// — the caller has no decision to make either way.
    pub fn tick(&mut self, now: Instant) -> Option<Step> {
        let next = self.next?;
        if now < next {
            return None;
        }
        let delta = (step(self.owed.0), step(self.owed.1));
        self.owed.0 -= delta.0;
        self.owed.1 -= delta.1;
        self.next = if self.owed == (0.0, 0.0) {
            None
        } else if now.saturating_duration_since(next) > CATCH_UP {
            Some(now + TICK)
        } else {
            Some(next + TICK)
        };
        if delta == (0.0, 0.0) {
            // Nothing was owed on either axis, which `notch` does not allow
            // and a caller asking again after the end would otherwise get.
            self.next = None;
            return None;
        }
        Some(Step { at: self.at, delta })
    }

    /// Nothing is owed any more: the tab in front changed, the pane was
    /// resized, or the event could not be sent.
    pub fn forget(&mut self) {
        self.owed = (0.0, 0.0);
        self.next = None;
    }

    /// What is still to be scrolled. For the tests and the status of the loop.
    pub fn owed(&self) -> (f64, f64) {
        self.owed
    }
}

/// How far one tick moves an axis that is owed `owed`.
///
/// Three rules in one expression: a fraction [`K`] of what is left, never less
/// than [`MIN_STEP`], and never more than what is left. The third is also the
/// end: a step that would leave behind less than a floor takes the remainder,
/// so the animation finishes on a real step rather than spending one more tick
/// on a hundredth of a pixel.
///
/// There is deliberately no fourth rule. Nothing caps the step from above, so
/// a hand that owes a thousand pixels moves three hundred on the next tick —
/// a ceiling there is what makes a scroll feel like it has a speed limit,
/// which is the one thing a wheel in a window does not have.
fn step(owed: f64) -> f64 {
    let left = owed.abs();
    if left == 0.0 {
        return 0.0;
    }
    let wanted = (left * K).max(MIN_STEP);
    if wanted >= left - MIN_STEP {
        return owed;
    }
    wanted.copysign(owed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::WHEEL_PIXELS;

    /// Every step of one animation, driven from `start` as fast as the clock
    /// allows, with the tick times it would really have had.
    fn profile(animator: &mut Animator, start: Instant) -> Vec<(f64, f64)> {
        let mut steps = Vec::new();
        let mut now = start;
        for _ in 0..400 {
            match animator.tick(now) {
                Some(step) => steps.push(step.delta),
                None => match animator.until(now) {
                    Some(left) => now += left.max(Duration::from_micros(1)),
                    None => break,
                },
            }
        }
        steps
    }

    fn down(animator: &mut Animator, now: Instant) {
        animator.notch((10, 20), (0.0, WHEEL_PIXELS), now);
    }

    /// A notch owes a notch, and the ticks pay it off exactly.
    #[test]
    fn a_notch_owes_a_notch_and_the_ticks_pay_it() {
        let start = Instant::now();
        let mut animator = Animator::default();
        assert_eq!(animator.owed(), (0.0, 0.0));
        assert_eq!(
            animator.until(start),
            None,
            "an idle animator asks for no wake-up"
        );

        down(&mut animator, start);
        assert_eq!(animator.owed(), (0.0, WHEEL_PIXELS));
        assert_eq!(
            animator.until(start),
            Some(Duration::ZERO),
            "the first step of a new animation is due at once"
        );

        let steps = profile(&mut animator, start);
        let moved: f64 = steps.iter().map(|delta| delta.1).sum();
        assert!(
            (moved - WHEEL_PIXELS).abs() < 1e-9,
            "the animation moved {moved} for a notch of {WHEEL_PIXELS}"
        );
        assert_eq!(animator.owed(), (0.0, 0.0));
        assert_eq!(animator.until(start), None, "a finished animation is idle");
    }

    /// The steps are the geometric series, with the floor under them and
    /// nothing above them, and never a step past what is owed.
    #[test]
    fn the_steps_decay_and_never_overshoot() {
        let start = Instant::now();
        let mut animator = Animator::default();
        down(&mut animator, start);

        let mut left = WHEEL_PIXELS;
        let mut now = start;
        let mut steps = Vec::new();
        while let Some(step) = animator.tick(now) {
            let delta = step.delta.1;
            assert!(delta > 0.0, "a downward notch moved {delta}");
            assert!(
                delta <= left + 1e-9,
                "a step of {delta} for {left} owed is an overshoot"
            );
            assert!(
                delta >= MIN_STEP.min(left) - 1e-9,
                "a step of {delta} is under the floor"
            );
            // Above the floor the step is exactly the fraction, and there is
            // no ceiling on it.
            if left * K > MIN_STEP && left - left * K > MIN_STEP {
                assert!(
                    (delta - left * K).abs() < 1e-9,
                    "a step of {delta} is not {K} of {left}"
                );
            }
            left -= delta;
            steps.push(delta);
            now += TICK;
        }
        assert!(left.abs() < 1e-9, "{left} was never paid");
        assert!(
            steps.len() >= 6 && steps.len() <= 16,
            "one notch took {} ticks: {steps:?}",
            steps.len()
        );
        // The decay: every step is smaller than the last until the floor, and
        // the floor is never so long a tail that it reads as creeping.
        let floored = steps
            .iter()
            .filter(|step| **step <= MIN_STEP + 1e-9)
            .count();
        assert!(floored <= 3, "a tail of {floored} minimum steps: {steps:?}");
    }

    /// A notch in the middle of an animation makes the next step bigger. It
    /// does not restart the animation, and it does not wait for it.
    #[test]
    fn a_second_notch_extends_the_target_without_restarting() {
        let start = Instant::now();
        let mut animator = Animator::default();
        down(&mut animator, start);

        let first = animator.tick(start).expect("a first step").delta.1;
        let second = animator.tick(start + TICK).expect("a second step").delta.1;
        assert!(
            second < first,
            "the animation is not decaying: {first}, {second}"
        );

        let owed = animator.owed().1;
        let was = animator.until(start + TICK).expect("the animation runs");
        down(&mut animator, start + TICK + Duration::from_millis(4));
        assert_eq!(
            animator.owed().1,
            owed + WHEEL_PIXELS,
            "the notch replaced the target instead of extending it"
        );
        assert_eq!(
            animator.until(start + TICK),
            Some(was),
            "the notch restarted the clock"
        );

        let next = animator
            .tick(start + TICK + TICK)
            .expect("a third step")
            .delta
            .1;
        assert!(
            next > second,
            "the notch did not add momentum: {second} then {next}"
        );
        assert!(
            next > first,
            "a second notch should outrun the first step of the first: {first} then {next}"
        );
    }

    /// Nothing caps the step, so a hand that rolls fast scrolls far — and it
    /// still stops when it stops, because what it costs to pay off a hundred
    /// times as much is a handful of ticks and not a hundred times as many.
    #[test]
    fn nothing_caps_the_step_and_a_big_pile_still_settles() {
        let start = Instant::now();
        let mut one = Animator::default();
        down(&mut one, start);
        let mut ten = Animator::default();
        let mut hundred = Animator::default();
        for _ in 0..10 {
            down(&mut ten, start);
        }
        for _ in 0..100 {
            down(&mut hundred, start);
        }

        let small = profile(&mut one, start);
        let large = profile(&mut ten, start);
        let huge = profile(&mut hundred, start);
        assert!(
            (large[0].1 - 10.0 * small[0].1).abs() < 1e-9,
            "ten notches should move ten times as far on the first tick: {} against {}",
            large[0].1,
            small[0].1
        );
        assert!(
            (huge[0].1 - 100.0 * small[0].1).abs() < 1e-9,
            "a hundred notches hit a ceiling at {}",
            huge[0].1
        );
        // Ten times the distance is a few ticks more, not ten times the ticks:
        // the tail is the same length and only the decay in front of it grows,
        // by log(10) ÷ log(1 ÷ (1 - K)) ≈ 6.5 of them.
        assert!(
            large.len() <= small.len() + 8 && huge.len() <= small.len() + 15,
            "{} ticks for one notch, {} for ten, {} for a hundred",
            small.len(),
            large.len(),
            huge.len()
        );
        let moved: f64 = large.iter().map(|delta| delta.1).sum();
        assert!(
            (moved - 10.0 * WHEEL_PIXELS).abs() < 1e-9,
            "{moved} for ten notches"
        );
    }

    /// The two axes are one animation but two sums, and a horizontal notch
    /// neither steals from nor waits for a vertical one.
    #[test]
    fn the_axes_are_independent() {
        let start = Instant::now();
        let mut animator = Animator::default();
        animator.notch((1, 2), (0.0, WHEEL_PIXELS), start);
        animator.notch((1, 2), (-WHEEL_PIXELS, 0.0), start);
        assert_eq!(animator.owed(), (-WHEEL_PIXELS, WHEEL_PIXELS));

        let step = animator.tick(start).expect("a step");
        assert!(step.delta.0 < 0.0 && step.delta.1 > 0.0, "{:?}", step.delta);
        assert_eq!(
            step.delta.0, -step.delta.1,
            "equal distances should move equally"
        );

        let steps = profile(&mut animator, start);
        let x: f64 = step.delta.0 + steps.iter().map(|delta| delta.0).sum::<f64>();
        let y: f64 = step.delta.1 + steps.iter().map(|delta| delta.1).sum::<f64>();
        assert!((x + WHEEL_PIXELS).abs() < 1e-9, "x moved {x}");
        assert!((y - WHEEL_PIXELS).abs() < 1e-9, "y moved {y}");
    }

    /// An animation only runs where the wheel was last turned.
    #[test]
    fn the_animation_follows_the_pointer() {
        let start = Instant::now();
        let mut animator = Animator::default();
        animator.notch((10, 20), (0.0, WHEEL_PIXELS), start);
        assert_eq!(animator.tick(start).expect("a step").at, (10, 20));
        animator.notch((70, 90), (0.0, WHEEL_PIXELS), start + TICK);
        assert_eq!(animator.tick(start + TICK).expect("a step").at, (70, 90));
    }

    /// A tab that is left, or a pane that is resized, owes nothing.
    #[test]
    fn forget_clears_what_is_owed() {
        let start = Instant::now();
        let mut animator = Animator::default();
        down(&mut animator, start);
        animator.tick(start).expect("a step");
        assert_ne!(animator.owed(), (0.0, 0.0));

        animator.forget();
        assert_eq!(animator.owed(), (0.0, 0.0));
        assert_eq!(animator.until(start), None);
        assert_eq!(
            animator.tick(start + TICK),
            None,
            "a forgotten animation ticks"
        );

        // And the next notch starts cleanly rather than finding a stuck clock.
        down(&mut animator, start + TICK);
        assert_eq!(animator.until(start + TICK), Some(Duration::ZERO));
    }

    /// Two notches that cancel leave nothing to animate.
    #[test]
    fn opposite_notches_cancel() {
        let start = Instant::now();
        let mut animator = Animator::default();
        animator.notch((1, 1), (0.0, WHEEL_PIXELS), start);
        animator.notch((1, 1), (0.0, -WHEEL_PIXELS), start);
        assert_eq!(animator.owed(), (0.0, 0.0));
        assert_eq!(animator.until(start), None);
        assert_eq!(animator.tick(start), None);
    }

    /// The schedule is the wall clock: a loop that was held up fires the ticks
    /// it missed, so the animation ends when it would have ended.
    #[test]
    fn a_late_pass_catches_up() {
        let start = Instant::now();
        let mut animator = Animator::default();
        down(&mut animator, start);
        animator.tick(start).expect("a first step");

        // Three ticks' worth of a slow paint, and three steps are due.
        let late = start + TICK * 3;
        for _ in 0..3 {
            assert!(animator.tick(late).is_some(), "a missed tick was dropped");
        }
        assert_eq!(animator.tick(late), None, "a fourth tick was not due");

        // But a stall longer than the animation is not replayed at once.
        let mut stalled = Animator::default();
        down(&mut stalled, start);
        stalled.tick(start).expect("a first step");
        let much_later = start + Duration::from_secs(1);
        assert!(stalled.tick(much_later).is_some());
        assert_eq!(
            stalled.tick(much_later),
            None,
            "a stall replayed the whole animation in one pass"
        );
    }
}
