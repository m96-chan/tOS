//! Stopping the machine, from the keystroke to the syscall.
//!
//! The unit tests beside `power.rs` drive the menu and the order a suspend
//! happens in. These ask the question only a whole compositor can answer:
//! whether the path from a key to `reboot(2)` is joined up, and whether every
//! way of not meaning it leaves the session exactly where it was.
//!
//! Nothing here can be allowed to really stop the machine running the tests,
//! which is what [`tos_system::power::PowerBackend`] is for. The recorder is
//! shared rather than handed over, because the compositor takes ownership of
//! the backend it is given and the test still has to be able to read it.

use std::cell::RefCell;
use std::io;
use std::rc::Rc;

use tos_compositor::power;
use tos_compositor::{Compositor, Config};
use tos_input::{InputEvent, KeyCode, KeyEvent, Modifiers};
use tos_session::Action;
use tos_system::power::{PowerAction, PowerBackend, Recorder, Request};

const SIZE: (u32, u32) = (800, 480);

/// A recorder the test can still read after the compositor has taken it.
#[derive(Clone, Default)]
struct Watched(Rc<RefCell<Recorder>>);

impl Watched {
    fn requests(&self) -> Vec<Request> {
        self.0.borrow().requests.clone()
    }
}

impl PowerBackend for Watched {
    fn sync(&mut self) {
        self.0.borrow_mut().sync()
    }

    fn power_off(&mut self) -> io::Result<()> {
        self.0.borrow_mut().power_off()
    }

    fn reboot(&mut self) -> io::Result<()> {
        self.0.borrow_mut().reboot()
    }

    fn suspend(&mut self) -> io::Result<()> {
        self.0.borrow_mut().suspend()
    }
}

/// A compositor on a machine with no hardware and a recorder where its power
/// backend should be.
fn compositor() -> (Compositor, Watched) {
    with_credential(None)
}

fn with_credential(credential: Option<std::path::PathBuf>) -> (Compositor, Watched) {
    let config = Config {
        command: Some(vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()]),
        bitmap_scale: Some(2),
        // The built-in face, so the test does not depend on the host's fonts,
        // and a root with no machine under it, so it does not depend on the
        // battery of whoever is running it either.
        font: Some("/nonexistent".into()),
        system_root: "/nonexistent-so-this-machine-has-no-hardware".into(),
        credential: credential.unwrap_or_else(|| "/nonexistent-so-there-is-no-password".into()),
        credential_user: "tos".into(),
        ..Config::default()
    };
    let mut compositor = Compositor::new(config, SIZE, None).expect("compositor");
    let watched = Watched::default();
    compositor
        .machine_mut()
        .set_power_backend(Box::new(watched.clone()));
    (compositor, watched)
}

fn press(compositor: &mut Compositor, code: KeyCode, modifiers: Modifiers) {
    compositor.handle_input(InputEvent::Key(KeyEvent::new(code, modifiers)));
}

fn type_text(compositor: &mut Compositor, text: &str) {
    for c in text.chars() {
        press(compositor, KeyCode::Char(c), Modifiers::NONE);
    }
}

fn enter(compositor: &mut Compositor) {
    press(compositor, KeyCode::Enter, Modifiers::NONE);
}

/// Move the cursor onto the row with this label and choose it.
fn choose(compositor: &mut Compositor, label: &str) {
    type_text(compositor, label);
    let chosen = compositor
        .overlay()
        .and_then(|overlay| overlay.selected_item())
        .map(|item| item.label.clone());
    assert_eq!(
        chosen.as_deref(),
        Some(label),
        "the filter did not land on {label}"
    );
    enter(compositor);
}

/// What the status bar has been told, most recent first.
fn said(compositor: &Compositor) -> Vec<String> {
    compositor
        .notifications()
        .history()
        .map(|notification| notification.text())
        .collect()
}

