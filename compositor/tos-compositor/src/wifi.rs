//! The state between keystrokes of joining a wireless network.
//!
//! `docs/design/wifi.md` (issue #137) decided all of this; the client it is
//! built on is [`tos_system::net::wpa`], which does the talking, and the menus
//! are in [`crate::compositor`]. What is left over is what lives here: the
//! scan that is running under an open menu, the join that is waiting for a
//! four-way handshake, and what to say about either on the next tick.
//!
//! Nothing here is on a thread, and that is the whole shape of it. Every
//! request is a local datagram answered in microseconds by a daemon on the
//! same machine, with a half-second timeout behind it for the day the daemon
//! is gone — so the waiting, for a scan to finish and for a handshake to
//! complete, is done by the tick that was going to happen anyway. Compare
//! [`crate::bluetooth`], which does need a thread, because `HCIINQUIRY`
//! genuinely blocks for eight seconds and nothing can be asked of it in the
//! meantime.
//!
//! # Where the supplicant comes from
//!
//! [`Wifi`] never opens a socket itself: it calls an [`Opener`], which is
//! [`SocketSupplicant::open`] on a real machine and, in a test, a closure
//! handing back a
//! [`RecordingSupplicant`](tos_system::net::wpa::RecordingSupplicant). That is
//! why the whole path — menu, scan, passphrase, join, wrong passphrase, tick —
//! is driven from the compositor's own tests with no radio, no daemon and no
//! privileges, the way [`crate::bluetooth::Scan::spawn`] takes a `Control`.
//!
//! A socket is opened when something needs one and dropped when the answer is
//! in; nothing is held open between keystrokes, so a supplicant restarted
//! under the session is simply talked to again next time. The one exception is
//! a join, which holds its client for the join's duration: the id it is
//! watching was handed out by that conversation, and a reconnect half way
//! through would be watching a number the new socket never issued.

use std::io;
use std::time::{Duration, Instant};

use tos_system::net::wpa::{Client, Found, Known, Security, SocketSupplicant, State, Supplicant};

use crate::overlay::OverlayItem;

/// How a [`Supplicant`] is got hold of, given an interface name.
///
/// A boxed closure rather than a second trait, because the only thing that
/// ever varies is where the socket comes from, and a test that has to
/// implement a factory trait to hand over a table is a test nobody writes.
pub type Opener = Box<dyn FnMut(&str) -> io::Result<Box<dyn Supplicant>>>;

/// What the list of networks in range is called.
pub const TITLE: &str = "wireless";

/// And what is on the end of that while the radio is still listening.
pub const SCANNING: &str = " — scanning";

/// How long the title goes on saying `— scanning` when nothing has changed.
///
/// A scan takes a second or two on a quiet band and rather longer on a busy
/// one, and the supplicant does not say when it is done without the event
/// stream this deliberately does not attach to. Ten seconds is long enough
/// that a slow scan is not called finished while it is still running, and
/// short enough that a title which will never change does not sit there
/// claiming otherwise for the life of the menu.
pub const SCAN_SETTLES: Duration = Duration::from_secs(10);

/// How long a join is given before it is called off.
///
/// Thirty seconds is a four-way handshake, a DHCP-less association and a
/// couple of retries with room to spare. The network is left configured when
/// it runs out, because a network that was out of range is one the supplicant
/// will join by itself when it is back.
pub const JOIN_DEADLINE: Duration = Duration::from_secs(30);

/// How often a pending join is asked about.
///
/// The compositor ticks rather more often than this — the blink phase alone
/// wants twice a second — and a join asks two questions each time it is
/// asked. This puts it on the once-a-second cadence the machine poll already
/// runs at, which is the rate the design writes down: nothing about a
/// handshake is learned faster by asking ten times a second, and a radio is
/// on a battery.
const JOIN_POLL: Duration = Duration::from_secs(1);

/// The scan behind an open [`OverlayKind::Wireless`] menu.
///
/// [`OverlayKind::Wireless`]: crate::OverlayKind::Wireless
struct Scanning {
    /// The radio it is a scan of, which is where the interface goes while the
    /// overlay is up — for the reason `Compositor::network_target` is held
    /// that way, and so that `OverlayKind` stays a `Copy` type with nothing
    /// in it.
    interface: String,
    /// The rows as last built: folded, strongest first, and compared against
    /// the next answer to decide whether the scan has produced anything.
    results: Vec<Found>,
    /// When the menu went up, which is when `SCAN` was sent.
    started: Instant,
    /// Whether the title still says `— scanning`.
    scanning: bool,
}

