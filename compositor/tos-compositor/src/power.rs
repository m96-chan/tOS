//! The session's side of the three ways a machine stops.
//!
//! `tos_system::power` knows how to end a machine: sync, then `reboot(2)`, or
//! sync then `mem` into `/sys/power/state`. None of that is the hard part.
//! The hard part is that there is no logind here — the compositor *is* the
//! session — so nothing else on the machine will take the screen and the
//! keyboard away before the kernel does, hand them back afterwards, or ask
//! whether the person at the keyboard really meant it. This module is those
//! three things, and nothing else: the menu, the confirmation it goes through,
//! and the order a suspend has to give the hardware up in and take it back.
//!
//! # Confirming
//!
//! `tos-install` gates its one destructive step by making the user type the
//! name of the disk: not a `y/n`, because the whole point is that the keystroke
//! which confirms must not be one the hand was already making. The same rule
//! costs nothing here. The confirmation is a second menu whose first row —
//! the one under the cursor the moment it opens — is `cancel`, so the enter
//! that chose "power off" cannot also agree to it, and whose other row is the
//! action's own name, which the overlay's filter makes reachable by typing it.
//! A power off is not an erased disk, so it is not worth a sentence to type;
//! it is worth not being one keystroke away from a machine full of work.
//!
//! Suspend has no confirmation, and that is the same rule rather than an
//! exception to it: a suspend loses nothing, is undone by pressing a key, and
//! a confirmation on the one action that is free to get wrong is how people
//! learn to press enter twice without reading either screen.
//!
//! # Sleeping
//!
//! A suspend is the only thing in tOS that takes the machine away underneath a
//! running session and gives it back. [`SUSPEND`] is the order that has to
//! happen in, as a list rather than as a straight-line function, for the reason
//! `main.rs` keeps `switch_away_plan` as one: the order is the entire design,
//! and a list can be read by a test on a machine with no screen to lose.
//!
//! What is given up is what the kernel would otherwise hand back in a state
//! tOS does not know: DRM master, so the mode can be set from scratch on the
//! far side rather than flipped against a framebuffer the driver may no longer
//! be scanning out, and the input grabs, which a device that gets re-probed
//! across the resume — every USB keyboard — comes back without. The virtual
//! terminal is not given up, because nothing takes it: a sleeping machine has
//! no foreground terminal to lose it to. It is *re-asserted* on the way back,
//! which is a different claim and is made for a different reason, written at
//! [`Hardware::reclaim_terminal`].

use std::io;

use tos_system::power::{self, PowerAction, PowerBackend};

use crate::overlay::{Overlay, OverlayItem};

/// The row that means "no", and the row the cursor starts on.
const CANCEL: &str = "cancel";

/// Everything the menu offers, in the order it offers it.
///
/// Least reversible first, which is the order every other desktop puts these
/// in and so the order the hand already knows — and, because the cursor opens
/// on the first row and nothing here is chosen without a second screen, the
/// order costs nothing in safety.
const ACTIONS: [PowerAction; 3] = [
    PowerAction::PowerOff,
    PowerAction::Reboot,
    PowerAction::Suspend,
];

/// The power menu.
pub fn menu() -> Overlay {
    let items = ACTIONS
        .iter()
        .map(|action| OverlayItem::with_detail(action.label(), detail(*action)))
        .collect();
    Overlay::new("power", items)
}

/// What each row says about itself.
///
/// The detail column names the syscall rather than describing a feeling about
/// it. Somebody reading "sync, then switch the machine off" knows that the
/// panes are not being asked to save and that the filesystems are; "shut down
/// safely" would tell them neither.
fn detail(action: PowerAction) -> &'static str {
    match action {
        PowerAction::PowerOff => "sync, then switch the machine off",
        PowerAction::Reboot => "sync, then restart the machine",
        PowerAction::Suspend => "sleep to RAM; a key wakes it",
    }
}

