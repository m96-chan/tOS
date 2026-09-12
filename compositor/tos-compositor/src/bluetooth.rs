//! The Bluetooth control surface, and the thread an inquiry runs on.
//!
//! `tos_system::bluetooth` can already take an adapter up, put it down, move
//! its kill switch and run an inquiry. All but one of those are an ioctl that
//! returns immediately, and the one that is not is the reason this module
//! exists: [`Bluetooth::scan`](tos_system::bluetooth::Bluetooth::scan) blocks
//! for as long as the controller is told to listen. The compositor is one
//! thread, and that thread is the one drawing — an inquiry called from
//! `perform_action` would freeze every pane, the cursor and the clock for
//! eight seconds, and the session would look crashed rather than busy.
//!
//! So a scan goes on a thread of its own and reports back down a channel that
//! [`Compositor::tick`](crate::Compositor::tick) drains, which is the same
//! shape as every other thing the loop has to notice without being asked: it
//! is polled where the frame is decided, rather than interrupting one. The
//! alternatives were both worse. Making the ioctl non-blocking is not on
//! offer — `HCIINQUIRY` has no such mode, and the kernel is going to sleep on
//! the controller whatever the caller wants. Splitting the inquiry into short
//! slices and running one per frame would mean an inquiry that keeps
//! restarting, which finds less than one long one does and costs the same
//! radio time.
//!
//! The thread is deliberately not joined and not cancellable. There is
//! nothing to cancel: `HCIINQUIRY` runs to its deadline, and a thread parked
//! in a syscall cannot be asked to stop. Dropping the [`Scan`] drops the
//! receiver, the send at the far end fails harmlessly, and the thread ends on
//! its own a few seconds later — or with the process, if the session is over
//! first.
//!
//! What is not here is a connect. See `docs/design/bluetooth.md`: without
//! something holding pairing state there is no paired device to connect to,
//! and a link that is made without one carries no profile anybody can use.
//! The menu says so rather than offering a button that cannot work.

use std::sync::mpsc::{self, Receiver, TryRecvError};

use tos_system::bluetooth::{Adapter, Bluetooth, Connection, Control, Discovered, Error};
use tos_system::Sysfs;

use crate::overlay::{Overlay, OverlayItem};

/// How long the controller is asked to listen.
///
/// The specification counts inquiry length in units of 1.28 seconds and the
/// usual advice is at least ten of them for a device that is not sitting on
/// the desk. This is well short of that, on purpose: a scan holds the radio
/// and the menu for its whole length, and a person who found nothing would
/// rather run it again than sit through half a minute of it. Nothing is lost
/// by being wrong here except results, and running it twice recovers them.
pub const SCAN_SECONDS: u8 = 8;

/// What choosing a row of the Bluetooth menu means.
///
/// The menu is built out of what the adapter is doing, so the rows are not the
/// same from one opening to the next: a powered adapter offers a scan and an
/// unpowered one does not. Choosing by position therefore has to go through
/// the list that was actually drawn, which is what [`Controls::rows`] keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    PowerOn,
    PowerOff,
    /// Set the software kill switch, which also takes the adapter down.
    Block,
    /// Clear it, which does not bring the adapter back up.
    Unblock,
    Scan,
    /// A row that is there to be read: the adapter's own line, a live link, a
    /// device an inquiry found, or the note saying why none of those can be
    /// connected to. Enter on one of these does nothing rather than being
    /// refused, because there is nothing to refuse.
    Nothing,
}

/// The compositor's Bluetooth state: the running scan, what the last one
/// found, and what the rows of the open menu mean.
///
/// One struct rather than three fields on the compositor, because the three
/// are only ever touched together — a scan finishing replaces the results,
/// which rebuilds the rows — and because keeping them apart would let them
/// disagree about which list the row indices belong to.
#[derive(Debug, Default)]
pub struct Controls {
    /// What each row of the menu as last built means, by position.
    rows: Vec<Choice>,
    /// The devices the last inquiry found.
    ///
    /// Kept here rather than re-read, because there is nowhere to re-read it
    /// from: an inquiry result exists only in the answer to the inquiry. The
    /// kernel's own cache is not reachable without repeating the scan, which
    /// is the expensive thing this is remembering to avoid.
    found: Vec<Discovered>,
    scan: Option<Scan>,
}