#[test]
fn the_gesture_everyone_arrives_with_opens_the_power_menu() {
    let (mut compositor, _) = compositor();
    press(
        &mut compositor,
        KeyCode::Delete,
        Modifiers::CTRL.union(Modifiers::ALT),
    );
    let overlay = compositor.overlay().expect("the power menu should be open");
    assert_eq!(overlay.title(), "power");
    let labels: Vec<&str> = overlay
        .items()
        .iter()
        .map(|item| item.label.as_str())
        .collect();
    assert_eq!(labels, ["power off", "reboot", "suspend"]);
}

#[test]
fn choosing_power_off_asks_again_before_anything_happens() {
    let (mut compositor, power) = compositor();
    compositor.perform(Action::PowerMenu);
    choose(&mut compositor, "power off");

    let overlay = compositor
        .overlay()
        .expect("the confirmation should be open");
    assert_eq!(overlay.title(), "power off now?");
    assert!(
        compositor.is_running(),
        "the session ended without an answer"
    );
    assert!(
        power.requests().is_empty(),
        "the machine was asked before the user was"
    );
}

#[test]
fn the_enter_that_asked_for_it_cannot_also_agree_to_it() {
    // The reason the confirmation is worth having: a second press of the key
    // that chose "power off" lands on `cancel`, which is the row the overlay
    // opens on.
    let (mut compositor, power) = compositor();
    compositor.perform(Action::PowerMenu);
    choose(&mut compositor, "power off");
    enter(&mut compositor);

    assert!(compositor.overlay().is_none(), "the menu is finished with");
    assert!(compositor.is_running(), "a stray enter ended the session");
    assert_eq!(compositor.shutdown_request(), None);
    assert!(power.requests().is_empty());
}

#[test]
fn escape_at_the_confirmation_leaves_the_session_alone() {
    let (mut compositor, power) = compositor();
    compositor.perform(Action::PowerMenu);
    choose(&mut compositor, "reboot");
    press(&mut compositor, KeyCode::Escape, Modifiers::NONE);

    assert!(compositor.overlay().is_none());
    assert!(compositor.is_running());
    assert!(power.requests().is_empty());
}

#[test]
fn confirming_a_power_off_ends_the_session_and_then_stops_the_machine() {
    let (mut compositor, power) = compositor();
    compositor.perform(Action::PowerMenu);
    choose(&mut compositor, "power off");
    choose(&mut compositor, "power off");

    // The session ends first and the machine stops afterwards, which is what
    // gets the console and the display back before the kernel starts talking.
    assert!(!compositor.is_running(), "the session should be over");
    assert_eq!(compositor.shutdown_request(), Some(PowerAction::PowerOff));
    assert!(
        power.requests().is_empty(),
        "nothing may happen while the loop still holds the screen"
    );

    compositor.shut_down().expect("the recorder agrees to it");
    assert_eq!(power.requests(), vec![Request::Sync, Request::PowerOff]);
}

#[test]
fn confirming_a_reboot_restarts_rather_than_switching_off() {
    let (mut compositor, power) = compositor();
    compositor.perform(Action::PowerMenu);
    choose(&mut compositor, "reboot");
    choose(&mut compositor, "reboot");

    assert_eq!(compositor.shutdown_request(), Some(PowerAction::Reboot));
    compositor.shut_down().expect("the recorder agrees to it");
    assert_eq!(power.requests(), vec![Request::Sync, Request::Reboot]);
}

#[test]
fn a_session_that_ended_for_any_other_reason_stops_no_machine() {
    let (mut compositor, power) = compositor();
    compositor.perform(Action::Quit);
    assert!(!compositor.is_running());
    assert_eq!(compositor.shutdown_request(), None);
    compositor.shut_down().expect("nothing to do");
    assert!(power.requests().is_empty(), "quitting is not a shutdown");
}

