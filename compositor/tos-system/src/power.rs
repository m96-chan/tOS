//! Battery and mains state, and the three ways a session ends.
//!
//! Reading is pure: everything comes out of a [`Sysfs`], so every shape a
//! real machine takes — one battery, two, none, a charger that calls itself
//! `USB`, a battery that reports `charge_*` instead of `energy_*` — is a
//! directory a test can build. Sysfs lies by omission constantly, so absence
//! is answered with `None` rather than a guess, and never with a panic.
//!
//! Acting goes through [`PowerBackend`], the same seam `apps/installer/src/exec.rs`
//! uses, for a blunter reason: a test that really called `reboot(2)` would
//! take the developer's machine down mid-run.

use crate::sysfs::Sysfs;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

/// Where the kernel publishes batteries and chargers.
const SUPPLY_DIR: &str = "/sys/class/power_supply";

/// The file a sleep state is asked for by writing to.
const SLEEP_STATE: &str = "/sys/power/state";

/// What a power supply is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupplyKind {
    /// `type` is `Battery`.
    Battery,
    /// Something feeding the machine from outside it. `Mains` is the usual
    /// answer, but a USB-C charger reports `USB`, and from the point of view
    /// of "is this laptop plugged in" the two are the same thing.
    Mains,
    /// A type tOS has no use for: `Wireless`, a mouse's own battery, and the
    /// rest of the menagerie that also lands in this directory.
    Other,
}

/// One entry under `/sys/class/power_supply`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Supply {
    /// Kernel name, such as `BAT0` or `ADP1`.
    pub name: String,
    pub kind: SupplyKind,
    /// Whether the kernel says it is delivering power. Batteries usually have
    /// no `online` file at all, which is why this is optional.
    pub online: Option<bool>,
}

/// What a battery is doing, from `status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChargeState {
    Charging,
    Discharging,
    Full,
    /// Plugged in, but the firmware has decided not to charge — common on
    /// machines with a charge threshold set.
    NotCharging,
    Unknown,
}

impl ChargeState {
    fn from_status(text: &str) -> ChargeState {
        let text = text.trim();
        if text.eq_ignore_ascii_case("charging") {
            ChargeState::Charging
        } else if text.eq_ignore_ascii_case("discharging") {
            ChargeState::Discharging
        } else if text.eq_ignore_ascii_case("full") {
            ChargeState::Full
        } else if text.eq_ignore_ascii_case("not charging") {
            ChargeState::NotCharging
        } else {
            ChargeState::Unknown
        }
    }

    /// A word for the status line.
    pub fn label(&self) -> &'static str {
        match self {
            ChargeState::Charging => "charging",
            ChargeState::Discharging => "discharging",
            ChargeState::Full => "full",
            ChargeState::NotCharging => "not charging",
            ChargeState::Unknown => "unknown",
        }
    }
}

/// The unit a battery measures itself in.
///
/// A machine exposes `energy_*` in microwatt hours or `charge_*` in microamp
/// hours, and which one is not something the caller gets to choose. Carrying
/// the unit is what stops two batteries that disagree from being added up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapacityUnit {
    MicrowattHours,
    MicroampHours,
}

/// One battery, as far as it can be read.
#[derive(Debug, Clone, PartialEq)]
pub struct Battery {
    /// Kernel name, such as `BAT0`.
    pub name: String,
    /// A bay can be empty; `present` is `0` when it is.
    pub present: bool,
    pub state: ChargeState,
    /// Charge, 0 to 100. `capacity` when the machine has it, worked out from
    /// the energy or charge counters when it does not, and `None` when there
    /// is nothing to work it out from.
    pub percent: Option<u8>,
    /// Charge left, in [`Battery::unit`].
    pub remaining: Option<u64>,
    /// Charge when full, in [`Battery::unit`]. This is the battery's present
    /// full rather than its design capacity, so a worn battery reads as 100%
    /// at the smaller number, the way the rest of the system reports it.
    pub full: Option<u64>,
    pub unit: Option<CapacityUnit>,
    /// Time to empty when discharging, or to full when charging. Absent
    /// whenever the machine reports no rate, reports a rate of zero — which
    /// an idle battery sitting on mains does constantly — or is neither
    /// charging nor discharging.
    pub time_remaining: Option<Duration>,
    /// Flow in watts, when the numbers allow it. Unsigned: the direction is
    /// [`Battery::state`].
    pub power_watts: Option<f32>,
}

