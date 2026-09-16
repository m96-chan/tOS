//! `wpa_supplicant`, talked to over its control socket.
//!
//! `docs/design/network.md` chose the supplicant and said why not D-Bus;
//! `docs/design/wifi.md` (issue #137) is what is built on top of that choice,
//! and this module is its first section. Nothing here scans, associates or
//! derives a key: the daemon does all of that, and this is the client that
//! asks it to.
//!
//! The whole of the seam is one method. [`Supplicant::request`] sends one
//! command and returns one reply, and everything typed — [`Client`], the
//! three parsers, the quoting rules — sits above it and is generic over it.
//! So [`RecordingSupplicant`] is a table of `(command, reply)` pairs, in
//! exactly the shape [`crate::net::RecordingKernel`] and
//! [`crate::net::dhcp::FakeServer`] already have, and every test below runs
//! with no radio, no daemon and no privileges.
//!
//! # Polling, not attaching
//!
//! The control socket can `ATTACH` and then receive unsolicited events —
//! `<3>CTRL-EVENT-SCAN-RESULTS`, `<3>CTRL-EVENT-CONNECTED` — interleaved
//! with replies on the same socket. That is not used. The compositor already
//! wakes at least once a second for the machine poll, and everything the
//! events would say can be asked for: a scan's results arrive by asking
//! `SCAN_RESULTS` on the next tick, and a join's outcome is [`Status`]
//! reaching [`State::Completed`] or [`Known::temp_disabled`] turning true.
//! One socket, one direction, no demultiplexing, and a recorder that is a
//! table rather than a script.
//!
//! # What is quoted, and what is not
//!
//! An SSID goes as hex — `SET_NETWORK 0 ssid 6b69746368656e2d7461626c65` —
//! because the quoted form has quoting rules (a `"` inside, a backslash, a
//! non-ASCII byte) that would otherwise have to be implemented here and
//! tested against the supplicant's own parser. Hex has none, and it is what
//! makes an SSID with a tab in it join at all. A passphrase goes quoted,
//! because that is the form the supplicant derives a key from, and is
//! therefore checked here — 8 to 63 characters of printable ASCII with no `"`
//! in it — with a message naming the rule it broke, before a byte reaches the
//! daemon. A 64-digit hex string is the derived key already and goes
//! unquoted. See [`Client::set_passphrase`].

use std::fs;
use std::io;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Where the supplicant puts its per-interface sockets, which is what
/// `ctrl_interface=` in `/etc/wpa_supplicant/tos-<interface>.conf` is set to
/// by the unit the image ships.
pub const CONTROL_DIR: &str = "/run/wpa_supplicant";

/// Where this end of the conversation binds.
///
/// A datagram socket with no address of its own gets no reply, because the
/// supplicant answers with `sendto` on the sender's address, so the client
/// has to have a name in the filesystem too. `/run` is a tmpfs on every
/// machine this runs on, so the names do not survive a reboot.
pub const OWN_DIR: &str = "/run/tos";

/// How long a reply is waited for.
///
/// Every request is a local datagram answered in microseconds by a daemon on
/// the same machine, so this is not a latency budget: it is what a supplicant
/// that died mid-conversation costs. Half a second is a visible hitch in one
/// frame and not a session that has stopped answering the keyboard.
pub const TIMEOUT: Duration = Duration::from_millis(500);

/// The largest reply that is read.
///
/// `SCAN_RESULTS` in a block of flats is the only thing that comes close, and
/// the supplicant's own control interface caps what it writes well below
/// this. A datagram longer than the buffer is truncated by the kernel rather
/// than split, so the worst case is a parse that drops a trailing BSS, not a
/// reply that arrives in two halves and desynchronises the next request.
const REPLY_MAX: usize = 16 * 1024;

/// The most datagrams thrown away before a request, so that a flooded socket
/// cannot hold the loop.
const DRAIN_MAX: usize = 64;

/// One request, one reply.
///
/// That is the whole of what has to be faked, which is why it is a trait with
/// a single method rather than an interface with a method per command. The
/// typed layer is [`Client`], generic over this, so every test of it runs
/// against [`RecordingSupplicant`].
pub trait Supplicant {
    /// Send `command`, wait for the answer.
    ///
    /// The reply is the text the supplicant sent with its trailing newline
    /// removed, and nothing has been made of it yet: `OK`, `FAIL`, an id, or
    /// several lines to be handed to one of the parsers.
    fn request(&mut self, command: &str) -> io::Result<String>;
}

/// So that a `Box<dyn Supplicant>` is itself a [`Supplicant`].
///
/// `wifi.rs` holds an opener of type
/// `Box<dyn FnMut(&str) -> io::Result<Box<dyn Supplicant>>>`, which is how a
/// test hands the compositor a table where the machine has a socket. Without
/// this impl the boxed value could not be put into a [`Client`] at all.
impl<S: Supplicant + ?Sized> Supplicant for Box<S> {
    fn request(&mut self, command: &str) -> io::Result<String> {
        (**self).request(command)
    }
}

/// The supplicant that is really there.
///
/// A `UnixDatagram` bound to `/run/tos/wpa-<pid>-<interface>` and connected to
/// `/run/wpa_supplicant/<interface>`. Connecting is what makes a missing
/// daemon an error at [`SocketSupplicant::open`] rather than a timeout on the
/// first question, which is what lets the menu say "no supplicant on wlan0"
/// instead of offering a row that hangs for half a second and then fails.
#[derive(Debug)]
pub struct SocketSupplicant {
    socket: UnixDatagram,
    /// The path this end is bound to, unlinked on drop.
    path: PathBuf,
}

impl SocketSupplicant {
    /// Open the socket for `interface` in the real places.
    pub fn open(interface: &str) -> io::Result<SocketSupplicant> {
        SocketSupplicant::open_at(Path::new(CONTROL_DIR), Path::new(OWN_DIR), interface)
    }

    /// Open the socket for `interface` with both directories named.
    ///
    /// `control_dir` is where the supplicant's socket is and `own_dir` is
    /// where this end binds. Parameters rather than constants because that is
    /// what lets the tests below put a fake supplicant in a temporary
    /// directory and drive the real socket code against it; [`open`] is this
    /// with the real paths.
    ///
    /// If `own_dir` cannot be created or bound in, this falls back to
    /// [`std::env::temp_dir`] rather than failing. `/run/tos` is root's to
    /// make and the compositor is root, so the fallback is not for the live
    /// image — it is for a session started somewhere stranger (a developer's
    /// machine, a container with a read-only `/run`), where a client socket
    /// in `/tmp` talks to the daemon exactly as well. The address this end
    /// binds is nobody's business but the kernel's.
    ///
    /// [`open`]: SocketSupplicant::open
    pub fn open_at(
        control_dir: &Path,
        own_dir: &Path,
        interface: &str,
    ) -> io::Result<SocketSupplicant> {
        let name = format!("wpa-{}-{interface}", std::process::id());
        let (socket, path) = match bind_in(own_dir, &name) {
            Ok(bound) => bound,
            Err(why) => {
                let fallback = std::env::temp_dir();
                if fallback == own_dir {
                    return Err(why);
                }
                bind_in(&fallback, &name)?
            }
        };
        let bound = SocketSupplicant { socket, path };
        bound.socket.connect(control_dir.join(interface))?;
        bound.socket.set_read_timeout(Some(TIMEOUT))?;
        Ok(bound)
    }