#[test]
fn choosing_suspend_needs_no_second_screen_and_keeps_the_session() {
    let (mut compositor, power) = compositor();
    compositor.perform(Action::PowerMenu);
    choose(&mut compositor, "suspend");

    assert!(compositor.overlay().is_none(), "suspend is not confirmed");
    assert!(
        compositor.is_running(),
        "a suspend does not end the session"
    );
    assert_eq!(compositor.shutdown_request(), None);
    // The sleep itself is the loop's to carry out, because the display and
    // the input devices are the loop's to give up.
    assert!(power.requests().is_empty());
    assert!(compositor.take_suspend_request());
    // And it is taken once: a flag still set on the far side would put the
    // machine straight back to sleep.
    assert!(!compositor.take_suspend_request());
}

#[test]
fn the_loop_sleeps_and_the_session_comes_back_wanting_the_whole_screen() {
    let (mut compositor, power) = compositor();
    compositor.perform(Action::PowerMenu);
    choose(&mut compositor, "suspend");
    assert!(compositor.take_suspend_request());

    // What `main.rs` does with the flag, on a backend that owns none of the
    // hardware a suspend has to give up.
    let outcome = power::suspend(&mut power::Unowned, compositor.machine_mut().power());
    assert!(outcome.slept);
    assert!(outcome.problems.is_empty());
    assert_eq!(power.requests(), vec![Request::Sync, Request::Suspend]);

    compositor.resumed(&outcome);
    assert!(compositor.needs_render(), "the screen was not asked for");
    assert!(compositor.is_running());
    assert!(said(&compositor).is_empty(), "{:?}", said(&compositor));
}

#[test]
fn a_suspend_that_went_wrong_is_said_and_not_died_of() {
    let (mut compositor, _) = compositor();
    let outcome = power::Outcome {
        slept: true,
        problems: vec![power::Problem {
            step: power::Step::RestoreDisplay,
            message: "Permission denied (os error 13)".into(),
        }],
    };
    compositor.resumed(&outcome);

    assert!(
        compositor.is_running(),
        "a bad resume is not a dead session"
    );
    let said = said(&compositor);
    assert!(
        said.iter()
            .any(|line| line.contains("taking the screen back")
                && line.contains("Permission denied")),
        "{said:?}"
    );
}

#[test]
fn suspending_a_machine_with_a_password_locks_it_before_it_sleeps() {
    // The lock goes up on this side of the sleep, not the far side: whoever
    // wakes the machine must never get a frame of the session first.
    let path = std::env::temp_dir().join(format!("tos-power-test-{}", std::process::id()));
    let hash = tos_crypt::sha512crypt::hash(b"the password", b"tOSlockscreen");
    std::fs::write(&path, format!("root:*:::::::\ntos:{hash}:::::::\n")).expect("credential file");
    let (mut compositor, _) = with_credential(Some(path.clone()));

    compositor.request_power(PowerAction::Suspend);
    assert!(compositor.is_locked(), "the session slept unlocked");
    assert!(compositor.take_suspend_request());
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_machine_with_no_password_suspends_without_being_told_it_cannot_lock() {
    // The live ISO. Being told "cannot lock" is not an answer to somebody who
    // asked for a suspend, and there is nothing to lock it with anyway.
    let (mut compositor, _) = compositor();
    compositor.request_power(PowerAction::Suspend);
    assert!(!compositor.is_locked());
    assert!(compositor.take_suspend_request());
    assert!(said(&compositor).is_empty(), "{:?}", said(&compositor));
}

#[test]
fn the_power_menu_is_on_the_sheet_the_bindings_are_read_from() {
    // The sheet is built from the live keymap, so this is the binding saying
    // what it is rather than a second telling of it.
    let keymap = tos_session::Keymap::default_bindings();
    let sheet = tos_session::describe::cheat_sheet(&keymap);
    let row = sheet
        .iter()
        .find(|row| row.action.contains("power off"))
        .expect("the power menu should be on the sheet");
    assert!(row.keys.contains("delete"), "{:?}", row.keys);
}
