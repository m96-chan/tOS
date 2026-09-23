//! The animation a wheel notch starts, the ticks that carry it out, and the
//! thread that keeps them on time.
//!
//! A terminal mouse reports notches: discrete, instantaneous, with no
//! acceleration and no fractional scroll. A page expects a wheel. Everything
//! between the two is here.
//!
//! Two things live in this file and they are deliberately separable.
//! [`Animator`] is the curve — arithmetic on a distance and a clock, no
//! socket, no thread, testable without an engine or a terminal. [`Wheel`] is
//! the thread that drives it: its own clock, its own `mouseWheel` events, and
//! a main loop that only ever says "a notch, here, on this tab" and "forget
//! it".
//!
//! ## The rule
//!
//! **Every notch is its own curve, and the curves are added together.** A
//! notch of 120 pixels is delivered over [`D`] following an ease-out — fast
//! at the start, slowing to nothing at the end — and a notch that arrives
//! while others are still running neither resets them nor waits for them: it
//! starts a curve of its own beside them, and each [`TICK`] every curve is
//! asked how far it should have got by now. What goes on the wire is the sum
//! of what they have not been given yet.
//!
//! That the curves are read from the wall clock rather than accumulated is the
//! whole of why this can live on a thread of its own and why a late tick costs
//! nothing: a notch that should have delivered 71 pixels by now delivers 71
//! whether it was asked at the right moment, four milliseconds late, or twice
//! in the same millisecond. There is no distance in flight to lose and no
//! backlog to replay.
//!
//! ## What this replaced, and why
//!
//! First `Input.synthesizeScrollGesture` — the engine animating a distance
//! itself — which pulsed because a gesture is an animation with its own
//! beginning and end and the hand's notches do not fall on them; there is one
//! dead stop per pile whatever the pile is worth. `docs/design/browser.md`
//! keeps the measurements.
//!
//! Then an **exponential approach**: one distance owed, and each tick sent a
//! fraction `K` = 0.3 of the remainder with a floor of 4 pixels. That is the
//! shape a lot of browsers use and it was measured the same way as everything
//! else here — the scroll offset each screencast frame carries, against
//! `chromium-shell` in docker on two vCPUs at 1280×770. One notch on its own
//! was good: `36 25 18 12 9 6 4 4 6`, nine frames and over. A steady hand — a
//! notch every 100 ms, which is what a person actually does — was not:
//!
//! ```text
//! 25 18 12 9 6 4 | 39 27 19 13 9 7 | 41 28 20 14 10 | 43 30 21 15 …
//! ```
//!
//! Every notch is delivered front-loaded, because the fraction is taken of
//! everything owed at once, so the page lurches where a notch lands and creeps
//! where one does not: a tenfold swing inside every notch, about ten times a
//! second. On the VM the person's word for it was that the page "shakes up and
//! down". No value of `K` fixes it — the swing *is* the exponential, and a
//! smaller `K` only makes the lurches further apart. So the exponential is
//! gone, and what replaced it is a curve per notch.
//!
//! ## What the additive curve gives, measured the same way
//!
//! Same engine, same page, same size, read off `metadata.scrollOffsetY` frame
//! by frame. This is `apps/browser/tests/engine.rs`, which asserts the shape
//! of all three and prints the numbers with `--nocapture`:
//!
//! ```text
//! one notch     17 16 14 13 12 10 9 8 7 5 4 3 2
//! steady hand   17 16 14 13 12 10 | 22 24 21 19 16 14 | 20 25 22 19 17 14 |
//!               15 26 23 20 18 15 | 12 25 44 18 [0] 29 | 21 24 22 19 16 14 |
//!               11 9 7 6 5 3 2 1
//! fast hand     17 16 14 27 27 25 35 36 32 37 41 35 37 42 36 35 42 [0] 37 32
//!               44 38 33 43 39 74 40 34 39 41 35 [0] 79 36 36 42 37 32 27 22
//!               18 15 11 9 6 4 3 1
//! ```
//!
//! The steady hand is the case the exponential lost. A notch every 100 ms
//! swings between 12 and 26 pixels a frame over the middle of the run — 2.2
//! times, against the exponential's ten — and the seam where one notch's curve
//! takes over from the last one's is not visible in the numbers, let alone on
//! a screen. The `44` and the `[0]` beside it are one frame that carried two
//! ticks and one that carried none, which is the screencast's 16.7 ms cadence
//! beating against the 16 ms tick and not the curve; on the machine this is
//! really for, where a frame is 24 to 27 ms, every frame holds a tick or two
//! and it cannot happen.
//!
//! The fast hand — twelve notches 50 ms apart, quicker than anybody really
//! rolls — never stalls for two frames running and stops 268 ms after the last
//! notch. One notch on its own is fourteen frames over 227 ms, which is longer
//! than the exponential's 140 and is the price: a curve that ends gently ends
//! later than one that is cut off by a floor.
//!
//! Starved of a core — the same three at `--cpus=1` rather than 2 — the
//! numbers barely move: `17 16 14 13 12 10 9 8 7 5 4 3 2` again for a notch,
//! 10 to 26 for the steady hand, 241 ms to settle after the fast one. That is
//! the thread doing its job. On the loop, where this used to be, a single core
//! is exactly where the ticks piled up.
//!
//! The page sees ordinary `wheel` events — about fourteen for a notch on its
//! own — so a site that listens for them, or calls `preventDefault` on them,
//! behaves as it would in a window, and a notch over an inner scroller scrolls
//! that scroller. `mouseWheel` deltas are applied by the engine on the frame
//! after the event (measured: one event, one frame, about 12 ms), so the
//! frames follow the ticks.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
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