/// The action a row of [`menu`] stands for.
///
/// By label rather than by index, so that a menu whose order changes cannot
/// quietly start rebooting the machines that meant to suspend.
pub fn action_named(label: &str) -> Option<PowerAction> {
    ACTIONS
        .iter()
        .copied()
        .find(|action| action.label() == label)
}

/// Whether this one has to be agreed to twice.
///
/// The line is drawn at whether the running panes survive it. A suspend gives
/// every program back exactly where it was; a power off and a reboot do not,
/// and no amount of asking the panes nicely would change that — there is no
/// session manager here to ask them with.
pub fn needs_confirming(action: PowerAction) -> bool {
    match action {
        PowerAction::PowerOff | PowerAction::Reboot => true,
        PowerAction::Suspend => false,
    }
}

/// The second screen: what is about to happen, and the two answers to it.
///
/// `cancel` is first because the overlay opens with its cursor on the first
/// row. That single fact is the whole safety property: the enter that chose
/// "power off" on the menu behind this one lands on "cancel" here, so a
/// double press — the commonest way anybody destroys anything — leaves the
/// session exactly as it was.
pub fn confirmation(action: PowerAction) -> Overlay {
    let items = vec![
        OverlayItem::with_detail(CANCEL, "leave the session running"),
        OverlayItem::with_detail(action.label(), "every pane is stopped where it is"),
    ];
    Overlay::new(format!("{} now?", action.label()), items)
}

/// Whether the row chosen on [`confirmation`] is the one that goes ahead.
///
/// The answer is the action's own name, which is as close as a two row menu
/// gets to `tos-install` asking for the name of the disk: it is reachable by
/// arrowing onto it, and it is reachable by typing "power off" into the
/// overlay's filter, and it is not reachable by pressing enter again.
pub fn confirmed(action: PowerAction, label: &str) -> bool {
    label != CANCEL && label == action.label()
}

/// One thing a suspend gives up, or takes back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Drop DRM master. Whatever the driver does to the CRTC across a resume,
    /// tOS finds out about it by setting the mode again from nothing rather
    /// than by trusting the state it left behind.
    ReleaseDisplay,
    /// Let go of every input device, so that a sleep which goes wrong does not
    /// leave one process holding every keyboard on the machine and reading
    /// none of them.
    UngrabInput,
    /// Sync, then write `mem`. Does not return until the machine is awake
    /// again, which is why everything after this line is the resume.
    Sleep,
    /// Put the console back into graphics mode with its keyboard off.
    ReclaimTerminal,
    /// Become DRM master again, and arrange for the next frame to be a mode
    /// set rather than a page flip.
    RestoreDisplay,
    /// Take the input devices back.
    GrabInput,
    /// Throw away whatever the devices queued while nobody was reading them.
    DrainInput,
}

impl Step {
    /// What to call this in a message to the user, as the thing that failed:
    /// "giving up the screen: Permission denied".
    pub fn what(&self) -> &'static str {
        match self {
            Step::ReleaseDisplay => "giving up the screen",
            Step::UngrabInput => "letting go of the keyboard",
            Step::Sleep => "going to sleep",
            Step::ReclaimTerminal => "taking the terminal back",
            Step::RestoreDisplay => "taking the screen back",
            Step::GrabInput => "taking the keyboard back",
            Step::DrainInput => "clearing what was typed while asleep",
        }
    }
}

/// The order a suspend happens in.
///
/// Read it as two halves either side of [`Step::Sleep`], and note that they
/// are not mirror images. Going down, the screen is given up before the
/// keyboard, because the screen is the thing the kernel will change underneath
/// tOS and the keyboard is the thing that would strand the user if the sleep
/// never happened. Coming back, the terminal is claimed before the screen and
/// the screen before the keyboard: the console is put back into graphics mode
/// first so that the kernel's own console has stopped drawing before tOS sets
/// a mode over it, and the keyboard is taken last because a keyboard that is
/// grabbed before there is anything on the screen is a machine that looks
/// dead while it types into a session nobody can see.
pub const SUSPEND: &[Step] = &[
    Step::ReleaseDisplay,
    Step::UngrabInput,
    Step::Sleep,
    Step::ReclaimTerminal,
    Step::RestoreDisplay,
    Step::GrabInput,
    Step::DrainInput,
];