impl Controls {
    pub fn new() -> Controls {
        Controls::default()
    }

    /// The menu over the panes.
    ///
    /// Takes the adapter and the links as arguments rather than going and
    /// reading them, so that the caller decides how fresh they are. The
    /// compositor reads both at the moment the menu opens: the once-a-second
    /// reading is for a status bar, and a control surface that offered "power
    /// on" for an adapter that came up half a second ago would be wrong in the
    /// one place it matters.
    pub fn menu(&mut self, adapter: Option<&Adapter>, connections: &[Connection]) -> Overlay {
        Overlay::new("bluetooth", self.items(adapter, connections))
    }

    /// The rows, which is also where `rows` is filled in.
    ///
    /// Separate from [`menu`](Self::menu) because a scan that finishes under
    /// an open menu has to replace the list without throwing away what the
    /// user has typed into it.
    pub fn items(
        &mut self,
        adapter: Option<&Adapter>,
        connections: &[Connection],
    ) -> Vec<OverlayItem> {
        let mut items: Vec<OverlayItem> = Vec::new();
        let mut rows: Vec<Choice> = Vec::new();
        let mut push = |label: String, detail: String, choice: Choice| {
            items.push(OverlayItem::with_detail(label, detail));
            rows.push(choice);
        };

        match adapter {
            // Most machines tOS runs on have no adapter, and that is not a
            // failure to report — but a menu that opened empty would look
            // like one, so it says the ordinary thing out loud.
            None => push(
                "this machine has no Bluetooth".into(),
                String::new(),
                Choice::Nothing,
            ),
            Some(adapter) => {
                push(adapter.summary(), "adapter".into(), Choice::Nothing);

                if adapter.is_hard_blocked() {
                    // Nothing software can do moves a physical switch, so
                    // there is no row here to press — only the reason the
                    // other rows are missing.
                    push(
                        format!("{} is blocked by a hardware switch", adapter.name),
                        "flip it to go on".into(),
                        Choice::Nothing,
                    );
                } else if adapter.is_blocked() {
                    // A soft blocked adapter cannot be taken up until the
                    // block goes, so unblocking is the only step offered:
                    // powering on would be an `ERFKILL` dressed up as a
                    // button.
                    push(
                        format!("unblock {}", adapter.name),
                        "rfkill".into(),
                        Choice::Unblock,
                    );
                } else if adapter.powered {
                    // The scan is first because it is what anybody opened this
                    // for; turning the adapter off is the rarer errand and can
                    // be further down.
                    if self.scan.is_some() {
                        push(
                            "scanning".into(),
                            format!("{SCAN_SECONDS} seconds"),
                            Choice::Nothing,
                        );
                    } else {
                        push(
                            "scan for devices".into(),
                            format!("{SCAN_SECONDS} seconds"),
                            Choice::Scan,
                        );
                    }
                    push(
                        format!("power {} off", adapter.name),
                        String::new(),
                        Choice::PowerOff,
                    );
                } else {
                    push(
                        format!("power {} on", adapter.name),
                        String::new(),
                        Choice::PowerOn,
                    );
                }

                // Blocking is offered whenever it is not already blocked,
                // powered or not: it is the off switch that stays off, and
                // wanting the radio silent is a different request from wanting
                // the adapter down for now.
                if !adapter.is_blocked() && adapter.rfkill.is_some() {
                    push(
                        format!("block {}", adapter.name),
                        "stays off until unblocked".into(),
                        Choice::Block,
                    );
                }

                for connection in connections {
                    push(
                        connection.address.clone(),
                        format!("connected, {}", connection.kind.label()),
                        Choice::Nothing,
                    );
                }
            }
        }

        for device in &self.found {
            push(
                device.address.clone(),
                device.kind().label().into(),
                Choice::Nothing,
            );
        }
        if !self.found.is_empty() {
            // The one dishonest thing this menu could do is list devices that
            // look choosable. They are not, and the reason is a decision
            // rather than an omission, so it is written down where the
            // question is asked.
            push(
                "pairing is not possible from here".into(),
                "docs/design/bluetooth.md".into(),
                Choice::Nothing,
            );
        }

        self.rows = rows;
        items
    }