/// How long one notch takes to be delivered in full.
///
/// This is the whole of the feel. Measured against `chromium-shell` in docker
/// on two vCPUs at 1280×770, as the scroll offset each screencast frame
/// carries — a steady hand is a notch every 100 ms, which is what a person
/// rolling a wheel produces:
///
/// | `D` | one notch settles in | a steady hand swings by |
/// |--------|----------------------|-------------------------|
/// | 150 ms | ~200 ms | 34 down to 8 — it pulses again |
/// | 220 ms | 227 ms | 2.2× (12 to 26) |
/// | 250 ms | ~290 ms | 2× (13 to 27) |
/// | 300 ms | ~340 ms | steadiest of the four |
///
/// The trade is between how steady a hand on the wheel looks and how long a
/// single notch takes, and what sets it is how much the curves overlap: at
/// 100 ms between notches, `D` = 220 keeps two and a bit curves running at any
/// moment, which is enough to fill the gaps between them. Below about 150 they
/// stop overlapping at all and the pulsing comes straight back in a new shape
/// — the last notch's curve is finished before the next one arrives, which is
/// a dead stop by another route. Above 300 a single notch stops feeling like a
/// notch: a third of a second to move 120 pixels is a page that lags the hand.
///
/// 220 is the shortest that still overlaps, which is the side to be on: it is
/// the only one of the four where nothing is being traded away. The measured
/// profiles, at two vCPUs and at one, are at the top of this file.
pub const D: Duration = Duration::from_millis(220);

/// How long the thread sleeps when there is nothing to animate.
///
/// A notch wakes it, so this is only how long it takes to notice that it has
/// been told to stop — which happens once, at the end of the program.
const IDLE: Duration = Duration::from_millis(250);

/// How far one notch has been delivered, as a fraction, `x` of the way through
/// [`D`].
///
/// Ease-out squared: `1 − (1 − x)²`. Its speed is `2(1 − x)`, so a notch
/// starts at twice its average pace and slows steadily to a stop — which is
/// what makes the end of a scroll a settling rather than a cut. Nothing here
/// is tuned; the tuning is [`D`].
fn ease(x: f64) -> f64 {
    let x = x.clamp(0.0, 1.0);
    1.0 - (1.0 - x) * (1.0 - x)
}

/// One notch of one axis, and how much of it has gone out.
#[derive(Debug, Clone, Copy)]
struct Notch {
    /// When the wheel was turned. The curve is read from this and the clock,
    /// never from the last tick, so a missed tick loses nothing.
    started_at: Instant,
    /// What this notch is worth, in CSS pixels, signed.
    amount: f64,
    /// How much of `amount` has already been sent.
    delivered: f64,
}

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

/// Where a step goes.
///
/// A trait rather than a connection, because this module has no business
/// knowing what CDP is and because the thread underneath it is worth testing
/// without an engine: a fake that records when it was called is all a test of
/// the clock needs. `apps/browser/src/app.rs` has the one implementation that
/// is not a fake.
pub trait Dispatch: Send + Sync {
    /// Put one step on the wire. An error is a socket that has gone, and it
    /// ends the animation rather than being retried.
    fn send(&self, step: Step) -> Result<(), String>;
}