    /// The path this end is bound to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Throw away anything already in the socket.
    ///
    /// Nothing here ever attaches, so the socket should be empty before every
    /// question. It is not empty after a request that timed out and whose
    /// reply then turned up anyway, and reading that reply as the answer to
    /// the *next* question is how a client ends up one answer behind for the
    /// rest of its life. One cheap drain makes a timeout cost half a second
    /// and nothing else.
    fn drain(&self) {
        if self.socket.set_nonblocking(true).is_err() {
            return;
        }
        let mut discard = [0u8; 64];
        for _ in 0..DRAIN_MAX {
            if self.socket.recv(&mut discard).is_err() {
                break;
            }
        }
        let _ = self.socket.set_nonblocking(false);
    }
}

impl Supplicant for SocketSupplicant {
    fn request(&mut self, command: &str) -> io::Result<String> {
        self.drain();
        self.socket.send(command.as_bytes())?;
        let mut buffer = vec![0u8; REPLY_MAX];
        let length = self.socket.recv(&mut buffer)?;
        let reply = String::from_utf8_lossy(&buffer[..length]).into_owned();
        Ok(reply.strip_suffix('\n').unwrap_or(&reply).to_string())
    }
}

impl Drop for SocketSupplicant {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Make `dir` and bind `name` in it.
///
/// A path left behind by a session that was killed is removed first: a
/// datagram socket's name outlives the process that bound it, and `bind`
/// answers an existing path with `EADDRINUSE` whether or not anything is
/// listening on it. The name carries this process's pid, so the only thing
/// that can be at that path is this process's own litter.
fn bind_in(dir: &Path, name: &str) -> io::Result<(UnixDatagram, PathBuf)> {
    fs::create_dir_all(dir)?;
    let path = dir.join(name);
    let _ = fs::remove_file(&path);
    let socket = UnixDatagram::bind(&path)?;
    Ok((socket, path))
}

/// A supplicant that answers out of a table and writes down what it was
/// asked.
///
/// Public rather than `#[cfg(test)]` for the same reason
/// [`crate::net::RecordingKernel`] is: it is how the compositor's whole
/// wireless path — menu, scan, passphrase, join, wrong passphrase, tick — is
/// driven from a test in another crate, on a machine with no radio.
///
/// The table is matched in order: the first send of a command is answered by
/// the first entry for it, the second by the second, and once the entries run
/// out the last one sticks. So a table with one entry per command is a
/// constant answer, which is what most of these are, and a table with two is a
/// reply that changes once — `STATUS` handshaking and then `COMPLETED`,
/// `LIST_NETWORKS` before and after a wrong passphrase — which is what a join
/// has to be driven through without a script and without a clock.
///
/// A command with no entry is answered `FAIL`, and is recorded like any other
/// — that last rule is deliberate: a test that forgot to teach the table a
/// command sees the command it forgot in the transcript, rather than an `OK`
/// it never wrote down.
#[derive(Debug, Clone, Default)]
pub struct RecordingSupplicant {
    /// `(command, reply)`, first match wins.
    pub replies: Vec<(String, String)>,
    /// Every command sent, in order.
    pub sent: Vec<String>,
}

impl RecordingSupplicant {
    pub fn new() -> RecordingSupplicant {
        RecordingSupplicant::default()
    }

    /// Answer `command` with `reply`.
    pub fn answering(mut self, command: &str, reply: &str) -> RecordingSupplicant {
        self.replies.push((command.to_string(), reply.to_string()));
        self
    }

    /// Answer `command` with `OK`, which is most of them.
    pub fn ok(self, command: &str) -> RecordingSupplicant {
        self.answering(command, "OK")
    }

    /// Everything it was asked, in order.
    pub fn transcript(&self) -> Vec<String> {
        self.sent.clone()
    }

    pub fn did(&self, needle: &str) -> bool {
        self.sent.iter().any(|line| line == needle)
    }
}

impl Supplicant for RecordingSupplicant {
    fn request(&mut self, command: &str) -> io::Result<String> {
        // How many times this command has been asked before, which is which of
        // its entries answers it. Counted out of the transcript rather than
        // kept in a cursor, so that the table stays a table: nothing about
        // `replies` has to be reset, cloned or torn down between questions.
        let asked_before = self.sent.iter().filter(|line| *line == command).count();
        self.sent.push(command.to_string());
        let matching: Vec<&String> = self
            .replies
            .iter()
            .filter(|(asked, _)| asked == command)
            .map(|(_, reply)| reply)
            .collect();
        // Once a command's entries run out the last one sticks, so a table
        // only has to write down the answers that change; with no entry at all
        // there is nothing to clamp to and the answer is `FAIL`.
        let which = asked_before.min(matching.len().saturating_sub(1));
        let reply = matching
            .get(which)
            .map(|reply| (*reply).clone())
            .unwrap_or_else(|| "FAIL".to_string());
        Ok(reply)
    }
}

/// The typed commands, over whatever [`Supplicant`] it was built on.
#[derive(Debug, Clone)]
pub struct Client<S: Supplicant> {
    supplicant: S,
}

impl<S: Supplicant> Client<S> {
    pub fn new(supplicant: S) -> Client<S> {
        Client { supplicant }
    }

    /// What it is talking to, which is how a test asks a
    /// [`RecordingSupplicant`] for its transcript.
    pub fn supplicant(&self) -> &S {
        &self.supplicant
    }

    /// Is the daemon there and answering? `PING` → `PONG`.
    pub fn ping(&mut self) -> io::Result<()> {
        let reply = self.supplicant.request("PING")?;
        if reply == "PONG" {
            Ok(())
        } else {
            Err(refused("PING", &reply))
        }
    }

    /// Ask for a scan.
    ///
    /// `FAIL-BUSY` is an answer, not an error: it means a scan is already
    /// running, which is precisely the state the caller was asking for.
    pub fn scan(&mut self) -> io::Result<()> {
        let reply = self.supplicant.request("SCAN")?;
        if reply == "OK" || reply == "FAIL-BUSY" {
            Ok(())
        } else {
            Err(refused("SCAN", &reply))
        }
    }

    /// What the last scan found, strongest first.
    pub fn scan_results(&mut self) -> io::Result<Vec<Found>> {
        let reply = self.ask("SCAN_RESULTS")?;
        Ok(parse_scan_results(&reply))
    }

    /// Where the association has got to.
    pub fn status(&mut self) -> io::Result<Status> {
        let reply = self.ask("STATUS")?;
        Ok(parse_status(&reply))
    }

    /// The networks the supplicant has been configured with.
    pub fn networks(&mut self) -> io::Result<Vec<Known>> {
        let reply = self.ask("LIST_NETWORKS")?;
        Ok(parse_networks(&reply))
    }

    /// Make an empty network and return its id.
    pub fn add_network(&mut self) -> io::Result<u32> {
        let reply = self.ask("ADD_NETWORK")?;
        reply
            .trim()
            .parse::<u32>()
            .map_err(|_| refused("ADD_NETWORK", &reply))
    }

    /// Set the SSID, as hex. See the module header for why not quoted.
    pub fn set_ssid(&mut self, id: u32, ssid: &str) -> io::Result<()> {
        self.expect_ok(&format!(
            "SET_NETWORK {id} ssid {}",
            to_hex(ssid.as_bytes())
        ))
    }