    /// What the row at this position means.
    ///
    /// An index the menu never drew answers [`Choice::Nothing`] rather than
    /// panicking: the list is rebuilt under the overlay when a scan lands, and
    /// a stale index is a missed keystroke, not a crash.
    pub fn choose(&self, index: usize) -> Choice {
        self.rows.get(index).copied().unwrap_or(Choice::Nothing)
    }

    pub fn is_scanning(&self) -> bool {
        self.scan.is_some()
    }

    /// Take a scan on.
    ///
    /// The previous results go now rather than when the new ones arrive. They
    /// describe what was in range several seconds ago, and leaving them under
    /// a row that says "scanning" would be claiming they are still there.
    pub fn begin(&mut self, scan: Scan) {
        self.found.clear();
        self.scan = Some(scan);
    }

    /// The scan's answer, once there is one.
    ///
    /// Called every frame, so it must never wait; it is a `try_recv` with the
    /// scan dropped behind it, and `None` is the ordinary answer.
    pub fn finished(&mut self) -> Option<Result<Vec<Discovered>, Error>> {
        let outcome = self.scan.as_mut()?.finished()?;
        self.scan = None;
        Some(outcome)
    }

    /// Keep what a scan found.
    ///
    /// A device that is discoverable answers every inquiry it hears, and one
    /// inquiry is several, so the same address comes back more than once. The
    /// kernel passes all of them through; a menu listing one headset four
    /// times would be reporting the radio rather than the room.
    pub fn set_found(&mut self, mut found: Vec<Discovered>) {
        let mut seen: Vec<String> = Vec::new();
        found.retain(|device| {
            if seen.contains(&device.address) {
                return false;
            }
            seen.push(device.address.clone());
            true
        });
        self.found = found;
    }

    /// What the last scan found, for a test and for anything that wants the
    /// list without rebuilding the menu.
    pub fn found(&self) -> &[Discovered] {
        &self.found
    }
}

/// An inquiry running somewhere that is not the frame loop.
///
/// Holds only the receiving end. There is no handle to the thread, because
/// there is nothing useful to do with one: it cannot be interrupted, and
/// joining it is exactly the block this type exists to avoid.
#[derive(Debug)]
pub struct Scan {
    /// The adapter it was started on, so that a result arriving after the
    /// adapter changed underneath can still be named.
    adapter: String,
    results: Receiver<Result<Vec<Discovered>, Error>>,
}

impl Scan {
    /// Start one.
    ///
    /// The thread builds its own [`Bluetooth`] out of a cloned [`Sysfs`] and a
    /// control of its own rather than borrowing the compositor's. That is what
    /// keeps the two apart: the frame loop goes on reading adapter flags once
    /// a second on its own socket while this one sits in `HCIINQUIRY`, and
    /// neither has to be locked against the other.
    ///
    /// `control` is the seam, and the reason it is a parameter rather than a
    /// `SystemControl` built in here: a test drives the whole path — thread,
    /// channel, and the tick that drains it — against a controller that does
    /// not exist.
    pub fn spawn<C>(sysfs: Sysfs, control: C, adapter: Adapter, seconds: u8) -> Scan
    where
        C: Control + Send + 'static,
    {
        let (sender, results) = mpsc::channel();
        let name = adapter.name.clone();
        std::thread::spawn(move || {
            let mut bluetooth = Bluetooth::new(sysfs, control);
            // A send with nobody at the other end is the ordinary way this
            // ends when the session closed the menu or quit outright. There is
            // no one left to tell, which is the point.
            let _ = sender.send(bluetooth.scan(&adapter, seconds));
        });
        Scan {
            adapter: name,
            results,
        }
    }