/// Everything a suspend has to take away from the session and give back.
///
/// The seam exists twice over. It is what lets the order above be tested on a
/// machine with no card, no console and no keyboard to lose — the same reason
/// `tos-system` puts a trait in front of `reboot(2)` — and it is what lets the
/// nested and headless backends say, by taking the defaults, that they own
/// none of this. A session drawing into another terminal holds no DRM master
/// and no grabs, so for it a suspend really is nothing but the sleep, and the
/// honest way to write that is an empty implementation rather than a branch.
pub trait Hardware {
    fn release_display(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn ungrab_input(&mut self) -> io::Result<()> {
        Ok(())
    }

    /// Claim the virtual terminal again.
    ///
    /// Nothing took it: a machine that is asleep has no foreground terminal to
    /// hand it to, and tOS is still where it was. What the resume does bring
    /// back is the kernel's own console, which restores itself on the way up
    /// and will happily draw its text over a terminal that is in `KD_TEXT`.
    /// Re-asserting graphics mode and `K_OFF` is two ioctls that a terminal
    /// already in that state does not notice, and skipping them is a session
    /// that comes back with a login banner printed across it.
    fn reclaim_terminal(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn restore_display(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn grab_input(&mut self) -> io::Result<()> {
        Ok(())
    }

    /// Drop the input that arrived while the session was not there to read it,
    /// and forget which keys were held.
    ///
    /// The same rule a blanked screen follows: an event nobody could see the
    /// target of was sent blind. The key that wakes a machine is aimed at
    /// waking it and at nothing else, and the modifier that was held when it
    /// went to sleep was let go somewhere the kernel could not report.
    fn drain_input(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A session that owns none of the machine it is drawn on.
///
/// The nested and headless backends: there is no DRM master to drop, no
/// console to reclaim and no device grabbed, so a suspend from one of them is
/// the sleep and nothing else. It is still a real sleep — it is still the
/// developer's machine and they still asked — which is why this exists rather
/// than the request being refused.
pub struct Unowned;

impl Hardware for Unowned {}

/// One step that did not work, kept so it can be said out loud.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub step: Step,
    /// What the kernel said, already rendered: an `io::Error` cannot be
    /// compared or cloned, and by the time anyone reads this the only thing
    /// left to do with it is put it on the status bar.
    pub message: String,
}

/// How a suspend went.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// Whether the machine actually slept. A suspend the kernel refused looks
    /// from the session's side exactly like one that happened and came back
    /// instantly, and the difference is worth telling the user about.
    pub slept: bool,
    /// What failed, in the order it was tried.
    pub problems: Vec<Problem>,
}

/// Sleep, and put the session back together afterwards.
///
/// Every step is attempted whatever the ones before it did, and the reason is
/// the failure mode this is guarding against. A suspend that gave up halfway
/// down would leave the session with no display and no keyboard and no way to
/// ask for either back; a suspend that gave up halfway *up* — because the card
/// would not hand master over again, say — would leave the keyboard ungrabbed
/// and every keystroke going to the kernel console behind tOS. Neither is
/// better than a screen that comes back wrong and says so, which is what
/// collecting the failures and carrying on gets.
///
/// For the same reason a refused sleep is not an early return. If the kernel
/// says no to `mem`, everything that was given up in preparation still has to
/// come back, or asking for a suspend on a machine that cannot suspend would
/// be a way of ending the session.
pub fn suspend(hardware: &mut dyn Hardware, backend: &mut dyn PowerBackend) -> Outcome {
    let mut outcome = Outcome::default();
    for step in SUSPEND {
        let result = match step {
            Step::ReleaseDisplay => hardware.release_display(),
            Step::UngrabInput => hardware.ungrab_input(),
            Step::Sleep => {
                // `request` is what puts the sync in front of the sleep, and a
                // sleep is exactly where it is wanted: a machine that goes to
                // sleep with dirty pages and never wakes — a battery that runs
                // out overnight — has lost them.
                let slept = power::request(backend, PowerAction::Suspend);
                outcome.slept = slept.is_ok();
                slept
            }
            Step::ReclaimTerminal => hardware.reclaim_terminal(),
            Step::RestoreDisplay => hardware.restore_display(),
            Step::GrabInput => hardware.grab_input(),
            Step::DrainInput => hardware.drain_input(),
        };
        if let Err(error) = result {
            outcome.problems.push(Problem {
                step: *step,
                message: error.to_string(),
            });
        }
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use tos_system::power::{Recorder, Request};

    /// Hardware that writes down what it was asked for instead of doing it,
    /// the way `tos_system::power::Recorder` stands in for `reboot(2)`.
    #[derive(Default)]
    struct Bench {
        steps: Vec<Step>,
        refusing: Vec<Step>,
    }

    impl Bench {
        fn refusing(step: Step) -> Bench {
            Bench {
                refusing: vec![step],
                ..Bench::default()
            }
        }

        fn did(&mut self, step: Step) -> io::Result<()> {
            self.steps.push(step);
            if self.refusing.contains(&step) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("{step:?} refused"),
                ));
            }
            Ok(())
        }
    }

    impl Hardware for Bench {
        fn release_display(&mut self) -> io::Result<()> {
            self.did(Step::ReleaseDisplay)
        }

        fn ungrab_input(&mut self) -> io::Result<()> {
            self.did(Step::UngrabInput)
        }

        fn reclaim_terminal(&mut self) -> io::Result<()> {
            self.did(Step::ReclaimTerminal)
        }

        fn restore_display(&mut self) -> io::Result<()> {
            self.did(Step::RestoreDisplay)
        }

        fn grab_input(&mut self) -> io::Result<()> {
            self.did(Step::GrabInput)
        }

        fn drain_input(&mut self) -> io::Result<()> {
            self.did(Step::DrainInput)
        }
    }

    #[test]
    fn the_menu_offers_the_three_ways_a_machine_stops() {
        let menu = menu();
        let labels: Vec<&str> = menu
            .items()
            .iter()
            .map(|item| item.label.as_str())
            .collect();
        assert_eq!(labels, ["power off", "reboot", "suspend"]);
        for label in labels {
            assert!(action_named(label).is_some(), "{label} names nothing");
        }
    }

    #[test]
    fn a_row_that_is_not_on_the_menu_names_nothing() {
        assert_eq!(action_named("hibernate"), None);
        assert_eq!(action_named(""), None);
    }

    #[test]
    fn the_two_that_stop_the_panes_are_confirmed_and_the_one_that_does_not_is_free() {
        assert!(needs_confirming(PowerAction::PowerOff));
        assert!(needs_confirming(PowerAction::Reboot));
        assert!(!needs_confirming(PowerAction::Suspend));
    }

    #[test]
    fn the_confirmation_opens_on_the_answer_that_changes_nothing() {
        // The property the whole confirmation exists for: the enter that chose
        // "power off" on the menu behind this lands here, and here it cancels.
        let confirmation = confirmation(PowerAction::PowerOff);
        let first = confirmation
            .selected_item()
            .expect("a row under the cursor")
            .label
            .clone();
        assert_eq!(first, CANCEL);
        assert!(!confirmed(PowerAction::PowerOff, &first));
    }

    #[test]
    fn confirming_means_choosing_the_action_by_name() {
        assert!(confirmed(PowerAction::Reboot, "reboot"));
        // Not the other action's name, and not the row next to it.
        assert!(!confirmed(PowerAction::Reboot, "power off"));
        assert!(!confirmed(PowerAction::Reboot, CANCEL));
    }

    #[test]
    fn the_confirmation_can_be_answered_by_typing_the_name() {
        // The overlay's own filter is what makes this reachable without the
        // arrow keys, which is as near as a two row menu gets to tos-install
        // asking for the name of the disk.
        let mut confirmation = confirmation(PowerAction::PowerOff);
        for c in "power off".chars() {
            confirmation.handle_key(&tos_input::KeyEvent::new(
                tos_input::KeyCode::Char(c),
                tos_input::Modifiers::NONE,
            ));
        }
        let chosen = confirmation.selected_item().expect("a match").label.clone();
        assert!(confirmed(PowerAction::PowerOff, &chosen));
    }

    #[test]
    fn a_suspend_gives_the_hardware_up_before_it_sleeps_and_takes_it_back_after() {
        let mut bench = Bench::default();
        let mut backend = Recorder::new();
        let outcome = suspend(&mut bench, &mut backend);

        assert!(outcome.slept);
        assert!(outcome.problems.is_empty(), "{:?}", outcome.problems);
        assert_eq!(
            bench.steps,
            SUSPEND
                .iter()
                .copied()
                .filter(|step| *step != Step::Sleep)
                .collect::<Vec<_>>()
        );
        // And the sleep itself flushed first, which is `power::request`'s job
        // and the reason the sleep goes through it rather than the backend.
        assert_eq!(backend.requests, vec![Request::Sync, Request::Suspend]);
    }

    #[test]
    fn the_screen_is_given_up_before_the_keyboard_is() {
        // A session that answered the other way round would spend the moment
        // between the two with no way to type and a screen it still owns,
        // which is the state a failed suspend would then strand it in.
        let release = SUSPEND.iter().position(|s| *s == Step::ReleaseDisplay);
        let ungrab = SUSPEND.iter().position(|s| *s == Step::UngrabInput);
        let sleep = SUSPEND.iter().position(|s| *s == Step::Sleep);
        assert!(release < ungrab && ungrab < sleep);
    }

    #[test]
    fn the_console_is_reclaimed_before_a_mode_is_set_over_it() {
        let terminal = SUSPEND.iter().position(|s| *s == Step::ReclaimTerminal);
        let display = SUSPEND.iter().position(|s| *s == Step::RestoreDisplay);
        let grab = SUSPEND.iter().position(|s| *s == Step::GrabInput);
        assert!(terminal < display, "the kernel console would draw over it");
        assert!(
            display < grab,
            "the keyboard would be taken before the screen"
        );
    }

    #[test]
    fn a_sleep_the_kernel_refuses_still_gives_everything_back() {
        // Otherwise asking to suspend a machine that cannot suspend would be a
        // way of ending the session: no master, no grabs, no way to ask again.
        let mut bench = Bench::default();
        let mut backend = Recorder::new().failing(Request::Suspend);
        let outcome = suspend(&mut bench, &mut backend);

        assert!(!outcome.slept);
        assert_eq!(outcome.problems.len(), 1);
        assert_eq!(outcome.problems[0].step, Step::Sleep);
        for step in [Step::RestoreDisplay, Step::GrabInput, Step::ReclaimTerminal] {
            assert!(bench.steps.contains(&step), "{step:?} was skipped");
        }
    }

    #[test]
    fn a_screen_that_will_not_come_back_does_not_cost_the_keyboard_as_well() {
        let mut bench = Bench::refusing(Step::RestoreDisplay);
        let mut backend = Recorder::new();
        let outcome = suspend(&mut bench, &mut backend);

        assert!(outcome.slept, "the machine still slept");
        assert_eq!(outcome.problems.len(), 1);
        assert_eq!(outcome.problems[0].step, Step::RestoreDisplay);
        assert!(bench.steps.contains(&Step::GrabInput));
        assert!(bench.steps.contains(&Step::DrainInput));
    }

    #[test]
    fn a_session_that_owns_nothing_still_sleeps() {
        let mut backend = Recorder::new();
        let outcome = suspend(&mut Unowned, &mut backend);
        assert!(outcome.slept);
        assert!(outcome.problems.is_empty());
        assert!(backend.did(Request::Suspend));
    }

    #[test]
    fn every_step_has_something_to_call_it_when_it_fails() {
        for step in SUSPEND {
            assert!(!step.what().is_empty(), "{step:?} has no name");
        }
    }
}
