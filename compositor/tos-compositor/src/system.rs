//! The machine underneath the session.
//!
//! `tos-system` knows how to read power, network, sound and Bluetooth out of
//! the kernel. This is where the compositor keeps what it read, and when it
//! reads it again.
//!
//! Two rules shape the module. The first is that none of it is required: a
//! machine with no battery, no sound card, no link and no adapter has to reach
//! a shell exactly as fast as one with all four, so every reader here is
//! allowed to find nothing, nothing is opened that turned out not to exist,
//! and no call in this file returns an error the compositor has to handle.
//! Absence is the ordinary answer, not a failure.
//!
//! The second is that readings are polled while actions are not. Battery
//! percentage, link state and the volume knob move on their own and slowly,
//! and nobody presses a key to make them happen, so [`Machine::poll`] goes and
//! looks on a timer and says whether anything it found is different — which is
//! what gives the status bar a reason to repaint on a session where no pane
//! has changed. Shutting down, joining a network or turning the volume up
//! happen because somebody asked, so they go straight through `tos-system`'s
//! own seams at the moment of asking.

use std::time::{Duration, Instant};

use tos_system::audio::{self, Card, Mixer, Volume};
use tos_system::bluetooth::{Adapter, Bluetooth, SystemControl};
use tos_system::net::{Interface, Network, SystemKernel};
use tos_system::power::{self, PowerBackend, PowerState, SystemPower};
use tos_system::Sysfs;

/// How often the machine is asked what it is doing.
///
/// A second is the finest granularity anything on screen has: a clock shows
/// minutes, a battery shows whole percent, and a cable is plugged in by hand.
/// The reads themselves are a handful of small sysfs files, which is cheaper
/// than the frame the change goes on to cause, and the loop is already awake
/// every tenth of a second for the cursor blink — so this is a deadline folded
/// into a wait that existed anyway rather than a poll loop of its own. A
/// blanked screen does not poll at all; see [`Machine::poll`].
pub const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// What the machine looked like the last time it was read.
///
/// Every field is optional and every `None` means the same thing: this machine
/// does not have that, or would not say. Nothing downstream has to tell the
/// difference between a missing battery and an unreadable one, because there
/// is nothing useful either could put on a status bar.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Reading {
    /// The batteries and chargers, when there are any at all. A desktop with
    /// no `/sys/class/power_supply` entries reads as `None` rather than as an
    /// empty state, so that a status bar can leave the slot out entirely.
    pub power: Option<PowerState>,
    /// The interface worth naming: the one carrying the default route, else
    /// the first one that is online, else the first wired or wireless link.
    pub link: Option<Interface>,
    /// The default card's level, when this machine has a card with a playback
    /// volume on it.
    pub volume: Option<Volume>,
    /// The first Bluetooth adapter, when the machine has one.
    pub adapter: Option<Adapter>,
}

/// The machine, as far as the compositor is concerned.
///
/// Holds the readers rather than rebuilding them per poll, because two of them
/// own something: the mixer keeps the control device it found its volume
/// control on, and reopening that every second would be a search of every
/// element on the card every second.
pub struct Machine {
    sysfs: Sysfs,
    network: Network<SystemKernel>,
    bluetooth: Bluetooth<SystemControl>,
    /// The default card's mixer, or `None` on a machine with no card and on
    /// one whose cards have no playback volume to drive. Opened once, at the
    /// first poll, because a card that is not there does not appear later.
    mixer: Option<Mixer<Card>>,
    /// Whether opening the mixer has been tried. Separate from `mixer` being
    /// `None`, which is also what a failed attempt leaves behind, so that the
    /// attempt is not repeated once a second for the life of the session.
    mixer_tried: bool,
    /// How the machine is asked to shut down, reboot or suspend.
    ///
    /// Boxed behind the trait for the reason the trait exists: a test drives
    /// the whole path through a recorder rather than by turning the developer's
    /// machine off.
    power: Box<dyn PowerBackend>,
    reading: Reading,
    /// When the last poll happened, or `None` before the first one — which is
    /// what makes the first [`Machine::poll`] read rather than wait out an
    /// interval before the status bar has anything on it.
    polled_at: Option<Instant>,
}

impl Machine {
    /// The machine this is running on.
    pub fn new() -> Machine {
        Machine::at(Sysfs::system())
    }