    /// The adapter this is running on.
    pub fn adapter(&self) -> &str {
        &self.adapter
    }

    /// The result, if it has arrived.
    fn finished(&mut self) -> Option<Result<Vec<Discovered>, Error>> {
        match self.results.try_recv() {
            Ok(outcome) => Some(outcome),
            Err(TryRecvError::Empty) => None,
            // The thread ended without sending, which only a panic inside it
            // can do. Reported as a failed scan rather than left as a `None`
            // forever, because a menu stuck on "scanning" with nothing coming
            // is the one state a person cannot get out of.
            Err(TryRecvError::Disconnected) => Some(Err(Error::Io(std::io::Error::other(
                "the scan did not finish",
            )))),
        }
    }
}

/// A scan that has already found what it is going to find.
///
/// Every test in this crate that needs a result to land wants the same three
/// pieces — an adapter that is up, a control that answers at once, and a
/// thread to carry the answer across — and the compositor's tests want them
/// from another module. It lives here rather than being written out twice,
/// because the [`Control`] trait has five methods and only one of them is ever
/// the point.
#[cfg(test)]
pub(crate) fn scan_that_found(devices: Vec<Discovered>) -> Scan {
    struct Answered(Vec<Discovered>);

    impl Control for Answered {
        fn device_flags(&mut self, _index: u16) -> std::io::Result<u32> {
            Ok(0)
        }
        fn power_up(&mut self, _index: u16) -> std::io::Result<()> {
            Ok(())
        }
        fn power_down(&mut self, _index: u16) -> std::io::Result<()> {
            Ok(())
        }
        fn set_soft_blocked(&mut self, _index: u32, _blocked: bool) -> std::io::Result<()> {
            Ok(())
        }
        fn inquiry(&mut self, _index: u16, _seconds: u8) -> std::io::Result<Vec<Discovered>> {
            Ok(self.0.clone())
        }
    }

    let adapter = Adapter {
        name: "hci0".to_string(),
        index: 0,
        address: "AA:BB:CC:DD:EE:FF".to_string(),
        powered: true,
        discoverable: false,
        connectable: true,
        inquiring: true,
        state_known: true,
        rfkill: None,
    };
    Scan::spawn(
        Sysfs::new("/nonexistent"),
        Answered(devices),
        adapter,
        SCAN_SECONDS,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::sync::mpsc::Sender;
    use tos_system::bluetooth::RfKill;

    /// An adapter as `read_adapter` would have built one, without a tree to
    /// read it out of. The menu never looks at anything but these fields.
    fn adapter(name: &str, powered: bool) -> Adapter {
        Adapter {
            name: name.to_string(),
            index: 0,
            address: "AA:BB:CC:DD:EE:FF".to_string(),
            powered,
            discoverable: false,
            connectable: powered,
            inquiring: false,
            state_known: true,
            rfkill: Some(RfKill {
                index: 0,
                soft: false,
                hard: false,
            }),
        }
    }

    fn blocked(name: &str, hard: bool) -> Adapter {
        Adapter {
            rfkill: Some(RfKill {
                index: 0,
                soft: true,
                hard,
            }),
            ..adapter(name, false)
        }
    }

    /// The choices the menu is offering, in the order it is offering them.
    fn offered(controls: &mut Controls, adapter: Option<&Adapter>) -> Vec<Choice> {
        controls.items(adapter, &[]);
        controls
            .rows
            .iter()
            .copied()
            .filter(|choice| *choice != Choice::Nothing)
            .collect()
    }

    fn labels(controls: &mut Controls, adapter: Option<&Adapter>) -> Vec<String> {
        controls
            .items(adapter, &[])
            .into_iter()
            .map(|item| item.label)
            .collect()
    }

    /// A control whose inquiry does not return until the test says so, which
    /// is what makes "the scan is not on this thread" an assertion rather than
    /// a race against a sleep.
    struct HeldInquiry {
        go: Receiver<()>,
        found: Vec<Discovered>,
    }

    impl Control for HeldInquiry {
        fn device_flags(&mut self, _index: u16) -> io::Result<u32> {
            Ok(0)
        }
        fn power_up(&mut self, _index: u16) -> io::Result<()> {
            Ok(())
        }
        fn power_down(&mut self, _index: u16) -> io::Result<()> {
            Ok(())
        }
        fn set_soft_blocked(&mut self, _index: u32, _blocked: bool) -> io::Result<()> {
            Ok(())
        }
        fn inquiry(&mut self, _index: u16, _seconds: u8) -> io::Result<Vec<Discovered>> {
            // Blocks exactly the way the real one does, and for exactly as
            // long as the test wants it to.
            let _ = self.go.recv();
            Ok(self.found.clone())
        }
    }

    fn held(found: Vec<Discovered>) -> (HeldInquiry, Sender<()>) {
        let (release, go) = mpsc::channel();
        (HeldInquiry { go, found }, release)
    }

    fn device(address: &str, class: u32) -> Discovered {
        Discovered {
            address: address.to_string(),
            class,
        }
    }

    /// Wait for the scan, which is allowed to take as long as a thread takes
    /// to start. A test that spun forever on a broken channel would hang CI
    /// rather than fail it.
    fn wait_for(controls: &mut Controls) -> Result<Vec<Discovered>, Error> {
        for _ in 0..1000 {
            if let Some(outcome) = controls.finished() {
                return outcome;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("the scan never came back");
    }

    #[test]
    fn a_machine_with_no_adapter_says_so_rather_than_opening_an_empty_menu() {
        let mut controls = Controls::new();
        assert_eq!(
            labels(&mut controls, None),
            ["this machine has no Bluetooth"]
        );
        assert!(offered(&mut controls, None).is_empty());
    }

    #[test]
    fn an_adapter_that_is_off_is_offered_power_before_anything_that_needs_it() {
        let mut controls = Controls::new();
        let off = adapter("hci0", false);
        assert_eq!(
            offered(&mut controls, Some(&off)),
            [Choice::PowerOn, Choice::Block]
        );
    }

    #[test]
    fn a_powered_adapter_is_the_only_one_offered_a_scan() {
        let mut controls = Controls::new();
        let on = adapter("hci0", true);
        assert_eq!(
            offered(&mut controls, Some(&on)),
            [Choice::Scan, Choice::PowerOff, Choice::Block]
        );
    }

    #[test]
    fn a_soft_blocked_adapter_is_offered_the_unblock_and_nothing_else() {
        // Powering on through a soft block is an ERFKILL, so offering it would
        // be offering a button that reports a failure.
        let mut controls = Controls::new();
        let off = blocked("hci0", false);
        assert_eq!(offered(&mut controls, Some(&off)), [Choice::Unblock]);
    }

    #[test]
    fn an_adapter_that_is_hard_blocked_is_told_about_rather_than_offered_a_fix() {
        let mut controls = Controls::new();
        let off = blocked("hci0", true);
        assert!(
            offered(&mut controls, Some(&off)).is_empty(),
            "nothing here can move a physical switch"
        );
        let said = labels(&mut controls, Some(&off)).join("\n");
        assert!(said.contains("hardware switch"), "{said}");
    }

    #[test]
    fn a_live_link_is_listed_but_is_not_something_to_choose() {
        use tos_system::bluetooth::LinkKind;

        let mut controls = Controls::new();
        let on = adapter("hci0", true);
        let links = [Connection {
            address: "11:22:33:44:55:66".into(),
            handle: 256,
            kind: LinkKind::Acl,
        }];
        let items = controls.items(Some(&on), &links);
        let row = items
            .iter()
            .position(|item| item.label == "11:22:33:44:55:66")
            .expect("the link should be listed");
        assert_eq!(items[row].detail, "connected, data");
        assert_eq!(controls.choose(row), Choice::Nothing);
    }

    #[test]
    fn what_a_scan_found_is_listed_with_what_kind_of_thing_answered() {
        let mut controls = Controls::new();
        // Major class 4 is audio/video, which is the whole reason #18 exists.
        controls.set_found(vec![device("11:22:33:44:55:66", 0x240404)]);
        let on = adapter("hci0", true);
        let items = controls.items(Some(&on), &[]);
        let found = items
            .iter()
            .find(|item| item.label == "11:22:33:44:55:66")
            .expect("the device should be listed");
        assert_eq!(found.detail, "audio");
        // And the list says why the device it just showed cannot be used.
        assert!(items.iter().any(|item| item.label.contains("pairing")));
    }

    #[test]
    fn one_device_answering_an_inquiry_four_times_is_one_row() {
        let mut controls = Controls::new();
        controls.set_found(vec![
            device("11:22:33:44:55:66", 0x240404),
            device("11:22:33:44:55:66", 0x240404),
            device("AA:00:00:00:00:01", 0x000100),
            device("11:22:33:44:55:66", 0x240404),
        ]);
        let addresses: Vec<&str> = controls
            .found()
            .iter()
            .map(|device| device.address.as_str())
            .collect();
        assert_eq!(addresses, ["11:22:33:44:55:66", "AA:00:00:00:00:01"]);
    }

    #[test]
    fn a_scan_that_is_running_is_said_rather_than_offered_again() {
        let (control, release) = held(Vec::new());
        let mut controls = Controls::new();
        let on = adapter("hci0", true);
        controls.begin(Scan::spawn(
            Sysfs::new("/nonexistent"),
            control,
            on.clone(),
            SCAN_SECONDS,
        ));
        assert!(!offered(&mut controls, Some(&on)).contains(&Choice::Scan));
        assert!(labels(&mut controls, Some(&on)).contains(&"scanning".to_string()));
        drop(release);
    }

    #[test]
    fn an_inquiry_does_not_block_the_thread_that_asked_for_it() {
        // The whole point of the module: the controller is still listening and
        // the caller has already come back, which on the real thing is the
        // difference between a busy session and a frozen one.
        let (control, release) = held(vec![device("11:22:33:44:55:66", 0x240404)]);
        let mut controls = Controls::new();
        controls.begin(Scan::spawn(
            Sysfs::new("/nonexistent"),
            control,
            adapter("hci0", true),
            SCAN_SECONDS,
        ));
        assert!(
            controls.finished().is_none(),
            "the inquiry has not returned, so neither should this"
        );

        release.send(()).expect("the scan thread should be waiting");
        let found = wait_for(&mut controls).expect("the scan should have succeeded");
        assert_eq!(found.len(), 1);
        assert!(
            !controls.is_scanning(),
            "a finished scan is not a running one"
        );
    }

    #[test]
    fn a_scan_of_an_adapter_that_is_down_comes_back_as_a_failure_not_a_freeze() {
        // `scan` refuses before it reaches the controller, so this exercises
        // the error crossing the channel rather than the inquiry.
        let (control, _release) = held(Vec::new());
        let mut controls = Controls::new();
        controls.begin(Scan::spawn(
            Sysfs::new("/nonexistent"),
            control,
            adapter("hci0", false),
            SCAN_SECONDS,
        ));
        let error = wait_for(&mut controls).expect_err("a down adapter cannot scan");
        assert!(error.to_string().contains("hci0"), "{error}");
    }

    #[test]
    fn starting_a_scan_drops_what_the_last_one_found() {
        let (control, release) = held(Vec::new());
        let mut controls = Controls::new();
        controls.set_found(vec![device("11:22:33:44:55:66", 0x240404)]);
        controls.begin(Scan::spawn(
            Sysfs::new("/nonexistent"),
            control,
            adapter("hci0", true),
            SCAN_SECONDS,
        ));
        assert!(
            controls.found().is_empty(),
            "the old results describe a room several seconds ago"
        );
        drop(release);
    }
}