/// A join that has been asked for and has not settled.
struct Join {
    /// The radio it is on. Read when a second join is asked for while this one
    /// is still in the air, so that the refusal can name what it is waiting
    /// on.
    interface: String,
    /// The network id the supplicant handed out, which is what `STATUS` and
    /// `LIST_NETWORKS` are read for.
    id: u32,
    ssid: String,
    /// [`JOIN_DEADLINE`] from the moment `SELECT_NETWORK` went out.
    deadline: Instant,
    /// Held open for the join's duration. See the module header.
    client: Client<Box<dyn Supplicant>>,
    /// When it was last asked, or `None` before the first ask.
    polled: Option<Instant>,
}

/// The compositor's wireless state.
pub struct Wifi {
    open: Opener,
    scan: Option<Scanning>,
    /// The interface and SSID chosen from the list, while the passphrase for
    /// it is being typed. The prompt is an overlay with nothing in it but a
    /// line of text, so what the line is *for* has to be remembered out here.
    choice: Option<(String, String)>,
    join: Option<Join>,
}

impl Default for Wifi {
    fn default() -> Wifi {
        Wifi::new()
    }
}

impl Wifi {
    /// Talking to the supplicant that is really there.
    pub fn new() -> Wifi {
        Wifi::with_opener(Box::new(|interface| {
            Ok(Box::new(SocketSupplicant::open(interface)?) as Box<dyn Supplicant>)
        }))
    }

    /// Talking to whatever the caller hands back, which is how a test drives
    /// the whole path against a table.
    pub fn with_opener(open: Opener) -> Wifi {
        Wifi {
            open,
            scan: None,
            choice: None,
            join: None,
        }
    }

    /// Point it at a different supplicant. For tests.
    pub fn set_opener(&mut self, open: Opener) {
        self.open = open;
    }

    /// One conversation with the supplicant on `interface`.
    ///
    /// The error is what the link menu turns into its "no supplicant on
    /// wlan0" row: connecting a datagram socket to a path nothing is bound to
    /// fails at once, which is what lets the menu say so rather than offer a
    /// row that hangs for half a second and then fails.
    pub fn client(&mut self, interface: &str) -> io::Result<Client<Box<dyn Supplicant>>> {
        Ok(Client::new((self.open)(interface)?))
    }

    // ---- the scan --------------------------------------------------------

    /// Open a scan on `interface`: what to show at once, and its title.
    ///
    /// `SCAN_RESULTS` before `SCAN`, deliberately. The supplicant scans on its
    /// own and keeps the last results, so asking first is what makes the menu
    /// open with something in it rather than with a hole where the list will
    /// be; the `SCAN` behind it is what makes the list current a second later.
    pub fn begin_scan(
        &mut self,
        interface: &str,
        now: Instant,
    ) -> io::Result<(Vec<OverlayItem>, String)> {
        let mut client = self.client(interface)?;
        let results = rows(client.scan_results()?);
        client.scan()?;
        let items = items(&results);
        self.scan = Some(Scanning {
            interface: interface.to_string(),
            results,
            started: now,
            scanning: true,
        });
        Ok((items, title(true)))
    }

    /// The interface an open wireless menu is about, if one is open.
    pub fn scanning_on(&self) -> Option<&str> {
        self.scan.as_ref().map(|scan| scan.interface.as_str())
    }

    /// Ask `SCAN_RESULTS` again; the rows and the title when either moved.
    ///
    /// `SCAN` is not repeated. The supplicant's own periodic scan keeps the
    /// list fresh, and a menu that hammers the radio is a menu that drains a
    /// battery.
    ///
    /// A refusal — a socket that went away under the menu, a supplicant that
    /// was restarted — leaves the rows exactly as they were and says nothing.
    /// This runs on every tick for as long as the menu is open, so anything it
    /// had to say it would say several times a second, and what it would be
    /// saying is that a list somebody is reading is a moment out of date.
    pub fn refresh_scan(&mut self, now: Instant) -> Option<(Vec<OverlayItem>, String)> {
        let interface = self.scan.as_ref()?.interface.clone();
        let mut client = self.client(&interface).ok()?;
        let found = rows(client.scan_results().ok()?);
        let scan = self.scan.as_mut()?;
        let moved = found != scan.results;
        let settled = now.saturating_duration_since(scan.started) >= SCAN_SETTLES;
        let was_scanning = scan.scanning;
        if moved || settled {
            scan.scanning = false;
        }
        if !moved && scan.scanning == was_scanning {
            return None;
        }
        scan.results = found;
        Some((items(&scan.results), title(scan.scanning)))
    }