/// Everything powering the machine right now.
#[derive(Debug, Clone, PartialEq)]
pub struct PowerState {
    /// Every battery, in kernel name order, including empty bays.
    pub batteries: Vec<Battery>,
    /// Every mains or USB supply.
    pub mains: Vec<Supply>,
}

impl PowerState {
    /// Whether there is a battery with anything in it. An empty bay does not
    /// count, or a laptop with one battery removed would show a blank gauge.
    pub fn has_battery(&self) -> bool {
        self.batteries.iter().any(|battery| battery.present)
    }

    /// Whether the machine is plugged in.
    ///
    /// A charger's `online` is the direct answer. Failing that, the battery's
    /// own status is a witness, and failing that, a machine with no battery
    /// at all is running on something, so it is on mains.
    pub fn on_mains(&self) -> Option<bool> {
        let stated: Vec<bool> = self
            .mains
            .iter()
            .filter_map(|supply| supply.online)
            .collect();
        if !stated.is_empty() {
            return Some(stated.into_iter().any(|online| online));
        }
        match self.state() {
            ChargeState::Charging | ChargeState::Full | ChargeState::NotCharging => Some(true),
            ChargeState::Discharging => Some(false),
            ChargeState::Unknown if !self.has_battery() => Some(true),
            ChargeState::Unknown => None,
        }
    }

    /// The whole machine's charge.
    ///
    /// Two batteries of different sizes are one gauge to the person looking
    /// at them, so they are weighted by capacity when they measure themselves
    /// the same way. When they do not — or when one of them only reports a
    /// percentage — the mean is the honest answer left.
    pub fn percent(&self) -> Option<u8> {
        let live: Vec<&Battery> = self.batteries.iter().filter(|b| b.present).collect();
        if live.is_empty() {
            return None;
        }

        let unit = live.first().and_then(|b| b.unit);
        let comparable = unit.is_some()
            && live
                .iter()
                .all(|b| b.unit == unit && b.remaining.is_some() && b.full.is_some());
        if comparable {
            let remaining: u64 = live.iter().filter_map(|b| b.remaining).sum();
            let full: u64 = live.iter().filter_map(|b| b.full).sum();
            if let Some(percent) = ratio_percent(Some(remaining), Some(full)) {
                return Some(percent);
            }
        }

        let known: Vec<u64> = live
            .iter()
            .filter_map(|b| b.percent)
            .map(u64::from)
            .collect();
        if known.is_empty() {
            return None;
        }
        Some(clamp_percent(
            known.iter().sum::<u64>() / known.len() as u64,
        ))
    }

    /// What the machine as a whole is doing. Charging wins over discharging,
    /// because a second battery draining into the first while the charger is
    /// in is still, to the person watching, a machine that is charging.
    pub fn state(&self) -> ChargeState {
        let states: Vec<ChargeState> = self
            .batteries
            .iter()
            .filter(|b| b.present)
            .map(|b| b.state)
            .collect();
        for wanted in [
            ChargeState::Charging,
            ChargeState::Discharging,
            ChargeState::Full,
            ChargeState::NotCharging,
        ] {
            if states.contains(&wanted) {
                return wanted;
            }
        }
        ChargeState::Unknown
    }

    /// Time until the machine stops, or until it is full.
    ///
    /// Per-battery times are added rather than averaged: Linux drains and
    /// fills batteries one at a time, so two hours left in one and one in the
    /// other is three hours of work. Absent unless at least one battery had
    /// something to say.
    pub fn time_remaining(&self) -> Option<Duration> {
        let times: Vec<Duration> = self
            .batteries
            .iter()
            .filter(|b| b.present)
            .filter_map(|b| b.time_remaining)
            .collect();
        if times.is_empty() {
            return None;
        }
        Some(times.into_iter().sum())
    }

    /// One line for the status bar.
    pub fn summary(&self) -> String {
        if !self.has_battery() {
            return match self.on_mains() {
                Some(true) => "on mains".to_string(),
                _ => "no battery".to_string(),
            };
        }
        let charge = match self.percent() {
            Some(percent) => format!("{percent}%"),
            None => "battery".to_string(),
        };
        let state = self.state();
        match (state, self.time_remaining()) {
            (ChargeState::Charging, Some(left)) => {
                format!("{charge} charging, {} to full", time_label(left))
            }
            (ChargeState::Discharging, Some(left)) => {
                format!("{charge} discharging, {} left", time_label(left))
            }
            _ => format!("{charge} {}", state.label()),
        }
    }
}