    /// The same, against a given root.
    ///
    /// Every reader in `tos-system` takes its root this way so it can be
    /// pointed at a directory laid out like a machine that is not this one.
    /// The compositor passes [`crate::config::Config::system_root`] through,
    /// which is how a test gets a compositor that finds no hardware at all
    /// instead of the developer's.
    pub fn at(sysfs: Sysfs) -> Machine {
        Machine {
            network: Network::new(sysfs.clone(), SystemKernel),
            bluetooth: Bluetooth::new(sysfs.clone(), SystemControl),
            sysfs,
            mixer: None,
            mixer_tried: false,
            power: Box::new(SystemPower::new()),
            reading: Reading::default(),
            polled_at: None,
        }
    }

    /// What the last poll found.
    pub fn reading(&self) -> &Reading {
        &self.reading
    }

    /// The root every reader is looking under.
    pub fn sysfs(&self) -> &Sysfs {
        &self.sysfs
    }

    /// Read the machine again if the interval has come round, and say whether
    /// anything is different.
    ///
    /// `true` is a reason to paint a frame: the session itself has not
    /// changed, so nothing else in the compositor would ask for one, and a
    /// battery that dropped a percent with nobody typing would otherwise not
    /// appear until the next keystroke.
    ///
    /// A blanked screen is not polled. There is nothing on it to be out of
    /// date, and the point of blanking is that the machine is left alone.
    pub fn poll(&mut self, now: Instant, blanked: bool) -> bool {
        if blanked {
            return false;
        }
        if let Some(last) = self.polled_at {
            if now.duration_since(last) < POLL_INTERVAL {
                return false;
            }
        }
        self.refresh(now)
    }

    /// Read the machine now, whatever the interval says.
    ///
    /// What an action calls when it has just changed something and wants the
    /// status bar to agree with it on this frame rather than within a second.
    pub fn refresh(&mut self, now: Instant) -> bool {
        self.polled_at = Some(now);
        let reading = self.read();
        let changed = reading != self.reading;
        self.reading = reading;
        changed
    }

    /// How long until the next poll is due, or `None` when one is due now —
    /// including before the first one, which is due immediately.
    ///
    /// The frame loop folds this into the wait it was going to do anyway, so a
    /// clock ticks over on the second rather than up to a tenth of a second
    /// after it.
    pub fn next_poll(&self, now: Instant) -> Option<Duration> {
        let last = self.polled_at?;
        POLL_INTERVAL
            .checked_sub(now.duration_since(last))
            .filter(|left| !left.is_zero())
    }

    /// The network, for the things a reading cannot answer: every interface
    /// rather than the interesting one, and bringing a link up.
    pub fn network(&mut self) -> &mut Network<SystemKernel> {
        &mut self.network
    }

    /// Bluetooth, for listing adapters, scanning and powering one on.
    pub fn bluetooth(&mut self) -> &mut Bluetooth<SystemControl> {
        &mut self.bluetooth
    }

    /// The default card's mixer, opening it if this is the first ask.
    ///
    /// `None` on a machine with no sound card, which is an ordinary machine.
    pub fn mixer(&mut self) -> Option<&Mixer<Card>> {
        self.open_mixer();
        self.mixer.as_ref()
    }

    /// Where power requests go. See [`tos_system::power::request`], which is
    /// what puts the sync in front of them.
    pub fn power(&mut self) -> &mut dyn PowerBackend {
        self.power.as_mut()
    }

    /// Send power requests somewhere else, so that a test can watch a session
    /// shut itself down without the machine under it going away.
    pub fn set_power_backend(&mut self, backend: Box<dyn PowerBackend>) {
        self.power = backend;
    }

    /// Put a reading in place directly.
    ///
    /// For a test that wants a status bar with a battery on it without a
    /// battery having to exist, and for an action that has just been told the
    /// new volume by the card it set it on.
    pub fn set_reading(&mut self, reading: Reading) {
        self.reading = reading;
    }

    /// Everything the machine will say about itself right now.
    fn read(&mut self) -> Reading {
        Reading {
            power: self.read_power(),
            link: self.read_link(),
            volume: self.read_volume(),
            adapter: self.bluetooth.default_adapter(),
        }
    }

    /// The power state, or `None` when the kernel lists no supplies at all.
    fn read_power(&self) -> Option<PowerState> {
        let state = power::read(&self.sysfs);
        if state.batteries.is_empty() && state.mains.is_empty() {
            return None;
        }
        Some(state)
    }