    /// Set the key.
    ///
    /// Two forms are accepted and they are not the same thing. A passphrase
    /// is 8 to 63 characters of printable ASCII with no `"` in it, which is
    /// what WPA2 allows and the whole of what the quoted form can carry
    /// unambiguously, and goes quoted for the supplicant to derive a key
    /// from. Exactly 64 hex digits is a key that has been derived already and
    /// goes unquoted. Anything else is refused here, naming the rule it
    /// broke, and nothing is sent — a supplicant told to take a passphrase it
    /// cannot use answers `FAIL` with no reason in it, and "wrong passphrase"
    /// half a minute later is a worse way to learn that a newline came along
    /// with a paste.
    pub fn set_passphrase(&mut self, id: u32, passphrase: &str) -> io::Result<()> {
        if is_derived_key(passphrase) {
            return self.expect_ok(&format!("SET_NETWORK {id} psk {passphrase}"));
        }
        if let Some(why) = passphrase_fault(passphrase) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, why));
        }
        self.expect_ok(&format!("SET_NETWORK {id} psk \"{passphrase}\""))
    }

    /// Say this network has no key at all.
    pub fn set_open(&mut self, id: u32) -> io::Result<()> {
        self.expect_ok(&format!("SET_NETWORK {id} key_mgmt NONE"))
    }

    /// Enable this network and associate with it.
    ///
    /// `SELECT_NETWORK` disables every other network for this association,
    /// which is the supplicant's own behaviour and the right one: somebody
    /// who chose a network from a list meant that one. `ENABLE_NETWORK`
    /// first, because a network left `[DISABLED]` by an earlier
    /// `DISABLE_NETWORK` is not one `SELECT_NETWORK` will associate with.
    pub fn select(&mut self, id: u32) -> io::Result<()> {
        self.expect_ok(&format!("ENABLE_NETWORK {id}"))?;
        self.expect_ok(&format!("SELECT_NETWORK {id}"))
    }

    /// Stop using this network without forgetting it.
    pub fn disable(&mut self, id: u32) -> io::Result<()> {
        self.expect_ok(&format!("DISABLE_NETWORK {id}"))
    }

    /// Forget this network. Not written to the file until [`Client::save`].
    pub fn remove(&mut self, id: u32) -> io::Result<()> {
        self.expect_ok(&format!("REMOVE_NETWORK {id}"))
    }

    /// Drop the association.
    pub fn disconnect(&mut self) -> io::Result<()> {
        self.expect_ok("DISCONNECT")
    }

    /// Write the configuration back to the file it was read from, which is
    /// what makes the machine rejoin at the next boot without being asked.
    /// Needs `update_config=1`, which the unit in `iso/mkiso.sh` writes.
    pub fn save(&mut self) -> io::Result<()> {
        self.expect_ok("SAVE_CONFIG")
    }

    /// Send a command that should be answered `OK`.
    fn expect_ok(&mut self, command: &str) -> io::Result<()> {
        let reply = self.supplicant.request(command)?;
        if reply == "OK" {
            Ok(())
        } else {
            Err(refused(command, &reply))
        }
    }

    /// Send a command whose reply is the answer rather than `OK`.
    ///
    /// A refusal still has to be an error: a `SCAN_RESULTS` that answered
    /// `FAIL` would otherwise parse into an empty list and be shown as "no
    /// networks in range", which is a lie nobody can get behind.
    fn ask(&mut self, command: &str) -> io::Result<String> {
        let reply = self.supplicant.request(command)?;
        if reply == "FAIL" || reply.starts_with("FAIL-") || reply.starts_with("FAIL\n") {
            return Err(refused(command, &reply));
        }
        Ok(reply)
    }
}

/// The error a refusal becomes.
///
/// It carries the command, so that the status line reads
/// `SET_NETWORK 0 psk: FAIL` rather than `failed` — the whole point of naming
/// it is that somebody reading a screenshot can tell which step of a join went
/// wrong. Only the first line of the reply is kept; the rest, on the rare
/// command that says more, belongs in a log and not in a status bar.
fn refused(command: &str, reply: &str) -> io::Error {
    let said = match reply.lines().next() {
        Some(line) if !line.is_empty() => line,
        _ => "no reply",
    };
    io::Error::other(format!("{command}: {said}"))
}

/// Lowercase hex, which is the form the supplicant's own parser reads.
fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    out
}

/// Exactly 64 hex digits: a key that has already been derived.
fn is_derived_key(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Which rule a passphrase broke, or `None` if it broke none.
///
/// The order is the order somebody wants to be told about: what is in the
/// string before how much of it there is, because a paste that brought a
/// newline along with it is a different mistake from one that was typed
/// short.
fn passphrase_fault(passphrase: &str) -> Option<String> {
    if let Some(bad) = passphrase.chars().find(|c| !(' '..='~').contains(c)) {
        return Some(format!(
            "a passphrase is printable ASCII, and this one has {bad:?} in it"
        ));
    }
    if passphrase.contains('"') {
        return Some("a passphrase cannot contain a \" character".to_string());
    }
    let length = passphrase.chars().count();
    if length < 8 {
        return Some(format!(
            "a passphrase is 8 to 63 characters, and this one is {length}"
        ));
    }
    if length > 63 {
        return Some(format!(
            "a passphrase is 8 to 63 characters, and this one is {length} \
             (a 64-digit hex key is the other thing that is accepted here)"
        ));
    }
    None
}

/// One BSS from a scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub bssid: String,
    pub frequency_mhz: u32,
    pub signal_dbm: i32,
    pub security: Security,
    /// The supplicant's printable rendering, which escapes any byte that is
    /// not printable ASCII as `\xNN`. It is shown as sent and joined by the
    /// hex form, so nothing has to be unescaped and nothing is lost.
    pub ssid: String,
}

/// What a network wants before it will let a machine on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Security {
    /// Nothing at all: a café, an airport, a printer.
    Open,
    /// A passphrase. WPA2, WPA3 (`SAE`) and the WPA1 nobody should still be
    /// running all reach the same prompt.
    Psk,
    /// Enterprise. Needs an identity, a method and often a certificate;
    /// #137 says so in the list rather than offering a row that fails.
    Eap,
    /// WEP, which is not security and is not supported.
    Wep,
    /// Flags this does not know. The menu offers a row that says it cannot
    /// join it, because a row that does nothing without saying why is the
    /// thing `network.md` refused to ship.
    Unknown,
}

impl Security {
    /// Read the security out of a scan line's flags field.
    pub fn from_flags(flags: &str) -> Security {
        let upper = flags.to_ascii_uppercase();
        if upper.contains("SAE") || upper.contains("WPA2-PSK") || upper.contains("WPA-PSK") {
            return Security::Psk;
        }
        if upper.contains("EAP") {
            return Security::Eap;
        }
        if upper.contains("WEP") {
            return Security::Wep;
        }
        // Everything left is either a list of flags that say nothing about
        // security — every access point advertises `[ESS]` — or something
        // this was not taught, and the two must not be confused: the first is
        // an open network somebody can join, the second is a network nothing
        // here knows how to try.
        let benign = ["ESS", "IBSS", "MESH", "WPS", "P2P", "UTF-8"];
        for token in upper.split(']') {
            let token = token.trim_start_matches('[').trim();
            if token.is_empty() {
                continue;
            }
            // `[WPS-PBC]`, `[WPS-PIN]` and `[WPS-AUTH]` are what an access
            // point advertises while its button is pressed, and they say
            // nothing about what it wants from a station that is not using
            // them: an open network with its button pressed is still open.
            if !benign.contains(&token) && !token.starts_with("WPS") {
                return Security::Unknown;
            }
        }
        Security::Open
    }
}

/// `SCAN_RESULTS`: a header line, then one tab-separated line per BSS.
///
/// A line with fewer than five fields is skipped rather than refused: one
/// malformed BSS is not a reason to lose the list, and the header goes by the
/// same rule, since it is a single field with ` / ` in it. The order the
/// supplicant sent is kept, which is strongest signal first.
pub fn parse_scan_results(text: &str) -> Vec<Found> {
    let mut found = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.splitn(5, '\t').collect();
        if fields.len() < 5 {
            continue;
        }
        let (Ok(frequency_mhz), Ok(signal_dbm)) = (
            fields[1].trim().parse::<u32>(),
            fields[2].trim().parse::<i32>(),
        ) else {
            continue;
        };
        found.push(Found {
            bssid: fields[0].to_string(),
            frequency_mhz,
            signal_dbm,
            security: Security::from_flags(fields[3]),
            ssid: fields[4].to_string(),
        });
    }
    found
}