    /// The row with this SSID on it, from the list as last built.
    ///
    /// By name rather than by position: duplicates have been folded, so a
    /// name is unique among the rows, and a name cannot come to mean a
    /// different network the way an index into a list that is replaced under
    /// the menu can.
    pub fn found(&self, ssid: &str) -> Option<(String, Security)> {
        let scan = self.scan.as_ref()?;
        let found = scan.results.iter().find(|bss| bss.ssid == ssid)?;
        Some((scan.interface.clone(), found.security))
    }

    /// The menu closed. Nothing is held open, so this is all there is to it.
    pub fn forget_scan(&mut self) {
        self.scan = None;
    }

    // ---- the passphrase --------------------------------------------------

    /// Remember what the passphrase about to be typed is for.
    pub fn choose(&mut self, interface: &str, ssid: &str) {
        self.choice = Some((interface.to_string(), ssid.to_string()));
    }

    /// And take it back when the line is answered or the prompt is escaped.
    pub fn take_choice(&mut self) -> Option<(String, String)> {
        self.choice.take()
    }

    // ---- the join --------------------------------------------------------

    /// The SSID a join is waiting on, if one is.
    pub fn joining(&self) -> Option<&str> {
        self.join.as_ref().map(|join| join.ssid.as_str())
    }

    /// Join `ssid` on `interface`, with a passphrase or without one.
    ///
    /// Returns what to say. The client is kept, because from here on the join
    /// is watched by [`Wifi::tick`] rather than waited for.
    pub fn begin_join(
        &mut self,
        interface: &str,
        ssid: &str,
        passphrase: Option<&str>,
        now: Instant,
    ) -> io::Result<String> {
        if let Some(join) = &self.join {
            return Ok(format!(
                "already joining {} on {}",
                join.ssid, join.interface
            ));
        }
        let mut client = self.client(interface)?;
        let id = join(&mut client, ssid, passphrase)?;
        self.join = Some(Join {
            interface: interface.to_string(),
            id,
            ssid: ssid.to_string(),
            deadline: now + JOIN_DEADLINE,
            client,
            polled: None,
        });
        Ok(format!("joining {ssid}"))
    }

    /// Look at a pending join, and say what has become of it.
    ///
    /// The table `docs/design/wifi.md` writes down, in its order: `STATUS`
    /// reaching `COMPLETED` on that id is a join; that id turning up
    /// `[TEMP-DISABLED]` in `LIST_NETWORKS` is a wrong passphrase, because the
    /// supplicant fails the four-way handshake and disables the network for a
    /// while rather than trying forever; the deadline is a network that is not
    /// answering; and an error on the socket is itself.
    ///
    /// `Some` is the end of the join in every case: whatever it says, the join
    /// is over and the client is dropped with it.
    pub fn tick(&mut self, now: Instant) -> Option<String> {
        let join = self.join.as_mut()?;
        if now >= join.deadline {
            let said = format!("{}: could not join", join.ssid);
            self.join = None;
            return Some(said);
        }
        // Before the questions, not after: the deadline costs nothing to look
        // at, and a join that has run out has nothing to learn from two more
        // round trips.
        if let Some(last) = join.polled {
            if now.saturating_duration_since(last) < JOIN_POLL {
                return None;
            }
        }
        join.polled = Some(now);

        let said = settled(join)?;
        self.join = None;
        Some(said)
    }
}

/// What has become of a join, or `None` while it is still in the air.
///
/// A free function rather than a method, so that the borrow of the join ends
/// with the sentence it produces and the caller can then drop the join it was
/// borrowed from.
fn settled(join: &mut Join) -> Option<String> {
    let status = match join.client.status() {
        Ok(status) => status,
        Err(error) => return Some(format!("{}: {error}", join.ssid)),
    };
    if status.state == State::Completed && status.id == Some(join.id) {
        return Some(format!("joined {}", join.ssid));
    }
    let networks = match join.client.networks() {
        Ok(networks) => networks,
        Err(error) => return Some(format!("{}: {error}", join.ssid)),
    };
    if !networks
        .iter()
        .any(|known| known.id == join.id && known.temp_disabled)
    {
        return None;
    }
    // A key that is wrong is not kept and tried again at every boot. Both
    // refusals are swallowed: what is worth saying here is that the passphrase
    // was wrong, and "REMOVE_NETWORK 3: FAIL" said instead of it would be a
    // worse answer to the question somebody actually asked.
    let _ = join.client.remove(join.id);
    let _ = join.client.save();
    Some(format!("{}: wrong passphrase", join.ssid))
}