    /// The one interface worth naming.
    ///
    /// The default route is the honest answer to "am I on the network", so it
    /// wins. Without one, a link that is up and addressed is still worth
    /// showing — a machine on a LAN with no gateway is not offline — and
    /// failing that the first real interface, so that "eth0 no carrier" can be
    /// said rather than nothing at all.
    fn read_link(&self) -> Option<Interface> {
        if let Some(interface) = self.network.active_interface() {
            return Some(interface);
        }
        let visible = self.network.visible_interfaces();
        visible
            .iter()
            .find(|interface| interface.is_online())
            .cloned()
            .or_else(|| visible.into_iter().next())
    }

    fn read_volume(&mut self) -> Option<Volume> {
        self.open_mixer();
        self.mixer.as_ref()?.volume().ok()
    }

    /// Find the card, once.
    ///
    /// A card that is not in `/proc/asound` is not there, and asking again
    /// every second would be a directory read per second for the answer "still
    /// no". A card that is there but has no playback volume is the same case:
    /// the search that established it walked every control element on the
    /// device, which is not worth repeating either.
    fn open_mixer(&mut self) {
        if self.mixer_tried {
            return;
        }
        self.mixer_tried = true;
        if audio::card_order(&self.sysfs).is_empty() {
            return;
        }
        // An error here is a card that exists and cannot be driven, which is
        // as far as this goes: there is no volume to show and no volume to
        // change, and neither is a reason to keep the session off the screen.
        self.mixer = audio::open_default_in(&self.sysfs).ok().flatten();
    }
}

impl Default for Machine {
    fn default() -> Machine {
        Machine::new()
    }
}