/// Every power supply the kernel knows about, in a settled order.
pub fn supplies(sysfs: &Sysfs) -> Vec<Supply> {
    sysfs
        .list(SUPPLY_DIR)
        .into_iter()
        .map(|name| {
            let kind = match sysfs
                .read(&format!("{SUPPLY_DIR}/{name}/type"))
                .map(|text| text.trim().to_string())
                .as_deref()
            {
                Some("Battery") => SupplyKind::Battery,
                Some("Mains") | Some("USB") | Some("USB_PD") | Some("UPS") => SupplyKind::Mains,
                _ => SupplyKind::Other,
            };
            let online = sysfs
                .read_number::<u64>(&format!("{SUPPLY_DIR}/{name}/online"))
                .map(|value| value != 0);
            Supply { name, kind, online }
        })
        .collect()
}

/// Read the machine's power state. Never fails: a machine with no
/// `/sys/class/power_supply` at all reads as no batteries and no chargers.
pub fn read(sysfs: &Sysfs) -> PowerState {
    let mut batteries = Vec::new();
    let mut mains = Vec::new();
    for supply in supplies(sysfs) {
        match supply.kind {
            SupplyKind::Battery => batteries.push(read_battery(sysfs, &supply.name)),
            SupplyKind::Mains => mains.push(supply),
            SupplyKind::Other => {}
        }
    }
    PowerState { batteries, mains }
}

fn read_battery(sysfs: &Sysfs, name: &str) -> Battery {
    let number = |file: &str| sysfs.read_number::<u64>(&format!("{SUPPLY_DIR}/{name}/{file}"));

    // A missing `present` means a soldered-in battery, which is always there.
    let present = number("present").map(|value| value != 0).unwrap_or(true);
    let state = sysfs
        .read(&format!("{SUPPLY_DIR}/{name}/status"))
        .map(|text| ChargeState::from_status(&text))
        .unwrap_or(ChargeState::Unknown);

    if !present {
        return Battery {
            name: name.to_string(),
            present: false,
            state: ChargeState::Unknown,
            percent: None,
            remaining: None,
            full: None,
            unit: None,
            time_remaining: None,
            power_watts: None,
        };
    }

    // `energy_*` is microwatt hours and `charge_*` microamp hours. A machine
    // exposes one pair, the other, or — on some ACPI implementations and most
    // phones — neither, leaving `capacity` as the only reading there is.
    let energy = (number("energy_now"), number("energy_full"));
    let charge = (number("charge_now"), number("charge_full"));
    let (unit, remaining, full) = match (energy, charge) {
        ((Some(now), full), _) => (Some(CapacityUnit::MicrowattHours), Some(now), full),
        (_, (Some(now), full)) => (Some(CapacityUnit::MicroampHours), Some(now), full),
        ((None, Some(full)), _) => (Some(CapacityUnit::MicrowattHours), None, Some(full)),
        (_, (None, Some(full))) => (Some(CapacityUnit::MicroampHours), None, Some(full)),
        _ => (None, None, None),
    };

    // `capacity` is what the firmware wants shown, so it wins where it exists.
    let percent = number("capacity")
        .map(clamp_percent)
        .or_else(|| ratio_percent(remaining, full));

    // Either counter can stand in for the other given a voltage, and a rate of
    // zero — what an idle battery on mains reports — is no rate at all.
    let voltage_uv = number("voltage_now").filter(|value| *value > 0);
    let power_uw = number("power_now").filter(|value| *value > 0).or_else(|| {
        let current = number("current_now").filter(|value| *value > 0)?;
        Some(current.saturating_mul(voltage_uv?) / 1_000_000)
    });
    let current_ua = number("current_now")
        .filter(|value| *value > 0)
        .or_else(|| {
            let power = number("power_now").filter(|value| *value > 0)?;
            Some(power.saturating_mul(1_000_000) / voltage_uv?)
        });

    let rate = match unit {
        Some(CapacityUnit::MicrowattHours) => power_uw,
        Some(CapacityUnit::MicroampHours) => current_ua,
        None => None,
    };
    let time_remaining = match state {
        ChargeState::Discharging => hours_left(remaining, rate),
        ChargeState::Charging => {
            let missing = match (remaining, full) {
                (Some(now), Some(full)) => Some(full.saturating_sub(now)),
                _ => None,
            };
            hours_left(missing, rate)
        }
        // Full, not charging and unknown have nowhere to be counting towards.
        _ => None,
    };

    Battery {
        name: name.to_string(),
        present: true,
        state,
        percent,
        remaining,
        full,
        unit,
        time_remaining,
        power_watts: power_uw.map(|uw| uw as f32 / 1_000_000.0),
    }
}