/// One page's scrolling: the notches still being delivered, where to send
/// them, and when the next step is due.
///
/// One of these rather than one per tab, for the reason [`crate::motion`] is
/// one: only the tab in front is being scrolled, and a switch drops what the
/// tab behind was owed rather than keeping a second copy to send into a page
/// nobody is looking at.
#[derive(Debug, Default)]
pub struct Animator {
    /// The notches still being delivered, one list per axis. A notch is
    /// appended and nothing else is touched — that is the whole of what makes
    /// a second notch add to the movement instead of restarting it.
    x: Vec<Notch>,
    y: Vec<Notch>,
    /// Where the pointer was for the most recent notch.
    ///
    /// The animation is sent where the wheel was last turned rather than where
    /// it was first turned, because a pointer that has moved onto a different
    /// scroller is a person who means the one they are pointing at now.
    at: (i32, i32),
    /// When the next tick is due. `None` is nothing running and nothing to do.
    next: Option<Instant>,
}

impl Animator {
    /// A notch, as the distance it is worth on each axis.
    ///
    /// It appends a curve and resets nothing: the notches already running keep
    /// their own start times and their own remaining distance, and this one is
    /// simply added to the sum. An axis given zero is given no curve, so a
    /// vertical notch never puts a horizontal one in the list.
    pub fn notch(&mut self, at: (i32, i32), distance: (f64, f64), now: Instant) {
        self.at = at;
        for (axis, amount) in [(&mut self.x, distance.0), (&mut self.y, distance.1)] {
            if amount == 0.0 {
                continue;
            }
            axis.push(Notch {
                started_at: now,
                amount,
                delivered: 0.0,
            });
        }
        if self.next.is_none() && !(self.x.is_empty() && self.y.is_empty()) {
            // A tick from now rather than at once: at the instant a notch
            // arrives its curve has delivered nothing, so a step taken here
            // would be a step of zero pixels.
            self.next = Some(now + TICK);
        }
    }

    /// How long until the next step is due, if anything is running.
    ///
    /// This is what the thread sleeps for. `Some(ZERO)` is "now".
    pub fn until(&self, now: Instant) -> Option<Duration> {
        self.next.map(|next| next.saturating_duration_since(now))
    }

    /// The step to send now, if one is due.
    ///
    /// `None` is "not yet", "nothing left", and also "every curve happens to
    /// be exactly where it was a moment ago" — the caller has no decision to
    /// make between them, because all three mean there is nothing to put on
    /// the wire.
    pub fn tick(&mut self, now: Instant) -> Option<Step> {
        let next = self.next?;
        if now < next {
            return None;
        }
        let delta = (deliver(&mut self.x, now), deliver(&mut self.y, now));
        self.next = if self.x.is_empty() && self.y.is_empty() {
            None
        } else if now.saturating_duration_since(next) > TICK {
            // More than a whole tick late: the thread was not scheduled. The
            // curves are read from the clock, so nothing was lost and there is
            // nothing to replay; the schedule simply starts again from here.
            Some(now + TICK)
        } else {
            Some(next + TICK)
        };
        if delta == (0.0, 0.0) {
            return None;
        }
        Some(Step { at: self.at, delta })
    }

    /// Nothing is being delivered any more: the tab in front changed, the tab
    /// was closed, or the event could not be sent.
    pub fn forget(&mut self) {
        self.x.clear();
        self.y.clear();
        self.next = None;
    }

    /// The pane changed size, so the point the animation is sent at may be
    /// outside the page.
    ///
    /// The notches are kept — a person's hand is still on the wheel and a
    /// resize is not a reason for the page to stop dead — and only the point
    /// is brought back inside the new viewport. See `docs/design/browser.md`.
    pub fn resized(&mut self, page: (i32, i32)) {
        self.at = (
            self.at.0.clamp(0, (page.0 - 1).max(0)),
            self.at.1.clamp(0, (page.1 - 1).max(0)),
        );
    }

    /// What is still to be delivered. For the tests and the status of the
    /// loop.
    pub fn owed(&self) -> (f64, f64) {
        (owed(&self.x), owed(&self.y))
    }

    /// Where the next step would be sent.
    pub fn at(&self) -> (i32, i32) {
        self.at
    }
}

/// How much one axis owes.
fn owed(notches: &[Notch]) -> f64 {
    notches
        .iter()
        .map(|notch| notch.amount - notch.delivered)
        .sum()
}

/// Ask every notch of one axis where it should have got to by `now`, and
/// return what none of them has been given yet. Notches that have arrived are
/// dropped.
fn deliver(notches: &mut Vec<Notch>, now: Instant) -> f64 {
    let whole = D.as_secs_f64();
    let mut step = 0.0;
    notches.retain_mut(|notch| {
        let x = now
            .saturating_duration_since(notch.started_at)
            .as_secs_f64()
            / whole;
        let want = if x >= 1.0 {
            // Exactly the amount, so that what a notch delivers in the end is
            // the notch and not the notch plus a rounding error.
            notch.amount
        } else {
            notch.amount * ease(x)
        };
        step += want - notch.delivered;
        notch.delivered = want;
        x < 1.0
    });
    step
}