/// How far into an association the supplicant has got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Disconnected,
    Inactive,
    Scanning,
    Authenticating,
    Associating,
    Associated,
    /// `4WAY_HANDSHAKE`, where a wrong passphrase is found out.
    FourWayHandshake,
    GroupHandshake,
    /// Associated and keyed. On a wireless link this is carrier, and
    /// `net::auto` asks for an address the moment it sees it.
    Completed,
    /// `INTERFACE_DISABLED`: the radio is off, by rfkill or by the link being
    /// down.
    InterfaceDisabled,
    /// Something this was not taught, carried through as sent so the status
    /// line can still say it.
    Unknown(String),
}

impl State {
    pub fn as_str(&self) -> &str {
        match self {
            State::Disconnected => "disconnected",
            State::Inactive => "inactive",
            State::Scanning => "scanning",
            State::Authenticating => "authenticating",
            State::Associating => "associating",
            State::Associated => "associated",
            State::FourWayHandshake => "handshaking",
            State::GroupHandshake => "handshaking",
            State::Completed => "associated",
            State::InterfaceDisabled => "disabled",
            State::Unknown(said) => said,
        }
    }
}

/// What `STATUS` says.
///
/// `wpa_state` is the only field that decides anything; the rest is what the
/// status line and the link menu say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub state: State,
    pub ssid: Option<String>,
    /// The id of the network being used, which is what `leave` and `forget`
    /// act on and what a join watches for.
    pub id: Option<u32>,
}

/// `STATUS`: `key=value` lines.
///
/// A `STATUS` with no `wpa_state` line is `State::Unknown("")` — not a default
/// of `Disconnected`, because "the supplicant did not say" and "the supplicant
/// said it is not connected" are different answers and only one of them means
/// something is wrong.
pub fn parse_status(text: &str) -> Status {
    let mut status = Status {
        state: State::Unknown(String::new()),
        ssid: None,
        id: None,
    };
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "wpa_state" => {
                status.state = match value {
                    "DISCONNECTED" => State::Disconnected,
                    "INACTIVE" => State::Inactive,
                    "SCANNING" => State::Scanning,
                    "AUTHENTICATING" => State::Authenticating,
                    "ASSOCIATING" => State::Associating,
                    "ASSOCIATED" => State::Associated,
                    "4WAY_HANDSHAKE" => State::FourWayHandshake,
                    "GROUP_HANDSHAKE" => State::GroupHandshake,
                    "COMPLETED" => State::Completed,
                    "INTERFACE_DISABLED" => State::InterfaceDisabled,
                    other => State::Unknown(other.to_string()),
                }
            }
            "ssid" => status.ssid = Some(value.to_string()),
            "id" => status.id = value.trim().parse::<u32>().ok(),
            _ => {}
        }
    }
    status
}

/// One network the supplicant has been configured with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Known {
    pub id: u32,
    pub ssid: String,
    /// `[CURRENT]`: the one being used.
    pub current: bool,
    /// `[DISABLED]`: turned off by `DISABLE_NETWORK`.
    pub disabled: bool,
    /// `[TEMP-DISABLED]`, which is how a wrong passphrase looks from outside:
    /// the supplicant tries the four-way handshake, fails it, and disables the
    /// network for a while rather than trying forever. That flag, on the
    /// network that was just selected, *is* "wrong passphrase" — there is no
    /// other way to learn it without attaching to the event stream, and
    /// attaching is what is being avoided.
    pub temp_disabled: bool,
}