/// A percentage from two counters, refusing the full-of-nothing battery that
/// would otherwise divide by zero.
fn ratio_percent(remaining: Option<u64>, full: Option<u64>) -> Option<u8> {
    let (remaining, full) = (remaining?, full?);
    if full == 0 {
        return None;
    }
    Some(clamp_percent(remaining.saturating_mul(100) / full))
}

/// Firmware does report 101%, and a gauge that runs past its end is worse
/// than one that stops.
fn clamp_percent(value: u64) -> u8 {
    value.min(100) as u8
}

/// Counters and a rate per hour, in whatever unit the two share.
fn hours_left(units: Option<u64>, rate: Option<u64>) -> Option<Duration> {
    let (units, rate) = (units?, rate?);
    if rate == 0 {
        return None;
    }
    Some(Duration::from_secs(units.saturating_mul(3600) / rate))
}

/// `1:05`, the way a clock says it.
pub fn time_label(duration: Duration) -> String {
    let minutes = duration.as_secs() / 60;
    format!("{}:{:02}", minutes / 60, minutes % 60)
}

/// Something the machine can be asked to do to itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerAction {
    PowerOff,
    Reboot,
    Suspend,
}

impl PowerAction {
    /// What to put on the menu entry.
    pub fn label(&self) -> &'static str {
        match self {
            PowerAction::PowerOff => "power off",
            PowerAction::Reboot => "reboot",
            PowerAction::Suspend => "suspend",
        }
    }
}

/// One thing a backend was asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    Sync,
    PowerOff,
    Reboot,
    Suspend,
}

/// Somewhere the machine can be shut down.
///
/// The seam exists so that everything above it can be driven in a test
/// without the test's machine going away underneath it.
pub trait PowerBackend {
    /// Flush the page cache. Nothing else here gives the filesystems a chance.
    fn sync(&mut self);
    /// Power off. On a real backend this does not return on success.
    fn power_off(&mut self) -> io::Result<()>;
    /// Reboot. On a real backend this does not return on success.
    fn reboot(&mut self) -> io::Result<()>;
    /// Suspend to RAM, returning once the machine has woken again.
    fn suspend(&mut self) -> io::Result<()>;
}

/// Carry out `action`, syncing first.
///
/// The sync belongs here rather than inside each backend so that the order is
/// part of the logic under test: there is no init to flush the disks for us,
/// and an unsynced poweroff loses whatever the panes were writing.
pub fn request(backend: &mut dyn PowerBackend, action: PowerAction) -> io::Result<()> {
    backend.sync();
    match action {
        PowerAction::PowerOff => backend.power_off(),
        PowerAction::Reboot => backend.reboot(),
        PowerAction::Suspend => backend.suspend(),
    }
}

/// The backend that really does it.
///
/// Poweroff and reboot go straight to `reboot(2)` rather than running
/// `/sbin/poweroff`, because tOS can be PID 1 with no init to ask and no
/// guarantee the binary is in the image at all.
///
/// Since #110 there is an init on the two paths that matter, and `/sbin/`
/// does carry those programs — so what this costs is now a real thing rather
/// than a theoretical one: a machine restarted this way never unmounts its
/// root, and an installed tOS root is a read-write ext4. The `sync(2)` above
/// is what stands in for a shutdown. Asking systemd instead is worth doing
/// and is not this change; the one path that still has no init to ask is the
/// rescue session out of the initramfs, so whatever replaces this has to keep
/// answering for that one.
pub struct SystemPower {
    sleep_state: PathBuf,
}

impl SystemPower {
    pub fn new() -> SystemPower {
        SystemPower {
            sleep_state: PathBuf::from(SLEEP_STATE),
        }
    }

    /// The same thing against a prepared tree. Only suspend reads the root;
    /// `reboot(2)` has no filesystem to redirect, which is exactly why it
    /// needs the trait above rather than a root here.
    pub fn rooted(root: impl Into<PathBuf>) -> SystemPower {
        SystemPower {
            sleep_state: root.into().join(SLEEP_STATE.trim_start_matches('/')),
        }
    }
}

impl Default for SystemPower {
    fn default() -> Self {
        SystemPower::new()
    }
}

impl PowerBackend for SystemPower {
    fn sync(&mut self) {
        // Safe: sync(2) takes no arguments and cannot fail.
        unsafe { libc::sync() };
    }