/// Written out by hand because the mixer holds a control device and the power
/// backend is a trait object, neither of which is worth printing, and because
/// what anyone debugging this wants is the reading.
impl std::fmt::Debug for Machine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Machine")
            .field("root", &self.sysfs.root())
            .field("reading", &self.reading)
            .field("has_mixer", &self.mixer.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A throwaway directory laid out like a machine, which cleans up after
    /// itself. The same shape `tos-system`'s own tests use.
    struct Fake {
        root: PathBuf,
    }

    impl Fake {
        fn new(name: &str) -> Fake {
            let root = std::env::temp_dir().join(format!(
                "tos-machine-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).expect("temp dir");
            Fake { root }
        }

        fn file(&self, path: &str, contents: &str) -> &Fake {
            let full = self.root.join(path.trim_start_matches('/'));
            std::fs::create_dir_all(full.parent().expect("parent")).expect("dirs");
            std::fs::write(full, contents).expect("write");
            self
        }

        fn machine(&self) -> Machine {
            Machine::at(Sysfs::new(&self.root))
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn a_machine_with_none_of_it_reads_as_having_none_of_it() {
        let fake = Fake::new("bare");
        let mut machine = fake.machine();
        // Nothing found is the same as nothing known, so this reads as no
        // change at all — which is the point: a machine with none of this
        // never asks for a frame on account of it.
        assert!(!machine.refresh(Instant::now()), "found something");
        let reading = machine.reading();
        assert_eq!(reading.power, None, "a machine with no supplies");
        assert_eq!(reading.volume, None, "a machine with no card");
        assert_eq!(reading.adapter, None, "a machine with no adapter");
    }

    #[test]
    fn the_first_poll_does_not_wait_out_an_interval() {
        let fake = Fake::new("first");
        let mut machine = fake.machine();
        assert_eq!(machine.next_poll(Instant::now()), None);
        fake.file("/sys/class/power_supply/BAT0/type", "Battery\n")
            .file("/sys/class/power_supply/BAT0/present", "1\n")
            .file("/sys/class/power_supply/BAT0/capacity", "62\n");
        let now = Instant::now();
        assert!(machine.poll(now, false), "the first poll found a battery");
        assert_eq!(
            machine.reading().power.as_ref().and_then(|p| p.percent()),
            Some(62)
        );
    }

    #[test]
    fn a_second_poll_inside_the_interval_does_not_go_and_look() {
        let fake = Fake::new("interval");
        fake.file("/sys/class/power_supply/BAT0/type", "Battery\n")
            .file("/sys/class/power_supply/BAT0/present", "1\n")
            .file("/sys/class/power_supply/BAT0/capacity", "62\n");
        let mut machine = fake.machine();
        let now = Instant::now();
        assert!(machine.poll(now, false));

        // The machine changes underneath, and the interval has not passed.
        fake.file("/sys/class/power_supply/BAT0/capacity", "61\n");
        assert!(!machine.poll(now, false), "polled again too soon");
        assert_eq!(
            machine.reading().power.as_ref().and_then(|p| p.percent()),
            Some(62)
        );

        // Once it has, the new value is found and reported as a change.
        let later = now + POLL_INTERVAL;
        assert!(machine.poll(later, false));
        assert_eq!(
            machine.reading().power.as_ref().and_then(|p| p.percent()),
            Some(61)
        );
    }

    #[test]
    fn a_poll_that_finds_nothing_new_is_not_a_reason_to_repaint() {
        let fake = Fake::new("steady");
        fake.file("/sys/class/power_supply/BAT0/type", "Battery\n")
            .file("/sys/class/power_supply/BAT0/present", "1\n")
            .file("/sys/class/power_supply/BAT0/capacity", "62\n");
        let mut machine = fake.machine();
        let now = Instant::now();
        assert!(machine.refresh(now));
        assert!(!machine.refresh(now + POLL_INTERVAL), "nothing moved");
    }

    #[test]
    fn a_blanked_screen_is_not_polled() {
        let fake = Fake::new("blanked");
        let mut machine = fake.machine();
        let now = Instant::now();
        assert!(!machine.poll(now, true));
        assert!(
            machine.next_poll(now).is_none(),
            "the poll was skipped, not done"
        );
    }

    #[test]
    fn the_next_poll_is_a_deadline_the_frame_loop_can_wait_on() {
        let fake = Fake::new("deadline");
        let mut machine = fake.machine();
        let now = Instant::now();
        machine.refresh(now);
        assert_eq!(machine.next_poll(now), Some(POLL_INTERVAL));
        let half = POLL_INTERVAL / 2;
        assert_eq!(machine.next_poll(now + half), Some(POLL_INTERVAL - half));
        assert_eq!(machine.next_poll(now + POLL_INTERVAL), None, "due now");
    }

    #[test]
    fn a_link_with_a_default_route_is_the_one_worth_naming() {
        let fake = Fake::new("route");
        fake.file("/sys/class/net/eth0/flags", "0x1003\n")
            .file("/sys/class/net/eth0/type", "1\n")
            .file("/sys/class/net/eth0/carrier", "1\n")
            .file("/sys/class/net/eth0/operstate", "up\n")
            .file("/sys/class/net/lo/flags", "0x9\n")
            .file("/sys/class/net/lo/type", "772\n")
            .file(
                "/proc/net/route",
                "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
                 eth0\t00000000\t0101A8C0\t0003\t0\t0\t0\t00000000\t0\t0\t0\n",
            );
        let mut machine = fake.machine();
        machine.refresh(Instant::now());
        let link = machine.reading().link.as_ref().expect("an interface");
        assert_eq!(link.name, "eth0");
        assert!(link.is_default);
    }

    #[test]
    fn with_no_route_at_all_the_link_is_still_named() {
        let fake = Fake::new("noroute");
        fake.file("/sys/class/net/eth0/flags", "0x1002\n")
            .file("/sys/class/net/eth0/type", "1\n")
            .file("/sys/class/net/eth0/operstate", "down\n")
            .file("/sys/class/net/eth0/carrier", "0\n")
            // A `device` link is how a real interface is told from a bridge.
            .file("/sys/class/net/eth0/device/uevent", "");
        let mut machine = fake.machine();
        machine.refresh(Instant::now());
        let link = machine.reading().link.as_ref().expect("an interface");
        assert_eq!(link.name, "eth0");
        assert!(!link.is_online());
        // Something truthful to put on a bar, rather than an empty slot.
        assert!(link.summary().contains("eth0"));
    }

    #[test]
    fn loopback_alone_is_not_a_link_worth_naming() {
        let fake = Fake::new("looponly");
        fake.file("/sys/class/net/lo/flags", "0x9\n")
            .file("/sys/class/net/lo/type", "772\n");
        let mut machine = fake.machine();
        machine.refresh(Instant::now());
        assert_eq!(machine.reading().link, None);
    }

    #[test]
    fn a_card_that_is_not_there_is_looked_for_once() {
        let fake = Fake::new("nocard");
        let mut machine = fake.machine();
        assert!(machine.mixer().is_none());
        assert!(machine.mixer_tried);
        assert!(machine.mixer().is_none());
    }
}