/// The animator on a thread of its own.
///
/// # Why it is not on the loop
///
/// It was, and on the installed machine the page moved like this, frame by
/// frame: `-420 -126 -90 -8 -14 -138 -310 …`. Several ticks' worth in one
/// frame and then almost nothing. The loop decodes a JPEG frame in about 9 ms,
/// writes 2.9 MB into shared memory and handles whatever the terminal said,
/// and while it is doing that it is not sending wheel events; the ticks it
/// missed then went out together, and the engine applied them as one jump.
///
/// The animation cannot be a thing the loop does when it gets round to it. So
/// it is not: this thread sleeps until the next tick, wakes, asks the curves
/// where they are, and sends. Nothing it does is blocked by a frame, and
/// nothing the loop does delays a tick by more than the time it takes to add a
/// notch under a mutex.
///
/// The loop keeps two jobs. It says what the hand did — [`Wheel::notch`], and
/// [`Wheel::forget`] when the tab in front changes or goes — and it reads
/// [`Wheel::activity`], which is how the ticks this thread sent count as input
/// for [`crate::motion`] without this thread touching that state at all.
pub struct Wheel {
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// What the loop and the animator thread share.
struct Shared {
    state: Mutex<State>,
    /// Knocked on when a notch arrives, when the pile is dropped, and when the
    /// thread is told to stop — so that none of the three waits out a tick.
    wake: Condvar,
    stop: AtomicBool,
    /// The clock [`Shared::activity`] is counted on.
    epoch: Instant,
    /// When a step was last put on the wire, in nanoseconds since `epoch`;
    /// zero is "never". An atomic rather than a callback, because what reads
    /// it is the loop's own [`crate::motion::Motion`] and a second thread
    /// writing into that would be two owners of one piece of state.
    activity: AtomicU64,
}

/// The pile and where it goes.
struct State {
    animator: Animator,
    /// The tab the notches belong to, and the socket they go out on. Both are
    /// the loop's to say, and both arrive with every notch — which is what
    /// makes a notch on a different tab start a different animation without
    /// this module knowing what a tab is.
    tab: String,
    to: Option<Arc<dyn Dispatch>>,
}

impl Wheel {
    /// Start the thread. It sleeps until there is something to animate.
    pub fn start() -> Wheel {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                animator: Animator::default(),
                tab: String::new(),
                to: None,
            }),
            wake: Condvar::new(),
            stop: AtomicBool::new(false),
            epoch: Instant::now(),
            activity: AtomicU64::new(0),
        });
        let thread = std::thread::spawn({
            let shared = Arc::clone(&shared);
            move || animate(&shared)
        });
        Wheel {
            shared,
            thread: Some(thread),
        }
    }

    /// The hand turned the wheel `distance` at `at`, over the tab `tab`, whose
    /// connection is `to`.
    ///
    /// A notch for a different tab from the last one drops what the last one
    /// had left: there is only ever one animation, and it belongs to the tab
    /// in front.
    pub fn notch(&self, tab: &str, to: Arc<dyn Dispatch>, at: (i32, i32), distance: (f64, f64)) {
        let Ok(mut state) = self.shared.state.lock() else {
            return;
        };
        if state.tab != tab {
            state.animator.forget();
            state.tab = tab.to_string();
        }
        state.to = Some(to);
        state.animator.notch(at, distance, Instant::now());
        drop(state);
        self.shared.wake.notify_all();
    }

    /// Drop what the tab in front had left, and let go of its connection.
    ///
    /// "Forget tab T" and "forget" are the same act here, because there is
    /// only ever one pile and the loop only ever forgets the tab in front — on
    /// a switch, on a close, and when the socket it was going out on failed.
    pub fn forget(&self) {
        let Ok(mut state) = self.shared.state.lock() else {
            return;
        };
        state.animator.forget();
        state.tab.clear();
        // The connection goes with it: a thread holding a socket open for a
        // page nobody is looking at is a page that cannot be closed.
        state.to = None;
    }

    /// The pane changed size. See [`Animator::resized`].
    pub fn resized(&self, page: (i32, i32)) {
        if let Ok(mut state) = self.shared.state.lock() {
            state.animator.resized(page);
        }
    }

    /// When this thread last put something on the wire, if it ever has.
    ///
    /// The loop feeds it to [`crate::motion::Motion::input`] every pass, which
    /// is what keeps a lossless still out of the middle of a scroll: the quiet
    /// interval runs from the last tick of the animation rather than from the
    /// last notch of the hand.
    pub fn activity(&self) -> Option<Instant> {
        match self.shared.activity.load(Ordering::Relaxed) {
            0 => None,
            nanos => Some(self.shared.epoch + Duration::from_nanos(nanos)),
        }
    }

    /// What is still to be delivered. For the tests and the status of the
    /// loop.
    pub fn owed(&self) -> (f64, f64) {
        match self.shared.state.lock() {
            Ok(state) => state.animator.owed(),
            Err(_) => (0.0, 0.0),
        }
    }

    /// Stop the thread and wait for it. Idempotent, because [`Drop`] does it
    /// too.
    pub fn stop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.shared.wake.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        // Whatever socket it was sending on is released here rather than
        // whenever this happens to be dropped.
        self.forget();
    }
}