    fn power_off(&mut self) -> io::Result<()> {
        reboot_syscall(RebootCommand::PowerOff)
    }

    fn reboot(&mut self) -> io::Result<()> {
        reboot_syscall(RebootCommand::Restart)
    }

    fn suspend(&mut self) -> io::Result<()> {
        // The kernel wakes the writing process when the machine comes back,
        // so this returns after the resume rather than before the suspend.
        std::fs::write(&self.sleep_state, "mem\n")
    }
}

/// Which way `reboot(2)` is being asked to end things.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RebootCommand {
    PowerOff,
    Restart,
}

/// `reboot(2)` only returns when it has failed, so any return is an error.
#[cfg(target_os = "linux")]
fn reboot_syscall(command: RebootCommand) -> io::Result<()> {
    let how = match command {
        RebootCommand::PowerOff => libc::LINUX_REBOOT_CMD_POWER_OFF,
        RebootCommand::Restart => libc::LINUX_REBOOT_CMD_RESTART,
    };
    // Safe: the argument is one of the kernel's own constants.
    unsafe { libc::reboot(how) };
    Err(io::Error::last_os_error())
}

/// Development happens on machines with no `reboot(2)` in this shape. The
/// stub keeps the workspace building there; CI builds the Linux side.
#[cfg(not(target_os = "linux"))]
fn reboot_syscall(command: RebootCommand) -> io::Result<()> {
    Err(io::Error::other(format!(
        "reboot(2) is Linux only; asked for {command:?}"
    )))
}

/// Writes down what it was asked to do and reports success.
///
/// This is what the tests assert against, and it is also the shape a
/// confirmation prompt wants: record the request, and only hand it to the
/// real backend once the person has agreed to it.
#[derive(Debug, Default)]
pub struct Recorder {
    pub requests: Vec<Request>,
    /// Requests that should fail, so error handling can be exercised.
    pub failures: Vec<Request>,
}

impl Recorder {
    pub fn new() -> Recorder {
        Recorder::default()
    }

    /// Make one request fail.
    pub fn failing(mut self, request: Request) -> Recorder {
        self.failures.push(request);
        self
    }

    /// Whether it was asked for this at all.
    pub fn did(&self, request: Request) -> bool {
        self.requests.contains(&request)
    }

    fn record(&mut self, request: Request) -> io::Result<()> {
        self.requests.push(request);
        if self.failures.contains(&request) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{request:?} refused"),
            ));
        }
        Ok(())
    }
}

impl PowerBackend for Recorder {
    fn sync(&mut self) {
        self.requests.push(Request::Sync);
    }

    fn power_off(&mut self) -> io::Result<()> {
        self.record(Request::PowerOff)
    }

    fn reboot(&mut self) -> io::Result<()> {
        self.record(Request::Reboot)
    }