/// The title over the list, with or without the scan on the end of it.
pub fn title(scanning: bool) -> String {
    match scanning {
        true => format!("{TITLE}{SCANNING}"),
        false => TITLE.to_string(),
    }
}

/// One row per network in range: the strongest of each SSID, strongest first.
///
/// A BSS with an empty SSID is a hidden network and is not a row. Joining one
/// needs its name typed, which `docs/design/wifi.md` puts outside this
/// milestone — so the row is not there, rather than there and broken.
///
/// Duplicates are the same SSID on two bands or two access points, which is
/// every mesh and every dual-band router: what somebody wants is one line per
/// network, and the strongest of them is the one the supplicant is going to
/// associate with anyway.
pub fn rows(found: Vec<Found>) -> Vec<Found> {
    let mut rows: Vec<Found> = Vec::new();
    for bss in found {
        if bss.ssid.is_empty() {
            continue;
        }
        match rows.iter_mut().find(|row| row.ssid == bss.ssid) {
            Some(row) if row.signal_dbm < bss.signal_dbm => *row = bss,
            Some(_) => {}
            None => rows.push(bss),
        }
    }
    // The supplicant already sends them strongest first, so this is almost
    // always a no-op; it is here because folding can promote a weak first
    // sighting of an SSID above a stronger one below it, and a list that is
    // sorted by the number printed on it is the one nobody has to think about.
    rows.sort_by_key(|row| std::cmp::Reverse(row.signal_dbm));
    rows
}

/// The rows as the overlay wants them.
pub fn items(found: &[Found]) -> Vec<OverlayItem> {
    found
        .iter()
        .map(|bss| OverlayItem::with_detail(bss.ssid.clone(), detail(bss)))
        .collect()
}

/// The right hand column of a row: how loud it is and what it wants.
pub fn detail(found: &Found) -> String {
    format!(
        "{} dBm  {}",
        found.signal_dbm,
        security_text(found.security)
    )
}

/// What a network's security is called on the row.
///
/// The three that cannot be joined say so on the row itself rather than only
/// when they are chosen, because a list where three of the lines do nothing is
/// a list somebody presses every line of to find out which.
pub fn security_text(security: Security) -> &'static str {
    match security {
        Security::Open => "open",
        Security::Psk => "WPA2",
        Security::Eap => "enterprise — cannot join",
        Security::Wep => "WEP — cannot join",
        Security::Unknown => "unknown security — cannot join",
    }
}

/// Why this network cannot be joined, or `None` when it can be.
///
/// A row that does nothing without saying why is the thing
/// `docs/design/network.md` refused to ship.
pub fn refusal(ssid: &str, security: Security) -> Option<String> {
    let because = match security {
        Security::Open | Security::Psk => return None,
        Security::Eap => "enterprise networks are not supported",
        Security::Wep => "WEP is not supported",
        Security::Unknown => "unknown security",
    };
    Some(format!("cannot join {ssid}: {because}"))
}

/// The id of a network the supplicant already knows this SSID by.
pub fn id_for(known: &[Known], ssid: &str) -> Option<u32> {
    known
        .iter()
        .find(|network| network.ssid == ssid)
        .map(|network| network.id)
}