impl Drop for Wheel {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The thread: sleep until the next tick, ask the curves, send.
///
/// The step is worked out under the lock and sent outside it, so that a notch
/// arriving from the loop waits for arithmetic rather than for a socket.
fn animate(shared: &Shared) {
    loop {
        let sending = {
            let Ok(mut state) = shared.state.lock() else {
                return;
            };
            loop {
                if shared.stop.load(Ordering::SeqCst) {
                    return;
                }
                let now = Instant::now();
                let wait = match state.animator.until(now) {
                    Some(left) if left.is_zero() => break,
                    Some(left) => left,
                    // Nothing to animate. A notch knocks, so this is only how
                    // long it takes to notice `stop`.
                    None => IDLE,
                };
                let Ok((next, _)) = shared.wake.wait_timeout(state, wait) else {
                    return;
                };
                state = next;
            }
            let due = state.animator.tick(Instant::now());
            due.and_then(|step| state.to.clone().map(|to| (to, step)))
        };
        let Some((to, step)) = sending else {
            continue;
        };
        if to.send(step).is_err() {
            // The socket is gone, which everything that cares hears on its own
            // account. What must not happen is a distance owed to a page that
            // cannot be sent to, because this thread would tick for ever.
            if let Ok(mut state) = shared.state.lock() {
                state.animator.forget();
                state.to = None;
            }
            continue;
        }
        let since = Instant::now().saturating_duration_since(shared.epoch);
        shared
            .activity
            .store(since.as_nanos().max(1) as u64, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::WHEEL_PIXELS;

    /// Every step of one animation, ticked on the schedule the thread would
    /// have kept.
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

    /// The curve delivers the notch, all of it, and hands it over in steps
    /// that only ever get smaller.
    #[test]
    fn a_notch_is_delivered_in_full_and_the_steps_only_shrink() {
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
            Some(TICK),
            "the first step is a tick away: at the notch itself the curve is at zero"
        );

        let steps: Vec<f64> = profile(&mut animator, start)
            .into_iter()
            .map(|delta| delta.1)
            .collect();
        let moved: f64 = steps.iter().sum();
        assert!(
            (moved - WHEEL_PIXELS).abs() < 1e-9,
            "the animation moved {moved} for a notch of {WHEEL_PIXELS}: {steps:?}"
        );
        assert_eq!(animator.owed(), (0.0, 0.0));
        assert_eq!(animator.until(start), None, "a finished animation is idle");

        // Ease-out: the pace is 2(1 - x), so every step is smaller than the
        // one before it. The first two are allowed to be equal because a notch
        // does not have to land on a tick.
        for pair in steps.windows(2).skip(1) {
            assert!(
                pair[1] <= pair[0] + 1e-9,
                "the curve sped up: {:?} then {:?} in {steps:?}",
                pair[0],
                pair[1]
            );
        }
        assert!(steps.iter().all(|step| *step > 0.0), "{steps:?}");
        // Over D at a tick each, which is what makes it a curve and not a jump.
        let ticks = (D.as_millis() / TICK.as_millis()) as usize;
        assert!(
            steps.len() >= ticks - 1 && steps.len() <= ticks + 1,
            "{} steps for a {D:?} curve of {TICK:?} ticks: {steps:?}",
            steps.len()
        );
    }

    /// Two notches a hundred milliseconds apart overlap, and the sum of them
    /// never falls away while both are running. This is the case the
    /// exponential lost.
    #[test]
    fn two_notches_overlap_and_the_step_never_falls_away() {
        let start = Instant::now();
        let apart = Duration::from_millis(100);
        let mut animator = Animator::default();
        down(&mut animator, start);

        let mut steps = Vec::new();
        let mut now = start;
        let mut second = false;
        while let Some(left) = animator.until(now) {
            now += left.max(Duration::from_micros(1));
            if !second && now.duration_since(start) >= apart {
                down(&mut animator, start + apart);
                second = true;
            }
            if let Some(step) = animator.tick(now) {
                steps.push((now.duration_since(start), step.delta.1));
            }
        }
        assert!(second, "the second notch never went in");

        let moved: f64 = steps.iter().map(|(_, step)| step).sum();
        assert!(
            (moved - 2.0 * WHEEL_PIXELS).abs() < 1e-9,
            "two notches moved {moved}"
        );
        // While both curves are running — from the second notch until the
        // first one is done — no step may be half of the one before it. That
        // is the seam, and it is the thing the person saw as shaking.
        let both = apart..D;
        let mut last: Option<f64> = None;
        for (when, step) in &steps {
            if both.contains(when) {
                if let Some(last) = last {
                    assert!(
                        *step >= last * 0.5,
                        "{step} after {last} at {when:?} is a seam: {steps:?}"
                    );
                }
            }
            last = Some(*step);
        }
    }

    /// A notch in the middle of another adds to it rather than restarting it,
    /// and it does not disturb the notch already running.
    #[test]
    fn a_second_notch_adds_a_curve_and_resets_nothing() {
        let start = Instant::now();
        let mut alone = Animator::default();
        down(&mut alone, start);
        let mut both = Animator::default();
        down(&mut both, start);

        // Four ticks in, the two are identical.
        let mut at = start;
        for _ in 0..4 {
            at += TICK;
            let one = alone.tick(at).expect("a step").delta.1;
            let two = both.tick(at).expect("a step").delta.1;
            assert!((one - two).abs() < 1e-9);
        }
        // A second notch, and from here the one with two curves is exactly the
        // one with one plus a curve of its own — nothing was reset.
        both.notch((10, 20), (0.0, WHEEL_PIXELS), at);
        let owed = both.owed().1;
        assert!(
            (owed - (alone.owed().1 + WHEEL_PIXELS)).abs() < 1e-9,
            "the notch replaced the curve instead of adding one: {owed}"
        );

        at += TICK;
        let one = alone.tick(at).expect("a step").delta.1;
        let two = both.tick(at).expect("a step").delta.1;
        assert!(
            two > one,
            "the second notch did not add movement: {one} then {two}"
        );
        // And the older curve is still on its own schedule: what the newer one
        // contributes is exactly a fresh notch's first step.
        let mut fresh = Animator::default();
        down(&mut fresh, at - TICK);
        let alone_again = fresh.tick(at).expect("a step").delta.1;
        assert!(
            (two - one - alone_again).abs() < 1e-9,
            "{two} is not {one} plus {alone_again}"
        );
    }

    /// Nothing caps a step, so a hand that rolls fast scrolls far — and it
    /// still stops when it stops, because every curve is over [`D`] after the
    /// notch that started it whatever else is running.
    #[test]
    fn nothing_caps_the_step_and_a_big_pile_still_settles_in_one_curve() {
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
        // And a hundred times the distance takes exactly as long as one: the
        // curves are the same length and they all started together. That is
        // the other half of "when I stop the wheel I want it to stop", and it
        // is a property of the shape rather than a number that was tuned.
        assert_eq!(large.len(), small.len(), "ten notches took longer than one");
        assert_eq!(huge.len(), small.len(), "a hundred notches took longer");
        let moved: f64 = large.iter().map(|delta| delta.1).sum();
        assert!(
            (moved - 10.0 * WHEEL_PIXELS).abs() < 1e-9,
            "{moved} for ten notches"
        );
    }

    /// The two axes are two lists, and a horizontal notch neither steals from
    /// nor waits for a vertical one.
    #[test]
    fn the_axes_are_independent() {
        let start = Instant::now();
        let mut animator = Animator::default();
        animator.notch((1, 2), (0.0, WHEEL_PIXELS), start);
        animator.notch((1, 2), (-WHEEL_PIXELS, 0.0), start);
        assert_eq!(animator.owed(), (-WHEEL_PIXELS, WHEEL_PIXELS));

        let step = animator.tick(start + TICK).expect("a step");
        assert!(step.delta.0 < 0.0 && step.delta.1 > 0.0, "{:?}", step.delta);
        assert_eq!(
            step.delta.0, -step.delta.1,
            "equal distances should move equally"
        );

        let steps = profile(&mut animator, start + TICK);
        let x: f64 = step.delta.0 + steps.iter().map(|delta| delta.0).sum::<f64>();
        let y: f64 = step.delta.1 + steps.iter().map(|delta| delta.1).sum::<f64>();
        assert!((x + WHEEL_PIXELS).abs() < 1e-9, "x moved {x}");
        assert!((y - WHEEL_PIXELS).abs() < 1e-9, "y moved {y}");
    }

    /// An animation only runs where the wheel was last turned, and a resize
    /// brings that point inside the page it now has.
    #[test]
    fn the_animation_follows_the_pointer_and_a_resize_keeps_it_on_the_page() {
        let start = Instant::now();
        let mut animator = Animator::default();
        animator.notch((10, 20), (0.0, WHEEL_PIXELS), start);
        assert_eq!(animator.tick(start + TICK).expect("a step").at, (10, 20));
        animator.notch((70, 90), (0.0, WHEEL_PIXELS), start + TICK);
        assert_eq!(
            animator.tick(start + TICK * 2).expect("a step").at,
            (70, 90)
        );

        // A pane that shrank under the point the wheel was turned at. The
        // notches stay; only the point moves.
        let owed = animator.owed();
        animator.resized((64, 48));
        assert_eq!(animator.owed(), owed, "a resize dropped the notches");
        assert_eq!(
            animator.tick(start + TICK * 3).expect("a step").at,
            (63, 47)
        );
        // A pane that grew leaves it alone.
        animator.resized((1280, 768));
        assert_eq!(
            animator.tick(start + TICK * 4).expect("a step").at,
            (63, 47)
        );
    }

    /// A tab that is left owes nothing, and the next notch starts cleanly.
    #[test]
    fn forget_drops_the_notches() {
        let start = Instant::now();
        let mut animator = Animator::default();
        down(&mut animator, start);
        animator.tick(start + TICK).expect("a step");
        assert_ne!(animator.owed(), (0.0, 0.0));

        animator.forget();
        assert_eq!(animator.owed(), (0.0, 0.0));
        assert_eq!(animator.until(start), None);
        assert_eq!(
            animator.tick(start + TICK * 2),
            None,
            "a forgotten animation ticks"
        );

        down(&mut animator, start + TICK * 2);
        assert_eq!(animator.until(start + TICK * 2), Some(TICK));
        assert_eq!(animator.owed(), (0.0, WHEEL_PIXELS));
    }

    /// Two notches that cancel leave nothing to animate — but not instantly:
    /// they are two curves and they cancel all the way down.
    #[test]
    fn opposite_notches_cancel() {
        let start = Instant::now();
        let mut animator = Animator::default();
        animator.notch((1, 1), (0.0, WHEEL_PIXELS), start);
        animator.notch((1, 1), (0.0, -WHEEL_PIXELS), start);
        assert_eq!(animator.owed(), (0.0, 0.0));
        // Every tick is a step of nothing, so nothing goes on the wire.
        for n in 1..20 {
            assert_eq!(animator.tick(start + TICK * n), None);
        }
        assert_eq!(animator.until(start + TICK * 20), None);
    }

    /// A tick that comes late delivers what the clock says, not what the
    /// missed ticks would have: the curve is absolute, so there is no backlog
    /// to fire in a burst.
    ///
    /// That burst is what the loop did on the installed machine —
    /// `-420 -126 -90 -8 -14 -138 -310` — and it is the reason the animation
    /// has a thread.
    #[test]
    fn a_late_tick_delivers_the_clock_and_not_a_backlog() {
        let start = Instant::now();
        let mut kept = Animator::default();
        let mut stalled = Animator::default();
        down(&mut kept, start);
        down(&mut stalled, start);

        // One ticks every 16 ms; the other is not looked at for a hundred.
        let late = start + Duration::from_millis(112);
        let mut on_time = 0.0;
        let mut at = start;
        while at < late {
            at += TICK;
            on_time += kept.tick(at).map(|step| step.delta.1).unwrap_or(0.0);
        }
        let in_one = stalled.tick(late).expect("a step").delta.1;
        assert!(
            (in_one - on_time).abs() < 1e-9,
            "seven ticks delivered {on_time} and one late tick {in_one}"
        );
        // And the one that stalled does not then fire six more.
        assert_eq!(
            stalled.tick(late),
            None,
            "a late tick was followed by the ticks it missed"
        );
        assert!((stalled.owed().1 - kept.owed().1).abs() < 1e-9);
    }

    /// A [`Dispatch`] that records when it was called, and nothing else.
    #[derive(Default)]
    struct Recorder {
        sent: Mutex<Vec<(Instant, Step)>>,
    }

    impl Dispatch for Recorder {
        fn send(&self, step: Step) -> Result<(), String> {
            self.sent
                .lock()
                .expect("the recorder")
                .push((Instant::now(), step));
            Ok(())
        }
    }

    /// The thread keeps the tick schedule while the thread that started it is
    /// busy.
    ///
    /// The 60 ms sleeps are a main loop decoding a frame, writing 2.9 MB into
    /// shared memory and handling what the terminal said, which on the
    /// installed machine is what made the ticks arrive in bursts. Here it does
    /// that between notches, as a hand on the wheel would, and twenty ticks
    /// still have to land on their own schedule — because the schedule is the
    /// animation.
    #[test]
    fn the_thread_ticks_on_time_while_the_loop_is_busy() {
        let recorder = Arc::new(Recorder::default());
        let wheel = Wheel::start();
        let start = Instant::now();
        wheel.notch("t", recorder.clone(), (10, 20), (0.0, WHEEL_PIXELS));
        for _ in 0..6 {
            std::thread::sleep(Duration::from_millis(60));
            wheel.notch("t", recorder.clone(), (10, 20), (0.0, WHEEL_PIXELS));
        }
        std::thread::sleep(Duration::from_millis(60));

        let sent = recorder.sent.lock().expect("the recorder").clone();
        assert!(
            sent.len() >= 20,
            "only {} ticks in 420 ms of {TICK:?}",
            sent.len()
        );
        let slack = Duration::from_millis(4);
        for (n, (at, step)) in sent.iter().take(20).enumerate() {
            let due = start + TICK * (n as u32 + 1);
            let off = if *at > due {
                at.duration_since(due)
            } else {
                due.duration_since(*at)
            };
            assert!(
                off <= slack,
                "tick {} landed {off:?} from its schedule",
                n + 1
            );
            assert!(step.delta.1 > 0.0, "tick {} sent nothing", n + 1);
        }
    }

    /// A notch for another tab is another animation, and forgetting is
    /// forgetting.
    #[test]
    fn the_thread_drops_a_pile_that_is_not_the_tab_in_front() {
        let recorder = Arc::new(Recorder::default());
        let wheel = Wheel::start();
        wheel.notch("a", recorder.clone(), (1, 1), (0.0, 10.0 * WHEEL_PIXELS));
        std::thread::sleep(Duration::from_millis(40));
        assert!(wheel.owed().1 > 0.0);

        wheel.notch("b", recorder.clone(), (1, 1), (0.0, WHEEL_PIXELS));
        assert!(
            wheel.owed().1 <= WHEEL_PIXELS,
            "the other tab's notches came along: {:?}",
            wheel.owed()
        );

        wheel.forget();
        assert_eq!(wheel.owed(), (0.0, 0.0));
        let was = recorder.sent.lock().expect("the recorder").len();
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(
            recorder.sent.lock().expect("the recorder").len(),
            was,
            "a forgotten animation went on sending"
        );
    }

    /// What the thread sends is what the loop hears about, and what it never
    /// sends it never claims.
    #[test]
    fn the_thread_says_when_it_last_sent_something() {
        let recorder = Arc::new(Recorder::default());
        let mut wheel = Wheel::start();
        assert_eq!(wheel.activity(), None, "an idle animator has sent nothing");

        let before = Instant::now();
        wheel.notch("t", recorder, (1, 1), (0.0, WHEEL_PIXELS));
        std::thread::sleep(Duration::from_millis(60));
        let at = wheel.activity().expect("something went out");
        assert!(at > before && at <= Instant::now());

        // And the thread ends when it is told to, rather than when the program
        // does.
        wheel.stop();
        let after = wheel.activity();
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(
            wheel.activity(),
            after,
            "a stopped animator went on ticking"
        );
        assert_eq!(wheel.owed(), (0.0, 0.0));
    }

    /// A socket that has gone ends the animation rather than being retried for
    /// ever.
    #[test]
    fn a_connection_that_failed_ends_the_animation() {
        struct Gone;
        impl Dispatch for Gone {
            fn send(&self, _: Step) -> Result<(), String> {
                Err("the connection was closed".to_string())
            }
        }

        let wheel = Wheel::start();
        wheel.notch("t", Arc::new(Gone), (1, 1), (0.0, 10.0 * WHEEL_PIXELS));
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(
            wheel.owed(),
            (0.0, 0.0),
            "the animation is still owed to a socket that has gone"
        );
        assert_eq!(wheel.activity(), None, "a failed send counted as activity");
    }
}