    fn suspend(&mut self) -> io::Result<()> {
        self.record(Request::Suspend)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway power supply tree, in the shape `sysfs.rs`'s own tests use.
    struct Tree {
        root: PathBuf,
    }

    impl Tree {
        fn new(name: &str) -> Tree {
            let root =
                std::env::temp_dir().join(format!("tos-power-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            Tree { root }
        }

        fn file(&self, path: &str, contents: &str) -> &Tree {
            let full = self.root.join(path.trim_start_matches('/'));
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, contents).unwrap();
            self
        }

        /// One supply: its `type`, and whichever other files it exposes.
        fn supply(&self, name: &str, kind: &str, fields: &[(&str, &str)]) -> &Tree {
            self.file(&format!("{SUPPLY_DIR}/{name}/type"), &format!("{kind}\n"));
            for (file, contents) in fields {
                self.file(
                    &format!("{SUPPLY_DIR}/{name}/{file}"),
                    &format!("{contents}\n"),
                );
            }
            self
        }

        fn sysfs(&self) -> Sysfs {
            Sysfs::new(&self.root)
        }

        fn state(&self) -> PowerState {
            read(&self.sysfs())
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn a_laptop_with_one_battery_reads_its_charge_and_its_charger() {
        let tree = Tree::new("one");
        tree.supply(
            "BAT0",
            "Battery",
            &[
                ("status", "Discharging"),
                ("capacity", "87"),
                ("energy_now", "48000000"),
                ("energy_full", "55000000"),
                ("power_now", "12000000"),
            ],
        )
        .supply("AC", "Mains", &[("online", "0")]);

        let state = tree.state();
        assert_eq!(state.batteries.len(), 1);
        assert_eq!(state.batteries[0].name, "BAT0");
        assert_eq!(state.percent(), Some(87));
        assert_eq!(state.state(), ChargeState::Discharging);
        assert_eq!(state.on_mains(), Some(false));
        // Four hours of 48 Wh at 12 W.
        assert_eq!(state.time_remaining(), Some(Duration::from_secs(4 * 3600)));
        assert_eq!(state.batteries[0].power_watts, Some(12.0));
        assert_eq!(state.summary(), "87% discharging, 4:00 left");
    }

    #[test]
    fn a_desktop_has_no_battery_and_says_so() {
        let tree = Tree::new("desktop");
        // A desktop still has the directory; it is just full of nothing that
        // belongs to the machine itself.
        tree.supply("hidpp_battery_0", "Battery", &[("present", "0")]);

        let state = tree.state();
        assert!(!state.has_battery());
        assert_eq!(state.percent(), None);
        assert_eq!(state.state(), ChargeState::Unknown);
        assert_eq!(state.time_remaining(), None);
        assert_eq!(state.on_mains(), Some(true));
        assert_eq!(state.summary(), "on mains");
    }

    #[test]
    fn a_machine_with_no_power_supplies_at_all_is_not_an_error() {
        let tree = Tree::new("empty");
        let state = tree.state();
        assert!(state.batteries.is_empty());
        assert!(state.mains.is_empty());
        assert_eq!(state.summary(), "on mains");
    }

    #[test]
    fn a_battery_mid_charge_counts_towards_full() {
        let tree = Tree::new("charging");
        tree.supply(
            "BAT0",
            "Battery",
            &[
                ("status", "Charging"),
                ("capacity", "50"),
                ("energy_now", "25000000"),
                ("energy_full", "50000000"),
                ("power_now", "25000000"),
            ],
        )
        .supply("AC", "Mains", &[("online", "1")]);

        let state = tree.state();
        assert_eq!(state.state(), ChargeState::Charging);
        assert_eq!(state.on_mains(), Some(true));
        // 25 Wh missing at 25 W is an hour.
        assert_eq!(state.time_remaining(), Some(Duration::from_secs(3600)));
        assert_eq!(state.summary(), "50% charging, 1:00 to full");
    }

    #[test]
    fn two_batteries_are_weighted_by_size_and_their_times_add_up() {
        let tree = Tree::new("two");
        tree.supply(
            "BAT0",
            "Battery",
            &[
                ("status", "Discharging"),
                ("energy_now", "10000000"),
                ("energy_full", "40000000"),
                ("power_now", "10000000"),
            ],
        )
        .supply(
            "BAT1",
            "Battery",
            &[
                ("status", "Full"),
                ("energy_now", "20000000"),
                ("energy_full", "20000000"),
                ("power_now", "0"),
            ],
        );

        let state = tree.state();
        assert_eq!(state.batteries.len(), 2);
        // 30 Wh of a 60 Wh machine, not the mean of 25% and 100%.
        assert_eq!(state.percent(), Some(50));
        assert_eq!(state.state(), ChargeState::Discharging);
        // Only the draining one has anywhere to count to.
        assert_eq!(state.time_remaining(), Some(Duration::from_secs(3600)));
    }

    #[test]
    fn a_battery_with_no_capacity_file_is_worked_out_from_its_counters() {
        let tree = Tree::new("nocapacity");
        tree.supply(
            "BAT0",
            "Battery",
            &[
                ("status", "Discharging"),
                ("energy_now", "30000000"),
                ("energy_full", "40000000"),
            ],
        );

        let state = tree.state();
        assert_eq!(state.percent(), Some(75));
        // No rate, so no honest guess at how long is left.
        assert_eq!(state.time_remaining(), None);
        assert_eq!(state.summary(), "75% discharging");
    }

    #[test]
    fn a_battery_that_counts_in_charge_rather_than_energy_reads_the_same() {
        let tree = Tree::new("charge");
        tree.supply(
            "BAT0",
            "Battery",
            &[
                ("status", "Discharging"),
                ("charge_now", "3000000"),
                ("charge_full", "6000000"),
                ("current_now", "1500000"),
                ("voltage_now", "11000000"),
            ],
        );

        let state = tree.state();
        assert_eq!(state.batteries[0].unit, Some(CapacityUnit::MicroampHours));
        assert_eq!(state.percent(), Some(50));
        // 3 Ah at 1.5 A is two hours, whatever the volts are.
        assert_eq!(state.time_remaining(), Some(Duration::from_secs(2 * 3600)));
        // 1.5 A at 11 V is 16.5 W, worked out because power_now is missing.
        assert_eq!(state.batteries[0].power_watts, Some(16.5));
    }

    #[test]
    fn a_full_but_idle_battery_does_not_divide_by_zero() {
        let tree = Tree::new("idle");
        tree.supply(
            "BAT0",
            "Battery",
            &[
                ("status", "Full"),
                ("capacity", "100"),
                ("energy_now", "50000000"),
                ("energy_full", "50000000"),
                ("power_now", "0"),
                ("voltage_now", "0"),
            ],
        )
        .supply("AC", "Mains", &[("online", "1")]);

        let state = tree.state();
        assert_eq!(state.percent(), Some(100));
        assert_eq!(state.time_remaining(), None);
        assert_eq!(state.batteries[0].power_watts, None);
        assert_eq!(state.summary(), "100% full");
    }

    #[test]
    fn a_battery_that_says_it_is_full_of_nothing_reports_no_percentage() {
        let tree = Tree::new("zerofull");
        tree.supply(
            "BAT0",
            "Battery",
            &[
                ("status", "Unknown"),
                ("energy_now", "0"),
                ("energy_full", "0"),
            ],
        );

        let state = tree.state();
        assert_eq!(state.percent(), None);
        assert_eq!(state.summary(), "battery unknown");
    }

    #[test]
    fn a_battery_that_only_knows_its_percentage_still_reads() {
        let tree = Tree::new("percentonly");
        // Phones and some ACPI machines expose capacity and nothing else.
        tree.supply(
            "battery",
            "Battery",
            &[("status", "Charging"), ("capacity", "42")],
        );

        let state = tree.state();
        assert_eq!(state.percent(), Some(42));
        assert_eq!(state.batteries[0].unit, None);
        assert_eq!(state.time_remaining(), None);
        assert_eq!(state.on_mains(), Some(true));
        assert_eq!(state.summary(), "42% charging");
    }

    #[test]
    fn a_battery_with_nothing_readable_at_all_is_absence_and_not_zero() {
        let tree = Tree::new("silent");
        tree.supply("BAT0", "Battery", &[]);

        let state = tree.state();
        assert!(state.has_battery());
        assert_eq!(state.percent(), None);
        assert_eq!(state.state(), ChargeState::Unknown);
        assert_eq!(state.on_mains(), None);
        assert_eq!(state.summary(), "battery unknown");
    }

    #[test]
    fn a_battery_with_a_full_counter_but_no_now_is_not_read_as_empty() {
        let tree = Tree::new("halfcounted");
        tree.supply(
            "BAT0",
            "Battery",
            &[("status", "Discharging"), ("energy_full", "50000000")],
        );

        let state = tree.state();
        assert_eq!(state.batteries[0].remaining, None);
        assert_eq!(state.batteries[0].full, Some(50000000));
        assert_eq!(state.percent(), None);
    }

    #[test]
    fn two_batteries_with_nothing_to_weigh_fall_back_to_their_mean() {
        // Weighting needs counters in a shared unit. A pair that only reports
        // a percentage has nothing to weigh by, so the mean is the honest
        // answer — and it has to be the mean of both, not whichever came
        // first out of the directory listing.
        let tree = Tree::new("mean");
        tree.supply(
            "BAT0",
            "Battery",
            &[("status", "Discharging"), ("capacity", "20")],
        )
        .supply(
            "BAT1",
            "Battery",
            &[("status", "Discharging"), ("capacity", "80")],
        );

        let state = tree.state();
        assert_eq!(state.batteries.len(), 2);
        assert_eq!(state.percent(), Some(50));
    }

    #[test]
    fn firmware_that_reports_past_full_is_clamped() {
        let tree = Tree::new("clamp");
        tree.supply(
            "BAT0",
            "Battery",
            &[
                ("status", "Full"),
                ("capacity", "103"),
                ("energy_now", "52000000"),
                ("energy_full", "50000000"),
            ],
        );

        assert_eq!(tree.state().percent(), Some(100));
    }

    #[test]
    fn supplies_are_sorted_and_told_apart_by_type() {
        let tree = Tree::new("kinds");
        tree.supply("ucsi-source-psy-1", "USB", &[("online", "1")])
            .supply("BAT0", "Battery", &[("capacity", "10")])
            .supply("hid-mouse", "Wireless", &[("capacity", "90")])
            .supply("AC", "Mains", &[("online", "0")]);

        let found = supplies(&tree.sysfs());
        let names: Vec<&str> = found.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["AC", "BAT0", "hid-mouse", "ucsi-source-psy-1"]);
        assert_eq!(found[1].kind, SupplyKind::Battery);
        assert_eq!(found[2].kind, SupplyKind::Other);

        // A USB-C charger delivering power means the laptop is plugged in,
        // even though the barrel socket says it is offline.
        let state = tree.state();
        assert_eq!(state.mains.len(), 2);
        assert_eq!(state.on_mains(), Some(true));
        // The mouse's battery is not this machine's battery.
        assert_eq!(state.batteries.len(), 1);
    }

    #[test]
    fn a_supply_with_no_type_file_is_not_mistaken_for_a_battery() {
        let tree = Tree::new("notype");
        tree.file(&format!("{SUPPLY_DIR}/odd/capacity"), "50\n");
        let found = supplies(&tree.sysfs());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, SupplyKind::Other);
        assert!(tree.state().batteries.is_empty());
    }

    #[test]
    fn a_status_nobody_recognises_is_unknown_rather_than_a_guess() {
        let tree = Tree::new("weirdstatus");
        tree.supply(
            "BAT0",
            "Battery",
            &[("status", "Threshold"), ("capacity", "60")],
        );
        assert_eq!(tree.state().state(), ChargeState::Unknown);
    }

    #[test]
    fn a_time_is_written_the_way_a_clock_says_it() {
        assert_eq!(time_label(Duration::from_secs(3600 + 5 * 60)), "1:05");
        assert_eq!(time_label(Duration::from_secs(59)), "0:00");
        assert_eq!(time_label(Duration::from_secs(11 * 3600)), "11:00");
    }

    #[test]
    fn powering_off_syncs_the_disks_first() {
        let mut backend = Recorder::new();
        request(&mut backend, PowerAction::PowerOff).unwrap();
        assert_eq!(backend.requests, vec![Request::Sync, Request::PowerOff]);
    }

    #[test]
    fn a_reboot_is_asked_for_without_the_machine_going_anywhere() {
        let mut backend = Recorder::new();
        request(&mut backend, PowerAction::Reboot).unwrap();
        assert!(backend.did(Request::Reboot));
        assert!(!backend.did(Request::PowerOff));
    }

    #[test]
    fn a_suspend_is_its_own_request() {
        let mut backend = Recorder::new();
        request(&mut backend, PowerAction::Suspend).unwrap();
        assert_eq!(backend.requests, vec![Request::Sync, Request::Suspend]);
    }

    #[test]
    fn a_refused_request_comes_back_as_an_error_after_the_sync() {
        let mut backend = Recorder::new().failing(Request::PowerOff);
        let refused = request(&mut backend, PowerAction::PowerOff);
        assert!(refused.is_err());
        // The sync still happened, and the attempt is still on the record.
        assert_eq!(backend.requests, vec![Request::Sync, Request::PowerOff]);
    }

    #[test]
    fn every_action_has_something_to_put_on_a_menu() {
        assert_eq!(PowerAction::PowerOff.label(), "power off");
        assert_eq!(PowerAction::Reboot.label(), "reboot");
        assert_eq!(PowerAction::Suspend.label(), "suspend");
    }

    #[test]
    fn the_real_backend_writes_mem_to_ask_for_a_suspend() {
        // Rooted somewhere harmless: suspend is the one real action that can
        // be exercised without the machine disappearing.
        let tree = Tree::new("suspend");
        tree.file(SLEEP_STATE, "freeze mem disk\n");
        let mut backend = SystemPower::rooted(&tree.root);
        backend.suspend().unwrap();
        assert_eq!(
            std::fs::read_to_string(tree.root.join(SLEEP_STATE.trim_start_matches('/'))).unwrap(),
            "mem\n"
        );
    }

    #[test]
    fn the_real_backend_reports_a_suspend_it_could_not_ask_for() {
        let tree = Tree::new("nosuspend");
        let mut backend = SystemPower::rooted(&tree.root);
        // No /sys/power/state under this root, which is every machine that
        // cannot sleep.
        assert!(backend.suspend().is_err());
    }

    /// Deliberately not run on Linux, where this really would power the
    /// machine off. What it checks is that the stub a developer's machine
    /// compiles refuses rather than pretending it worked.
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn off_linux_the_real_backend_refuses_instead_of_pretending() {
        let mut backend = SystemPower::new();
        assert!(backend.power_off().is_err());
        assert!(backend.reboot().is_err());
    }
}