/// `LIST_NETWORKS`: a header, then `id / ssid / bssid / flags`.
pub fn parse_networks(text: &str) -> Vec<Known> {
    let mut known = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.splitn(4, '\t').collect();
        if fields.len() < 4 {
            continue;
        }
        let Ok(id) = fields[0].trim().parse::<u32>() else {
            continue;
        };
        let flags = fields[3];
        known.push(Known {
            id,
            ssid: fields[1].to_string(),
            current: flags.contains("[CURRENT]"),
            // `[TEMP-DISABLED]` does not contain `[DISABLED]`, which is why
            // the brackets are matched and not the word.
            disabled: flags.contains("[DISABLED]"),
            temp_disabled: flags.contains("[TEMP-DISABLED]"),
        });
    }
    known
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Instant;

    /// `SCAN_RESULTS` as `wpa_supplicant` 2:2.10 writes it, in the shape the
    /// `mac80211_hwsim` witness in `docs/design/wifi.md` dumps.
    const SCAN_RESULTS: &str = concat!(
        "bssid / frequency / signal level / flags / ssid\n",
        "02:00:00:00:01:00\t2412\t-30\t[WPA2-PSK-CCMP][ESS]\tkitchen-table\n",
        "02:00:00:00:02:00\t5180\t-52\t[ESS]\tcafe\n",
    );

    /// `STATUS` from the same witness, associated and keyed.
    const STATUS: &str = concat!(
        "bssid=02:00:00:00:01:00\n",
        "freq=2412\n",
        "ssid=kitchen-table\n",
        "id=0\n",
        "mode=station\n",
        "pairwise_cipher=CCMP\n",
        "group_cipher=CCMP\n",
        "key_mgmt=WPA2-PSK\n",
        "wpa_state=COMPLETED\n",
        "address=02:00:00:00:00:00\n",
    );

    /// `LIST_NETWORKS` from the same witness, after a wrong passphrase on
    /// network 2.
    const LIST_NETWORKS: &str = concat!(
        "network id / ssid / bssid / flags\n",
        "0\tkitchen-table\tany\t[CURRENT]\n",
        "1\tcafe\tany\t[DISABLED]\n",
        "2\toffice\tany\t[TEMP-DISABLED]\n",
    );

    /// What a running supplicant actually wrote, copied out of the serial log
    /// of the `mac80211_hwsim` witness in `docs/design/wifi.md` — wpa_supplicant
    /// v2.10 on the image built from this tree, 2026-09-16. The examples above
    /// are what the format is documented to be; this is what it was.
    mod captured {
        use super::super::*;

        const SCAN_RESULTS: &str = "bssid / frequency / signal level / flags / ssid\n\
            02:00:00:00:01:00\t2412\t-30\t[WPA2-PSK-CCMP][WPS][ESS]\thwsim-ap\n";

        /// The supplicant with nothing configured, which is what a fresh boot
        /// looks like: `INACTIVE`, and three fields no parser here wants.
        const STATUS_INACTIVE: &str = "wpa_state=INACTIVE\n\
            p2p_device_address=42:00:00:00:00:00\n\
            address=02:00:00:00:00:00\n\
            uuid=362db47b-a53a-5191-88fb-5458b986b2e4\n";

        /// Joined, from the menu, with the right passphrase.
        const STATUS_COMPLETED: &str = "bssid=02:00:00:00:01:00\n\
            freq=2412\n\
            ssid=hwsim-ap\n\
            id=0\n\
            mode=station\n\
            wifi_generation=4\n\
            pairwise_cipher=CCMP\n\
            group_cipher=CCMP\n\
            key_mgmt=WPA2-PSK\n\
            wpa_state=COMPLETED\n\
            p2p_device_address=42:00:00:00:00:00\n\
            address=02:00:00:00:00:00\n\
            uuid=362db47b-a53a-5191-88fb-5458b986b2e4\n";

        const NETWORKS_CURRENT: &str =
            "network id / ssid / bssid / flags\n0\thwsim-ap\tany\t[CURRENT]\n";
        const NETWORKS_NONE: &str = "network id / ssid / bssid / flags\n";

        #[test]
        fn the_scan_a_real_supplicant_wrote_parses_to_the_access_point() {
            let found = parse_scan_results(SCAN_RESULTS);
            assert_eq!(found.len(), 1);
            assert_eq!(found[0].bssid, "02:00:00:00:01:00");
            assert_eq!(found[0].frequency_mhz, 2412);
            assert_eq!(found[0].signal_dbm, -30);
            // `[WPS]` beside the security flags, which the benign list has
            // to know about or every access point with WPS on would be
            // "unknown security".
            assert_eq!(found[0].security, Security::Psk);
            assert_eq!(found[0].ssid, "hwsim-ap");
        }

        #[test]
        fn the_status_a_real_supplicant_wrote_parses_in_both_states() {
            let inactive = parse_status(STATUS_INACTIVE);
            assert_eq!(inactive.state, State::Inactive);
            assert_eq!(inactive.ssid, None);
            assert_eq!(inactive.id, None);

            let completed = parse_status(STATUS_COMPLETED);
            assert_eq!(completed.state, State::Completed);
            assert_eq!(completed.ssid.as_deref(), Some("hwsim-ap"));
            assert_eq!(completed.id, Some(0));
        }

        #[test]
        fn the_network_list_a_real_supplicant_wrote_parses_full_and_empty() {
            let known = parse_networks(NETWORKS_CURRENT);
            assert_eq!(known.len(), 1);
            assert_eq!(known[0].id, 0);
            assert_eq!(known[0].ssid, "hwsim-ap");
            assert!(known[0].current);
            assert!(!known[0].disabled && !known[0].temp_disabled);
            assert!(parse_networks(NETWORKS_NONE).is_empty());
        }
    }

    #[test]
    fn a_scan_result_line_becomes_a_network_in_range() {
        let found = parse_scan_results(SCAN_RESULTS);
        assert_eq!(found.len(), 2, "two BSSes and a header");
        assert_eq!(
            found[0],
            Found {
                bssid: "02:00:00:00:01:00".to_string(),
                frequency_mhz: 2412,
                signal_dbm: -30,
                security: Security::Psk,
                ssid: "kitchen-table".to_string(),
            }
        );
        assert_eq!(found[1].security, Security::Open);
        assert_eq!(found[1].signal_dbm, -52);
    }

    #[test]
    fn the_order_the_supplicant_sent_is_the_order_returned() {
        let found = parse_scan_results(SCAN_RESULTS);
        let ssids: Vec<&str> = found.iter().map(|bss| bss.ssid.as_str()).collect();
        assert_eq!(ssids, vec!["kitchen-table", "cafe"]);
    }

    #[test]
    fn an_escaped_ssid_is_kept_exactly_as_the_supplicant_wrote_it() {
        let text = concat!(
            "bssid / frequency / signal level / flags / ssid\n",
            "02:00:00:00:03:00\t2437\t-44\t[WPA2-PSK-CCMP][ESS]\tcaf\\xc3\\xa9\n",
        );
        let found = parse_scan_results(text);
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].ssid, "caf\\xc3\\xa9",
            "it is shown as sent and joined by its hex, so nothing is unescaped"
        );
    }

    #[test]
    fn a_scan_line_with_too_few_fields_is_skipped_and_the_rest_survive() {
        let text = concat!(
            "bssid / frequency / signal level / flags / ssid\n",
            "02:00:00:00:01:00\t2412\t-30\t[ESS]\tkitchen-table\n",
            "02:00:00:00:09:00\t2412\t-30\n",
            "02:00:00:00:02:00\t5180\t-52\t[ESS]\tcafe\n",
        );
        let ssids: Vec<String> = parse_scan_results(text)
            .into_iter()
            .map(|bss| bss.ssid)
            .collect();
        assert_eq!(ssids, vec!["kitchen-table", "cafe"]);
    }

    #[test]
    fn a_scan_line_whose_numbers_are_not_numbers_is_skipped() {
        let text = "02:00:00:00:01:00\tchannel one\t-30\t[ESS]\tkitchen-table\n";
        assert!(parse_scan_results(text).is_empty());
    }

    #[test]
    fn an_empty_scan_result_is_an_empty_list_and_not_an_error() {
        assert!(parse_scan_results("").is_empty());
    }

    #[test]
    fn a_scan_result_with_only_a_header_is_an_empty_list() {
        assert!(parse_scan_results("bssid / frequency / signal level / flags / ssid\n").is_empty());
    }

    #[test]
    fn a_hidden_network_still_parses_with_an_empty_ssid() {
        let text = "02:00:00:00:04:00\t2412\t-61\t[WPA2-PSK-CCMP][ESS]\t\n";
        let found = parse_scan_results(text);
        assert_eq!(found.len(), 1, "the menu decides not to show it, not this");
        assert_eq!(found[0].ssid, "");
    }

    #[test]
    fn an_access_point_with_its_wps_button_pressed_is_still_what_it_was() {
        assert_eq!(Security::from_flags("[WPS-PBC][ESS]"), Security::Open);
        assert_eq!(Security::from_flags("[ESS][WPS-PIN]"), Security::Open);
        assert_eq!(
            Security::from_flags("[WPA2-PSK-CCMP][WPS-AUTH][ESS]"),
            Security::Psk
        );
    }

    #[test]
    fn security_is_read_out_of_the_flags() {
        let table = [
            ("[WPA2-PSK-CCMP][ESS]", Security::Psk),
            ("[WPA-PSK-TKIP][ESS]", Security::Psk),
            ("[WPA2-PSK-CCMP][WPA2-PSK-SHA256-CCMP][ESS]", Security::Psk),
            ("[WPA2-SAE-CCMP][ESS]", Security::Psk),
            ("[WPA2-PSK+SAE-CCMP][ESS]", Security::Psk),
            ("[WPA2-EAP-CCMP][ESS]", Security::Eap),
            ("[WPA2-EAP-SUITE-B-192-GCMP-256][ESS]", Security::Eap),
            ("[WEP][ESS]", Security::Wep),
            ("[ESS]", Security::Open),
            ("[WPS][ESS]", Security::Open),
            ("[P2P]", Security::Open),
            ("[ESS][UTF-8]", Security::Open),
            ("[IBSS]", Security::Open),
            ("", Security::Open),
            ("[OWE-CCMP][ESS]", Security::Unknown),
            ("[DMG][IBSS][something-new]", Security::Unknown),
        ];
        for (flags, expected) in table {
            assert_eq!(Security::from_flags(flags), expected, "flags {flags:?}");
        }
    }

    #[test]
    fn a_status_says_the_state_the_ssid_and_the_id() {
        let status = parse_status(STATUS);
        assert_eq!(status.state, State::Completed);
        assert_eq!(status.ssid.as_deref(), Some("kitchen-table"));
        assert_eq!(status.id, Some(0));
    }

    #[test]
    fn a_status_with_no_wpa_state_is_unknown_rather_than_disconnected() {
        let status = parse_status("address=02:00:00:00:00:00\nmode=station\n");
        assert_eq!(status.state, State::Unknown(String::new()));
        assert_eq!(status.ssid, None);
        assert_eq!(status.id, None);
    }

    #[test]
    fn every_state_the_supplicant_writes_is_recognised() {
        let table = [
            ("DISCONNECTED", State::Disconnected),
            ("INACTIVE", State::Inactive),
            ("SCANNING", State::Scanning),
            ("AUTHENTICATING", State::Authenticating),
            ("ASSOCIATING", State::Associating),
            ("ASSOCIATED", State::Associated),
            ("4WAY_HANDSHAKE", State::FourWayHandshake),
            ("GROUP_HANDSHAKE", State::GroupHandshake),
            ("COMPLETED", State::Completed),
            ("INTERFACE_DISABLED", State::InterfaceDisabled),
        ];
        for (written, expected) in table {
            let status = parse_status(&format!("wpa_state={written}\n"));
            assert_eq!(status.state, expected, "wpa_state={written}");
        }
    }

    #[test]
    fn a_state_this_was_not_taught_is_carried_through_as_sent() {
        let status = parse_status("wpa_state=UNINITIALIZED\n");
        assert_eq!(status.state, State::Unknown("UNINITIALIZED".to_string()));
        assert_eq!(status.state.as_str(), "UNINITIALIZED");
    }

    #[test]
    fn a_status_line_with_no_equals_sign_is_skipped() {
        let status = parse_status("this is not a key\nwpa_state=SCANNING\n");
        assert_eq!(status.state, State::Scanning);
    }

    #[test]
    fn list_networks_becomes_the_networks_that_are_known() {
        let known = parse_networks(LIST_NETWORKS);
        assert_eq!(known.len(), 3, "three networks and a header");
        assert_eq!(
            known[0],
            Known {
                id: 0,
                ssid: "kitchen-table".to_string(),
                current: true,
                disabled: false,
                temp_disabled: false,
            }
        );
        assert!(known[1].disabled);
        assert!(!known[1].temp_disabled);
    }

    #[test]
    fn temp_disabled_is_not_read_as_disabled() {
        let known = parse_networks(LIST_NETWORKS);
        let office = &known[2];
        assert!(
            office.temp_disabled,
            "which is the only way a wrong passphrase is seen"
        );
        assert!(
            !office.disabled,
            "a network the supplicant is resting is not one somebody turned off"
        );
    }

    #[test]
    fn a_network_line_with_too_few_fields_is_skipped() {
        let text = concat!(
            "network id / ssid / bssid / flags\n",
            "0\tkitchen-table\tany\t\n",
            "nonsense\n",
            "1\tcafe\tany\t[DISABLED]\n",
        );
        let known = parse_networks(text);
        assert_eq!(known.len(), 2);
        assert_eq!(known[0].ssid, "kitchen-table");
        assert!(
            !known[0].current,
            "no flags at all is a network doing nothing"
        );
    }

    #[test]
    fn an_empty_network_list_is_an_empty_list() {
        assert!(parse_networks("").is_empty());
        assert!(parse_networks("network id / ssid / bssid / flags\n").is_empty());
    }

    // The typed layer, against the table.

    #[test]
    fn a_ping_is_answered_by_a_pong() {
        let mut client = Client::new(RecordingSupplicant::new().answering("PING", "PONG"));
        client.ping().expect("a live supplicant");
        assert_eq!(client.supplicant().transcript(), vec!["PING"]);
    }

    #[test]
    fn a_ping_that_is_not_answered_by_a_pong_names_the_command() {
        let mut client = Client::new(RecordingSupplicant::new());
        let why = client.ping().expect_err("no supplicant");
        assert_eq!(why.to_string(), "PING: FAIL");
    }

    #[test]
    fn a_scan_is_asked_for_by_name() {
        let mut client = Client::new(RecordingSupplicant::new().ok("SCAN"));
        client.scan().expect("a scan");
        assert!(client.supplicant().did("SCAN"));
    }

    #[test]
    fn a_scan_that_is_already_running_is_not_an_error() {
        let mut client = Client::new(RecordingSupplicant::new().answering("SCAN", "FAIL-BUSY"));
        client
            .scan()
            .expect("a scan already running is the state that was asked for");
    }

    #[test]
    fn a_scan_that_is_refused_for_any_other_reason_is_an_error() {
        let mut client = Client::new(RecordingSupplicant::new());
        assert_eq!(
            client.scan().expect_err("refused").to_string(),
            "SCAN: FAIL"
        );
    }

    #[test]
    fn scan_results_are_asked_for_and_parsed() {
        let mut client =
            Client::new(RecordingSupplicant::new().answering("SCAN_RESULTS", SCAN_RESULTS));
        let found = client.scan_results().expect("results");
        assert_eq!(found.len(), 2);
        assert_eq!(client.supplicant().transcript(), vec!["SCAN_RESULTS"]);
    }

    #[test]
    fn a_refused_query_is_an_error_rather_than_an_empty_list() {
        let mut client = Client::new(RecordingSupplicant::new());
        assert_eq!(
            client.scan_results().expect_err("refused").to_string(),
            "SCAN_RESULTS: FAIL"
        );
    }

    #[test]
    fn a_status_is_asked_for_and_parsed() {
        let mut client = Client::new(RecordingSupplicant::new().answering("STATUS", STATUS));
        assert_eq!(client.status().expect("a status").state, State::Completed);
        assert!(client.supplicant().did("STATUS"));
    }

    #[test]
    fn the_networks_are_asked_for_and_parsed() {
        let mut client =
            Client::new(RecordingSupplicant::new().answering("LIST_NETWORKS", LIST_NETWORKS));
        assert_eq!(client.networks().expect("networks").len(), 3);
        assert!(client.supplicant().did("LIST_NETWORKS"));
    }

    #[test]
    fn adding_a_network_returns_the_id_the_supplicant_chose() {
        let mut client = Client::new(RecordingSupplicant::new().answering("ADD_NETWORK", "3"));
        assert_eq!(client.add_network().expect("an id"), 3);
    }

    #[test]
    fn adding_a_network_answered_with_something_that_is_not_an_id_is_an_error() {
        let mut client = Client::new(RecordingSupplicant::new());
        assert_eq!(
            client.add_network().expect_err("refused").to_string(),
            "ADD_NETWORK: FAIL"
        );
    }

    #[test]
    fn an_ssid_is_sent_as_hex_and_never_quoted() {
        let mut client = Client::new(
            RecordingSupplicant::new().ok("SET_NETWORK 0 ssid 6b69746368656e2d7461626c65"),
        );
        client.set_ssid(0, "kitchen-table").expect("the ssid");
        assert_eq!(
            client.supplicant().transcript(),
            vec!["SET_NETWORK 0 ssid 6b69746368656e2d7461626c65"]
        );
    }

    #[test]
    fn an_ssid_with_a_tab_or_a_quote_in_it_is_hex_like_any_other() {
        let mut client = Client::new(RecordingSupplicant::new());
        let _ = client.set_ssid(1, "a\tb\"c");
        assert_eq!(
            client.supplicant().transcript(),
            vec!["SET_NETWORK 1 ssid 6109622263"],
            "which is what makes it joinable at all"
        );
    }

    #[test]
    fn a_passphrase_is_sent_quoted() {
        let command = "SET_NETWORK 0 psk \"correct horse battery staple\"";
        let mut client = Client::new(RecordingSupplicant::new().ok(command));
        client
            .set_passphrase(0, "correct horse battery staple")
            .expect("the key");
        assert_eq!(client.supplicant().transcript(), vec![command]);
    }

    #[test]
    fn a_sixty_four_digit_hex_key_is_sent_unquoted_as_the_key_it_is() {
        let key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let command = format!("SET_NETWORK 0 psk {key}");
        let mut client = Client::new(RecordingSupplicant::new().ok(&command));
        client.set_passphrase(0, key).expect("the key");
        assert_eq!(client.supplicant().transcript(), vec![command]);
    }

    #[test]
    fn a_passphrase_that_is_too_short_is_refused_before_anything_is_sent() {
        let mut client = Client::new(RecordingSupplicant::new());
        let why = client.set_passphrase(0, "short").expect_err("too short");
        assert_eq!(why.kind(), io::ErrorKind::InvalidInput);
        assert!(
            why.to_string().contains("8 to 63"),
            "it has to say which rule: {why}"
        );
        assert!(
            client.supplicant().transcript().is_empty(),
            "nothing reaches the supplicant"
        );
    }

    #[test]
    fn a_passphrase_that_is_too_long_is_refused_before_anything_is_sent() {
        let mut client = Client::new(RecordingSupplicant::new());
        let why = client
            .set_passphrase(0, &"x".repeat(64))
            .expect_err("too long");
        assert_eq!(why.kind(), io::ErrorKind::InvalidInput);
        assert!(why.to_string().contains("8 to 63"), "{why}");
        assert!(client.supplicant().transcript().is_empty());
    }

    #[test]
    fn a_passphrase_with_a_quote_in_it_is_refused_before_anything_is_sent() {
        let mut client = Client::new(RecordingSupplicant::new());
        let why = client
            .set_passphrase(0, "say \"hello\" now")
            .expect_err("a quote");
        assert_eq!(why.kind(), io::ErrorKind::InvalidInput);
        assert!(why.to_string().contains('"'), "{why}");
        assert!(client.supplicant().transcript().is_empty());
    }

    #[test]
    fn a_passphrase_that_is_not_printable_ascii_is_refused_before_anything_is_sent() {
        let mut client = Client::new(RecordingSupplicant::new());
        let why = client
            .set_passphrase(0, "ぱすわーどですよ")
            .expect_err("not ASCII");
        assert_eq!(why.kind(), io::ErrorKind::InvalidInput);
        assert!(why.to_string().contains("printable ASCII"), "{why}");
        assert!(client.supplicant().transcript().is_empty());
    }

    #[test]
    fn a_passphrase_with_a_pasted_newline_in_it_is_refused() {
        let mut client = Client::new(RecordingSupplicant::new());
        let why = client
            .set_passphrase(0, "correct horse\n")
            .expect_err("a newline");
        assert!(why.to_string().contains("printable ASCII"), "{why}");
        assert!(client.supplicant().transcript().is_empty());
    }

    #[test]
    fn the_shortest_and_the_longest_passphrase_are_both_accepted() {
        for length in [8usize, 63] {
            let passphrase = "p".repeat(length);
            let command = format!("SET_NETWORK 0 psk \"{passphrase}\"");
            let mut client = Client::new(RecordingSupplicant::new().ok(&command));
            client
                .set_passphrase(0, &passphrase)
                .unwrap_or_else(|why| panic!("{length} characters: {why}"));
        }
    }

    #[test]
    fn an_open_network_sets_key_mgmt_and_no_key() {
        let mut client = Client::new(RecordingSupplicant::new().ok("SET_NETWORK 2 key_mgmt NONE"));
        client.set_open(2).expect("open");
        assert_eq!(
            client.supplicant().transcript(),
            vec!["SET_NETWORK 2 key_mgmt NONE"]
        );
    }

    #[test]
    fn selecting_a_network_enables_it_first() {
        let mut client = Client::new(
            RecordingSupplicant::new()
                .ok("ENABLE_NETWORK 4")
                .ok("SELECT_NETWORK 4"),
        );
        client.select(4).expect("selected");
        assert_eq!(
            client.supplicant().transcript(),
            vec!["ENABLE_NETWORK 4", "SELECT_NETWORK 4"]
        );
    }

    #[test]
    fn a_select_stops_at_the_first_refusal() {
        let mut client = Client::new(RecordingSupplicant::new());
        let why = client.select(4).expect_err("refused");
        assert_eq!(why.to_string(), "ENABLE_NETWORK 4: FAIL");
        assert_eq!(
            client.supplicant().transcript(),
            vec!["ENABLE_NETWORK 4"],
            "the second command is not sent after the first failed"
        );
    }

    #[test]
    fn leaving_and_forgetting_and_saving_are_each_one_command() {
        let mut client = Client::new(
            RecordingSupplicant::new()
                .ok("DISABLE_NETWORK 0")
                .ok("DISCONNECT")
                .ok("REMOVE_NETWORK 0")
                .ok("SAVE_CONFIG"),
        );
        client.disable(0).expect("disabled");
        client.disconnect().expect("disconnected");
        client.remove(0).expect("removed");
        client.save().expect("saved");
        assert_eq!(
            client.supplicant().transcript(),
            vec![
                "DISABLE_NETWORK 0",
                "DISCONNECT",
                "REMOVE_NETWORK 0",
                "SAVE_CONFIG"
            ]
        );
    }

    #[test]
    fn a_refusal_carries_the_command_it_was_a_reply_to() {
        let mut client = Client::new(RecordingSupplicant::new());
        let why = client.set_passphrase(0, "correct horse").expect_err("FAIL");
        assert_eq!(
            why.to_string(),
            "SET_NETWORK 0 psk \"correct horse\": FAIL",
            "so that the status line says which step went wrong"
        );
    }

    #[test]
    fn a_refusal_with_a_reason_keeps_the_reason() {
        let mut client = Client::new(
            RecordingSupplicant::new().answering("REMOVE_NETWORK 9", "FAIL-UNKNOWN-NETWORK"),
        );
        assert_eq!(
            client.remove(9).expect_err("refused").to_string(),
            "REMOVE_NETWORK 9: FAIL-UNKNOWN-NETWORK"
        );
    }

    #[test]
    fn a_command_with_no_entry_in_the_table_is_still_written_down() {
        let mut client = Client::new(RecordingSupplicant::new());
        let _ = client.disconnect();
        assert!(
            client.supplicant().did("DISCONNECT"),
            "a test that forgot to teach the table sees what it forgot"
        );
    }

    #[test]
    fn the_first_matching_entry_in_the_table_wins() {
        let mut supplicant = RecordingSupplicant::new()
            .answering("PING", "PONG")
            .answering("PING", "FAIL");
        assert_eq!(supplicant.request("PING").expect("a reply"), "PONG");
    }

    #[test]
    fn a_second_entry_for_the_same_command_answers_the_second_ask() {
        // What a join is driven through: the network is being handshaked with
        // on the first look and has been temporarily disabled by the time of
        // the second, which is the whole of how a wrong passphrase is found
        // out without attaching to the event stream.
        let mut supplicant = RecordingSupplicant::new()
            .answering("LIST_NETWORKS", "network id / ssid / bssid / flags\n")
            .answering(
                "LIST_NETWORKS",
                "network id / ssid / bssid / flags\n0\tcafe\tany\t[TEMP-DISABLED]\n",
            );
        assert!(!supplicant
            .request("LIST_NETWORKS")
            .expect("a reply")
            .contains("TEMP-DISABLED"));
        assert!(supplicant
            .request("LIST_NETWORKS")
            .expect("a reply")
            .contains("TEMP-DISABLED"));
        // And once the entries run out, the last one is what the table goes on
        // saying, so a test only has to write down the answers that change.
        assert!(supplicant
            .request("LIST_NETWORKS")
            .expect("a reply")
            .contains("TEMP-DISABLED"));
    }

    #[test]
    fn a_join_sends_the_sequence_the_design_writes_down() {
        let commands = [
            "ADD_NETWORK",
            "SET_NETWORK 1 ssid 6b69746368656e2d7461626c65",
            "SET_NETWORK 1 psk \"correct horse battery staple\"",
            "ENABLE_NETWORK 1",
            "SELECT_NETWORK 1",
            "SAVE_CONFIG",
        ];
        let mut supplicant = RecordingSupplicant::new().answering("ADD_NETWORK", "1");
        for command in &commands[1..] {
            supplicant = supplicant.ok(command);
        }
        let mut client = Client::new(supplicant);

        let id = client.add_network().expect("an id");
        client.set_ssid(id, "kitchen-table").expect("the ssid");
        client
            .set_passphrase(id, "correct horse battery staple")
            .expect("the key");
        client.select(id).expect("selected");
        client.save().expect("saved");

        assert_eq!(client.supplicant().transcript(), commands.to_vec());
    }

    #[test]
    fn joining_an_open_network_sends_key_mgmt_where_the_key_would_have_been() {
        let commands = [
            "ADD_NETWORK",
            "SET_NETWORK 0 ssid 63616665",
            "SET_NETWORK 0 key_mgmt NONE",
            "ENABLE_NETWORK 0",
            "SELECT_NETWORK 0",
            "SAVE_CONFIG",
        ];
        let mut supplicant = RecordingSupplicant::new().answering("ADD_NETWORK", "0");
        for command in &commands[1..] {
            supplicant = supplicant.ok(command);
        }
        let mut client = Client::new(supplicant);

        let id = client.add_network().expect("an id");
        client.set_ssid(id, "cafe").expect("the ssid");
        client.set_open(id).expect("open");
        client.select(id).expect("selected");
        client.save().expect("saved");

        assert_eq!(client.supplicant().transcript(), commands.to_vec());
        assert!(
            !client
                .supplicant()
                .transcript()
                .iter()
                .any(|command| command.contains("psk")),
            "an open network gets no key"
        );
    }

    #[test]
    fn a_boxed_supplicant_is_a_supplicant() {
        let boxed: Box<dyn Supplicant> =
            Box::new(RecordingSupplicant::new().answering("PING", "PONG"));
        let mut client = Client::new(boxed);
        client.ping().expect("a live supplicant");
    }

    // The socket, against a supplicant that is not one.

    /// A throwaway directory that cleans up after itself.
    struct Dir {
        root: PathBuf,
    }

    impl Dir {
        fn new(name: &str) -> Dir {
            let root = std::env::temp_dir().join(format!("tos-wpa-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(root.join("wpa_supplicant")).unwrap();
            Dir { root }
        }

        fn control(&self) -> PathBuf {
            self.root.join("wpa_supplicant")
        }

        fn own(&self) -> PathBuf {
            self.root.join("tos")
        }

        /// A socket where the supplicant's would be, bound and not yet
        /// answering.
        fn daemon(&self, interface: &str) -> UnixDatagram {
            UnixDatagram::bind(self.control().join(interface)).unwrap()
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn a_request_goes_out_on_the_socket_and_the_reply_comes_back() {
        let dir = Dir::new("round-trip");
        let daemon = dir.daemon("wlan0");
        let answering = thread::spawn(move || {
            let mut buffer = [0u8; 256];
            let (length, from) = daemon.recv_from(&mut buffer).unwrap();
            assert_eq!(&buffer[..length], b"PING");
            // The supplicant answers with sendto on the sender's address,
            // which is the whole reason this end is bound to a path at all.
            let address = from.as_pathname().expect("a bound client").to_path_buf();
            daemon.send_to(b"PONG\n", address).unwrap();
        });

        let mut supplicant =
            SocketSupplicant::open_at(&dir.control(), &dir.own(), "wlan0").expect("a socket");
        assert_eq!(
            supplicant.request("PING").expect("a reply"),
            "PONG",
            "the trailing newline is the transport's and not the answer's"
        );
        answering.join().expect("the fake supplicant");
    }

    #[test]
    fn the_whole_client_runs_over_a_real_socket() {
        let dir = Dir::new("over-a-socket");
        let daemon = dir.daemon("wlan0");
        let answering = thread::spawn(move || {
            let mut buffer = [0u8; 256];
            for _ in 0..2 {
                let (length, from) = daemon.recv_from(&mut buffer).unwrap();
                let asked = String::from_utf8_lossy(&buffer[..length]).into_owned();
                let reply = if asked == "ADD_NETWORK" {
                    "7\n"
                } else {
                    "OK\n"
                };
                let address = from.as_pathname().expect("a bound client").to_path_buf();
                daemon.send_to(reply.as_bytes(), address).unwrap();
            }
        });

        let mut client = Client::new(
            SocketSupplicant::open_at(&dir.control(), &dir.own(), "wlan0").expect("a socket"),
        );
        let id = client.add_network().expect("an id");
        assert_eq!(id, 7);
        client.set_ssid(id, "cafe").expect("the ssid");
        answering.join().expect("the fake supplicant");
    }

    #[test]
    fn the_clients_own_socket_is_unlinked_when_it_is_dropped() {
        let dir = Dir::new("unlinked");
        let _daemon = dir.daemon("wlan0");
        let supplicant =
            SocketSupplicant::open_at(&dir.control(), &dir.own(), "wlan0").expect("a socket");
        let path = supplicant.path().to_path_buf();
        assert!(path.exists(), "it is bound while it is open");
        drop(supplicant);
        assert!(
            !path.exists(),
            "and leaves nothing behind, or the next session cannot bind it"
        );
    }

    #[test]
    fn a_path_left_behind_by_a_killed_session_is_bound_again() {
        let dir = Dir::new("rebound");
        let _daemon = dir.daemon("wlan0");
        let first = SocketSupplicant::open_at(&dir.control(), &dir.own(), "wlan0").expect("once");
        let path = first.path().to_path_buf();
        // Leaking it is what a killed process does: the socket's name stays
        // in the filesystem, and bind answers an existing path with
        // EADDRINUSE whether or not anything is listening on it.
        std::mem::forget(first);
        let second = SocketSupplicant::open_at(&dir.control(), &dir.own(), "wlan0").expect("twice");
        assert_eq!(second.path(), path);
    }

    #[test]
    fn a_supplicant_that_never_answers_costs_half_a_second_and_not_the_session() {
        let dir = Dir::new("silent");
        let _daemon = dir.daemon("wlan0");
        let mut supplicant =
            SocketSupplicant::open_at(&dir.control(), &dir.own(), "wlan0").expect("a socket");

        let started = Instant::now();
        let why = supplicant.request("PING").expect_err("nobody answered");
        let waited = started.elapsed();

        assert!(
            matches!(
                why.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ),
            "a timeout and not something else: {why} ({:?})",
            why.kind()
        );
        // The timeout is half a second; the bound here is loose because a
        // test machine under load schedules when it feels like it. What is
        // being asserted is that it came back at all.
        assert!(
            waited < Duration::from_secs(2),
            "waited {waited:?}, which is a session that has stopped answering"
        );
    }

    #[test]
    fn opening_a_socket_where_no_supplicant_is_listening_fails_at_once() {
        let dir = Dir::new("no-daemon");
        let why = SocketSupplicant::open_at(&dir.control(), &dir.own(), "wlan0")
            .expect_err("no supplicant on wlan0");
        assert_eq!(
            why.kind(),
            io::ErrorKind::NotFound,
            "so the menu can say so instead of offering a row that hangs"
        );
    }

    #[test]
    fn a_reply_left_behind_by_a_timed_out_request_is_not_read_as_the_next_answer() {
        let dir = Dir::new("late");
        let daemon = dir.daemon("wlan0");
        let mut supplicant =
            SocketSupplicant::open_at(&dir.control(), &dir.own(), "wlan0").expect("a socket");

        // The first question times out, and its answer turns up afterwards.
        supplicant.request("PING").expect_err("nobody answered yet");
        let mut buffer = [0u8; 256];
        let (_, from) = daemon.recv_from(&mut buffer).unwrap();
        let address = from.as_pathname().expect("a bound client").to_path_buf();
        daemon.send_to(b"PONG\n", &address).unwrap();

        // The second question must be answered by the second answer.
        let answering = thread::spawn(move || {
            let mut buffer = [0u8; 256];
            let (_, from) = daemon.recv_from(&mut buffer).unwrap();
            let address = from.as_pathname().expect("a bound client").to_path_buf();
            daemon.send_to(b"OK\n", address).unwrap();
        });
        assert_eq!(
            supplicant.request("SAVE_CONFIG").expect("a reply"),
            "OK",
            "or the client is one answer behind for the rest of its life"
        );
        answering.join().expect("the fake supplicant");
    }
}