/// Configure and select a network, in the order the design writes down.
///
/// An SSID the supplicant already has is reused rather than added again: a
/// second `SET_NETWORK psk` on it replaces the key, which is exactly what
/// retyping a passphrase means, and adding a second network with the same name
/// is how a configuration file fills up with the same café four times.
///
/// It stops at the first error, which can leave a network added and named with
/// no key on it — the case is a passphrase [`Client::set_passphrase`] refuses
/// before sending. That network is not written to the file, because
/// `SAVE_CONFIG` is the last step and was never reached, and the next attempt
/// finds it by SSID and reuses it rather than adding another.
pub fn join<S: Supplicant>(
    client: &mut Client<S>,
    ssid: &str,
    passphrase: Option<&str>,
) -> io::Result<u32> {
    let known = client.networks()?;
    let id = match id_for(&known, ssid) {
        Some(id) => id,
        None => client.add_network()?,
    };
    client.set_ssid(id, ssid)?;
    match passphrase {
        Some(passphrase) => client.set_passphrase(id, passphrase)?,
        None => client.set_open(id)?,
    }
    client.select(id)?;
    client.save()?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;
    use tos_system::net::wpa::RecordingSupplicant;

    /// `SCAN_RESULTS` with a hidden network, a duplicate and an enterprise
    /// one in it, which is what the design's three shapes of row are read
    /// from.
    const SCAN_RESULTS: &str = concat!(
        "bssid / frequency / signal level / flags / ssid\n",
        "02:00:00:00:01:00\t2412\t-30\t[WPA2-PSK-CCMP][ESS]\tkitchen-table\n",
        "02:00:00:00:02:00\t5180\t-52\t[ESS]\tcafe\n",
        "02:00:00:00:03:00\t5220\t-45\t[WPA2-PSK-CCMP][ESS]\tkitchen-table\n",
        "02:00:00:00:04:00\t2437\t-67\t[WPA2-EAP-CCMP][ESS]\toffice\n",
        "02:00:00:00:05:00\t2462\t-40\t[WPA2-PSK-CCMP][ESS]\t\n",
    );

    /// A supplicant several conversations share, so a test can read the whole
    /// transcript after the compositor has opened and dropped four clients.
    struct Shared(Rc<RefCell<RecordingSupplicant>>);

    impl Supplicant for Shared {
        fn request(&mut self, command: &str) -> io::Result<String> {
            self.0.borrow_mut().request(command)
        }
    }

    /// A [`Wifi`] talking to `table`, and the table it is talking to.
    fn wifi(table: RecordingSupplicant) -> (Wifi, Rc<RefCell<RecordingSupplicant>>) {
        let shared = Rc::new(RefCell::new(table));
        let opener = shared.clone();
        (
            Wifi::with_opener(Box::new(move |_| {
                Ok(Box::new(Shared(opener.clone())) as Box<dyn Supplicant>)
            })),
            shared,
        )
    }

    fn transcript(shared: &Rc<RefCell<RecordingSupplicant>>) -> Vec<String> {
        shared.borrow().transcript()
    }

    #[test]
    fn a_row_says_how_loud_a_network_is_and_what_it_wants() {
        let found = tos_system::net::wpa::parse_scan_results(SCAN_RESULTS);
        assert_eq!(detail(&found[0]), "-30 dBm  WPA2");
        assert_eq!(detail(&found[1]), "-52 dBm  open");
        assert_eq!(detail(&found[3]), "-67 dBm  enterprise — cannot join");
    }

    #[test]
    fn the_two_securities_that_are_refused_say_so_on_the_row() {
        assert_eq!(security_text(Security::Wep), "WEP — cannot join");
        assert_eq!(
            security_text(Security::Unknown),
            "unknown security — cannot join"
        );
    }

    #[test]
    fn a_hidden_network_is_not_a_row_and_a_duplicate_is_folded_to_the_strongest() {
        let rows = rows(tos_system::net::wpa::parse_scan_results(SCAN_RESULTS));
        let ssids: Vec<&str> = rows.iter().map(|row| row.ssid.as_str()).collect();
        assert_eq!(ssids, vec!["kitchen-table", "cafe", "office"]);
        assert_eq!(
            rows[0].signal_dbm, -30,
            "the weaker of the two kitchen-tables was kept"
        );
    }

    #[test]
    fn the_rows_are_strongest_first_whatever_order_they_arrived_in() {
        let found = vec![
            bss("far", -80, Security::Open),
            bss("near", -20, Security::Psk),
            bss("middling", -50, Security::Open),
        ];
        let ssids: Vec<String> = rows(found).into_iter().map(|row| row.ssid).collect();
        assert_eq!(ssids, vec!["near", "middling", "far"]);
    }

    fn bss(ssid: &str, signal_dbm: i32, security: Security) -> Found {
        Found {
            bssid: "02:00:00:00:00:00".to_string(),
            frequency_mhz: 2412,
            signal_dbm,
            security,
            ssid: ssid.to_string(),
        }
    }

    #[test]
    fn a_network_that_cannot_be_joined_says_which_one_and_why() {
        assert_eq!(
            refusal("office", Security::Eap).as_deref(),
            Some("cannot join office: enterprise networks are not supported")
        );
        assert_eq!(
            refusal("old", Security::Wep).as_deref(),
            Some("cannot join old: WEP is not supported")
        );
        assert_eq!(
            refusal("strange", Security::Unknown).as_deref(),
            Some("cannot join strange: unknown security")
        );
        assert_eq!(refusal("cafe", Security::Open), None);
        assert_eq!(refusal("kitchen-table", Security::Psk), None);
    }

    #[test]
    fn a_join_sends_the_sequence_the_design_writes_down() {
        let table = RecordingSupplicant::new()
            .answering("LIST_NETWORKS", "network id / ssid / bssid / flags\n")
            .answering("ADD_NETWORK", "0")
            .ok("SET_NETWORK 0 ssid 6b69746368656e2d7461626c65")
            .ok("SET_NETWORK 0 psk \"correct horse battery staple\"")
            .ok("ENABLE_NETWORK 0")
            .ok("SELECT_NETWORK 0")
            .ok("SAVE_CONFIG");
        let mut client = Client::new(table);

        let id = join(
            &mut client,
            "kitchen-table",
            Some("correct horse battery staple"),
        )
        .expect("the join");
        assert_eq!(id, 0);
        assert_eq!(
            client.supplicant().transcript(),
            vec![
                "LIST_NETWORKS",
                "ADD_NETWORK",
                "SET_NETWORK 0 ssid 6b69746368656e2d7461626c65",
                "SET_NETWORK 0 psk \"correct horse battery staple\"",
                "ENABLE_NETWORK 0",
                "SELECT_NETWORK 0",
                "SAVE_CONFIG",
            ]
        );
    }

    #[test]
    fn an_ssid_the_supplicant_already_knows_is_reused_rather_than_added_again() {
        let table = RecordingSupplicant::new()
            .answering(
                "LIST_NETWORKS",
                "network id / ssid / bssid / flags\n0\tcafe\tany\t[DISABLED]\n",
            )
            .ok("SET_NETWORK 0 ssid 63616665")
            .ok("SET_NETWORK 0 key_mgmt NONE")
            .ok("ENABLE_NETWORK 0")
            .ok("SELECT_NETWORK 0")
            .ok("SAVE_CONFIG");
        let mut client = Client::new(table);

        assert_eq!(join(&mut client, "cafe", None).expect("the join"), 0);
        assert!(
            !client.supplicant().did("ADD_NETWORK"),
            "a second network was added for a name the supplicant already had"
        );
    }

    #[test]
    fn an_open_network_sets_key_mgmt_where_the_key_would_have_been() {
        let table = RecordingSupplicant::new()
            .answering("LIST_NETWORKS", "network id / ssid / bssid / flags\n")
            .answering("ADD_NETWORK", "1")
            .ok("SET_NETWORK 1 ssid 63616665")
            .ok("SET_NETWORK 1 key_mgmt NONE")
            .ok("ENABLE_NETWORK 1")
            .ok("SELECT_NETWORK 1")
            .ok("SAVE_CONFIG");
        let mut client = Client::new(table);

        join(&mut client, "cafe", None).expect("the join");
        assert!(client.supplicant().did("SET_NETWORK 1 key_mgmt NONE"));
        assert!(!client
            .supplicant()
            .transcript()
            .iter()
            .any(|line| line.contains(" psk ")));
    }

    #[test]
    fn a_scan_shows_what_the_supplicant_already_had_and_asks_for_more() {
        let (mut wifi, shared) = wifi(
            RecordingSupplicant::new()
                .answering("SCAN_RESULTS", SCAN_RESULTS)
                .ok("SCAN"),
        );
        let now = Instant::now();

        let (items, title) = wifi.begin_scan("wlan0", now).expect("the scan");
        assert_eq!(title, "wireless — scanning");
        assert_eq!(items[0].label, "kitchen-table");
        assert_eq!(items[0].detail, "-30 dBm  WPA2");
        assert_eq!(transcript(&shared), vec!["SCAN_RESULTS", "SCAN"]);
        assert_eq!(wifi.scanning_on(), Some("wlan0"));
    }

    #[test]
    fn the_title_stops_saying_scanning_when_the_results_change() {
        let more = format!("{SCAN_RESULTS}02:00:00:00:06:00\t2412\t-70\t[ESS]\tnext-door\n");
        let (mut wifi, _shared) = wifi(
            RecordingSupplicant::new()
                .answering("SCAN_RESULTS", SCAN_RESULTS)
                .answering("SCAN_RESULTS", &more)
                .ok("SCAN"),
        );
        let now = Instant::now();
        wifi.begin_scan("wlan0", now).expect("the scan");

        let (items, title) = wifi
            .refresh_scan(now + Duration::from_millis(500))
            .expect("the answer moved");
        assert_eq!(title, "wireless");
        assert_eq!(items.len(), 4);
    }

    #[test]
    fn a_scan_that_finds_nothing_new_stops_saying_scanning_after_ten_seconds() {
        let (mut wifi, _shared) = wifi(
            RecordingSupplicant::new()
                .answering("SCAN_RESULTS", SCAN_RESULTS)
                .ok("SCAN"),
        );
        let now = Instant::now();
        wifi.begin_scan("wlan0", now).expect("the scan");

        assert!(
            wifi.refresh_scan(now + Duration::from_secs(9)).is_none(),
            "the title moved while the radio could still be listening"
        );
        let (_, title) = wifi
            .refresh_scan(now + SCAN_SETTLES)
            .expect("ten seconds is long enough");
        assert_eq!(title, "wireless");
        assert!(
            wifi.refresh_scan(now + Duration::from_secs(11)).is_none(),
            "it went on reporting a title that had already stopped changing"
        );
    }

    #[test]
    fn a_scan_that_cannot_be_refreshed_leaves_the_rows_alone_and_says_nothing() {
        let (mut wifi, _shared) = wifi(
            RecordingSupplicant::new()
                .answering("SCAN_RESULTS", SCAN_RESULTS)
                .answering("SCAN_RESULTS", "FAIL")
                .ok("SCAN"),
        );
        let now = Instant::now();
        wifi.begin_scan("wlan0", now).expect("the scan");
        assert!(wifi
            .refresh_scan(now + Duration::from_millis(500))
            .is_none());
        assert_eq!(
            wifi.found("kitchen-table").map(|(_, security)| security),
            Some(Security::Psk),
            "the rows were thrown away because one answer went missing"
        );
    }

    #[test]
    fn the_row_that_was_chosen_is_looked_up_by_name() {
        let (mut wifi, _shared) = wifi(
            RecordingSupplicant::new()
                .answering("SCAN_RESULTS", SCAN_RESULTS)
                .ok("SCAN"),
        );
        wifi.begin_scan("wlan0", Instant::now()).expect("the scan");

        assert_eq!(
            wifi.found("office"),
            Some(("wlan0".to_string(), Security::Eap))
        );
        assert_eq!(wifi.found("nowhere"), None);
        wifi.forget_scan();
        assert_eq!(wifi.found("office"), None);
        assert_eq!(wifi.scanning_on(), None);
    }

    /// Everything a join sends, answered.
    fn joining_table() -> RecordingSupplicant {
        RecordingSupplicant::new()
            .answering("LIST_NETWORKS", "network id / ssid / bssid / flags\n")
            .answering("ADD_NETWORK", "0")
            .ok("SET_NETWORK 0 ssid 6b69746368656e2d7461626c65")
            .ok("SET_NETWORK 0 psk \"correct horse battery staple\"")
            .ok("ENABLE_NETWORK 0")
            .ok("SELECT_NETWORK 0")
            .ok("SAVE_CONFIG")
    }

    const COMPLETED: &str = "ssid=kitchen-table\nid=0\nwpa_state=COMPLETED\n";
    const HANDSHAKING: &str = "ssid=kitchen-table\nid=0\nwpa_state=4WAY_HANDSHAKE\n";

    #[test]
    fn a_status_that_reaches_completed_on_that_id_is_a_join() {
        let (mut wifi, _shared) = wifi(joining_table().answering("STATUS", COMPLETED));
        let now = Instant::now();

        assert_eq!(
            wifi.begin_join(
                "wlan0",
                "kitchen-table",
                Some("correct horse battery staple"),
                now
            )
            .expect("the join"),
            "joining kitchen-table"
        );
        assert_eq!(wifi.joining(), Some("kitchen-table"));
        assert_eq!(wifi.tick(now).as_deref(), Some("joined kitchen-table"));
        assert_eq!(wifi.joining(), None, "the client was held on to");
    }

    #[test]
    fn a_join_is_asked_about_once_a_second_and_not_once_a_frame() {
        let (mut wifi, shared) = wifi(joining_table().answering("STATUS", HANDSHAKING));
        let now = Instant::now();
        wifi.begin_join(
            "wlan0",
            "kitchen-table",
            Some("correct horse battery staple"),
            now,
        )
        .expect("the join");
        let sent = transcript(&shared).len();

        assert_eq!(wifi.tick(now), None);
        let after_one = transcript(&shared).len();
        assert_eq!(after_one, sent + 2, "one STATUS and one LIST_NETWORKS");
        for frame in 1..9 {
            assert_eq!(wifi.tick(now + Duration::from_millis(frame * 100)), None);
        }
        assert_eq!(
            transcript(&shared).len(),
            after_one,
            "the radio was asked again before the second was up"
        );
        assert_eq!(wifi.tick(now + Duration::from_secs(1)), None);
        assert_eq!(transcript(&shared).len(), after_one + 2);
    }

    #[test]
    fn a_network_that_goes_temp_disabled_under_the_join_is_a_wrong_passphrase() {
        let (mut wifi, shared) = wifi(
            joining_table()
                .answering("STATUS", HANDSHAKING)
                .answering(
                    "LIST_NETWORKS",
                    "network id / ssid / bssid / flags\n0\tkitchen-table\tany\t[TEMP-DISABLED]\n",
                )
                .ok("REMOVE_NETWORK 0")
                .ok("SAVE_CONFIG"),
        );
        let now = Instant::now();
        wifi.begin_join(
            "wlan0",
            "kitchen-table",
            Some("correct horse battery staple"),
            now,
        )
        .expect("the join");

        assert_eq!(
            wifi.tick(now).as_deref(),
            Some("kitchen-table: wrong passphrase")
        );
        let said = transcript(&shared);
        assert!(said.contains(&"REMOVE_NETWORK 0".to_string()), "{said:?}");
        assert_eq!(
            said.last().map(String::as_str),
            Some("SAVE_CONFIG"),
            "a key that is wrong was left in the file: {said:?}"
        );
        assert_eq!(wifi.joining(), None);
    }

    #[test]
    fn a_join_that_never_settles_is_called_off_at_the_deadline() {
        let (mut wifi, _shared) = wifi(joining_table().answering("STATUS", HANDSHAKING));
        let now = Instant::now();
        wifi.begin_join(
            "wlan0",
            "kitchen-table",
            Some("correct horse battery staple"),
            now,
        )
        .expect("the join");

        assert_eq!(wifi.tick(now + Duration::from_secs(29)), None);
        assert_eq!(
            wifi.tick(now + JOIN_DEADLINE).as_deref(),
            Some("kitchen-table: could not join")
        );
        assert_eq!(wifi.joining(), None);
    }

    #[test]
    fn a_socket_that_errors_under_a_join_names_the_command_that_failed() {
        let (mut wifi, _shared) = wifi(joining_table());
        let now = Instant::now();
        wifi.begin_join(
            "wlan0",
            "kitchen-table",
            Some("correct horse battery staple"),
            now,
        )
        .expect("the join");

        // Nothing taught the table a `STATUS`, so it answers `FAIL`.
        assert_eq!(
            wifi.tick(now).as_deref(),
            Some("kitchen-table: STATUS: FAIL")
        );
        assert_eq!(wifi.joining(), None);
    }

    #[test]
    fn a_second_join_while_one_is_in_the_air_says_what_it_is_waiting_on() {
        let (mut wifi, _shared) = wifi(joining_table().answering("STATUS", HANDSHAKING));
        let now = Instant::now();
        wifi.begin_join(
            "wlan0",
            "kitchen-table",
            Some("correct horse battery staple"),
            now,
        )
        .expect("the join");

        assert_eq!(
            wifi.begin_join("wlan0", "cafe", None, now)
                .expect("an answer"),
            "already joining kitchen-table on wlan0"
        );
    }

    #[test]
    fn a_passphrase_the_client_refuses_stops_before_the_key_is_sent() {
        let (mut wifi, shared) = wifi(joining_table());
        let error = wifi
            .begin_join("wlan0", "kitchen-table", Some("short"), Instant::now())
            .expect_err("a five character passphrase");

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(
            !transcript(&shared)
                .iter()
                .any(|line| line.contains(" psk ")),
            "the passphrase reached the daemon: {:?}",
            transcript(&shared)
        );
        assert_eq!(wifi.joining(), None);
    }

    #[test]
    fn what_a_passphrase_is_for_is_remembered_while_it_is_being_typed() {
        let (mut wifi, _shared) = wifi(RecordingSupplicant::new());
        assert_eq!(wifi.take_choice(), None);
        wifi.choose("wlan0", "kitchen-table");
        assert_eq!(
            wifi.take_choice(),
            Some(("wlan0".to_string(), "kitchen-table".to_string()))
        );
        assert_eq!(wifi.take_choice(), None, "the choice was taken twice");
    }
}
