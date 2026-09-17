//! The compositor itself.
//!
//! It owns the panes, the session layout, the fonts and the display, and runs
//! the loop that reads PTYs and input, updates terminals, and paints frames.

use std::collections::HashMap;
use std::io;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use tos_font::{BitmapFont, FontStack, GlyphSource};
use tos_input::encode::{encode_alternate_scroll, EncodeContext};
use tos_input::{
    encode_focus, encode_key, encode_mouse, encode_paste, InputEvent, KeyEvent, MouseAction,
    MouseButton, MouseEvent,
};
use tos_platform::Display;
use tos_render::{render, Rect as PixelRect, RenderOptions, Surface};
use tos_session::{
    describe, Action, Arrangement, Axis, DividerId, Keymap, PaneId, Rect, Resolution, Session,
};
use tos_system::audio::Volume;
use tos_system::bluetooth::{Adapter, Connection, SystemControl};
use tos_system::net::auto::{Autoconfigure, Step};
use tos_system::net::wpa::Security;
use tos_system::net::{dhcp, Interface, Kind, Lease};
use tos_system::power::PowerAction;
use tos_system::Sysfs;
use tos_term::TermEvent;

use crate::bluetooth::{Choice, Controls, Scan, SCAN_SECONDS};
use crate::chrome::{self, Chrome};
use crate::clock::Clock;
use crate::config::Config;
use crate::copymode::{CopyMode, CopyOutcome};
use crate::ime::{self, Ime, ImeOutcome};
use crate::launcher;
use crate::lock::{self, Backdrop, LockOutcome, LockScreen, Repaint};
use crate::notify::{self, Chosen, Notifications};
use crate::overlay::{Overlay, OverlayItem, OverlayOutcome, Placement};
use crate::pane::Pane;
use crate::pointer::{self, Pointer};
use crate::power;
use crate::selection::{Selection, SelectionMode};
use crate::splash::Splash;
use crate::status::{self, Bar, Hit, Piece, Segment};
use crate::system::Machine;
use crate::wifi::{self, Wifi};

/// How often the cursor and blinking text change phase.
const BLINK_INTERVAL: Duration = Duration::from_millis(530);
/// Longest a frame may wait when nothing is happening.
const IDLE_TIMEOUT_MS: i32 = 100;
/// Longest it may wait when the screen is dark.
///
/// Nothing on a blanked screen can need repainting, so the only thing worth
/// coming back for is a deadline of its own, and those are folded into the
/// wait below and win whenever they are nearer. This is what is left when
/// there is none: a heartbeat rather than a poll loop. Not unbounded, because
/// the loop is also where `main` looks at the flags its signal handlers set
/// and where a pane that has gone is noticed, and a dark screen should never
/// be the reason either of those takes a noticeable while.
const BLANKED_TIMEOUT_MS: i32 = 60_000;
/// How long to wait when a pane still has input queued for its child.
const WRITE_RETRY_TIMEOUT_MS: i32 = 4;
/// The most a program may put into one selection with OSC 52. The clipboard
/// carries text a person copies and pastes, and 64 KiB is already a thousand
/// full lines — far more than anyone pastes into a shell. The cap is what
/// keeps a pane from parking megabytes in the compositor that nobody will
/// ever paste.
const MAX_CLIPBOARD_BYTES: usize = 64 * 1024;
/// How close together two presses have to be to count as a double click.
const MULTI_CLICK_INTERVAL: Duration = Duration::from_millis(400);
/// The OSC 52 selector for the clipboard, which an explicit copy writes.
const CLIPBOARD: char = 'c';
/// The selector for primary, which is where the mouse puts what it selects
/// and where a middle click pastes from.
const PRIMARY: char = 'p';

/// The last left press, kept so that the one after it can tell whether it is
/// a second or a third click of the same gesture.
#[derive(Debug, Clone, Copy)]
struct Click {
    pane: PaneId,
    col: usize,
    row: usize,
    at: Instant,
    count: u32,
}

/// What the mouse is holding on to between a press and the release that ends
/// it.
///
/// One value rather than a field each, because there is one pointer: a drag
/// that started on a divider must not also be dragging a selection out of the
/// pane beside it, and two `Option`s would be two states that can both be
/// `Some` and a rule written down in comments to say they must not be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Grab {
    /// A selection being dragged out of this pane.
    Pane(PaneId),
    /// A divider being dragged.
    Divider(DividerGrab),
}

/// A divider under the pointer, and where in it the press landed.
///
/// The offset matters once the gap is more than one cell wide: without it,
/// grabbing a thick divider anywhere but its leading edge would snap it under
/// the pointer on the first cell of movement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DividerGrab {
    id: DividerId,
    offset: u32,
}

/// Which menu an open overlay is, and so what choosing a row means.
///
/// The overlay itself is generic; this is the compositor's side of it. The
/// other system menus — power, network, Bluetooth, audio — each add a variant
/// here and a match arm in [`Compositor::choose`], and reuse everything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayKind {
    /// A program from `$PATH`, which starts in a new pane.
    Launcher,
    /// What has been said on the status bar, newest first.
    Notifications,
    /// A line of text: the new name for the active workspace.
    RenameWorkspace,
    /// The key bindings, which are a list to read rather than to choose from.
    Bindings,
    /// The Bluetooth controls: the adapter and what can be done to it. Rebuilt
    /// under the user when a scan lands, so the row that was chosen is looked
    /// up in [`crate::bluetooth::Controls`] rather than read off the label.
    Bluetooth,
    /// The three ways a machine stops: power off, reboot, suspend.
    Power,
    /// The second half of a power off or a reboot: the menu that has to be
    /// answered before it happens. The action is carried in the kind rather
    /// than looked up again from the row, so that the thing being confirmed is
    /// decided once, by the menu that asked.
    ConfirmPower(PowerAction),
    /// The machine's interfaces. Choosing one opens [`OverlayKind::Link`]
    /// for it.
    Networks,
    /// What can be done to the interface named by
    /// [`Compositor::network_target`].
    ///
    /// The interface is not carried in the variant, because that would make
    /// `OverlayKind` a type with a `String` in it: it is copied out of the
    /// open overlay on every keystroke that closes one, and every other menu
    /// would start paying for a payload it has not got.
    Link,
    /// The wireless networks in range of the radio named by
    /// [`crate::wifi::Wifi::scanning_on`], refreshed under the user while the
    /// scan runs. Payload-free for the reason [`OverlayKind::Link`] is.
    Wireless,
    /// The passphrase for the network [`crate::wifi::Wifi::take_choice`]
    /// names, typed behind bullets. Payload-free for the same reason again.
    Passphrase,
}

/// Which way a volume binding turns the knob.
///
/// One [`Compositor::change_volume`] rather than three near-identical methods,
/// because everything except the one call into the mixer — no card, a card
/// that refused, refreshing the reading, saying what happened — is the same
/// for all three, and three copies of it would be three places for the "no
/// card" case to be got wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Knob {
    Up,
    Down,
    Mute,
}

/// What to put on the status bar after the volume moved.
///
/// Muted is said instead of the level, not beside it, because the level of a
/// muted card is not the question anybody has: turning a muted card up is a
/// thing people do by mistake, and "45%" would look exactly like it had
/// worked. A card with no mute switch is muted by being turned to zero, so on
/// that hardware the two readings agree anyway.
fn volume_status(volume: Volume) -> String {
    if volume.muted {
        "muted".to_string()
    } else {
        format!("volume {}%", volume.percent)
    }
}

/// The rows of [`OverlayKind::Link`].
///
/// Named rather than written twice, because the label is how the row is
/// matched when it is chosen: the menu that offers "bring the link up" and
/// the arm that acts on it are the same string or they are a row that does
/// nothing when pressed. Only one of the first two is ever offered — the one
/// the link is not already doing.
const BRING_UP: &str = "bring the link up";
const TAKE_DOWN: &str = "take the link down";
const REQUEST_ADDRESS: &str = "ask for an address";
const JOIN: &str = "join a wireless network";

/// The two rows that name the network a radio is on, and the row that says
/// there is nothing to ask.
///
/// Prefixes rather than whole labels, because the SSID is on the end of each
/// of them: `leave kitchen-table` is the row, and what it is matched by when
/// it is chosen is everything up to the name.
const LEAVE: &str = "leave ";
const FORGET: &str = "forget ";
const NO_SUPPLICANT: &str = "no supplicant on ";

/// What the row above says when it is chosen, which is the only thing anybody
/// can do about it.
const INSTALL_SUPPLICANT: &str = "is wpasupplicant installed?";

/// How often the links are looked at on the machine's own behalf (#124).
///
/// The same second everything else about the machine is read on, for the same
/// reason: a cable is plugged in by hand, and the loop is awake anyway.
const AUTO_LOOK: Duration = Duration::from_secs(1);

/// And how often behind a blank.
///
/// A blanked machine is left alone everywhere else in tOS — [`Machine::poll`]
/// does not even read it, and its deadline is kept out of the wait — and this
/// is the one exception, because the machine this whole path exists for is a
/// machine with nobody at it, and a screen that blanked ten minutes ago is
/// what that machine's screen has done. A cable plugged into it would
/// otherwise wait for a keystroke that is never coming.
///
/// A minute, because that is [`BLANKED_TIMEOUT_MS`]: the dark loop already
/// comes round that often to look at its signal flags, so this rides a
/// wakeup that was going to happen and adds none. Asking for less would mean
/// waking a sleeping machine more often to find the same cable still out.
const AUTO_LOOK_BLANKED: Duration = Duration::from_millis(BLANKED_TIMEOUT_MS as u64);

/// The right hand column of a row in the interface list.
///
/// What somebody deciding which interface to poke wants, in the order they
/// want it: what sort of link it is, what network it is on if it is wireless,
/// and then the one fact that answers "is this the one" — an address, or the
/// reason there is not one. `Interface::summary` is the status bar's answer
/// to a related question and starts with the name, which is already the label
/// here, so this is not it.
fn link_detail(interface: &Interface) -> String {
    let mut parts = vec![interface.kind.as_str().to_string()];
    if let Some(ssid) = interface.ssid() {
        parts.push(ssid.to_string());
    }
    match interface.ipv4().or_else(|| interface.ipv6()) {
        Some(address) => parts.push(address.to_string()),
        None if !interface.admin_up => parts.push("down".to_string()),
        None if !interface.carrier => parts.push("no carrier".to_string()),
        None => parts.push(format!("{}, no address", interface.state.as_str())),
    }
    if interface.is_default {
        parts.push("default route".to_string());
    }
    parts.join("  ")
}

/// The running compositor.
pub struct Compositor {
    config: Config,
    /// The account this session's panes run as, read once here rather than at
    /// every spawn: a passwd file that changes under a running session must
    /// not mean that the third pane is somebody the first two are not.
    ///
    /// `None` is a machine whose `/etc/passwd` has no line for `TOS_USER`,
    /// which is the live image's business only if somebody edits it. A pane
    /// then starts as whoever the compositor is, which is what it did before
    /// any of this existed.
    session_account: Option<crate::account::Account>,
    session: Session,
    panes: HashMap<PaneId, Pane>,
    fonts: FontStack,
    keymap: Keymap,
    /// Japanese input: the romaji table and the dictionary, one per session.
    ///
    /// Beside `fonts` and `keymap` because it is the same kind of thing — a
    /// megabyte of data read once and consulted by whichever pane has the
    /// focus. What a person is half-way through typing is not here; that is
    /// on the pane, in [`crate::ime::ImeContext`].
    ime: Ime,
    chrome: Chrome,
    /// Display size in pixels.
    size: (u32, u32),
    /// Selections stored by OSC 52 selector: 'c' is the clipboard, which an
    /// explicit copy writes, and 'p' is primary, which the mouse writes.
    clipboard: HashMap<char, Vec<u8>>,
    blink_visible: bool,
    last_blink: Instant,
    /// Everything the compositor and its panes have had to say, queued for the
    /// status bar and kept for the history list.
    notifications: Notifications,
    /// The open menu, if any. While it is open it owns the keyboard.
    overlay: Option<(OverlayKind, Overlay)>,
    /// The lock screen, if the session is locked. While it is up it owns the
    /// input — every kind of it, not only the keys — and the screen shows
    /// nothing of the session.
    lock: Option<LockScreen>,
    /// The picture that goes with whichever screen is up (#132): the login
    /// screen's, above its box, or the lock screen's, in the corner.
    ///
    /// One slot and not two, because the two screens are never up at once.
    /// Which file filled it is decided where the screen goes up, and where it
    /// is drawn by [`LockScreen::draw`] from the screen's own purpose.
    ///
    /// Held only while that screen is up: it is read when the screen goes up
    /// and let go when it is answered, because a megabyte of pixels nobody is
    /// going to look at again is a megabyte a session could have had.
    picture: Option<Splash>,
    /// Copy mode, and the pane it is selecting in. While it is up it owns the
    /// keyboard: no key reaches the pane and no binding fires.
    ///
    /// The pane is remembered rather than looked up from the focus each time,
    /// because a selection belongs to the text it was drawn over. Nothing can
    /// move the focus while copy mode has the keyboard, but a pane can still
    /// die under it, and a copy mode that followed the focus would come back
    /// pointing at lines it never saw.
    copy: Option<(PaneId, CopyMode)>,
    /// The last pane died while the screen was locked.
    ///
    /// Ending the session is a way out of a locked screen, so a locked one
    /// cannot be allowed to take it. The session ends when the password is
    /// accepted instead.
    session_ended_while_locked: bool,
    needs_full_redraw: bool,
    running: bool,
    /// The mouse pointer: where it is, whether it is being shown, and where
    /// the last frame drew it. See [`crate::pointer`].
    pointer: Pointer,
    /// What a mouse button went down on, while it is still down.
    mouse_grab: Option<Grab>,
    /// The previous left press, for double and triple click.
    last_click: Option<Click>,
    /// Some pane still has input queued, so the loop must not idle.
    pending_writes: bool,
    /// When somebody last did anything. Input only: see [`Compositor::handle_input`].
    last_activity: Instant,
    /// Whether the display has been told to stop showing anything.
    ///
    /// What the display was last told, rather than what it is about to be
    /// told, so that the keystroke which arrives at a panel that is still
    /// physically dark is recognised as one.
    blanked: bool,
    /// Whether the lock deadline has already been acted on this idle period.
    ///
    /// A deadline fires once, not on every pass after it. Without this a
    /// machine with no password would read the credential file and find it
    /// missing a few times a second for as long as nobody touched it.
    idle_lock_done: bool,
    /// The display refused to go dark, so this idle period stops asking.
    blank_refused: bool,
    /// The machine underneath the session: battery, link, volume and adapter,
    /// re-read on a timer rather than on damage, because nothing a person does
    /// to a pane is what makes a cable go in. See [`crate::system`].
    machine: Machine,
    /// The Bluetooth menu's state and the inquiry thread, if one is running.
    /// Kept on the compositor rather than in the overlay, because a scan
    /// outlives the menu that started it: closing the box does not stop the
    /// controller listening, and the answer still has somewhere to land.
    bluetooth: Controls,
    /// A suspend has been agreed to and has not happened yet.
    ///
    /// The compositor cannot carry one out itself: giving up DRM master and
    /// the input grabs is the loop's business, because the loop is what holds
    /// the display and the devices. So this is a flag the loop takes, the same
    /// shape as the VT switch the kernel asks about. See [`crate::power`].
    suspend_requested: bool,
    /// How this session is ending, when it is ending because somebody asked
    /// the machine to stop rather than asked tOS to.
    ///
    /// Kept rather than acted on for the same reason, and for one more: a
    /// `reboot(2)` from inside the loop would leave the console in graphics
    /// mode with its keyboard off, so whatever the kernel says on the way down
    /// — including why it could not unmount something — would be said onto a
    /// screen nobody can read. The session ends first, the terminal and the
    /// display go back, and only then does the machine stop.
    shutdown: Option<PowerAction>,
    /// This machine has already been told it has no sound card.
    ///
    /// Whether there is a card is settled once, at the first ask, and never
    /// changes for the life of the session — so saying it again is saying the
    /// same true thing a second time. Without this, holding a volume key down
    /// on a machine with no card fills the notification queue with one
    /// sentence repeated, and pushes off the bar whatever was actually worth
    /// reading.
    said_no_sound_card: bool,
    /// The clock on the status bar, with its zone already read.
    clock: Clock,
    /// What the clock last said.
    ///
    /// The repaint trigger, and the reason it is the rendered text rather than
    /// a timestamp: a bar showing `%H:%M` has to repaint when the minute turns
    /// over and not sixty times before it, and one showing `%S` has to repaint
    /// every second. Comparing what would be drawn answers both without the
    /// clock having to be asked how precise it is.
    clock_text: String,
    /// The interface [`OverlayKind::Link`] is about, put here when the
    /// interface list was chosen from and read when its menu is.
    network_target: Option<String>,
    /// A DHCP acquisition in flight: the interface it is for, and where its
    /// answer will arrive.
    ///
    /// It is on a thread because of how long it is allowed to take. A server
    /// that is there answers in milliseconds, but a network with no server on
    /// it is fifteen seconds of waiting, and the frame loop cannot spend
    /// fifteen seconds anywhere: the clock would stop, the cursor would stop
    /// blinking, and the keyboard would appear to have died — on a machine
    /// whose owner has just been told something is being asked for.
    ///
    /// Only the waiting is on the thread. The ioctls that put the lease on
    /// the link happen back here, on the thread that owns the [`Machine`],
    /// which is why nothing has to be shared but the answer.
    dhcp: Option<(String, mpsc::Receiver<io::Result<Lease>>)>,
    /// The wired links brought up and addressed without anybody asking (#124).
    ///
    /// Policy only: what it decides is carried out through the same
    /// `Network::bring_up` and [`Compositor::request_address`] a person's
    /// keystroke goes through, so there is one way to get an address on this
    /// machine and the automatic path is the menu with nobody at it.
    auto: Autoconfigure,
    /// When the links were last looked at on the machine's behalf, or `None`
    /// before the first look — which is due immediately, because a machine
    /// that has just booted is exactly the machine this is for.
    auto_looked_at: Option<Instant>,
    /// The wireless menus' state: the scan under an open list, the network a
    /// passphrase is being typed for, and a join waiting on a handshake (#137).
    ///
    /// On the compositor rather than in the `OverlayKind` for the reason
    /// `network_target` is, and on the compositor rather than in the overlay
    /// for the reason [`Compositor::bluetooth`] is: a join outlives the menu
    /// that started it, and the answer still has somewhere to land.
    ///
    /// [`Compositor::bluetooth`]: Compositor::bluetooth
    wifi: Wifi,
}

impl Compositor {
    /// Build a compositor for a display of this size.
    pub fn new(
        config: Config,
        size: (u32, u32),
        physical_mm: Option<(u32, u32)>,
    ) -> io::Result<Self> {
        let fonts = build_fonts(&config, size, physical_mm);
        // Before the config is moved into the struct, and once rather than per
        // frame: this reads the time zone database off the disk.
        let clock = Clock::new(&config.status.clock_format, &config.status.zone);
        // Before the struct, and allowed to fail: a machine with no
        // dictionary types kana and converts nothing, which is a far better
        // session than no session.
        let (ime, dictionary_problem) = Ime::open(config.ime_dictionary.as_deref());
        let mut compositor = Compositor {
            session: Session::new(),
            panes: HashMap::new(),
            fonts,
            keymap: Keymap::default_bindings(),
            ime,
            chrome: config.chrome,
            size,
            clipboard: HashMap::new(),
            blink_visible: true,
            last_blink: Instant::now(),
            notifications: Notifications::new(),
            overlay: None,
            lock: None,
            picture: None,
            copy: None,
            session_ended_while_locked: false,
            needs_full_redraw: true,
            running: true,
            pointer: Pointer::default(),
            mouse_grab: None,
            last_click: None,
            pending_writes: false,
            last_activity: Instant::now(),
            blanked: false,
            idle_lock_done: false,
            blank_refused: false,
            machine: Machine::at(Sysfs::new(&config.system_root)),
            bluetooth: Controls::new(),
            suspend_requested: false,
            shutdown: None,
            said_no_sound_card: false,
            // Seeded rather than left empty, so that the first frame — which
            // happens before the first tick — has a time on it rather than a
            // gap where one is about to appear.
            clock_text: clock.text(unix_now()),
            clock,
            network_target: None,
            dhcp: None,
            auto: Autoconfigure::new(),
            auto_looked_at: None,
            wifi: Wifi::new(),
            // Read before `config` is moved in, and kept even when it says
            // nothing can be dropped to: `credentials_for` is what decides
            // that, at the pane, so that the rule lives in one place.
            session_account: crate::account::read(
                &config.passwd,
                &config.group,
                &config.credential_user,
            )
            .ok(),
            config,
        };

        // A configured dictionary that would not open is worth saying, for
        // the reason `config_file` reports an unknown key: a setting that
        // silently does nothing looks exactly like one that is broken.
        if let Some(problem) = dictionary_problem {
            compositor
                .notifications
                .status(format!("no dictionary: {problem}"));
        }

        // Either a session, or the screen that has to be answered before there
        // is one. Nothing is spawned behind a login screen: a session that had
        // already started its shell before anybody said who they were would be
        // a boundary in the drawing only (#112).
        match compositor.session_gate() {
            Some(hash) => compositor.show_login(hash),
            None => compositor.begin_session()?,
        }
        Ok(compositor)
    }

    /// The credential this session has to be opened with, if it has one.
    ///
    /// `None` on a machine with no password, which is the live image and an
    /// installed machine whose owner declined one: there would be nothing to
    /// check, and a login screen with nothing to check against is a brick.
    /// That is the same rule the lock obeys, arriving at the other end of the
    /// session, and it is why neither needs to be told what live media is.
    ///
    /// `None` too where the display is not a machine's console — see
    /// [`Config::gated`].
    fn session_gate(&self) -> Option<String> {
        if !self.config.gated {
            return None;
        }
        lock::read_credential(&self.config.credential, &self.config.credential_user).ok()
    }

    /// Put the login screen up. There is no session behind it.
    fn show_login(&mut self, hash: String) {
        self.keymap.cancel_pending();
        self.release_grab();
        self.lock = Some(LockScreen::login(hash, self.config.credential_user.clone()));
        self.picture = Splash::load(&self.config.splash);
        self.needs_full_redraw = true;
    }

    /// Start a session: one workspace, one pane, one shell.
    ///
    /// The session is built here rather than in [`Compositor::new`] because it
    /// is built more than once now — at startup, and again every time somebody
    /// logs in after one ended. What it starts from is a new [`Session`], not
    /// the tree the last person left: a workspace layout is as much theirs as
    /// the shell history in it was.
    fn begin_session(&mut self) -> io::Result<()> {
        self.session = Session::new();
        self.panes.clear();
        let root = self.session.root_pane();
        let area = self
            .session
            .active()
            .geometry(self.grid_area())
            .first()
            .map(|(_, rect)| *rect)
            .unwrap_or(Rect::new(0, 0, 80, 24));
        let pane = self.spawn_pane(area)?;
        self.panes.insert(root, pane);
        self.sync_layout();
        self.needs_full_redraw = true;
        Ok(())
    }

    /// Take the session apart, leaving the machine with none.
    ///
    /// Dropping the panes closes their pseudoterminals, which is what tells
    /// the programs in them that the person they were talking to has gone.
    /// Everything else here is something that belonged to that person and
    /// would otherwise be handed to whoever logs in next: what they copied,
    /// the menu they left open, the selection they were dragging.
    fn end_session(&mut self) {
        self.panes.clear();
        self.session = Session::new();
        self.overlay = None;
        self.copy = None;
        self.mouse_grab = None;
        self.clipboard.clear();
        self.session_ended_while_locked = false;
        self.needs_full_redraw = true;
    }

    /// End the session and ask who is there, or end the compositor where
    /// there is nobody to ask.
    ///
    /// The second half is the live image and every machine with no password:
    /// leaving is what it has always meant there, and the init that started
    /// this starts another one. What it is not any more is a fall through the
    /// floor onto a root shell (#112).
    fn log_out(&mut self) {
        match self.session_gate() {
            Some(hash) => {
                self.end_session();
                self.show_login(hash);
            }
            None => self.running = false,
        }
    }

    fn spawn_pane(&self, area: Rect) -> io::Result<Pane> {
        self.spawn_pane_running(area, self.config.command.as_deref())
    }

    /// Start a pane on a particular command rather than the configured one.
    fn spawn_pane_running(&self, area: Rect, command: Option<&[String]>) -> io::Result<Pane> {
        Pane::spawn(
            area,
            self.cell_size(),
            self.config.scrollback,
            &self.config.palette,
            command,
            self.session_account.as_ref(),
        )
    }

    pub fn cell_size(&self) -> (u32, u32) {
        let metrics = self.fonts.metrics();
        (metrics.cell_width.max(1), metrics.cell_height.max(1))
    }

    /// The part of the screen panes are laid out in, in cells.
    pub fn grid_area(&self) -> Rect {
        let (cw, ch) = self.cell_size();
        let cols = (self.size.0 / cw).max(1);
        let rows = (self.size.1 / ch).max(1);
        let status = if self.config.status_bar && rows > 2 {
            1
        } else {
            0
        };
        Rect::new(0, 0, cols, rows - status)
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    /// The machine underneath: what the last poll found, and the seams that
    /// change it.
    pub fn machine(&self) -> &Machine {
        &self.machine
    }

    pub fn machine_mut(&mut self) -> &mut Machine {
        &mut self.machine
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn pane(&self, id: PaneId) -> Option<&Pane> {
        self.panes.get(&id)
    }

    pub fn pane_mut(&mut self, id: PaneId) -> Option<&mut Pane> {
        self.panes.get_mut(&id)
    }

    /// The input method, for a caller that wants to hand it a dictionary.
    ///
    /// The seam a test reaches through: `Ime::load` takes a `&dyn Source`, so
    /// four entries written inline are a dictionary and nothing has to be on
    /// the machine running the tests.
    pub fn ime(&self) -> &Ime {
        &self.ime
    }

    pub fn ime_mut(&mut self) -> &mut Ime {
        &mut self.ime
    }

    pub fn clipboard(&self, selector: char) -> Option<&[u8]> {
        self.clipboard.get(&selector).map(|v| v.as_slice())
    }

    pub fn notifications(&self) -> &Notifications {
        &self.notifications
    }

    /// File descriptors that should be polled for readiness.
    pub fn pty_fds(&self) -> Vec<std::os::unix::io::RawFd> {
        self.panes.values().map(|p| p.pty.fd()).collect()
    }

    /// React to the display changing size.
    pub fn resize(&mut self, size: (u32, u32)) {
        if size == self.size {
            return;
        }
        self.size = size;
        self.sync_layout();
        self.needs_full_redraw = true;
    }

    /// Give every pane the area the layout says it has.
    pub fn sync_layout(&mut self) {
        let area = self.grid_area();
        let cell = self.cell_size();
        let geometry = self.session.active().geometry(area);
        for (id, rect) in geometry {
            if let Some(pane) = self.panes.get_mut(&id) {
                pane.set_area(rect, cell);
            }
        }
    }

    /// Read from every PTY and let the terminals catch up.
    pub fn pump_panes(&mut self) -> bool {
        let mut changed = false;
        let mut finished = Vec::new();
        let mut buf = vec![0u8; 64 * 1024];

        for (&id, pane) in self.panes.iter_mut() {
            // `wants_frame` and not the damage, because the damage a pane
            // holding a synchronized update open is sitting on is the damage
            // `render_frame` deliberately left standing. Counting it here
            // would report a change on every pass of the loop for as long as
            // the program kept the update open, which is exactly the frame
            // [`Compositor::needs_render`] declines to ask for.
            let before = pane.wants_frame();
            let alive = pane.pump(&mut buf);
            if !alive && !pane.pty.is_alive() {
                finished.push(id);
            }
            if pane.wants_frame() || before {
                changed = true;
            }
        }

        // Terminal events are handled outside the borrow above.
        let ids: Vec<PaneId> = self.panes.keys().copied().collect();
        for id in ids {
            if self.handle_terminal_events(id) {
                changed = true;
            }
            if let Some(pane) = self.panes.get_mut(&id) {
                pane.flush_responses();
                // A large paste does not fit the PTY buffer in one write, so
                // the remainder is retried until the child has read it.
                if pane.flush_input() {
                    self.pending_writes = true;
                }
                if pane.input_overflowed() {
                    self.notifications
                        .status("input dropped: pane is not reading");
                    changed = true;
                }
            }
        }

        for id in finished {
            self.close_pane(id);
            changed = true;
        }
        changed
    }

    fn handle_terminal_events(&mut self, id: PaneId) -> bool {
        // The events are taken first so the pane borrow ends before any of
        // them need to touch the compositor as a whole.
        let events = match self.panes.get_mut(&id) {
            Some(pane) => pane.terminal.take_events(),
            None => return false,
        };
        if events.is_empty() {
            return false;
        }

        let mut changed = false;
        for event in events {
            match event {
                TermEvent::TitleChanged(title) => {
                    if let Some(pane) = self.panes.get_mut(&id) {
                        pane.title = title;
                    }
                    changed = true;
                }
                TermEvent::IconTitleChanged(_) | TermEvent::CwdChanged(_) => {}
                TermEvent::Bell => {
                    // A visible bell is the only kind a display server with no
                    // audio stack can offer.
                    self.notifications.from_pane(id, "", "bell");
                    changed = true;
                }
                TermEvent::Notify { title, body } => {
                    // Which pane asked is part of the notification: it is the
                    // one thing the application cannot say for itself, and the
                    // history list uses it to take you there.
                    self.notifications.from_pane(id, title, body);
                    changed = true;
                }
                TermEvent::ClipboardStore { selection, data } => {
                    // An oversized selection is dropped whole rather than
                    // truncated: half a copied command line is exactly the
                    // kind of thing that does damage when it lands in a
                    // shell, and what the user already had is worth more than
                    // a mangled replacement. They are told, because from the
                    // pane's side the copy looked like it worked.
                    if data.len() > MAX_CLIPBOARD_BYTES {
                        self.notifications.status(format!(
                            "clipboard write refused in pane {}: over {} KiB",
                            id.0 + 1,
                            MAX_CLIPBOARD_BYTES / 1024
                        ));
                        changed = true;
                    } else if is_clipboard_selector(selection) {
                        self.clipboard.insert(selection, data);
                    }
                }
                TermEvent::ClipboardLoad { selection } => {
                    // Reads are off unless the user asked for them. The query
                    // is just bytes on a PTY, so a `cat` of a hostile file, or
                    // anything running over ssh in that pane, can send it, and
                    // what comes back is whatever was last copied — a password
                    // as readily as a path. xterm and kitty refuse by default
                    // for the same reason.
                    //
                    // The refusal is an empty selection rather than silence:
                    // OSC 52 has no way to spell "no", a program that gets
                    // nothing back waits out its own timeout, and an empty
                    // answer is both indistinguishable from an empty clipboard
                    // and a case every reader already handles.
                    let data = if self.config.allow_clipboard_read {
                        self.clipboard.get(&selection).cloned().unwrap_or_default()
                    } else {
                        self.notifications
                            .status(format!("clipboard read refused in pane {}", id.0 + 1));
                        changed = true;
                        Vec::new()
                    };
                    if let Some(pane) = self.panes.get_mut(&id) {
                        pane.terminal.report_clipboard(selection, &data);
                    }
                }
                TermEvent::Repaint | TermEvent::CursorStyleChanged(_) => changed = true,
                TermEvent::ReportingChanged | TermEvent::WindowOp(..) => {}
            }
        }
        changed
    }

    // ---- input ----------------------------------------------------------

    /// Handle one input event. Returns true when something needs repainting.
    pub fn handle_input(&mut self, event: InputEvent) -> bool {
        // Activity is input, and only input. A pane producing output is not a
        // person being present: a `tail -f` on a log that turns over all night
        // would hold the screen on and the lock off for as long as the machine
        // kept running, and an idle timer that a program can hold open is not
        // one. Every kind of input counts, because every kind of it is
        // somebody doing something — a key, the mouse, a paste, and the host
        // terminal saying its window has been switched to.
        self.last_activity = Instant::now();
        // The idle period ends here, so the deadlines in it are owed another
        // turn — including the one the display refused, which may have been
        // refusing for a reason that has since gone away.
        self.idle_lock_done = false;
        self.blank_refused = false;

        // A dark screen takes the event that woke it and gives it to nobody.
        // Whoever sent it could not see what they were aiming at, and the
        // panel is still off at this moment — it comes back at the end of this
        // pass — so every event that arrived while it was off was sent blind.
        // A key let through would go to whatever program has the focus, where
        // `q`, `space` and `enter` each mean something, and to the lock, where
        // it would be the first character of a password the field is not
        // showing yet. Swallowing costs a keystroke; letting it through costs
        // whatever the keystroke did.
        if self.blanked {
            return true;
        }
        // The gate is here and not in `handle_key`, which is where an overlay
        // puts its own. An overlay owns the keyboard; a lock has to own the
        // input, and the two are not the same thing: `Mouse`, `Pointer` and
        // `Paste` are routed below without ever passing through `handle_key`.
        // A lock gated one level down would let a middle click paste the
        // primary selection into a shell, let a drag select and copy what is
        // on screen, and let the host terminal's bracketed paste type for the
        // person who is not there.
        if self.lock.is_some() {
            return self.locked_input(event);
        }
        match event {
            InputEvent::Key(key) => {
                // Typing puts the arrow away. Somebody at the keyboard is
                // reading the line they are typing, and the pointer is
                // wherever their hand left it — which, for anyone who typed
                // after clicking into a pane, is directly on top of that line.
                // It comes back on the next motion, so getting it back costs
                // the same gesture as wanting it.
                let put_away = self.pointer.hide();
                self.handle_key(key) || put_away
            }
            // A host terminal reports cells; a device reports pixels. The two
            // are separate types so the conversion can never be skipped, and
            // this is the conversion. The only thing that sends a `Mouse`
            // event is the nested backend, whose framebuffer is one pixel per
            // host column and two per host row, so a host cell names a pixel
            // and the pixel names a cell of the grid tOS lays panes out in —
            // two steps, neither of them the identity.
            //
            // Passing the host's cell numbers straight through skipped both,
            // and they do not cancel: on a 200x50 host terminal with an 8x16
            // font the framebuffer is 200x100 pixels and the grid is 25x6
            // cells, so the middle of the terminal arrived as a cell well off
            // the far corner of the grid and matched no pane at all. Almost
            // every click in nested mode was dropped.
            //
            // Still no arrow, and now because there should not be one rather
            // than for want of somewhere to put it. A nested session runs
            // inside somebody's terminal window, which is drawing the host's
            // own cursor under their hand already; this is the one backend
            // whose pointer tOS does not have to paint, and painting a second
            // one on top would not be subtle — the arrow is sized by the font
            // cell, which is several host columns across and as many host rows
            // tall, so it would sit over whatever it was pointing at.
            InputEvent::Mouse(mouse) => {
                let (cw, ch) = self.cell_size();
                let (pw, ph) = tos_platform::nested::HOST_CELL_PIXELS;
                let x = (mouse.col as u32).saturating_mul(pw);
                let y = (mouse.row as u32).saturating_mul(ph);
                self.route_mouse(x / cw, y / ch, mouse.button, mouse.action, mouse.modifiers)
            }
            InputEvent::Pointer(pointer) => {
                let (cw, ch) = self.cell_size();
                let x = pointer.x.max(0.0) as u32;
                let y = pointer.y.max(0.0) as u32;
                // Before the routing rather than after it, because the two
                // answers are ORed together and `route_mouse` says `false` for
                // a motion that grabbed nothing. That used to mean a bare
                // motion asked for no frame at all, which was correct while
                // there was nothing on screen to move.
                let moved = self.pointer.moved_to(x, y);
                let routed = self.route_mouse(
                    x / cw,
                    y / ch,
                    pointer.button,
                    pointer.action,
                    pointer.modifiers,
                );
                routed || moved
            }
            InputEvent::Paste(text) => {
                self.paste_text(&text);
                true
            }
            InputEvent::FocusGained => self.forward_focus(true),
            InputEvent::FocusLost => self.forward_focus(false),
        }
    }

    /// Whether the arrow is drawn over the box that is up.
    ///
    /// Only over a login screen. See [`Compositor::locked_input`] for why a
    /// lock is the other answer.
    fn lock_shows_the_pointer(&self) -> bool {
        self.lock.as_ref().map(LockScreen::purpose) == Some(lock::Purpose::Login)
    }

    /// Everything that arrives while the screen is locked.
    ///
    /// A key goes to the lock and nothing else goes anywhere, with one
    /// exception. Focus notifications are dropped along with the rest —
    /// telling a pane it has the focus is still writing to a pane on behalf of
    /// somebody who has not proved who they are, and an exception is how a
    /// gate stops being one.
    ///
    /// The exception is where the pointer is, and only at a login screen.
    /// What the gate is for is what a button does — a middle click pasting
    /// the primary selection into a shell, a drag selecting and copying what
    /// is on screen — and a motion carries an `(x, y)` and nothing else. A
    /// lock has a session behind it and a hand in front of it that has not
    /// said whose it is, so that one keeps the older rule of taking nothing
    /// at all. A login has no session behind it and nobody has walked away
    /// from it, and the arrow is the only thing tOS has to say that the mouse
    /// works — on the first screen it ever shows, where a missing one is
    /// indistinguishable from a mouse that is not plugged in.
    ///
    /// Either way the buttons go nowhere: this reaches `Pointer::moved_to`
    /// and never `route_mouse`, so there is no selection, no paste and no
    /// pane to click into.
    fn locked_input(&mut self, event: InputEvent) -> bool {
        if let InputEvent::Pointer(pointer) = event {
            if !self.lock_shows_the_pointer() {
                return false;
            }
            let x = pointer.x.max(0.0) as u32;
            let y = pointer.y.max(0.0) as u32;
            return self.pointer.moved_to(x, y);
        }
        let InputEvent::Key(key) = event else {
            return false;
        };
        let Some(lock) = &mut self.lock else {
            return false;
        };
        match lock.handle_key(&key, Instant::now()) {
            LockOutcome::Consumed => false,
            LockOutcome::Changed => true,
            LockOutcome::Unlocked => {
                self.unlock();
                true
            }
        }
    }

    fn forward_focus(&mut self, gained: bool) -> bool {
        let focus = self.session.focus();
        if let Some(pane) = self.panes.get_mut(&focus) {
            if pane.terminal.modes.focus_events {
                let bytes = encode_focus(gained).to_vec();
                pane.write(&bytes);
            }
        }
        false
    }

    fn handle_key(&mut self, key: KeyEvent) -> bool {
        // An open overlay owns the keyboard: not the bindings, not the pane
        // underneath. Anything else and a menu would be typing into a shell.
        if self.overlay.is_some() {
            return self.overlay_key(&key);
        }
        // Copy mode owns it in the same way, and for the same reason: its
        // whole keymap is single letters that the pane would otherwise get.
        // It is gated here rather than in `handle_input` because, unlike the
        // lock, it claims only the keyboard — the mouse still selects, and a
        // press on it is what ends the mode.
        if self.copy.is_some() {
            return self.copy_key(&key);
        }
        // The leader indicator is drawn from the keymap rather than queued as
        // a notification: arming the leader is not news, and a message that
        // says so would cost whatever is in the queue its turn on screen. The
        // keypress that disarms it still needs a frame, though, even when the
        // action it ran changed nothing else.
        let was_armed = self.keymap.is_pending();
        let changed = match self.keymap.resolve(&key) {
            Resolution::Action(action) => self.perform(action),
            Resolution::Pending => true,
            Resolution::Passthrough => {
                // The IME answers first, and only for keys the bindings did
                // not want. `resolve` has already computed exactly the set of
                // keys that belong to the focused pane, which is exactly the
                // IME's input; consulted ahead of it, kana mode would turn
                // `super+h` into へ and the leader into a character.
                if let Some(changed) = self.ime_key(&key) {
                    return changed;
                }
                let focus = self.session.focus();
                let Some(pane) = self.panes.get_mut(&focus) else {
                    return false;
                };
                let ctx = EncodeContext::from_terminal(&pane.terminal);
                let bytes = encode_key(&key, &ctx);
                if bytes.is_empty() {
                    return false;
                }
                // Typing returns the view to the live screen, as it must: the
                // input is going to a program that is drawing there.
                let scrolled = pane.terminal.display_offset() != 0;
                pane.terminal.reset_display_offset();
                pane.write(&bytes);
                scrolled
            }
        };
        changed || was_armed != self.keymap.is_pending()
    }

    // ---- Japanese input -------------------------------------------------

    /// Offer a key to the input method, and say what it did with it.
    ///
    /// `None` means the key is still the pane's and the path below
    /// [`Compositor::handle_key`]'s `Passthrough` arm runs untouched — which
    /// in Direct mode is every key, and is why Direct mode costs nothing and
    /// is byte for byte what it was before any of this existed.
    fn ime_key(&mut self, key: &KeyEvent) -> Option<bool> {
        // `Keymap::resolve` returns `Passthrough` for releases as well as
        // presses. Without this guard every character would be typed twice,
        // which is the same guard `Overlay::handle_key` keeps and for the
        // same reason.
        if !key.is_press() {
            return None;
        }
        let focus = self.session.focus();
        let pane = self.panes.get_mut(&focus)?;
        let outcome = ime::handle_key(&mut self.ime, &mut pane.ime, focus, key);
        if outcome == ImeOutcome::Passthrough {
            return None;
        }
        // Typing returns the view to the live screen for the same reason it
        // does below: what is being typed is going to a program that is
        // drawing there, and a preedit painted over scrolled-back history
        // would be sitting nowhere near where its text will land.
        if pane.terminal.display_offset() != 0 {
            pane.terminal.reset_display_offset();
        }
        if let ImeOutcome::Commit(text) = outcome {
            // The UTF-8 bytes, straight to the child — deliberately not
            // `encode_paste`. A commit is typing, and bracketing it would put
            // the program into paste mode for text the user typed one
            // character at a time: no autoindent in vim, no history expansion
            // in a shell, and no way for them to see why.
            //
            // Control characters are filtered for the reason `encode_paste`
            // filters them: a candidate comes out of a file on disk and must
            // not be able to be an escape sequence.
            let bytes: Vec<u8> = text
                .chars()
                .filter(|c| !c.is_control())
                .collect::<String>()
                .into();
            if !bytes.is_empty() {
                pane.write(&bytes);
            }
        }
        Some(true)
    }

    /// Turn Japanese input on or off for the focused pane.
    fn toggle_ime(&mut self) -> bool {
        let focus = self.session.focus();
        // A candidate list is a proposal about a preedit that is about to be
        // thrown away, so it goes first.
        self.ime.end_conversion(focus);
        let Some(pane) = self.panes.get_mut(&focus) else {
            return false;
        };
        let on = pane.ime.toggle();
        // Said out loud, because with nothing typed yet the mode is otherwise
        // invisible: the only other evidence of it is what the next keystroke
        // does, which is a bad way to find out.
        self.notifications.status(if on {
            "kana input on"
        } else {
            "kana input off"
        });
        true
    }

    /// Paint every pane's preedit, and the candidate window over the one pane
    /// that is converting.
    ///
    /// Every pane, not just the focused one: a preedit belongs to the pane it
    /// is destined for, so moving the focus leaves it where it was rather
    /// than carrying it or committing it.
    fn draw_ime(&mut self, surface: &mut Surface<'_>, geometry: &[(PaneId, Rect)]) {
        for (id, rect) in geometry {
            let conversion = self.ime.conversion(*id);
            let Some(pane) = self.panes.get(id) else {
                continue;
            };
            let painted = ime::draw(
                surface,
                &mut self.fonts,
                &self.chrome,
                *rect,
                &pane.terminal,
                &pane.ime,
                conversion,
            );
            if let Some(pane) = self.panes.get_mut(id) {
                pane.ime.set_painted(painted);
            }
        }
    }

    /// Route a mouse event that is already in display cell coordinates.
    fn route_mouse(
        &mut self,
        cell_x: u32,
        cell_y: u32,
        button: Option<MouseButton>,
        action: MouseAction,
        modifiers: tos_input::Modifiers,
    ) -> bool {
        let area = self.grid_area();

        // Copy mode ends here, above everything else, because the rule is
        // about the press and not about where it landed: one selection cannot
        // have two owners, and whoever reached for the mouse has stopped
        // driving one from the keyboard. The ordering is load-bearing — the
        // bar below returns without ever looking at a pane, so a press there
        // handled after this point would switch workspaces and leave copy
        // mode holding a pane nobody can see, eating every keystroke with no
        // highlight anywhere to explain why.
        let mut changed = false;
        if action == MouseAction::Press && self.copy.is_some() {
            self.leave_copy_mode();
            changed = true;
        }

        // An open overlay owns the mouse, exactly as `handle_key` gives it the
        // keyboard and for the same reason: it is modal, and nothing under it
        // is what anybody is aiming at while it is up. Above the bar rather
        // than below it, because the bar returns without ever looking at a
        // pane — a press there with the launcher open would switch workspaces
        // underneath a menu that stayed on screen listing the programs of the
        // workspace that left. Above the panes for the plainer reason that a
        // press on one would start selecting text through the box.
        if self.overlay.is_some() {
            // Nothing is let go of here. A grab cannot be taken while a menu
            // is up, since this is where the press that would take one stops,
            // and one taken before the menu opened was dropped by
            // [`Compositor::open_overlay`] — which is the only end of the
            // gesture that can be relied on to arrive.
            return self.overlay_mouse(cell_x, cell_y, button, action) || changed;
        }

        // The bar next, because it is nowhere in the geometry below: the row
        // it occupies is the row `grid_area` took away, so a press there
        // matches no pane and would be dropped. Only a press, and only the
        // left button: a drag that started in a pane and wandered down here
        // still belongs to the selection it started, and falls through to the
        // clamp that keeps it in its own pane.
        if action == MouseAction::Press
            && button == Some(MouseButton::Left)
            && self.status_row() == Some(cell_y)
        {
            self.release_grab();
            return self.click_status(cell_x) || changed;
        }

        // Then the dividers, which are in the gaps between panes and so in no
        // pane's rectangle: the hit test below finds nothing there, exactly
        // as it found nothing on the bar, and until now a press on the line
        // between two panes did nothing at all. Above the panes rather than
        // below them because a drag that has hold of a divider has to keep it
        // while the pointer is over a pane, which is where a divider spends
        // every cell of its travel.
        if let Some(moved) = self.drag_divider(cell_x, cell_y, button, action) {
            return moved || changed;
        }

        let geometry = self.session.active().geometry(area);

        // A drag that started in a pane keeps going there even once the
        // pointer leaves it, which is what makes selection usable. A grab on a
        // pane that has since closed is dropped rather than wedging the mouse.
        let grabbed = self.grabbed_pane().and_then(|id| {
            geometry
                .iter()
                .find(|(pane, _)| *pane == id)
                .map(|(pane, rect)| (*pane, *rect))
        });
        if self.grabbed_pane().is_some() && grabbed.is_none() {
            self.release_grab();
        }

        let hit = geometry
            .iter()
            .find(|(_, rect)| rect.contains(cell_x, cell_y))
            .map(|(pane, rect)| (*pane, *rect));

        // A press always re-targets: it starts a new interaction, and the pane
        // under the pointer is the one it belongs to. Everything else follows
        // the grab, so a drag can leave the pane it started in.
        let target = if action == MouseAction::Press {
            if let Some((pane, _)) = hit {
                if self.grabbed_pane().is_some_and(|grabbed| grabbed != pane) {
                    self.release_grab();
                }
            }
            hit.or(grabbed)
        } else {
            grabbed.or(hit)
        };
        let Some((pane_id, rect)) = target else {
            return false;
        };
        // A pane with no area cannot be interacted with, and clamping into it
        // would be a division by an empty range.
        if rect.is_empty() {
            return false;
        }

        if action == MouseAction::Press && self.session.focus() != pane_id {
            let previous = self.session.focus();
            self.session.set_focus(pane_id);
            self.clear_selection(previous);
            self.needs_full_redraw = true;
            changed = true;
        }

        // Clamp into the pane so a drag past its edge still selects sensibly.
        let local_col = cell_x.clamp(rect.x, rect.right() - 1) - rect.x;
        let local_row = cell_y.clamp(rect.y, rect.bottom() - 1) - rect.y;
        let local = MouseEvent {
            button,
            action,
            col: local_col as usize,
            row: local_row as usize,
            modifiers,
        };

        let tracking = match self.panes.get(&pane_id) {
            Some(pane) => pane.terminal.mouse(),
            None => return changed,
        };

        // Wheel events scroll the compositor's own scrollback unless the
        // program is tracking the mouse itself.
        if let Some(button) = button {
            if button.is_wheel() && !tracking.is_enabled() {
                if action != MouseAction::Press {
                    return changed;
                }
                return self.scroll_wheel(pane_id, button) || changed;
            }
        }

        if tracking.is_enabled() {
            if let Some(bytes) = encode_mouse(&local, tracking) {
                if let Some(pane) = self.panes.get_mut(&pane_id) {
                    pane.write(&bytes);
                }
            }
            return changed;
        }

        // Otherwise the mouse belongs to the compositor, and selects text.
        let mode = if action == MouseAction::Press && button == Some(MouseButton::Left) {
            SelectionMode::for_clicks(self.count_click(pane_id, local.col, local.row))
        } else {
            SelectionMode::Cell
        };
        let mut copied = None;
        let mut paste = false;
        // Whether the pointer is dragging this pane's selection, which is the
        // grab and nothing else. Asking the pane instead would be asking the
        // wrong question: its flag says a selection is being made, and copy
        // mode is making one with the keyboard while the mouse hangs idle, so
        // a bare motion across the pane would walk the highlight away from the
        // copy cursor and `y` would yank text nobody saw highlighted.
        let dragging = self.mouse_grab == Some(Grab::Pane(pane_id));
        if let Some(pane) = self.panes.get_mut(&pane_id) {
            let at = pane.anchor_at(local.col, local.row);
            match action {
                MouseAction::Press if button == Some(MouseButton::Left) => {
                    pane.selection_in_progress = true;
                    pane.set_selection(Some(Selection::new(at, modifiers.alt(), mode)));
                    self.mouse_grab = Some(Grab::Pane(pane_id));
                    changed = true;
                }
                MouseAction::Drag | MouseAction::Motion if dragging => {
                    if let Some(mut selection) = pane.selection {
                        selection.drag_to(at);
                        pane.set_selection(Some(selection));
                    }
                    changed = true;
                }
                MouseAction::Release if dragging => {
                    pane.selection_in_progress = false;
                    self.mouse_grab = None;
                    copied = pane.selected_text();
                    changed = true;
                }
                MouseAction::Press if button == Some(MouseButton::Middle) => {
                    paste = true;
                }
                _ => {}
            }
        }

        // What the mouse selects goes to primary, the way X11 has always done
        // it, so that dragging over a word does not throw away whatever was
        // deliberately copied to the clipboard.
        if let Some(text) = copied {
            self.clipboard.insert(PRIMARY, text.into_bytes());
        }
        if paste {
            let data = self.clipboard.get(&PRIMARY).cloned().unwrap_or_default();
            let text = String::from_utf8_lossy(&data).into_owned();
            self.paste_text(&text);
            changed = true;
        }
        changed
    }

    /// How many presses in a row this one is, counting only presses close
    /// enough in time and on the same cell of the same pane to be one gesture.
    ///
    /// This lives here rather than in `tos-input` because the count is about
    /// where the presses landed, and only the compositor knows that: it owns
    /// the pane geometry that turns a pointer position into a cell. A device
    /// driver sees pixels, and a host terminal hands over reports that never
    /// carried a click count in the first place.
    fn count_click(&mut self, pane: PaneId, col: usize, row: usize) -> u32 {
        let now = Instant::now();
        let count = match self.last_click {
            Some(last)
                if last.pane == pane
                    && (last.col, last.row) == (col, row)
                    && now.duration_since(last.at) <= MULTI_CLICK_INTERVAL =>
            {
                last.count + 1
            }
            _ => 1,
        };
        self.last_click = Some(Click {
            pane,
            col,
            row,
            at: now,
            count,
        });
        count
    }

    /// Forget a pane's selection, which is what every event that moves the
    /// text out from under it has to do.
    fn clear_selection(&mut self, id: PaneId) {
        if let Some(pane) = self.panes.get_mut(&id) {
            pane.clear_selection();
        }
    }

    /// The divider part of the mouse: grab one on a press, move it while it
    /// is held, drop it on the release.
    ///
    /// `None` means this event has nothing to do with a divider and the pane
    /// routing below should have it. Anything else is `Some`, including the
    /// press that grabs — which moves nothing and so asks for no frame.
    fn drag_divider(
        &mut self,
        cell_x: u32,
        cell_y: u32,
        button: Option<MouseButton>,
        action: MouseAction,
    ) -> Option<bool> {
        // The left button only, because it is the only one that starts a drag.
        // A wheel notch arrives here as a press too — it is how the device
        // reports one — and it is not the beginning of anything, so it falls
        // through to the arm below that swallows it while a divider is held.
        if action == MouseAction::Press && button == Some(MouseButton::Left) {
            // A press starts a new interaction whatever the last one was, so
            // a release that never arrived — a button let go over another
            // virtual terminal, a device that stopped reporting — cannot wedge
            // a divider to the pointer forever.
            if matches!(self.mouse_grab, Some(Grab::Divider(_))) {
                self.mouse_grab = None;
            }
            // A zoomed workspace draws no dividers, and a strip that resizes
            // a layout nobody can see is worse than one that does nothing.
            if self.session.active().zoomed().is_some() {
                return None;
            }
            let area = self.grid_area();
            let divider = self.session.active().divider_at(area, cell_x, cell_y)?;
            // One grab, one owner: whatever the pane path thought it was
            // dragging, it is not dragging it now.
            self.release_grab();
            let offset = match divider.axis {
                Axis::Columns => cell_x - divider.rect.x,
                Axis::Rows => cell_y - divider.rect.y,
            };
            self.mouse_grab = Some(Grab::Divider(DividerGrab {
                id: divider.id,
                offset,
            }));
            return Some(false);
        }

        let Some(Grab::Divider(grab)) = self.mouse_grab else {
            return None;
        };
        match action {
            MouseAction::Drag | MouseAction::Motion => {
                Some(self.move_divider(grab, cell_x, cell_y))
            }
            // The left button only, for the same reason it is the only one
            // that starts a drag: a right or middle button let go mid-drag is
            // not the end of the drag, and ending it there would leave the
            // divider behind while the hand that is still holding the left
            // button goes on moving.
            MouseAction::Release if button == Some(MouseButton::Left) => {
                self.mouse_grab = None;
                Some(false)
            }
            // The wheel, or another button, while a divider is held: taken,
            // because the pointer is in the middle of saying something else.
            _ => Some(false),
        }
    }

    /// Put the divider being dragged where the pointer is.
    ///
    /// Measured from where the divider is now rather than accumulated from
    /// where the drag began: the weights are shares of a split and a cell of
    /// travel is not always a cell of movement, so the only honest target is
    /// the distance still to go. A move the layout refuses leaves the divider
    /// where it is and the pointer running ahead of it, and it is picked up
    /// again as soon as the pointer comes back.
    fn move_divider(&mut self, grab: DividerGrab, cell_x: u32, cell_y: u32) -> bool {
        let area = self.grid_area();
        // Zoom is a binding, and the keyboard still works while a button is
        // down.
        if self.session.active().zoomed().is_some() {
            self.mouse_grab = None;
            return false;
        }
        let Some(divider) = self.session.active().divider(area, grab.id) else {
            // The split went away under the drag, which is what closing a
            // pane beside it does.
            self.mouse_grab = None;
            return false;
        };
        let (at, from) = match divider.axis {
            Axis::Columns => (cell_x as i32, divider.rect.x as i32),
            Axis::Rows => (cell_y as i32, divider.rect.y as i32),
        };
        let amount = at - grab.offset as i32 - from;
        if amount == 0 {
            return false;
        }
        if !self
            .session
            .active_mut()
            .layout
            .resize_at(area, grab.id, amount)
        {
            return false;
        }
        self.sync_layout();
        self.needs_full_redraw = true;
        true
    }

    /// The pane a selection is being dragged out of, if that is what the
    /// mouse is holding.
    fn grabbed_pane(&self) -> Option<PaneId> {
        match self.mouse_grab {
            Some(Grab::Pane(pane)) => Some(pane),
            _ => None,
        }
    }

    /// Abandon an interaction that was still in progress in another pane.
    fn release_grab(&mut self) {
        if let Some(Grab::Pane(pane)) = self.mouse_grab.take() {
            if let Some(pane) = self.panes.get_mut(&pane) {
                pane.selection_in_progress = false;
            }
        }
    }

    fn scroll_wheel(&mut self, id: PaneId, button: MouseButton) -> bool {
        const LINES: usize = 3;
        let Some(pane) = self.panes.get_mut(&id) else {
            return false;
        };
        // On the alternate screen there is no history, so the convention is to
        // send arrow keys instead, which is what pagers expect.
        if pane.terminal.modes.alt_screen {
            if pane.terminal.mouse().alternate_scroll {
                let ctx = EncodeContext::from_terminal(&pane.terminal);
                if let Some(bytes) = encode_alternate_scroll(button, LINES, &ctx) {
                    pane.write(&bytes);
                }
            }
            return false;
        }
        let delta = match button {
            MouseButton::WheelUp => LINES as isize,
            MouseButton::WheelDown => -(LINES as isize),
            _ => return false,
        };
        pane.terminal.scroll_display(delta)
    }

    fn paste_text(&mut self, text: &str) {
        // Nothing to paste is nothing to do. Without this an empty clipboard
        // still sent `\x1b[200~\x1b[201~` to a program that had asked for
        // bracketed paste — a paste mode entered and left around no text at
        // all, which some readline configurations answer by redrawing the
        // prompt — and still snapped the viewport back to the live screen. A
        // key that does nothing has to do nothing visible either.
        if text.is_empty() {
            return;
        }
        let focus = self.session.focus();
        let Some(pane) = self.panes.get_mut(&focus) else {
            return;
        };
        let bracketed = pane.terminal.modes.bracketed_paste;
        let bytes = encode_paste(text, bracketed);
        pane.terminal.reset_display_offset();
        pane.write(&bytes);
    }

    // ---- actions --------------------------------------------------------

    /// Carry out a compositor action. Returns true when a repaint is needed.
    pub fn perform(&mut self, action: Action) -> bool {
        let before = self.session.focus();
        let changed = self.perform_action(action);
        // Moving focus away ends whatever was being selected there: the
        // selection belongs to an interaction the user has left behind.
        if self.session.focus() != before {
            self.clear_selection(before);
        }
        changed
    }

    fn perform_action(&mut self, action: Action) -> bool {
        let area = self.grid_area();
        match action {
            Action::Split(axis) => self.split(axis),
            Action::ClosePane => {
                let focus = self.session.focus();
                self.close_pane(focus);
                true
            }
            Action::Focus(direction) => {
                let moved = self.session.focus_direction(area, direction);
                self.needs_full_redraw |= moved;
                moved
            }
            // Cycling resyncs the layout where moving in a direction does not,
            // because cycling can land on a pane the zoom was hiding: focusing
            // one gives every other pane on the workspace its geometry back.
            // A workspace with one pane cycles to itself and has changed
            // nothing, which is the false.
            Action::FocusNext => {
                let before = self.session.focus();
                let moved = self.session.focus_next() != before;
                if moved {
                    self.sync_layout();
                    self.needs_full_redraw = true;
                }
                moved
            }
            Action::FocusPrevious => {
                let before = self.session.focus();
                let moved = self.session.focus_previous() != before;
                if moved {
                    self.sync_layout();
                    self.needs_full_redraw = true;
                }
                moved
            }
            Action::Resize(direction, amount) => {
                let arrangement = self.session.active().arrangement();
                if arrangement != Arrangement::Splits {
                    // A derived arrangement has no divider to move, and a key
                    // that silently does nothing is indistinguishable from a
                    // key that is broken — the same reason a split with no
                    // room says so rather than shrugging.
                    self.notifications
                        .status(format!("no dividers in the {} layout", arrangement.name()));
                    true
                } else if self.session.resize_focused(area, direction, amount) {
                    self.sync_layout();
                    self.needs_full_redraw = true;
                    true
                } else {
                    false
                }
            }
            Action::ToggleZoom => {
                let changed = self.session.toggle_zoom();
                if changed {
                    self.sync_layout();
                    self.needs_full_redraw = true;
                }
                changed
            }
            Action::Balance => {
                if self.session.balance() {
                    self.sync_layout();
                    self.needs_full_redraw = true;
                } else {
                    let name = self.session.active().arrangement().name();
                    self.notifications
                        .status(format!("the {name} layout is already even"));
                }
                true
            }
            Action::NextLayout => self.cycle_layout(true),
            Action::PreviousLayout => self.cycle_layout(false),
            Action::NewWorkspace => {
                let pane_id = self.session.new_workspace();
                match self.spawn_pane(self.grid_area()) {
                    Ok(pane) => {
                        self.panes.insert(pane_id, pane);
                        self.sync_layout();
                        self.needs_full_redraw = true;
                        true
                    }
                    Err(e) => {
                        // Undo the workspace rather than leave an empty one.
                        self.session.close_pane(pane_id);
                        self.report_error("new workspace", e);
                        true
                    }
                }
            }
            Action::NextWorkspace => {
                self.session.next_workspace();
                self.sync_layout();
                self.needs_full_redraw = true;
                true
            }
            Action::PreviousWorkspace => {
                self.session.previous_workspace();
                self.sync_layout();
                self.needs_full_redraw = true;
                true
            }
            Action::SelectWorkspace(n) => {
                let changed = self.session.select_workspace(n);
                if changed {
                    self.sync_layout();
                    self.needs_full_redraw = true;
                }
                changed
            }
            Action::MovePaneToWorkspace(n) => {
                let moved = self.session.move_focused_to_workspace(area, n);
                if !moved {
                    // A refusal with nothing said reads as a dropped
                    // keystroke, which is why a refused split says so too.
                    // Which of the reasons applied — no such workspace, the
                    // one already on screen, the last pane of this one, or no
                    // room over there — does not come back through a bool, so
                    // the message says the one thing true of all of them.
                    self.notifications.status("the pane cannot move there");
                }
                // Resynced whether or not the pane went. The panes are sized
                // from geometry this arm may have just changed, and a bool
                // does not say how far the session got before it gave up;
                // leaving the resync out is how a workspace ends up drawn in
                // rectangles none of its programs have been told about.
                // Resyncing a workspace that did not change costs nothing.
                self.sync_layout();
                self.needs_full_redraw = true;
                true
            }
            Action::RenameWorkspace => {
                // The prompt opens on the name the workspace has now, which is
                // both the value to edit and the only place it is written
                // down; clearing the line is how the number is asked back.
                let name = self.session.active().name.clone();
                self.open_overlay(
                    OverlayKind::RenameWorkspace,
                    Overlay::prompt("rename workspace", name),
                );
                true
            }
            Action::Scroll(lines) => self.scroll_focused(lines as isize),
            Action::ScrollPage(pages) => {
                let rows = self
                    .panes
                    .get(&self.session.focus())
                    .map(|p| p.terminal.rows() as isize)
                    .unwrap_or(1);
                // Negative pages mean back in time, the same convention
                // `Action::Scroll` uses.
                self.scroll_focused(pages as isize * rows.max(1))
            }
            Action::ScrollToBottom => {
                let focus = self.session.focus();
                if let Some(pane) = self.panes.get_mut(&focus) {
                    pane.terminal.reset_display_offset();
                    return true;
                }
                false
            }
            Action::Copy => {
                let focus = self.session.focus();
                match self.panes.get(&focus).and_then(|p| p.selected_text()) {
                    Some(text) => {
                        self.clipboard.insert(CLIPBOARD, text.into_bytes());
                        self.notifications.status("copied");
                    }
                    // The clipboard is left exactly as it was: what somebody
                    // copied a minute ago is worth more than the nothing that
                    // is selected now, and a key pressed with no selection is
                    // far more often a miss than a request to forget. Saying
                    // so out loud is what copy mode's yank does with the same
                    // empty hands, and a binding that looks broken when it is
                    // only empty-handed is worth one line of status.
                    None => self.notifications.status("nothing to copy"),
                }
                true
            }
            Action::Paste => {
                let data = self.clipboard.get(&CLIPBOARD).cloned().unwrap_or_default();
                // An empty clipboard is a no-op down to the frame: nothing
                // goes to the pane, so there is nothing to redraw either, and
                // nothing to say about it — a session that has copied nothing
                // yet is not a session that has gone wrong.
                if data.is_empty() {
                    return false;
                }
                let text = String::from_utf8_lossy(&data).into_owned();
                self.paste_text(&text);
                true
            }
            Action::OpenLauncher => {
                // The scan happens here, once, rather than per keystroke.
                let overlay = Overlay::new("run a program", launcher::programs_on_path());
                self.open_overlay(OverlayKind::Launcher, overlay);
                true
            }
            Action::ShowNotifications => {
                let items = self.notifications.open_history(Instant::now());
                let overlay = Overlay::new("notifications", items);
                self.open_overlay(OverlayKind::Notifications, overlay);
                true
            }
            Action::ShowBindings => {
                self.open_overlay(OverlayKind::Bindings, self.binding_sheet());
                true
            }
            Action::Refresh => {
                self.needs_full_redraw = true;
                true
            }
            Action::Lock => self.lock_session(),
            Action::CopyMode => self.enter_copy_mode(),
            Action::ShowBluetooth => self.open_bluetooth(),
            Action::PowerMenu => {
                self.open_overlay(OverlayKind::Power, power::menu());
                true
            }
            Action::VolumeUp => self.change_volume(Knob::Up),
            Action::VolumeDown => self.change_volume(Knob::Down),
            Action::ToggleMute => self.change_volume(Knob::Mute),
            Action::ToggleStatusBar => {
                // The bar owns a row of the display, so this is a layout
                // change as much as a drawing one: `grid_area` gives the row
                // back, `sync_layout` tells the panes they are a line taller,
                // and the terminals inside them are resized and told so. A
                // toggle that only stopped drawing would leave every pane the
                // wrong height and the bottom row of the session unpainted.
                self.config.status_bar = !self.config.status_bar;
                self.sync_layout();
                self.needs_full_redraw = true;
                true
            }
            Action::ShowNetworks => {
                self.open_networks();
                true
            }
            Action::ImeToggle => self.toggle_ime(),
            // Quit means "I am done with this session". Where there is a
            // login screen to come back to, that is a log out and the machine
            // stays up asking who is there; where there is not, it is the
            // whole of what tOS was doing and the init that started it starts
            // another.
            Action::Quit => {
                self.log_out();
                true
            }
        }
    }

    // ---- copy mode ------------------------------------------------------

    /// Take the keyboard and start moving a selection with it.
    ///
    /// The mode starts where the terminal's cursor is drawn rather than where
    /// the program thinks it is. The two differ only when the viewport has
    /// been scrolled back, and there the program's cursor is off screen
    /// entirely: entering copy mode at a point nobody can see, and then
    /// yanking the viewport back to it on the first motion, would undo the
    /// scrolling the user did to find what they wanted to copy.
    fn enter_copy_mode(&mut self) -> bool {
        let focus = self.session.focus();
        let Some(pane) = self.panes.get_mut(&focus) else {
            return false;
        };
        let cursor = pane.terminal.cursor();
        let copy = CopyMode::new(pane.anchor_at(cursor.x, cursor.y));
        // The same flag a mouse drag sets, and for the same reason: it says a
        // selection belongs to an interaction that is still happening, so a
        // program writing to the pane does not clear it out from under it. It
        // says nothing about the pointer, which is why the drag arms of
        // `route_mouse` read the grab instead — a mode driven by the keyboard
        // must not be dragged by a mouse that is only being moved past.
        pane.selection_in_progress = true;
        pane.set_selection(copy.selection());
        pane.terminal.damage_mut().mark_all();
        self.copy = Some((focus, copy));
        true
    }

    /// Give the pane back its keyboard and its selection.
    ///
    /// The highlight goes with the mode. What was copied is in the clipboard
    /// by then, and a highlight left behind is the stale selection #36 was
    /// about: the text under it moves on, and the highlight stops describing
    /// anything.
    fn leave_copy_mode(&mut self) {
        let Some((id, _)) = self.copy.take() else {
            return;
        };
        if let Some(pane) = self.panes.get_mut(&id) {
            pane.selection_in_progress = false;
            pane.clear_selection();
            pane.terminal.damage_mut().mark_all();
        }
    }

    /// Hand a key to copy mode, and act on what it says.
    ///
    /// The mode is taken out of the compositor and put back rather than
    /// borrowed where it lies, because every outcome but one reaches for a
    /// second piece of the compositor: the pane for its grid and its
    /// viewport, and a yank for the clipboard and the status queue as well.
    fn copy_key(&mut self, key: &KeyEvent) -> bool {
        let Some((id, mut copy)) = self.copy.take() else {
            return false;
        };
        let Some(pane) = self.panes.get_mut(&id) else {
            // The pane died under the mode. There is nothing left to select
            // in, and `self.copy` is already None.
            return true;
        };
        match copy.handle_key(key, pane.terminal.grid()) {
            CopyOutcome::Consumed => {
                self.copy = Some((id, copy));
                false
            }
            CopyOutcome::Changed => {
                // The viewport follows the cursor rather than the cursor
                // being held inside the viewport, which is what lets a `k` on
                // the top row scroll into history instead of doing nothing.
                let delta = copy.scroll_to_show(pane.terminal.grid());
                if delta != 0 {
                    pane.terminal.scroll_display(delta);
                }
                pane.set_selection(copy.selection());
                // The copy cursor is drawn by the compositor over cells the
                // terminal has no reason to think have changed, so moving it
                // has to ask for the repaint itself.
                pane.terminal.damage_mut().mark_all();
                self.copy = Some((id, copy));
                true
            }
            CopyOutcome::Copied => {
                let text = copy.yanked().text(pane.terminal.grid());
                self.copy = Some((id, copy));
                self.leave_copy_mode();
                // An explicit copy writes the clipboard, not primary: this is
                // somebody deciding to keep something, which is exactly what
                // a drag over a word must not be allowed to overwrite.
                match text {
                    Some(text) => {
                        self.clipboard.insert(CLIPBOARD, text.into_bytes());
                        self.notifications.status("copied");
                    }
                    None => self.notifications.status("nothing to copy"),
                }
                true
            }
            CopyOutcome::Left => {
                self.copy = Some((id, copy));
                self.leave_copy_mode();
                true
            }
        }
    }

    // ---- sound ----------------------------------------------------------

    // Picking an output device is not built, and this is why rather than an
    // oversight.
    //
    // `tos-system` can already enumerate cards and attach a mixer to any of them,
    // so the menu itself would be an afternoon: an `OverlayKind` variant over
    // `audio::card_order`, the way the launcher is an overlay over `$PATH`. What
    // it would not be is the thing the issue is asking for. A card is not an
    // output. The machine this is most likely to run on has two cards — the codec
    // and the HDMI audio on the graphics card — and choosing between speakers and
    // the headphone socket, which is what "output device" means to the person
    // asking, happens *within* one card, through its own auto-mute enumeration or
    // through whichever of `Speaker` and `Headphone` that hardware exposes. A card
    // picker would therefore be a menu that confidently does not do what its title
    // says, which is worse than no menu.
    //
    // The second output that is genuinely a different device is a Bluetooth sink,
    // and a Bluetooth sink has no `/dev/snd/controlC*` at all: it is a BlueZ
    // transport, reached over a bus tOS does not carry. So the shape of the
    // chooser — a list of ALSA cards, or a list of sinks of which some are not
    // cards — is decided by #18 and by the sound server question in
    // `docs/design/audio.md`, and building the ALSA-card version first would mean
    // building the wrong one and then throwing it away. The live ISO also ships no
    // `snd_*` modules at all (`iso/mkiso.sh`), so today the list this menu would
    // show is empty on the only hardware tOS actually boots on.

    /// Move the default card's volume and say where it ended up.
    ///
    /// The mixer answers with the level as it reads back rather than with the
    /// level that was asked for, and that answer is what reaches the bar: a
    /// card whose range is `0..=3` cannot be at 55%, and a card that is muted
    /// by a switch does not get louder when it is turned up. Telling the user
    /// what was asked for would be right almost always and wrong exactly when
    /// it mattered.
    ///
    /// [`Machine::refresh`] is called rather than waited for because the poll
    /// that would otherwise notice is up to a second away, and a second is
    /// long enough to press the key again — so the status bar would show the
    /// level from two presses ago while the user is still pressing. The
    /// reading is refreshed rather than written from the [`Volume`] in hand so
    /// that there is one path by which the machine's state gets into the
    /// reading, and it is the one that asks the machine.
    fn change_volume(&mut self, knob: Knob) -> bool {
        // The borrow of the mixer ends with this statement: `refresh` and the
        // notification below both want the compositor back.
        let moved = self.machine.mixer().map(|mixer| match knob {
            Knob::Up => mixer.volume_up(),
            Knob::Down => mixer.volume_down(),
            Knob::Mute => mixer.toggle_mute(),
        });
        match moved {
            None => {
                // Said once a session, not once a keypress; see
                // `said_no_sound_card`. Said at all, because a volume key that
                // does nothing and says nothing is indistinguishable from a
                // volume key tOS failed to read.
                if !self.said_no_sound_card {
                    self.said_no_sound_card = true;
                    self.notifications.status("no sound card");
                    return true;
                }
                false
            }
            // A card that is there and will not take a write is worth the same
            // complaint as a split that would not open: what the kernel said,
            // once, rather than a key that quietly stops working.
            Some(Err(error)) => {
                self.report_error("volume", error);
                true
            }
            Some(Ok(volume)) => {
                self.machine.refresh(Instant::now());
                self.notifications.status(volume_status(volume));
                true
            }
        }
    }

    /// The copy mode that is up, if any.
    pub fn copy_mode(&self) -> Option<&CopyMode> {
        self.copy.as_ref().map(|(_, copy)| copy)
    }

    // ---- the lock -------------------------------------------------------

    /// Put the lock screen up, or say why there is no lock to put up.
    ///
    /// Public because the binding is not the only way in: an idle deadline
    /// reaches the same state machine, and a test reaches it without
    /// synthesising a keypress.
    ///
    /// The credential is read here rather than when a password is offered, so
    /// that a machine with no password never gets a locked screen at all. That
    /// one rule is the whole of what makes the live ISO behave: nothing in the
    /// compositor knows what live media is, only that this machine was never
    /// given a password to unlock with.
    pub fn lock_session(&mut self) -> bool {
        if self.lock.is_some() {
            return false;
        }
        match lock::read_credential(&self.config.credential, &self.config.credential_user) {
            Ok(hash) => {
                self.engage_lock(hash);
                true
            }
            Err(why) => {
                self.notifications.status(format!("cannot lock: {why}"));
                true
            }
        }
    }

    /// Put the lock up because a deadline came due rather than because
    /// somebody asked.
    ///
    /// Identical to the binding except on a machine with no password, and
    /// that difference is the point. The binding says so, because a person
    /// pressed a key and is owed an answer. This says nothing: nobody asked,
    /// nothing is wrong, and a live ISO would otherwise find "cannot lock"
    /// waiting on the status bar every time its user walked away from it.
    /// Such a session blanks and stays unlocked, which is "no credential, no
    /// lock" arriving by the other road.
    fn lock_on_idle(&mut self) -> bool {
        match lock::read_credential(&self.config.credential, &self.config.credential_user) {
            Ok(hash) => {
                self.engage_lock(hash);
                true
            }
            Err(_) => false,
        }
    }

    fn engage_lock(&mut self, hash: String) {
        // A half-pressed leader and a drag in progress both belong to the
        // person who was here before; neither should still be going when the
        // session comes back.
        self.keymap.cancel_pending();
        self.release_grab();
        self.lock = Some(LockScreen::new(hash, self.config.credential_user.clone()));
        // Read here rather than held across the session for the reason on the
        // field: a lock is up for as long as somebody is away from the
        // machine, and the pixels are worth having only then.
        self.picture = Splash::load_lock(&self.config.lock_picture);
        // An open overlay is left exactly as it was, under the lock rather
        // than closed by it. Nothing of it is drawn while the lock is up, and
        // the person who gets it back is the person who left it there.
        self.needs_full_redraw = true;
    }

    /// Whether the session is locked, which the DRM loop asks before it agrees
    /// to a VT switch and before it lets go of the display.
    pub fn is_locked(&self) -> bool {
        self.lock.is_some()
    }

    pub fn lock_screen(&self) -> Option<&LockScreen> {
        self.lock.as_ref()
    }

    /// The one way out, and there is no other.
    ///
    /// Two things can be behind this screen. A session, which comes back
    /// untouched — that is a lock. Or none, because this is the login screen
    /// at the start of a machine's day, or because the last pane died while
    /// the screen was locked; then answering it starts one.
    fn unlock(&mut self) {
        self.lock = None;
        // The picture belonged to the screen that has just been answered.
        self.picture = None;
        // Nothing under the lock was drawn while it was up, and the damage
        // that would have said what to repaint was thrown away with each
        // locked frame. The whole screen is the only honest answer.
        self.needs_full_redraw = true;
        // The phase stood still while the lock was up (#164), so it is
        // whatever it was when the screen went away — and a phase that has
        // been still for an hour would flip on the first tick after this.
        // Started again here instead, lit, for the reason an unblank starts
        // it lit: where the caret is, is the first thing anybody looks for on
        // a screen they have just got back.
        self.blink_visible = true;
        self.last_blink = Instant::now();
        self.session_ended_while_locked = false;
        if !self.panes.is_empty() {
            return;
        }
        // Nothing behind this screen, and nowhere to come back to: a display
        // that is not a machine's console has no login boundary, so a session
        // that ended under the lock ends the compositor with it, exactly as it
        // did before there was one.
        if !self.config.gated {
            self.running = false;
            return;
        }
        if let Err(e) = self.begin_session() {
            // A machine that cannot start a shell is not one to sit at a
            // prompt on. Say so and hand it back to the init, which is the
            // only thing left that can do anything about it.
            self.notifications
                .status(format!("cannot start a session: {e}"));
            self.running = false;
        }
    }

    // ---- idle -----------------------------------------------------------

    /// Whether the screen is dark because nobody has been here.
    pub fn is_blanked(&self) -> bool {
        self.blanked
    }

    /// Act on the idle deadlines, and bring the screen back the moment
    /// somebody is here again. Returns true when a frame is needed.
    ///
    /// Nothing here can fail in a way that ends a session. A display that
    /// will not blank leaves one lit, which is a thing to say rather than a
    /// thing to stop for.
    ///
    /// The time arrives as an argument for the reason the lock's does: five
    /// minutes from now has to be somewhere a test can stand without spending
    /// five minutes getting there.
    pub fn apply_idle(&mut self, now: Instant, display: &mut dyn Display) -> bool {
        let idle = now.saturating_duration_since(self.last_activity);
        let mut changed = false;

        // The lock is looked at before the blank, and the order is load
        // bearing on the pass where both are due — because they were given the
        // same interval, or because the loop was away while another VT had the
        // screen. Unblanking puts back the last frame that was drawn, so the
        // last frame drawn before the screen goes dark must never be the
        // session of somebody who is not here.
        if !self.idle_lock_done
            && self.lock.is_none()
            && self.config.idle_lock.is_some_and(|after| idle >= after)
        {
            self.idle_lock_done = true;
            changed |= self.lock_on_idle();
        }

        let dark = !self.blank_refused && self.config.idle_blank.is_some_and(|after| idle >= after);
        if dark != self.blanked {
            match display.blank(dark) {
                // Remembered only once the display agrees. A screen that
                // could not be put to sleep is still lit, and one remembered
                // as dark would swallow the next keystroke to wake a panel
                // that was never off.
                Ok(()) => {
                    self.blanked = dark;
                    if dark {
                        // A dark screen gives everything it is sent to
                        // nobody, the release that would have ended a drag
                        // included, so the drag ends here instead — the same
                        // thing [`Compositor::engage_lock`] does about the
                        // same hole, for the same reason.
                        self.release_grab();
                    } else {
                        // A backend that put the panel to sleep decides for
                        // itself what is on it when it wakes, and painting all
                        // of it is the only thing the session can do about
                        // that.
                        self.needs_full_redraw = true;
                        // The cursor comes back lit. Where the caret is, is
                        // the first thing anyone looks for on a screen they
                        // have just woken.
                        self.blink_visible = true;
                        self.last_blink = now;
                        changed = true;
                    }
                }
                Err(e) => {
                    // A screen that will not go out is not a reason to end
                    // somebody's session, which is what letting this error
                    // reach the loop would do. Say so, and stop asking: a
                    // display that has refused once will refuse again, and
                    // asking it on every pass is how a machine nobody is
                    // using becomes a machine that is busy all night.
                    self.report_error("blanking the display", e);
                    self.blank_refused = true;
                }
            }
        }
        changed
    }

    /// How long until the next idle deadline, when one is still to come.
    ///
    /// A deadline that has already been acted on is not waited for. The blank
    /// is level triggered off `blanked`, so it drops out once the screen is
    /// dark; the lock fires once per idle period, so a machine with no
    /// password does not spend the night rediscovering that it has none.
    fn next_idle_deadline(&self, now: Instant) -> Option<Duration> {
        let idle = now.saturating_duration_since(self.last_activity);
        let blank = self
            .config
            .idle_blank
            .filter(|_| !self.blanked && !self.blank_refused);
        let lock = self
            .config
            .idle_lock
            .filter(|_| self.lock.is_none() && !self.idle_lock_done);
        [blank, lock]
            .into_iter()
            .flatten()
            .map(|after| after.saturating_sub(idle))
            .min()
    }

    // ---- overlays -------------------------------------------------------

    /// Put a menu up over the panes.
    pub fn open_overlay(&mut self, kind: OverlayKind, overlay: Overlay) {
        // Whatever the mouse was holding, it has stopped holding it. An
        // overlay opens on a binding and the keyboard works while a button is
        // down, so a drag can still be in progress — and the release that
        // would have ended it is an event the menu eats, while escape, which
        // is how a menu is usually closed, is not a mouse event at all. Doing
        // it here rather than at either of those is what makes it one place
        // instead of a list of them.
        self.release_grab();
        self.overlay = Some((kind, overlay));
        // The overlay covers cells the panes are not going to repaint, and
        // closing it uncovers them again, so both ends need a full frame.
        self.needs_full_redraw = true;
    }

    pub fn overlay(&self) -> Option<&Overlay> {
        self.overlay.as_ref().map(|(_, overlay)| overlay)
    }

    /// Where the open menu's box is on screen, if one is open and the display
    /// is big enough to have drawn it.
    ///
    /// The frame and the mouse both go through here, so anything else that
    /// wants to know where a row is — a test aiming at one — is asking the
    /// same question rather than working it out again.
    pub fn overlay_placement(&self) -> Option<Placement> {
        let area = self.overlay_area();
        let cell = self.cell_size();
        self.overlay
            .as_ref()
            .and_then(|(_, overlay)| overlay.placement(area, cell))
    }

    fn close_overlay(&mut self) {
        self.overlay = None;
        self.needs_full_redraw = true;
    }

    /// Where the open overlay is drawn: the grid, in pixels.
    ///
    /// A method because the box is centred in it, so the frame that draws the
    /// box and the press that hits it have to be centring in the same
    /// rectangle — the bar's row is not part of it, and a hit test that
    /// included the row the panes do not get would put every row of the menu
    /// half a cell out.
    fn overlay_area(&self) -> PixelRect {
        let (cw, ch) = self.cell_size();
        let area = self.grid_area();
        PixelRect::new(0, 0, area.width * cw, area.height * ch)
    }

    /// Give a key to the open overlay. Returns true when a repaint is needed.
    fn overlay_key(&mut self, key: &KeyEvent) -> bool {
        let Some((_, overlay)) = &mut self.overlay else {
            return false;
        };
        let outcome = overlay.handle_key(key);
        self.overlay_outcome(outcome)
    }

    /// Give a mouse event to the open overlay, in display cells.
    ///
    /// Separate from [`Compositor::overlay_key`] only as far as the outcome:
    /// a click on a row and an enter on the same row are the same answer, and
    /// they come back here to be acted on by the same code.
    fn overlay_mouse(
        &mut self,
        cell_x: u32,
        cell_y: u32,
        button: Option<MouseButton>,
        action: MouseAction,
    ) -> bool {
        let area = self.overlay_area();
        let cell = self.cell_size();
        let Some((_, overlay)) = &mut self.overlay else {
            return false;
        };
        let outcome = overlay.handle_mouse(cell_x, cell_y, button, action, area, cell);
        self.overlay_outcome(outcome)
    }

    /// Act on what the open overlay reported, however it was asked.
    fn overlay_outcome(&mut self, outcome: OverlayOutcome) -> bool {
        let Some((kind, overlay)) = &self.overlay else {
            return false;
        };
        let kind = *kind;
        // Read out of the overlay before it is closed, since closing drops it
        // along with the row that was chosen and the line that was typed.
        let answer = match outcome {
            OverlayOutcome::Consumed => return false,
            OverlayOutcome::Changed => return true,
            // Cancelling changes nothing but the screen.
            OverlayOutcome::Cancelled => None,
            OverlayOutcome::Chosen(index) => {
                Some((Some(index), overlay.items()[index].label.clone()))
            }
            OverlayOutcome::Accepted => Some((None, overlay.query().to_string())),
        };
        // Escape. The wireless menus are the two that keep something behind
        // them, and both keep it only for as long as the box is up: nothing is
        // held open, so letting go is the whole of closing them.
        if answer.is_none() {
            match kind {
                OverlayKind::Wireless => self.wifi.forget_scan(),
                OverlayKind::Passphrase => drop(self.wifi.take_choice()),
                _ => {}
            }
        }
        self.close_overlay();
        if let Some((row, label)) = answer {
            self.choose(kind, row, &label);
        }
        true
    }

    /// Act on what an overlay reported: the row that was chosen, or the line
    /// a prompt accepted. One arm per menu.
    ///
    /// Both the row's position and its text are passed, because a menu built
    /// out of names is answered by name and a menu built out of a list the
    /// compositor already holds is answered by position. A prompt has no row,
    /// which is what `None` means.
    fn choose(&mut self, kind: OverlayKind, row: Option<usize>, label: &str) {
        match kind {
            OverlayKind::Launcher => self.launch(label),
            OverlayKind::Notifications => {
                let Some(index) = row else { return };
                match self.notifications.choose(index) {
                    Chosen::Clear => self.notifications.clear_history(),
                    // Where a notification came from is the useful thing to do
                    // with it: a build that finished is a pane to go and look
                    // at, wherever that pane has ended up.
                    Chosen::Pane(pane) => {
                        if self.session.set_focus(pane) {
                            self.sync_layout();
                            self.needs_full_redraw = true;
                        }
                    }
                    Chosen::Nowhere => {}
                }
            }
            // Closing the overlay has already asked for the frame that puts
            // the new name in the status bar.
            OverlayKind::RenameWorkspace => self.session.rename_active(label),
            // Nothing to choose: the sheet is there to be read, so enter
            // closes it the way escape does.
            OverlayKind::Bindings => {}
            OverlayKind::Bluetooth => {
                let Some(index) = row else { return };
                self.choose_bluetooth(index);
            }
            OverlayKind::Power => {
                // A row that names nothing is a menu that has been rebuilt
                // wrong; doing nothing is the only safe answer on this menu.
                let Some(action) = power::action_named(label) else {
                    return;
                };
                if power::needs_confirming(action) {
                    self.open_overlay(
                        OverlayKind::ConfirmPower(action),
                        power::confirmation(action),
                    );
                    return;
                }
                self.request_power(action);
            }
            // Only the row that names the action goes ahead; every other
            // answer, including escape and the enter that opened this, leaves
            // the session alone. See [`crate::power::confirmation`].
            OverlayKind::ConfirmPower(action) => {
                if power::confirmed(action, label) {
                    self.request_power(action);
                }
            }
            // The list is rebuilt from the machine each time it is opened, so
            // the row's text is the only thing about it that is still true by
            // the time this runs: an interface that went away between opening
            // the menu and choosing from it is simply a name the machine no
            // longer knows, and every arm below already has to cope with that.
            OverlayKind::Networks => self.open_link_menu(label),
            OverlayKind::Link => self.act_on_link(label),
            // By name and not by position, for the reason the interface list
            // above is: the rows are replaced under the menu on every tick
            // while the radio is listening, and duplicates have been folded,
            // so an SSID is both unique among the rows and still the same
            // network however the list has been shuffled since it was drawn.
            OverlayKind::Wireless => self.join_network(label),
            OverlayKind::Passphrase => self.accept_passphrase(label),
        }
    }

    // ---- the network ----------------------------------------------------

    /// Put the interface list up.
    ///
    /// Reading it needs nothing: `/sys/class/net` is world readable and
    /// `getifaddrs(3)` asks no permission, so this menu opens on the live ISO
    /// and for an ordinary user exactly as it does for root. Only the rows
    /// inside it can fail, and each of them says so when it does — which is
    /// the shape the issue asks for, status everywhere and configuration
    /// where it is allowed.
    fn open_networks(&mut self) {
        let interfaces = self.machine.network().visible_interfaces();
        if interfaces.is_empty() {
            // Not an empty menu. An empty list with a query line under it
            // looks like a menu that has not loaded yet, and this machine is
            // not going to grow an interface while it is open.
            self.notifications.status("no wired or wireless interfaces");
            return;
        }
        let items = interfaces
            .iter()
            .map(|interface| {
                OverlayItem::with_detail(interface.name.clone(), link_detail(interface))
            })
            .collect();
        self.open_overlay(OverlayKind::Networks, Overlay::new("network", items));
    }

    /// Put up what can be done to one interface.
    fn open_link_menu(&mut self, interface: &str) {
        let Some(found) = self.machine.network().interface(interface) else {
            self.notifications
                .status(format!("{interface} is no longer there"));
            return;
        };
        let mut items = vec![
            OverlayItem::with_detail(
                if found.admin_up { TAKE_DOWN } else { BRING_UP },
                if found.admin_up {
                    "switch the link off"
                } else {
                    "switch the link on"
                },
            ),
            OverlayItem::with_detail(REQUEST_ADDRESS, "DHCP, and the route and resolvers with it"),
        ];
        if found.kind == Kind::Wireless {
            items.extend(self.wireless_rows(&found.name));
        }
        self.network_target = Some(found.name.clone());
        self.open_overlay(OverlayKind::Link, Overlay::new(found.summary(), items));
    }

    /// Do what a row of the link menu says.
    fn act_on_link(&mut self, label: &str) {
        let Some(interface) = self.network_target.take() else {
            return;
        };
        match label {
            BRING_UP | TAKE_DOWN => {
                let up = label == BRING_UP;
                let result = if up {
                    self.machine.network().bring_up(&interface)
                } else {
                    self.machine.network().take_down(&interface)
                };
                match result {
                    // The link has just moved, so the poll interval is not
                    // the right amount of time to wait before saying so.
                    Ok(()) => {
                        self.machine.refresh(Instant::now());
                        self.notifications
                            .status(format!("{interface} {}", if up { "up" } else { "down" }));
                    }
                    Err(error) => self.report_error(
                        &format!("{interface} {}", if up { "up" } else { "down" }),
                        error,
                    ),
                }
            }
            REQUEST_ADDRESS => self.request_address(&interface),
            JOIN => self.open_wireless(&interface),
            // The SSID is on the end of the label, which is where it has to be
            // read from: the menu was built from a `STATUS` that is now a
            // keystroke old, and the network it named is the one somebody
            // pressed a row about.
            _ if label.starts_with(LEAVE) => {
                self.leave_network(&interface, &label[LEAVE.len()..], false)
            }
            _ if label.starts_with(FORGET) => {
                self.leave_network(&interface, &label[FORGET.len()..], true)
            }
            // The one row in tOS whose whole purpose is to be pressed and say
            // why it cannot do anything. See [`Compositor::wireless_rows`].
            _ if label.starts_with(NO_SUPPLICANT) => self.notifications.status(INSTALL_SUPPLICANT),
            _ => {}
        }
    }

    /// The rows a radio adds to the link menu.
    ///
    /// Three shapes, which is what `docs/design/wifi.md` (#137) writes down:
    /// associated, and the network it is on can be left or forgotten by name;
    /// not associated, and there is only the list to open; or no supplicant
    /// answering on the socket at all, which is what a machine gets after
    /// `apt remove wpasupplicant` and on the initramfs rescue session, and
    /// which says so rather than offering a row that fails.
    ///
    /// `STATUS` is asked here rather than taken from the machine reading,
    /// which knows the SSID the kernel reports and not the network id the
    /// supplicant hands out — and it is the id that `leave` and `forget` act
    /// on.
    fn wireless_rows(&mut self, interface: &str) -> Vec<OverlayItem> {
        let mut client = match self.wifi.client(interface) {
            Ok(client) => client,
            Err(_) => {
                return vec![OverlayItem::with_detail(
                    format!("{NO_SUPPLICANT}{interface}"),
                    INSTALL_SUPPLICANT,
                )]
            }
        };
        let mut rows = Vec::new();
        // A `STATUS` that was refused is still a supplicant that answered, so
        // the socket is there and the list can be opened; what cannot be said
        // is which network it is on, and `leave` on a network nobody can name
        // is the row this is here to avoid offering.
        if let Ok(status) = client.status() {
            if let (Some(_), Some(ssid)) = (status.id, status.ssid.as_deref()) {
                rows.push(OverlayItem::with_detail(
                    format!("{LEAVE}{ssid}"),
                    "stop using it, without forgetting it",
                ));
                rows.push(OverlayItem::with_detail(
                    format!("{FORGET}{ssid}"),
                    "forget it, so the next boot does not rejoin it",
                ));
            }
        }
        rows.push(OverlayItem::with_detail(JOIN, "the networks in range"));
        rows
    }

    /// Stop using the network this radio is on, and optionally forget it.
    ///
    /// `STATUS` again rather than an id remembered when the menu was built:
    /// the supplicant may have moved between the two keystrokes, and acting on
    /// a stale id is how somebody forgets the network they were on last week
    /// instead of the one in front of them.
    fn leave_network(&mut self, interface: &str, ssid: &str, forget: bool) {
        let mut client = match self.wifi.client(interface) {
            Ok(client) => client,
            Err(error) => {
                self.notifications.status(format!("{interface}: {error}"));
                return;
            }
        };
        let id = match client.status() {
            Ok(status) => match status.id {
                Some(id) => id,
                None => {
                    self.notifications
                        .status(format!("{interface} is not on a network"));
                    return;
                }
            },
            Err(error) => {
                self.notifications.status(format!("{interface}: {error}"));
                return;
            }
        };
        let outcome = match forget {
            true => client.remove(id).and_then(|()| client.save()),
            false => client.disable(id).and_then(|()| client.disconnect()),
        };
        match outcome {
            Ok(()) => {
                // The link has just moved, so the poll interval is not the
                // right amount of time to wait before the bar agrees with it.
                self.machine.refresh(Instant::now());
                self.notifications.status(match forget {
                    true => format!("forgot {ssid}"),
                    false => format!("left {ssid}"),
                });
            }
            Err(error) => self.notifications.status(format!("{ssid}: {error}")),
        }
    }

    /// Put the networks in range up, and ask for a fresh scan behind them.
    fn open_wireless(&mut self, interface: &str) {
        match self.wifi.begin_scan(interface, Instant::now()) {
            Ok((items, title)) => {
                self.open_overlay(OverlayKind::Wireless, Overlay::new(title, items))
            }
            Err(error) => self.notifications.status(format!("{interface}: {error}")),
        }
    }

    /// A row of the wireless list was chosen.
    ///
    /// The scan is let go first, whatever happens next: the menu it belonged
    /// to has already been closed by [`Compositor::overlay_outcome`], and a
    /// scan nothing is showing is a scan nothing should be refreshing.
    fn join_network(&mut self, ssid: &str) {
        let found = self.wifi.found(ssid);
        self.wifi.forget_scan();
        // The list was replaced under the keystroke, which the tick can do
        // while the radio is listening. Saying nothing is right: the menu has
        // closed, and there is no network of that name to say anything about.
        let Some((interface, security)) = found else {
            return;
        };
        if let Some(why) = wifi::refusal(ssid, security) {
            self.notifications.status(why);
            return;
        }
        if security == Security::Psk {
            self.wifi.choose(&interface, ssid);
            self.open_overlay(
                OverlayKind::Passphrase,
                Overlay::secret_prompt(format!("{ssid} — passphrase"), ""),
            );
            return;
        }
        self.begin_join(&interface, ssid, None);
    }

    /// A passphrase was typed and accepted.
    fn accept_passphrase(&mut self, passphrase: &str) {
        let Some((interface, ssid)) = self.wifi.take_choice() else {
            return;
        };
        self.begin_join(&interface, &ssid, Some(passphrase));
    }

    /// Configure and select a network, and say what happened.
    ///
    /// A passphrase the client refuses — too short, too long, a `"` in it, a
    /// newline that came along with a paste — is the one error that puts the
    /// prompt back up. The message goes on the status line verbatim, because
    /// it already names the rule that was broken, and the line keeps what was
    /// typed, because the fix is nearly always one character.
    fn begin_join(&mut self, interface: &str, ssid: &str, passphrase: Option<&str>) {
        match self
            .wifi
            .begin_join(interface, ssid, passphrase, Instant::now())
        {
            Ok(said) => self.notifications.status(said),
            Err(error) if error.kind() == io::ErrorKind::InvalidInput => {
                self.notifications.status(error.to_string());
                self.wifi.choose(interface, ssid);
                self.open_overlay(
                    OverlayKind::Passphrase,
                    Overlay::secret_prompt(
                        format!("{ssid} — passphrase"),
                        passphrase.unwrap_or_default(),
                    ),
                );
            }
            Err(error) => self.notifications.status(format!("{ssid}: {error}")),
        }
    }

    /// The wireless menus' own tick: a list refreshed under the user while the
    /// radio is listening, and a join watched to wherever it ends up.
    ///
    /// Both are polls rather than waits, which is the whole of why nothing
    /// here is on a thread: every request is a local datagram answered in
    /// microseconds, and the waiting — for a scan to finish, for a handshake
    /// to complete — is done by the tick that was going to happen anyway.
    fn poll_wireless(&mut self, now: Instant) -> bool {
        let mut changed = false;
        if matches!(self.overlay, Some((OverlayKind::Wireless, _))) {
            // `set_items` rather than reopening, the way the Bluetooth menu
            // takes in an inquiry, so that a query typed while the radio was
            // listening survives the answer.
            if let Some((items, title)) = self.wifi.refresh_scan(now) {
                if let Some((_, overlay)) = &mut self.overlay {
                    overlay.set_items(items);
                    overlay.set_title(title);
                }
                self.needs_full_redraw = true;
                changed = true;
            }
        }
        if let Some(said) = self.wifi.tick(now) {
            self.notifications.status(said);
            changed = true;
        }
        changed
    }

    /// Bring the wired links up and get them addresses, with nobody asking.
    ///
    /// #124: `sshd` is listening three seconds into the boot and there is no
    /// address for anybody to reach it at, because everything in the tree that
    /// brings a link up is a menu row. This is the same two rows — "bring the
    /// link up" and "ask for an address" — pressed by the machine itself, in
    /// the order and on the links [`Autoconfigure`] chooses.
    ///
    /// The interfaces are read here rather than taken from the machine
    /// reading, which holds one link and not the list, and which is not read
    /// at all behind a blank. Both matter: a second card is a link this has
    /// to see, and a blanked machine is the machine this is for.
    fn autoconfigure(&mut self, now: Instant) -> bool {
        let interval = if self.blanked {
            AUTO_LOOK_BLANKED
        } else {
            AUTO_LOOK
        };
        if let Some(last) = self.auto_looked_at {
            if now.saturating_duration_since(last) < interval {
                return false;
            }
        }
        self.auto_looked_at = Some(now);

        let interfaces = self.machine.network().visible_interfaces();
        let Some(step) = self.auto.next(&interfaces, self.dhcp.is_some()) else {
            return false;
        };
        match step {
            Step::BringUp(interface) => match self.machine.network().bring_up(&interface) {
                // Said nothing about: nobody asked, so the answer is what is
                // worth a line and "the switch is on" is not it. The address
                // it leads to is announced by `collect_address`, exactly as it
                // is when a person presses the row.
                Ok(()) => self.machine.refresh(now),
                // This one is said. A link that cannot be brought up is the
                // whole reason a machine nobody is at is not on the network,
                // and it is the only part of this path somebody could do
                // something about.
                Err(error) => {
                    self.report_error(&format!("{interface} up"), error);
                    true
                }
            },
            Step::Ask(interface) => {
                self.request_address(&interface);
                true
            }
        }
    }

    /// Start a DHCP acquisition on an interface.
    ///
    /// Everything that can be decided here is decided here, so that the
    /// thread below carries no judgement at all: whether one is already
    /// running, and whether the interface has a hardware address to be known
    /// by. A DISCOVER from `00:00:00:00:00:00` is one no server will answer,
    /// and finding that out fifteen seconds later is worse than not starting.
    fn request_address(&mut self, interface: &str) {
        if let Some((busy, _)) = &self.dhcp {
            self.notifications
                .status(format!("already asking on {busy}"));
            return;
        }
        let Some(mac) = self.machine.network().hardware_address(interface) else {
            self.notifications
                .status(format!("{interface} has no hardware address to ask from"));
            return;
        };

        let (sender, receiver) = mpsc::channel();
        let on = interface.to_string();
        // Named, because a thread that is asleep in `recvfrom` for fifteen
        // seconds is a thread somebody will eventually find in a backtrace.
        let spawned = std::thread::Builder::new()
            .name("tos-dhcp".to_string())
            // The receiver is dropped when the answer is collected, so a send
            // into a closed channel is the ordinary end of a conversation
            // nobody is listening to any more, not a failure.
            .spawn(move || drop(sender.send(dhcp::acquire_on(&on, mac))));

        match spawned {
            Ok(_) => {
                self.dhcp = Some((interface.to_string(), receiver));
                self.notifications
                    .status(format!("asking for an address on {interface}"));
            }
            Err(error) => self.report_error(&format!("dhcp on {interface}"), error),
        }
    }

    /// Take the answer if the DHCP thread has one, and put it on the link.
    ///
    /// Called from [`Compositor::tick`], which runs at least once a second
    /// because the machine poll is on that deadline — so a lease is applied
    /// within a second of arriving without anything new having to be woken
    /// up for it. Returns true when there is something new to paint.
    fn collect_address(&mut self) -> bool {
        let Some((interface, receiver)) = &self.dhcp else {
            return false;
        };
        let answer = match receiver.try_recv() {
            Ok(answer) => answer,
            Err(mpsc::TryRecvError::Empty) => return false,
            // The thread went away without sending, which means it panicked:
            // there is no answer coming, and leaving the slot occupied would
            // mean no address could ever be asked for again.
            Err(mpsc::TryRecvError::Disconnected) => Err(io::Error::other(
                "the DHCP client stopped without answering",
            )),
        };
        let interface = interface.clone();
        self.dhcp = None;

        match answer {
            Ok(lease) => match self.machine.network().configure(&interface, &lease) {
                Ok(()) => {
                    self.machine.refresh(Instant::now());
                    self.notifications
                        .status(format!("{interface} {}", lease.describe()));
                }
                // The lease is real and the machine is not allowed to use it,
                // which is the live ISO's whole situation. Both halves are
                // said: what was offered, and what stopped it being taken.
                Err(error) => self.notifications.status(format!(
                    "{interface}: cannot take {}: {error}",
                    lease.describe()
                )),
            },
            Err(error) => self.notifications.status(format!("{interface}: {error}")),
        }
        true
    }

    /// The cheat sheet, built from the keymap that is resolving these keys.
    ///
    /// Not from the `--help` text, and not from a copy of the defaults: a
    /// sheet that is a second telling of the bindings is one that will
    /// eventually be telling you about a key that no longer does that. This
    /// one cannot be wrong, and a keymap that was customised at startup
    /// describes itself here without anything being taught about it.
    fn binding_sheet(&self) -> Overlay {
        let title = match describe::leader_name(&self.keymap) {
            Some(leader) => format!("key bindings (leader {leader})"),
            None => "key bindings".to_string(),
        };
        // The description is the label, so that typing "split" finds the key
        // rather than only the other way round: what you have forgotten is
        // the key, and what you can still name is what you wanted to do.
        let items = describe::cheat_sheet(&self.keymap)
            .into_iter()
            .map(|row| OverlayItem::with_detail(row.action, row.keys))
            .collect();
        Overlay::new(title, items)
    }

    /// Open a pane running `program`, using the same path a split does.
    fn launch(&mut self, program: &str) {
        if tos_pty::which(program).is_none() {
            // The list came from $PATH, so this means it went away in between;
            // spawning would leave a pane that dies on its own.
            self.notifications.status(format!("not found: {program}"));
            return;
        }
        let command = vec![program.to_string()];
        if let Some(id) = self.split_running(Axis::Columns, Some(&command)) {
            // Until the program sets a title of its own, its name is the most
            // truthful thing the status bar can say about the pane.
            if let Some(pane) = self.panes.get_mut(&id) {
                pane.title = program.to_string();
            }
        }
    }

    fn scroll_focused(&mut self, lines: isize) -> bool {
        let focus = self.session.focus();
        match self.panes.get_mut(&focus) {
            // A negative delta means further back in history.
            Some(pane) => pane.terminal.scroll_display(-lines),
            None => false,
        }
    }

    /// Arrange the active workspace the next way round, or the previous one.
    ///
    /// Nothing is said on the status line, and that is deliberate rather than
    /// forgotten: the bar carries the arrangement's name for as long as it is
    /// in force, which is the question worth answering, while a notification
    /// answers it once and queues. Cycling three keys quickly would leave the
    /// message slot showing the first arrangement with "(+2)" after it —
    /// naming, at length, a layout the panes have already left.
    fn cycle_layout(&mut self, forward: bool) -> bool {
        if forward {
            self.session.next_layout();
        } else {
            self.session.previous_layout();
        }
        self.sync_layout();
        self.needs_full_redraw = true;
        true
    }

    fn split(&mut self, axis: Axis) -> bool {
        self.split_running(axis, None);
        true
    }

    /// Split the focused pane, running `command` in the new one. `None` runs
    /// whatever the configuration says a pane runs.
    ///
    /// Returns the new pane, or `None` when there was no room or the process
    /// could not be started; either way the message says so.
    fn split_running(&mut self, axis: Axis, command: Option<&[String]>) -> Option<PaneId> {
        let area = self.grid_area();
        let Some(new_id) = self.session.split_focused(area, axis) else {
            // Refusing is the right answer when the pane is too small; saying
            // so beats silently creating a pane with nowhere to go.
            self.notifications.status("no room to split");
            return None;
        };
        let pane_area = self
            .session
            .active()
            .geometry(area)
            .into_iter()
            .find(|(id, _)| *id == new_id)
            .map(|(_, rect)| rect)
            .unwrap_or(Rect::new(0, 0, 80, 24));

        let spawned = match command {
            Some(command) => self.spawn_pane_running(pane_area, Some(command)),
            None => self.spawn_pane(pane_area),
        };
        match spawned {
            Ok(pane) => {
                self.panes.insert(new_id, pane);
                self.sync_layout();
                self.needs_full_redraw = true;
                Some(new_id)
            }
            Err(e) => {
                // The layout must not keep a pane with no process behind it.
                self.session.close_pane(new_id);
                self.sync_layout();
                self.report_error("split", e);
                None
            }
        }
    }

    /// Close a pane, ending the session when it was the last one.
    pub fn close_pane(&mut self, id: PaneId) {
        let closed = self.session.close_pane(id);
        if closed.is_empty() {
            // The session refused, which means this was the final pane.
            self.panes.remove(&id);
            // Quitting is a way out of a locked screen, and a program exiting
            // is a way to quit that does not go through a binding: a shell
            // that reaches its end of file while nobody is there would
            // otherwise hand the machine back. The session is over, but it
            // does not end until somebody says who they are.
            if self.lock.is_some() {
                self.session_ended_while_locked = true;
            } else {
                self.log_out();
            }
            return;
        }
        for pane in closed {
            self.panes.remove(&pane);
            if self.mouse_grab == Some(Grab::Pane(pane)) {
                self.mouse_grab = None;
            }
            // A divider being dragged is dropped whichever pane went, not
            // only one beside it. Closing frees nodes in the layout tree and
            // splitting hands the same slots out again, so a drag that
            // outlived a pane could come back pointing at a split that was
            // built after it — and resize something nobody was holding.
            if matches!(self.mouse_grab, Some(Grab::Divider(_))) {
                self.mouse_grab = None;
            }
            // A copy mode whose pane has gone has nothing left to select in,
            // and leaving it up would swallow the keyboard on behalf of text
            // that no longer exists.
            if self.copy.as_ref().is_some_and(|(id, _)| *id == pane) {
                self.copy = None;
            }
            // The preedit died with the pane's context, which is the whole of
            // what happens: nothing was ever sent, so there is nothing to
            // flush and nothing to lose. The candidate list is the one piece
            // that is not on the pane, so it is dropped here by hand.
            self.ime.end_conversion(pane);
        }
        self.sync_layout();
        self.needs_full_redraw = true;
    }

    fn report_error(&mut self, what: &str, error: io::Error) {
        self.notifications.status(format!("{what} failed: {error}"));
    }

    // ---- bluetooth ------------------------------------------------------

    /// Put the Bluetooth controls up.
    ///
    /// The adapter and its links are read here rather than taken from
    /// [`Machine::reading`](crate::system::Machine::reading), which can be a
    /// second old. A second is nothing on a status bar and everything on a
    /// menu: the row that says "power hci0 on" for an adapter that came up
    /// while the key was being pressed is the one row a person would press
    /// twice and then distrust.
    fn open_bluetooth(&mut self) -> bool {
        let (adapter, connections) = self.adapter_now();
        let overlay = self.bluetooth.menu(adapter.as_ref(), &connections);
        self.open_overlay(OverlayKind::Bluetooth, overlay);
        true
    }

    /// The adapter and the links it holds, read now.
    fn adapter_now(&mut self) -> (Option<Adapter>, Vec<Connection>) {
        let adapter = self.machine.bluetooth().default_adapter();
        let connections = match adapter.as_ref() {
            Some(adapter) => self.machine.bluetooth().connections(&adapter.name),
            None => Vec::new(),
        };
        (adapter, connections)
    }

    /// Do what the chosen row said, and come back with the menu redrawn.
    ///
    /// Reopening is not politeness. Every one of these is a step towards
    /// something else — unblock, then power on, then scan — and a menu that
    /// closed after each would make the ordinary errand four keystrokes of
    /// reopening. The adapter is re-read on the way back in, so the menu that
    /// returns is the one the action left behind rather than the one it
    /// started from.
    fn choose_bluetooth(&mut self, index: usize) {
        let choice = self.bluetooth.choose(index);
        // Before the adapter is looked for, not after: most rows of this menu
        // are there to be read, and pressing enter on the line that says this
        // machine has no Bluetooth must not answer "no adapter".
        if choice == Choice::Nothing {
            return;
        }
        let Some(adapter) = self.machine.bluetooth().default_adapter() else {
            // The adapter went away between the menu being drawn and the row
            // being chosen, which a USB dongle does by being pulled out.
            self.notifications.status("bluetooth: no adapter");
            return;
        };
        let outcome = match choice {
            Choice::PowerOn => self
                .machine
                .bluetooth()
                .power_on(&adapter)
                .map(|()| format!("{} on", adapter.name)),
            Choice::PowerOff => self
                .machine
                .bluetooth()
                .power_off(&adapter)
                .map(|()| format!("{} off", adapter.name)),
            Choice::Block => self
                .machine
                .bluetooth()
                .set_blocked(&adapter, true)
                .map(|()| format!("{} blocked", adapter.name)),
            Choice::Unblock => self
                .machine
                .bluetooth()
                .set_blocked(&adapter, false)
                .map(|()| format!("{} unblocked", adapter.name)),
            Choice::Scan => self.start_scan(adapter.clone()),
            Choice::Nothing => return,
        };
        match outcome {
            Ok(said) => self.notifications.status(format!("bluetooth: {said}")),
            Err(why) => self.notifications.status(format!("bluetooth: {why}")),
        }
        // The status bar is showing a reading taken up to a second ago, and
        // the thing it is a reading of has just been changed by hand. Asking
        // now is what stops the bar disagreeing with the menu in front of it.
        self.machine.refresh(Instant::now());
        self.open_bluetooth();
    }

    /// Start an inquiry on the thread that is not this one.
    ///
    /// The refusals in front of it — a scan already running, an adapter that
    /// is down or blocked — are answered here rather than by the thread,
    /// because an error that takes eight seconds to arrive reads as a failure
    /// of the radio rather than of the request.
    fn start_scan(&mut self, adapter: Adapter) -> Result<String, tos_system::bluetooth::Error> {
        if self.bluetooth.is_scanning() {
            return Ok("already scanning".to_string());
        }
        if adapter.is_blocked() {
            return Err(tos_system::bluetooth::Error::Blocked {
                hardware: adapter.is_hard_blocked(),
            });
        }
        if !adapter.powered {
            return Err(tos_system::bluetooth::Error::NotPowered(adapter.name));
        }
        let sysfs = self.machine.sysfs().clone();
        self.bluetooth
            .begin(Scan::spawn(sysfs, SystemControl, adapter, SCAN_SECONDS));
        Ok(format!("scanning for {SCAN_SECONDS} seconds"))
    }

    /// Take in whatever the inquiry thread has to say. True when it said
    /// anything, which is a frame.
    fn collect_scan(&mut self) -> bool {
        let Some(outcome) = self.bluetooth.finished() else {
            return false;
        };
        match outcome {
            Ok(found) => {
                self.bluetooth.set_found(found);
                let count = self.bluetooth.found().len();
                self.notifications.status(match count {
                    0 => "bluetooth: nothing answered".to_string(),
                    1 => "bluetooth: one device".to_string(),
                    n => format!("bluetooth: {n} devices"),
                });
            }
            Err(why) => self.notifications.status(format!("bluetooth: {why}")),
        }
        // A menu still on screen was drawn before any of this was known, and
        // the rows it is offering are the ones the next keystroke will be
        // answered against. `set_items` rather than reopening, so that a query
        // typed while the controller was listening survives the answer.
        if matches!(self.overlay, Some((OverlayKind::Bluetooth, _))) {
            let (adapter, connections) = self.adapter_now();
            let items = self.bluetooth.items(adapter.as_ref(), &connections);
            if let Some((_, overlay)) = &mut self.overlay {
                overlay.set_items(items);
            }
            self.needs_full_redraw = true;
        }
        true
    }

    // ---- rendering ------------------------------------------------------

    /// Advance the blink phase and any animations. Returns true when anything
    /// changed.
    pub fn tick(&mut self) -> bool {
        self.tick_at(Instant::now())
    }

    /// [`Compositor::tick`] against a time the caller names.
    ///
    /// Split out for the same reason [`Compositor::tick_clock`] takes one: the
    /// rules below are about how long something has been on screen, and a test
    /// that can only ask for the time now has to spend three real seconds to
    /// watch a notification not be retired. One reading serves the whole tick,
    /// including the blink phase, so everything the frame is told happened,
    /// happened at the same instant.
    fn tick_at(&mut self, now: Instant) -> bool {
        let mut changed = false;
        // What the lock itself has to be repainted for, which is the only
        // thing that can reach the display while one is up. Everything else
        // this function finds — an animation, a reading that moved, a minute
        // turning over, a lease landing — belongs to a screen that is behind
        // the lock and is not being drawn, so it accumulates into `changed`
        // and is dropped at the end (#164).
        let mut lock_changed = false;
        // Animated images move on their own clock. The compositor owns that
        // clock and hands the time to each terminal, which keeps the terminal
        // model free of time of its own.
        for pane in self.panes.values_mut() {
            if pane.terminal.advance_animations(now) {
                changed = true;
            }
        }
        // The blink phase stands still while the screen is dark, for the
        // reason the queue below does: a cursor nobody can see does not need
        // to be somewhere in particular, and flipping it would repaint the
        // whole session behind the blank twice a second.
        //
        // And it stands still behind a lock, which is the same rule arriving
        // from the other side (#164). A lock draws its own caret and draws it
        // solid — `LockScreen::draw_field` has never been handed this phase —
        // so flipping it there was a change to nothing, and `render_locked`
        // repaints every pixel of the display for any change at all: a whole
        // screen, twice a second, for as long as a machine sat at a login
        // screen, to put back the image that was already on it.
        //
        // What does move on that screen is the wait after a wrong password
        // counting itself down, and nothing else in tOS would ever ask for
        // the frame that turns "try again in 12s" into 11. So the metronome
        // is kept and only what it means changes.
        if !self.blanked && now.saturating_duration_since(self.last_blink) >= BLINK_INTERVAL {
            self.last_blink = now;
            match self.lock.as_ref().map(|lock| lock.wait_left(now).is_some()) {
                Some(counting_down) => lock_changed |= counting_down,
                None => {
                    self.blink_visible = !self.blink_visible;
                    changed = true;
                }
            }
        }
        // A notification only spends its time on screen while it is on screen:
        // the leader indicator has the slot while the leader is armed, and one
        // shown there instead would be one nobody read.
        // A locked screen draws no status bar and no banner, so a message
        // that spent its time there would have spent it unread. The queue
        // stands still until the session comes back, which is the same reason
        // the leader indicator holds it: time on screen means on screen. A
        // blanked screen is the same case again — there is nothing on it, and
        // it does not matter whose decision that was.
        // Copy mode is the fourth, and it is the leader case exactly:
        // [`Compositor::status_message`] hands it the same slot ahead of the
        // queue, so a message raised while it is up is not drawn either — and
        // copy mode is a mode somebody stays in, walking a selection across a
        // screen, so three seconds behind it is not a near miss. The rule is
        // about the slot rather than about any one thing that takes it, so
        // anything new that claims the slot belongs on this list too.
        if self.lock.is_none()
            && !self.blanked
            && !self.keymap.is_pending()
            && self.copy.is_none()
            && self.notifications.advance(now)
        {
            // A banner is over the panes, and the cells it covered are only
            // repainted on damage they have not got. Retiring it has to
            // uncover them.
            self.needs_full_redraw |= self.notification_is_a_banner();
            changed = true;
        }
        // The machine moves without anybody touching the session: a battery
        // drains, a charger comes out, a link goes down. Nothing in the panes
        // is damaged by any of it, so this poll is the only thing that would
        // ever ask for the frame those changes belong on.
        if self.machine.poll(now, self.blanked) {
            changed = true;
        }
        // An inquiry is the one thing in tOS that runs off this thread, and
        // this is where its answer comes back on to it. Polled rather than
        // waited on: the loop is here every tenth of a second anyway, and a
        // scan that lands while the screen is blanked can wait for the
        // keystroke that lights it, since nobody is reading it in the dark.
        if self.collect_scan() {
            changed = true;
        }
        // And the clock moves without the machine moving either. The poll
        // above is what wakes the loop for it — its deadline is folded into
        // the wait in `frame_timeout_ms_at`, so the loop is up within a tenth
        // of a second of every second — but a minute turning over is not a
        // change in any reading, so it would report nothing and the bar would
        // go on showing the old minute until somebody typed.
        if self.tick_clock(unix_now()) {
            changed = true;
        }
        // Not folded into the poll above: a DHCP answer is something somebody
        // asked for and is waiting on, so it is collected even while the
        // screen is dark rather than left in the channel until it is woken.
        if self.collect_address() {
            changed = true;
        }
        // The scan under an open wireless menu, and the join waiting on a
        // handshake; both asked about here for the reason the DHCP answer
        // above is collected here, which is that the loop is awake anyway.
        // Before `autoconfigure`, so that a join which has just completed is a
        // radio with carrier by the time the policy looks at the links, and
        // its address is asked for on the same pass rather than a second
        // later.
        if self.poll_wireless(now) {
            changed = true;
        }
        // And this is the same thing with nobody waiting on it: the links a
        // person would have brought up by hand, brought up because nobody is
        // there to do it. After `collect_address` rather than before it, so a
        // lease that has just landed frees the client for the next link on
        // the pass it lands on rather than a second later.
        if self.autoconfigure(now) {
            changed = true;
        }
        // And none of it is on the screen while a lock is up. The work above
        // is all still done — the machine goes on reading itself, a link
        // brought up by nobody goes on being brought up (#124), a scan goes
        // on being collected — because a locked machine is still a machine.
        // What changes is only whether any of it is a reason to paint, and
        // over a lock it is not: no bar is drawn, the queue stands still, and
        // the panes are behind a screen that covers them. Reporting it drew
        // the whole display, twice a second, for something nobody could see.
        if self.lock.is_some() {
            return lock_changed;
        }
        changed
    }

    /// Whether anything has changed since the last frame.
    ///
    /// A pane holding a synchronized update open is not asked. `render_frame`
    /// skips it and deliberately leaves its damage standing, so that the update
    /// is not lost — which means the damage is not an answer to "is there a
    /// frame to draw" but to "is there one owed once the program lets go".
    /// Counting it asked for a frame that could not draw a single row of that
    /// pane, and DECSET 2026 has no timeout anywhere in tOS, so that was not
    /// one wasted frame but one per pass of the loop for as long as the program
    /// kept the update open. A full redraw draws the pane anyway, which is why
    /// that is still asked first.
    pub fn needs_render(&self) -> bool {
        self.needs_full_redraw
            || self.pointer_moved_since_it_was_drawn()
            || self.panes.values().any(|pane| pane.wants_frame())
    }

    /// Where the arrow belongs this frame, or `None` for no arrow at all.
    ///
    /// A lock is the only thing that takes it away other than the pointer's
    /// own state: a locked screen shows nothing of the session, and an arrow
    /// left on top of the password box would be the one thing on screen still
    /// tracking a hand that has not proved whose it is. `locked_input` drops
    /// pointer events there anyway, so it cannot move while it is gone.
    ///
    /// A login screen is not that (#122). Nothing is behind it and nobody has
    /// walked away from it, and what a motion would give away — an `(x, y)`
    /// over a panel showing nothing but a password box — is nothing.
    fn pointer_rect(&self) -> Option<PixelRect> {
        if self.lock.is_some() && !self.lock_shows_the_pointer() {
            return None;
        }
        let rect = self.pointer.rect(self.cell_size())?;
        // Clipped to the panel so that the remembered rectangle is the one
        // that was actually painted. `intersect` keeps the corner and only
        // shrinks the far edges, so the hotspot — and therefore the shape of
        // what gets drawn — is untouched by this.
        let clipped = rect.intersect(&PixelRect::new(0, 0, self.size.0, self.size.1));
        (!clipped.is_empty()).then_some(clipped)
    }

    /// Whether the arrow is somewhere other than where it was last painted.
    ///
    /// This is what makes a bare motion produce a frame. There is no
    /// screen-space damage to mark — `Damage` belongs to a pane's grid and the
    /// pointer is in neither — and a `moved` flag set by the motion would be
    /// one more thing to remember to clear, with a stuck one costing a frame
    /// per poll for the rest of the session. Comparing wanted against painted
    /// answers the question and retires itself: after a frame the two agree,
    /// so a hand that has stopped moving stops asking.
    fn pointer_moved_since_it_was_drawn(&self) -> bool {
        self.pointer_rect() != self.pointer.painted()
    }

    /// Whether a notification on the queue is drawn as a banner over the panes
    /// rather than in a slot on the status bar.
    ///
    /// Two places need this and they have to be the same question: the one
    /// that draws the banner, and the one that asks for the repaint retiring
    /// it owes the cells underneath. They were once `!status_bar` and
    /// `!status_bar || !shows_message()`, and a bar whose layout leaves the
    /// `message` segment out fell into the gap — the banner was drawn over
    /// pane row 0, nothing damaged that row when its time was up, and it sat
    /// there until something unrelated forced a full redraw.
    fn notification_is_a_banner(&self) -> bool {
        !self.config.status_bar || !self.config.status.shows_message()
    }

    /// Paint a frame.
    pub fn render_frame(&mut self, surface: &mut Surface<'_>, retained: bool) {
        if self.lock.is_some() {
            self.render_locked(surface, retained);
            return;
        }
        let force = self.needs_full_redraw || !retained;
        let (cw, ch) = self.cell_size();
        let area = self.grid_area();
        let focus = self.session.focus();
        let geometry = self.session.active().geometry(area);

        // The preedit is in nobody's grid, so nothing marks the rows it
        // covered as needing another look: `render()` skips a row that is not
        // dirty, and the frame after a commit would leave the committed
        // glyphs on screen twice. Marking what the IME painted last frame is
        // the same trick the graphics code uses to repaint only the rows a
        // moving image covers. Deliberately not `needs_full_redraw`, which is
        // what an overlay does: an overlay opens once, and a full screen
        // repaint per keystroke is the exact cost the retained path exists to
        // avoid.
        for pane in self.panes.values_mut() {
            if let Some((from, to)) = pane.ime.take_painted() {
                pane.terminal.damage_mut().mark_range(from, to);
            }
        }

        // And the same trick for the arrow, with one wrinkle the preedit does
        // not have: the preedit is always over the pane it is being typed
        // into, and the pointer is over whatever it is pointing at. Working
        // out what that was is `uncover_pointer` below.
        let pointer = self.pointer_rect();
        let uncover = self.pointer.painted().filter(|was| Some(*was) != pointer);
        let mut repaint_chrome = false;
        if !force {
            if let Some(was) = uncover {
                repaint_chrome = self.uncover_pointer(was, &geometry);
            }
        }

        // Which panes asked not to be drawn mid-update. Worked out before the
        // pane loop rather than inside it because the uncovering above has to
        // know as well, and two copies of the question is two answers waiting
        // to disagree.
        let skipped: Vec<PaneId> = if force {
            Vec::new()
        } else {
            geometry
                .iter()
                .filter(|(id, _)| {
                    self.panes
                        .get(id)
                        .is_some_and(|pane| pane.terminal.modes.synchronized_output)
                })
                .map(|(id, _)| *id)
                .collect()
        };

        if force {
            surface.clear(self.chrome.background);
        } else if let Some(was) = uncover.filter(|_| repaint_chrome) {
            // Nothing owns these pixels, so nothing is going to repaint them
            // and the arrow would stay where it was. Painting the background
            // back is safe over the panes this rectangle also touches, because
            // it happens before they draw and their damaged rows cover the
            // whole of the part that overlaps — but only over the panes that
            // are going to draw. A pane holding a synchronized update open is
            // not, and DECSET 2026 has no timeout, so background painted across
            // it is a hole in the picture for as long as the program keeps the
            // update open rather than for the single frame this trick is costed
            // at. Its rows are marked and it repaints them the moment it comes
            // back; until then the old arrow sits on it, which is the whole of
            // what "the previous frame stays on screen" already means.
            for piece in self.outside_the_skipped_panes(was, &geometry, &skipped) {
                surface.fill(piece, self.chrome.background);
            }
        }

        let mut drawn: Vec<PaneId> = Vec::with_capacity(geometry.len());
        for (id, rect) in &geometry {
            // Mutable because the pane owns its texture cache, which the
            // renderer fills in as it draws.
            let Some(pane) = self.panes.get_mut(id) else {
                continue;
            };
            // A pane that is synchronising its output asked not to be drawn
            // mid-update, so the previous frame stays on screen.
            if skipped.contains(id) {
                continue;
            }
            drawn.push(*id);
            let pixel_rect = PixelRect::new(
                (rect.x * cw) as i32,
                (rect.y * ch) as i32,
                rect.width * cw,
                rect.height * ch,
            );
            let options = RenderOptions {
                blink_visible: self.blink_visible,
                focused: *id == focus,
                draw_cursor: true,
                selection: pane.display_selection(),
                selection_background: self.chrome.selection(),
                copy_cursor: self
                    .copy
                    .as_ref()
                    .filter(|(copying, _)| copying == id)
                    .and_then(|(_, copy)| copy.display_cursor(pane.terminal.grid())),
                // The chrome's foreground rather than the accent the
                // selection is painted in: the copy cursor spends most of its
                // life sitting on one end of that highlight, and an outline
                // in the colour of the thing under it is no outline at all.
                copy_cursor_color: self.chrome.foreground,
                force,
                inactive_fade: self.config.inactive_fade,
            };
            render(
                surface,
                pixel_rect,
                &pane.terminal,
                &mut self.fonts,
                &mut pane.textures,
                &options,
            );
        }

        if force || repaint_chrome {
            let focused_rect = geometry
                .iter()
                .find(|(id, _)| *id == focus)
                .map(|(_, rect)| *rect);
            for (axis, divider) in self.session.active().dividers(area) {
                // Only draw dividers that the zoom state leaves visible.
                if self.session.active().zoomed().is_some() {
                    break;
                }
                let touches_focus = focused_rect
                    .map(|rect| {
                        divider.x <= rect.right()
                            && divider.right() >= rect.x
                            && divider.y <= rect.bottom()
                            && divider.bottom() >= rect.y
                    })
                    .unwrap_or(false);
                let color = if touches_focus {
                    self.chrome.divider_focused
                } else {
                    self.chrome.divider
                };
                chrome::draw_divider(
                    surface,
                    &mut self.fonts,
                    PixelRect::new(
                        (divider.x * cw) as i32,
                        (divider.y * ch) as i32,
                        divider.width * cw,
                        divider.height * ch,
                    ),
                    axis,
                    color,
                    self.chrome.background,
                );
            }
        }

        if self.config.status_bar {
            self.draw_status(surface, area, ch);
        }
        // With no bar there is nowhere for a message to live, and going quiet
        // is the one thing it must not do: this used to be why
        // `--no-status-bar` made a failed split look like a dead key. A bar
        // whose layout leaves the `message` segment out is the same case
        // arrived at a different way, and gets the same banner rather than a
        // configuration that silently swallows every failure.
        if self.notification_is_a_banner() {
            if let Some(text) = self.notifications.status_line() {
                let over = PixelRect::new(0, 0, area.width * cw, ch);
                notify::draw_banner(surface, &mut self.fonts, over, &self.chrome, &text);
            }
        }

        // Last, and over everything: the overlay is modal, and the panes below
        // it have already painted whatever they wanted to this frame.
        //
        // The area is taken from `overlay_area` rather than worked out again
        // here, because the mouse asks the same question to decide which row
        // a click landed on: two copies of this arithmetic would be a click
        // that lands one row off the row it was aimed at, and nothing would
        // catch it until somebody resized a pane.
        let over = self.overlay_area();
        if let Some((_, overlay)) = &mut self.overlay {
            overlay.draw(surface, &mut self.fonts, over, &self.chrome);
        }

        // And the preedit over the pane it is being typed into. Never open at
        // the same time as an overlay, so the order between the two is not a
        // decision anybody has to make.
        self.draw_ime(surface, &geometry);

        // The arrow last of all, over the overlay and over the preedit. It is
        // the one thing on screen that is not part of the session: whatever it
        // is pointing at, it has to be on top of, or it is pointing from
        // underneath.
        if let Some(rect) = pointer {
            // The corner, not the rectangle: `pointer_rect` clipped it to the
            // panel so that what is remembered as painted is what was painted,
            // and the arrow is sized by the cell rather than by whatever the
            // clip left of it.
            pointer::draw(
                surface,
                (rect.x, rect.y),
                (cw, ch),
                self.chrome.foreground,
                self.chrome.background,
            );
        }
        self.pointer.set_painted(pointer);

        for (id, pane) in self.panes.iter_mut() {
            // A pane skipped for synchronized output was not painted, so its
            // damage still describes work outstanding; clearing it here would
            // lose the whole update.
            let skipped = !drawn.contains(id);
            if !skipped {
                pane.terminal.clear_damage();
            }
        }
        self.needs_full_redraw = false;
    }

    /// The pieces of `was` that no pane skipped this frame is sitting under —
    /// what the frame is free to paint the background over.
    ///
    /// Rectangles rather than a mask because there are at most a handful of
    /// them and the only thing that will ever be asked to draw them is
    /// `Surface::fill`. Only the skipped panes are cut out, not every pane: the
    /// background over a pane that is about to draw is painted over again by
    /// the rows the uncovering just marked, and leaving that alone keeps this to
    /// one rectangle in the case that is not about synchronized output at all.
    fn outside_the_skipped_panes(
        &self,
        was: PixelRect,
        geometry: &[(PaneId, Rect)],
        skipped: &[PaneId],
    ) -> Vec<PixelRect> {
        /// One rectangle with another cut out of it: a band above, a band
        /// below, and the two sides of what is left between them.
        fn cut_out(rect: PixelRect, hole: PixelRect) -> Vec<PixelRect> {
            let overlap = rect.intersect(&hole);
            if overlap.is_empty() {
                return vec![rect];
            }
            let mut pieces = Vec::new();
            if overlap.y > rect.y {
                let height = (overlap.y - rect.y) as u32;
                pieces.push(PixelRect::new(rect.x, rect.y, rect.width, height));
            }
            if overlap.bottom() < rect.bottom() {
                let height = (rect.bottom() - overlap.bottom()) as u32;
                pieces.push(PixelRect::new(rect.x, overlap.bottom(), rect.width, height));
            }
            if overlap.x > rect.x {
                let width = (overlap.x - rect.x) as u32;
                pieces.push(PixelRect::new(rect.x, overlap.y, width, overlap.height));
            }
            if overlap.right() < rect.right() {
                let width = (rect.right() - overlap.right()) as u32;
                pieces.push(PixelRect::new(
                    overlap.right(),
                    overlap.y,
                    width,
                    overlap.height,
                ));
            }
            pieces
        }

        let (cw, ch) = self.cell_size();
        let mut pieces = vec![was];
        for rect in geometry
            .iter()
            .filter(|(id, _)| skipped.contains(id))
            .map(|(_, rect)| rect)
        {
            let hole = PixelRect::new(
                (rect.x * cw) as i32,
                (rect.y * ch) as i32,
                rect.width * cw,
                rect.height * ch,
            );
            pieces = pieces
                .iter()
                .flat_map(|piece| cut_out(*piece, hole))
                .collect();
        }
        pieces
    }

    /// Mark what the arrow covered last frame as needing another look, and say
    /// whether any of what it covered was the compositor's own chrome.
    ///
    /// Rows of panes, because rows of panes is the whole of the damage model:
    /// `tos_term::Damage` is per pane and row granular, and tOS has no
    /// screen-space dirty rectangle anywhere. Where the arrow sat over a pane
    /// that is enough — the pane repaints those rows and the old arrow is gone
    /// with them — so the rectangle is mapped back through
    /// `session.active().geometry(area)` to find which pane, and which of its
    /// own rows, each cell of it was.
    ///
    /// Where it did not sit over a pane there is nobody to ask, and that is
    /// what the return value is for. Deliberately not `needs_full_redraw`,
    /// which is the obvious way to write this and is wrong for the same reason
    /// the retained path exists at all: a divider is one cell wide, the arrow
    /// is about one cell wide, and dragging a divider is a gesture that sits on
    /// one for as long as the resize takes — so a full redraw here would be a
    /// whole panel repainted per report a mouse makes, for the entire drag.
    /// What is outside every pane is exactly three things: the dividers, the
    /// status bar and the strip at the right and bottom edges where whole cells
    /// do not quite divide the panel. The bar repaints itself on every frame
    /// regardless, so it needs nothing; the caller handles the other two by
    /// painting the background back over the rectangle and running the divider
    /// pass again.
    fn uncover_pointer(&mut self, was: PixelRect, geometry: &[(PaneId, Rect)]) -> bool {
        let (cw, ch) = self.cell_size();
        let first_col = was.x.max(0) as u32 / cw;
        let last_col = (was.right() - 1).max(0) as u32 / cw;
        let first_row = was.y.max(0) as u32 / ch;
        let last_row = (was.bottom() - 1).max(0) as u32 / ch;
        let wanted = (last_col - first_col + 1) * (last_row - first_row + 1);

        // Panes tile the grid and never overlap, so the cells each one accounts
        // for can simply be added up and compared with the cells the arrow
        // covered. Anything left over was not a pane.
        let mut accounted = 0;
        for (id, rect) in geometry {
            let from_col = first_col.max(rect.x);
            let to_col = last_col.min(rect.right().saturating_sub(1));
            let from_row = first_row.max(rect.y);
            let to_row = last_row.min(rect.bottom().saturating_sub(1));
            if from_col > to_col || from_row > to_row {
                continue;
            }
            accounted += (to_col - from_col + 1) * (to_row - from_row + 1);
            if let Some(pane) = self.panes.get_mut(id) {
                // Into the pane's own row numbering, and one past the last
                // because `mark_range` is half open.
                let from = (from_row - rect.y) as usize;
                let to = (to_row - rect.y) as usize + 1;
                pane.terminal.damage_mut().mark_range(from, to);
            }
        }

        if self.config.status_bar {
            let bar_row = self.grid_area().height;
            if (first_row..=last_row).contains(&bar_row) {
                accounted += last_col - first_col + 1;
            }
        }

        accounted < wanted
    }

    /// Paint a locked frame: the lock, and nothing else at all.
    ///
    /// The panes, the dividers, the status bar, the notification banner and
    /// any open menu are all skipped rather than painted over. Painting over
    /// is not erasing: a frame that is not a full redraw only repaints the
    /// cells a pane marked as damaged, so a box drawn on top of a session
    /// leaves every undamaged cell of that session exactly where it was, and
    /// the status bar underneath it goes on saying what the panes are called.
    ///
    /// The clear happens on every locked frame and not only the first, because
    /// a display with two buffers hands out the other one next time and a
    /// clear that ran once cleared one of them.
    fn render_locked(&mut self, surface: &mut Surface<'_>, retained: bool) {
        // The locked screen is retained like every other one (#164). It used
        // to erase the display and draw the box again for every frame,
        // including the ones where nothing had changed, which is how a screen
        // with nothing moving on it came to repaint 384,000 pixels twice a
        // second — and, on a driver whose flips are not reported and whose
        // every present is therefore a mode set, to blink twice a second with
        // it.
        //
        // Three things still need the whole screen:
        //
        // - **`needs_full_redraw`**, which is the lock arriving over a
        //   session, a resize, a screen coming back from blanked, and a VT
        //   switch back. Each is a surface holding something that is not this
        //   screen.
        // - **A surface that is not retained**, the same condition the
        //   session path takes `force` from.
        // - **An arrow that has moved.** What it uncovered is not always the
        //   background: over a login screen it can be the picture, which
        //   composites with alpha and so cannot be painted a second time to
        //   cover the hole. Whoever is moving a mouse over a lock is not the
        //   case this is protecting, which is a machine nobody is at.
        let full = self.needs_full_redraw || !retained || self.pointer_moved_since_it_was_drawn();
        if full {
            surface.clear(self.chrome.background);
        }
        let area = PixelRect::new(0, 0, self.size.0, self.size.1);
        if let Some(lock) = &self.lock {
            lock.draw(
                surface,
                &mut self.fonts,
                area,
                &self.chrome,
                Backdrop {
                    splash: self.picture.as_ref(),
                    repaint: if full {
                        Repaint::Everything
                    } else {
                        Repaint::TheBox
                    },
                },
                Instant::now(),
            );
        }

        // The clear took the arrow with it, so what is remembered as painted
        // has to be what this frame painted. Over a lock that is nothing:
        // `pointer_rect` says there is no arrow and `locked_input` drops the
        // events that would move one, and remembering a rectangle that could
        // never be matched would leave `needs_render` true for every pass of a
        // locked session — a frame per poll for as long as nobody is there.
        //
        // Over a login the arrow is drawn, and over the box rather than under
        // it, for the reason the unlocked path draws it last: whatever it is
        // pointing at, it has to be on top of. The rectangle is remembered for
        // the same reason as above, and settles the same way — a pointer that
        // is not moving matches what was painted and asks for nothing.
        let pointer = self.pointer_rect();
        if let Some(rect) = pointer {
            let (cw, ch) = self.cell_size();
            pointer::draw(
                surface,
                (rect.x, rect.y),
                (cw, ch),
                self.chrome.foreground,
                self.chrome.background,
            );
        }
        self.pointer.set_painted(pointer);
        // The panes are still running and still marking damage nobody is
        // drawing. Dropping it is what lets the loop idle instead of finding
        // work outstanding on every pass, and nothing is lost by it: unlocking
        // asks for a full redraw, which repaints every cell whatever the
        // damage says.
        for pane in self.panes.values_mut() {
            pane.terminal.clear_damage();
        }
        self.needs_full_redraw = false;
    }

    /// The bar as it will be drawn this frame: every configured segment asked
    /// for its pieces, then laid out.
    ///
    /// Built rather than cached because everything on it is derived from state
    /// that moves — the focused pane's title, the notification queue, the last
    /// reading — and a cache would need invalidating from each of those. It is
    /// a handful of `String`s once per frame, against a frame that touches
    /// every pixel of the display.
    ///
    /// Also what a click consults, which is the point of it being a value:
    /// where the third workspace starts is worked out once, and the drawing
    /// and the routing read the same answer.
    fn status_bar(&self) -> Bar {
        let pieces = |segments: &[Segment]| -> Vec<Vec<Piece>> {
            segments
                .iter()
                .map(|segment| self.segment_pieces(*segment))
                .collect()
        };
        Bar::lay_out(
            &pieces(&self.config.status.left),
            &pieces(&self.config.status.right),
            self.grid_area().width,
        )
    }

    /// What one segment has to say, which is sometimes nothing.
    ///
    /// An empty vector is how absence is said, and it is said for two
    /// different reasons that ought to look the same on the bar: a machine
    /// with no battery has no charge to report, and a session with nothing
    /// queued has no message to show. Neither is worth a slot saying so.
    fn segment_pieces(&self, segment: Segment) -> Vec<Piece> {
        let reading = self.machine.reading();
        let one = |text: String| vec![Piece::new(text)];
        match segment {
            Segment::Workspaces => {
                let active = self.session.active_index();
                self.session
                    .workspaces()
                    .iter()
                    .enumerate()
                    .map(|(index, workspace)| {
                        // Numbered from one, the way the digit bindings are, so
                        // that clicking the third workspace and pressing
                        // super+3 reach the same call with the same argument.
                        Piece::new(workspace.name.clone())
                            .active(index == active)
                            .clicking(Hit::Workspace(index + 1))
                    })
                    .collect()
            }
            Segment::Panes => {
                let focus = self.session.focus();
                self.session
                    .active()
                    .panes()
                    .iter()
                    .enumerate()
                    .filter_map(|(index, id)| {
                        let pane = self.panes.get(id)?;
                        Some(
                            Piece::new(chrome::pane_label(index, &pane.terminal, &pane.title))
                                .active(*id == focus)
                                .clicking(Hit::Pane(*id)),
                        )
                    })
                    .collect()
            }
            Segment::Title => match self.focused_label() {
                Some(label) => one(label),
                None => Vec::new(),
            },
            Segment::Layout => match self.session.active().arrangement() {
                // Splits is not announced. It is what every session starts in
                // and where most of them stay, so naming it would spend a slot
                // on a word that never changes; the segment appearing at all
                // is itself the news that the panes are somewhere other than
                // where the splits left them.
                Arrangement::Splits => Vec::new(),
                other => one(other.name().to_string()),
            },
            Segment::Message => match self.status_message() {
                // The elastic one: it is the only thing on the bar whose
                // length is not the compositor's own choice, and clicking it
                // opens the history, because the message that has just gone
                // past is the one somebody wants back.
                Some(text) => vec![Piece::new(text).elastic().clicking(Hit::Notifications)],
                None => Vec::new(),
            },
            // What [`Compositor::tick_clock`] last worked out, rather than
            // the time now. One reading per tick, drawn by every frame in
            // between, which is also what makes the repaint trigger and the
            // thing on screen provably the same string.
            Segment::Clock => one(self.clock_text.clone()),
            Segment::Battery => match reading.power.as_ref().and_then(status::battery_text) {
                Some(text) => one(text),
                None => Vec::new(),
            },
            Segment::Network => match &reading.link {
                Some(link) => one(status::network_text(link)),
                None => Vec::new(),
            },
            Segment::Volume => match &reading.volume {
                Some(volume) => one(status::volume_text(volume)),
                None => Vec::new(),
            },
            Segment::Bluetooth => match &reading.adapter {
                Some(adapter) => one(status::bluetooth_text(adapter)),
                None => Vec::new(),
            },
        }
    }

    /// The focused pane's label, and how far back it is looking.
    fn focused_label(&self) -> Option<String> {
        let focus = self.session.focus();
        let panes = self.session.active().panes();
        let index = panes.iter().position(|id| *id == focus).unwrap_or(0);
        let pane = self.panes.get(&focus)?;
        let label = chrome::pane_label(index, &pane.terminal, &pane.title);
        match pane.terminal.display_offset() {
            0 => Some(label),
            scrolled => Some(format!("{label}  [scrollback {scrolled}]")),
        }
    }

    /// The leader indicator, or else whatever is at the front of the queue.
    ///
    /// The leader comes first because it is the state of the keyboard right
    /// now and lasts only until the next key, where a notification has three
    /// seconds it can just as well spend later. Arming the leader does not
    /// cost the queue its turn: [`Compositor::tick`] stops the clock on the
    /// message while the indicator has the slot.
    fn status_message(&self) -> Option<String> {
        if self.keymap.is_pending() {
            return Some("leader".to_string());
        }
        // Copy mode belongs beside the leader indicator rather than in the
        // queue behind it: both say what the keyboard is doing at this
        // instant, and a notification allowed to cover either would be three
        // seconds in which the sheet's account of the keys is wrong. And, like
        // the leader, taking the slot costs the queue nothing: `tick` holds
        // its clock for exactly as long as this returns something else.
        if let Some(copy) = self.copy_mode() {
            return Some(copy.status().to_string());
        }
        self.notifications.status_line()
    }

    /// The row the bar sits on, when there is one.
    ///
    /// `None` when the bar is off, and also when the display is too short for
    /// [`Compositor::grid_area`] to have given up a row for it — otherwise a
    /// click on the last row of a two row display would switch workspaces
    /// instead of reaching the pane that is drawn there.
    fn status_row(&self) -> Option<u32> {
        let (_, ch) = self.cell_size();
        let rows = (self.size.1 / ch).max(1);
        let area = self.grid_area();
        (self.config.status_bar && area.height < rows).then_some(area.height)
    }

    /// A left press on the status bar.
    ///
    /// The bar is outside every pane's geometry — that is what `grid_area`
    /// subtracting a row means — so before this a press here matched nothing
    /// and was dropped, which left a strip of workspaces that looks clickable
    /// and was not.
    fn click_status(&mut self, col: u32) -> bool {
        match self.status_bar().hit(col) {
            Some(Hit::Workspace(number)) => {
                // `select_workspace` says it succeeded when it was asked for
                // the one already active, which is the right answer for a
                // binding and the wrong one here: a click on the workspace you
                // are in should not cost a full redraw of the session.
                if self.session.active_index() + 1 == number {
                    return false;
                }
                let changed = self.session.select_workspace(number);
                if changed {
                    self.sync_layout();
                    self.needs_full_redraw = true;
                }
                changed
            }
            Some(Hit::Pane(id)) => {
                if self.session.focus() == id {
                    return false;
                }
                let previous = self.session.focus();
                self.session.set_focus(id);
                // The bar lists every pane in the workspace, including the
                // ones a zoom is hiding, so this is the one focus change that
                // can arrive for a pane nobody can see — and `set_focus`
                // answers that by leaving the zoom, which hands every pane in
                // the workspace a different rectangle. A pane only hears about
                // its rectangle here, so without this the next frame would
                // draw the splits back while the pane that had been zoomed,
                // and the program inside it, were still sized to the whole
                // workspace.
                self.sync_layout();
                self.clear_selection(previous);
                self.needs_full_redraw = true;
                true
            }
            Some(Hit::Notifications) => self.perform(Action::ShowNotifications),
            None => false,
        }
    }

    /// Let the clock catch up, and say whether the bar has to be repainted.
    ///
    /// Separate from [`Compositor::tick`], and taking the time rather than
    /// reading it, so that a test can watch a minute turn over without having
    /// to wait one out.
    ///
    /// A session with no clock on its bar does none of this and asks for no
    /// frames on account of one — which matters, because the alternative is a
    /// machine that wakes up, paints every pixel and goes back to sleep once a
    /// minute for the rest of its life in order to redraw nothing.
    fn tick_clock(&mut self, unix: i64) -> bool {
        if !self.config.status.shows_clock() {
            return false;
        }
        let text = self.clock.text(unix);
        if text == self.clock_text {
            return false;
        }
        self.clock_text = text;
        // The clock is kept current even while nobody can see it, because it
        // costs one formatted string and it means the bar is right on the
        // frame it comes back rather than on the one after. Asking for that
        // frame is the part that is skipped: there is nothing on a dark or
        // hidden bar for a new minute to change.
        !self.blanked && self.config.status_bar
    }

    fn draw_status(&mut self, surface: &mut Surface<'_>, area: Rect, cell_height: u32) {
        let bar = self.status_bar();
        let colors = self.chrome.bar();
        let rect = PixelRect::new(
            0,
            (area.height * cell_height) as i32,
            self.size.0,
            cell_height,
        );
        bar.draw(surface, &mut self.fonts, rect, &colors);
    }

    /// Feed bytes straight into the focused pane's terminal, bypassing the
    /// PTY. Used by `--preload` so a screenshot can show known content.
    pub fn inject(&mut self, bytes: &[u8]) {
        let focus = self.session.focus();
        if let Some(pane) = self.panes.get_mut(&focus) {
            pane.terminal.advance(bytes);
        }
    }

    /// How long the loop may wait for something to happen. An animation with a
    /// frame due sooner than the idle timeout pulls the wait in, so playback
    /// keeps its own pace instead of the poll timer's, and the idle deadlines
    /// pull it in the same way, so that a session a minute from locking locks
    /// on the minute rather than up to a tenth of a second later.
    fn frame_timeout_ms(&self) -> i32 {
        self.frame_timeout_ms_at(Instant::now())
    }

    fn frame_timeout_ms_at(&self, now: Instant) -> i32 {
        // The wait when nothing at all is due, which is longer while the
        // screen is dark: a blanked session has no blink phase to flip, no
        // notification counting down its time on screen, and no animation
        // frame anybody could see, so the only reason left to come back is a
        // deadline, and those are folded in below.
        let longest = if self.blanked {
            BLANKED_TIMEOUT_MS
        } else {
            IDLE_TIMEOUT_MS
        };
        let animation = self
            .panes
            .values()
            .filter_map(|pane| pane.terminal.next_animation_delay(now))
            .min()
            .filter(|_| !self.blanked);
        // The poll is a deadline like the others: folded into the wait rather
        // than given a timer, so a clock turns over on the second instead of
        // up to a tenth of one after it. Skipped while the screen is dark,
        // because a dark screen is not polled either.
        let poll = self.machine.next_poll(now).filter(|_| !self.blanked);
        let soonest = [animation, poll, self.next_idle_deadline(now)]
            .into_iter()
            .flatten()
            .min();
        match soonest {
            Some(delay) => delay.as_millis().clamp(1, longest as u128) as i32,
            None => longest,
        }
    }

    /// Run one iteration: poll, read, handle, and render if needed.
    pub fn run_once(
        &mut self,
        display: &mut dyn Display,
        input_fds: &[std::os::unix::io::RawFd],
        mut read_input: impl FnMut(std::os::unix::io::RawFd) -> Vec<InputEvent>,
    ) -> io::Result<()> {
        let mut fds = self.pty_fds();
        fds.extend_from_slice(input_fds);
        // With input still queued the loop has to come back promptly to retry
        // it, rather than waiting for something to read.
        let timeout = if self.pending_writes {
            WRITE_RETRY_TIMEOUT_MS
        } else {
            self.frame_timeout_ms()
        };
        self.pending_writes = false;
        let ready = tos_platform::tty::poll_readable(&fds, timeout)?;

        let mut dirty = false;
        for fd in &ready {
            if input_fds.contains(fd) {
                for event in read_input(*fd) {
                    dirty |= self.handle_input(event);
                }
            }
        }
        dirty |= self.pump_panes();
        dirty |= self.tick();
        // After the input, so that the event which arrived at a dark screen is
        // swallowed by the screen still being dark, and before the frame, so
        // that the frame this pass paints is the one a blanked display shows
        // when it comes back.
        dirty |= self.apply_idle(Instant::now(), display);

        if dirty || self.needs_render() {
            let retained = display.retains_contents();
            display.frame(&mut |surface| self.render_frame(surface, retained))?;
        }
        Ok(())
    }

    // ---- power ----------------------------------------------------------

    /// Act on a power action that has been asked for and, where it needed one,
    /// agreed to.
    ///
    /// Public because the menu is not the only way in: a test reaches it
    /// without driving two overlays, and a lid switch or a power button would
    /// arrive here too. It is also the one place that decides which of the
    /// three the session is expected to survive, which is the difference
    /// between the two fields it sets.
    pub fn request_power(&mut self, action: PowerAction) -> bool {
        match action {
            PowerAction::Suspend => {
                // Locked before the machine sleeps rather than after it wakes.
                // Somebody who suspends a laptop is shutting the lid and
                // walking away from it, and the session has to be behind the
                // password by the time anything can be on the screen again; a
                // lock applied on the way back is one that races whoever
                // pressed the key to wake it. This is `lock_on_idle` and not
                // `lock_session` because a machine with no password has no
                // lock to offer, and "cannot lock" is not an answer to
                // somebody who asked for a suspend.
                self.lock_on_idle();
                self.suspend_requested = true;
                true
            }
            ending => {
                self.shutdown = Some(ending);
                // Ending the loop is what gets the console and the display
                // handed back before [`Compositor::shut_down`] stops the
                // machine. The panes are not asked anything: there is nothing
                // to ask them with, which is what the confirmation said.
                self.running = false;
                true
            }
        }
    }

    /// Whether a suspend has been asked for since this was last called.
    ///
    /// Edge triggered, like [`tos_platform::take_switch_away`] and for the
    /// same reason: the loop acts on it in a place where the display and the
    /// input devices can safely be given up, and a flag that stayed set would
    /// put the machine to sleep again the moment it woke.
    pub fn take_suspend_request(&mut self) -> bool {
        std::mem::take(&mut self.suspend_requested)
    }

    /// How this session is ending, when it is ending by request. `None` for a
    /// session that stopped for any of the other reasons.
    pub fn shutdown_request(&self) -> Option<PowerAction> {
        self.shutdown
    }

    /// The machine is awake again.
    ///
    /// Everything the sleep invalidated, put back in one place: the loop has
    /// already reclaimed the screen and the keyboard by the time this is
    /// called, and this is the session's half of the same resume.
    pub fn resumed(&mut self, outcome: &power::Outcome) {
        // The CRTC has been set again from nothing, so there is no previous
        // frame on the screen for a partial repaint to build on — and the
        // damage that would have said which cells to repaint was collected
        // against a screen that no longer exists.
        self.needs_full_redraw = true;
        let now = Instant::now();
        // Where the caret is, is the first thing anyone looks for on a screen
        // they have just brought back. The same reason unblanking does it.
        self.blink_visible = true;
        self.last_blink = now;
        // Waking a machine is somebody being there, so the idle period starts
        // again from here. Nothing was missed while it slept: `Instant` is
        // `CLOCK_MONOTONIC`, which does not count time spent suspended, so the
        // deadlines stood still along with everything else.
        self.last_activity = now;
        self.idle_lock_done = false;
        self.blank_refused = false;
        // Read the machine now rather than within the second the poll would
        // take: a laptop that went to sleep on its charger and woke off it
        // would otherwise still be showing last night's battery.
        self.machine.refresh(now);
        // A suspend that did not happen, or a screen that came back wrong, is
        // a thing to say rather than a thing to end a session over — the same
        // rule the display that will not blank follows.
        for problem in &outcome.problems {
            self.notifications
                .status(format!("{}: {}", problem.step.what(), problem.message));
        }
    }

    /// Carry out the ending this session was given, if it was given one.
    ///
    /// Called by the loop after it has stopped and handed the console and the
    /// display back, which is the whole reason it is separate from asking for
    /// it: `reboot(2)` does not return on success, so anything that has to
    /// happen before the machine stops has to have happened before this call.
    /// An error means the syscall was refused — tOS without `CAP_SYS_BOOT`, a
    /// container — and by then there is no status bar left to say so on, which
    /// is why this is an error to print rather than a notification to queue.
    pub fn shut_down(&mut self) -> io::Result<()> {
        let Some(action) = self.shutdown.take() else {
            return Ok(());
        };
        // `request` is what puts the sync in front of it. There is no init
        // here to flush the filesystems on the way down, so an unsynced
        // poweroff loses whatever the panes were writing.
        tos_system::power::request(self.machine.power(), action)
    }
}

/// Seconds since the epoch, which is the only form of the time anything here
/// deals in: [`crate::clock`] turns it into a date, and nothing else needs it.
///
/// A clock the kernel has set before 1970 reads as the epoch rather than as an
/// error. There is no sensible thing for a status bar to do about a machine
/// whose battery-backed clock has failed, and refusing to draw one is not it.
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or(0)
}

/// The selectors OSC 52 defines: the clipboard, primary, secondary, select,
/// and the eight cut buffers. The selector arrives as a raw character, so
/// without this a pane could invent a new one per store and grow the clipboard
/// map for as long as it liked; with it the map holds at most thirteen
/// selections of [`MAX_CLIPBOARD_BYTES`] each.
fn is_clipboard_selector(selection: char) -> bool {
    matches!(selection, 'c' | 'p' | 'q' | 's' | '0'..='7')
}

/// Assemble the font stack from configuration and display characteristics.
fn build_fonts(config: &Config, size: (u32, u32), physical_mm: Option<(u32, u32)>) -> FontStack {
    let pixel_size = config
        .font_size
        .unwrap_or_else(|| tos_platform::suggested_font_size(size.0, size.1, physical_mm));

    let primary: Option<Box<dyn GlyphSource>> = load_ttf(config, pixel_size);

    let mut stack = match primary {
        Some(font) => FontStack::new(font),
        None => {
            let scale = config
                .bitmap_scale
                .map(BitmapFont::new)
                .unwrap_or_else(|| BitmapFont::for_display(size.0, size.1));
            FontStack::new(Box::new(scale))
        }
    };
    let cell = stack.metrics();
    for path in &config.font_fallback {
        // A fallback that will not load is worth saying so about: the person
        // named this file, unlike the ones found by searching.
        match tos_font::TtfFont::from_path(path, pixel_size) {
            Ok(font) => stack.push_fallback(Box::new(font.fit_wide_cell(cell))),
            Err(e) => eprintln!("tos: font fallback {}: {e}", path.display()),
        }
    }
    // Without kanji somewhere in the stack Japanese is a row of hollow boxes,
    // so a face is looked for even when nothing asked for one. The ISO's own
    // face is monospace and covers kana, so this usually finds nothing to do.
    if !stack.covers(tos_font::ttf::FULL_WIDTH_PROBE) {
        if let Some(font) = tos_font::TtfFont::system_cjk(pixel_size) {
            stack.push_fallback(Box::new(font.fit_wide_cell(cell)));
        }
    }
    // The built-in face is always available as a last resort, so a missing
    // glyph in the main font never leaves a hole.
    stack.push_fallback(Box::new(BitmapFont::for_display(size.0, size.1)));
    stack
}

fn load_ttf(config: &Config, pixel_size: f32) -> Option<Box<dyn GlyphSource>> {
    let font = match &config.font {
        Some(path) => tos_font::TtfFont::from_path(path, pixel_size).ok()?,
        None => tos_font::TtfFont::system(pixel_size)?,
    };
    Some(Box::new(font))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tos_input::KeyCode;

    fn compositor() -> Compositor {
        compositor_with(Config::default())
    }

    /// Build a compositor on top of a config, filling in the parts every test
    /// wants: a child that only sleeps, and the built-in bitmap font.
    fn compositor_with(config: Config) -> Compositor {
        let config = Config {
            command: Some(vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()]),
            bitmap_scale: Some(1),
            font: Some("/nonexistent-so-the-bitmap-font-is-used".into()),
            // A machine with no battery, no card, no link and no adapter, so
            // that what a test asserts about the session is not the
            // developer's laptop showing through — and so that nothing here
            // can reach a real sound card.
            system_root: "/nonexistent-so-this-machine-has-no-hardware".into(),
            ..config
        };
        Compositor::new(config, (640, 360), None).expect("compositor")
    }

    /// A font stack built from `config`, at a size the tests can reason about.
    fn fonts(config: Config) -> FontStack {
        build_fonts(&config, (640, 360), None)
    }

    /// Nothing to assert about CJK on a machine with no CJK face.
    fn a_cjk_face() -> Option<std::path::PathBuf> {
        tos_font::TtfFont::find_system_cjk_font()
    }

    #[test]
    fn kanji_are_found_without_being_configured() {
        if a_cjk_face().is_none() {
            return;
        }
        let fonts = fonts(Config::default());
        assert!(fonts.covers('漢'), "Japanese would render as boxes");
        assert!(fonts.covers('あ') && fonts.covers('ア'));
    }

    #[test]
    fn a_configured_fallback_supplies_the_glyphs() {
        let Some(path) = a_cjk_face() else { return };
        let fonts = fonts(Config {
            // A primary with no kanji in it, so only the fallback can answer.
            font: Some("/nonexistent-so-the-bitmap-font-is-used".into()),
            bitmap_scale: Some(1),
            font_fallback: vec![path],
            ..Config::default()
        });
        assert!(fonts.covers('漢'));
    }

    #[test]
    fn a_fallback_that_will_not_load_is_skipped() {
        let fonts = fonts(Config {
            font_fallback: vec!["/nonexistent.ttf".into()],
            bitmap_scale: Some(1),
            ..Config::default()
        });
        // ASCII still works, which is the whole point of not giving up here.
        assert!(fonts.covers('A'));
    }

    #[test]
    fn a_wide_glyph_fills_two_cells() {
        if a_cjk_face().is_none() {
            return;
        }
        let mut fonts = fonts(Config::default());
        let cell = fonts.metrics();
        let glyph = fonts.glyph('漢', tos_font::RasterStyle::REGULAR).clone();
        assert!(
            glyph.width > cell.cell_width,
            "a kanji narrower than two cells is the missing box"
        );
        assert!(
            glyph.left + glyph.width as i32 <= (cell.cell_width * 2) as i32,
            "a kanji wider than two cells would overwrite its neighbour"
        );
    }

    #[test]
    fn a_new_compositor_has_one_pane_with_a_process() {
        let compositor = compositor();
        assert_eq!(compositor.panes.len(), 1);
        assert_eq!(compositor.session.all_panes().len(), 1);
    }

    #[test]
    fn the_grid_leaves_room_for_the_status_bar() {
        let compositor = compositor();
        let (_, ch) = compositor.cell_size();
        let total_rows = 360 / ch;
        assert_eq!(compositor.grid_area().height, total_rows - 1);
    }

    #[test]
    fn splitting_starts_a_second_process() {
        let mut compositor = compositor();
        assert!(compositor.perform(Action::Split(Axis::Columns)));
        assert_eq!(compositor.panes.len(), 2);
        // Both panes are narrower than the whole screen.
        let area = compositor.grid_area();
        for (_, rect) in compositor.session.active().geometry(area) {
            assert!(rect.width < area.width);
        }
    }

    #[test]
    fn panes_are_told_their_new_size() {
        let mut compositor = compositor();
        let before = compositor
            .pane(compositor.session.focus())
            .unwrap()
            .terminal
            .cols();
        compositor.perform(Action::Split(Axis::Columns));
        let after = compositor
            .pane(compositor.session.focus())
            .unwrap()
            .terminal
            .cols();
        assert!(after < before, "{after} should be narrower than {before}");
    }

    #[test]
    fn the_layout_keys_rearrange_the_panes_and_tell_them_so() {
        // Three panes in a row. A column split with a row split inside it is
        // already exactly what `tall` derives, which is the two arithmetics
        // agreeing rather than the key doing nothing — but it makes for a
        // test that could not tell the two apart.
        let mut compositor = compositor();
        compositor.perform(Action::Split(Axis::Columns));
        compositor.perform(Action::Split(Axis::Columns));
        let area = compositor.grid_area();
        let before = compositor.session.active().geometry(area);

        assert!(compositor.perform(Action::NextLayout));
        assert_eq!(compositor.session.active().arrangement(), Arrangement::Tall);
        let after = compositor.session.active().geometry(area);
        assert_ne!(after, before, "the panes did not move");
        // A rearrangement that did not reach the terminals would leave three
        // programs drawing into the rectangles they used to have.
        for (id, rect) in &after {
            let pane = compositor.pane(*id).expect("a live pane");
            assert_eq!(pane.terminal.cols(), rect.width as usize);
            assert_eq!(pane.terminal.rows(), rect.height as usize);
        }

        // Backwards from `tall` is where forwards would have ended up.
        assert!(compositor.perform(Action::PreviousLayout));
        assert_eq!(
            compositor.session.active().arrangement(),
            Arrangement::Splits
        );
        assert_eq!(compositor.session.active().geometry(area), before);
    }

    #[test]
    fn a_divider_that_is_not_on_screen_refuses_to_move_and_says_which_layout_ate_it() {
        let mut compositor = compositor();
        compositor.perform(Action::Split(Axis::Columns));
        compositor.perform(Action::NextLayout);

        // Cycling itself says nothing, so the first thing in the queue is the
        // refusal: a key that quietly did nothing would be indistinguishable
        // from a key that is broken.
        assert!(compositor.perform(Action::Resize(tos_session::Direction::Left, 2)));
        let message = compositor
            .notifications
            .status_line()
            .expect("a refusal worth reading");
        assert!(message.contains("tall"), "{message}");
        assert!(message.contains("divider"), "{message}");

        assert!(compositor.perform(Action::Balance));
        let latest = compositor
            .notifications
            .history()
            .next()
            .expect("a refusal worth reading")
            .status_text();
        assert!(latest.contains("tall"), "{latest}");
    }

    #[test]
    fn closing_the_last_pane_stops_the_compositor() {
        let mut compositor = compositor();
        assert!(compositor.is_running());
        compositor.perform(Action::ClosePane);
        assert!(!compositor.is_running());
    }

    #[test]
    fn closing_one_of_two_panes_keeps_running() {
        let mut compositor = compositor();
        compositor.perform(Action::Split(Axis::Rows));
        compositor.perform(Action::ClosePane);
        assert!(compositor.is_running());
        assert_eq!(compositor.panes.len(), 1);
    }

    #[test]
    fn resizing_the_display_resizes_the_panes() {
        let mut compositor = compositor();
        let before = compositor
            .pane(compositor.session.focus())
            .unwrap()
            .terminal
            .cols();
        compositor.resize((320, 360));
        let after = compositor
            .pane(compositor.session.focus())
            .unwrap()
            .terminal
            .cols();
        assert!(after < before);
    }

    #[test]
    fn workspaces_get_their_own_pane() {
        let mut compositor = compositor();
        compositor.perform(Action::NewWorkspace);
        assert_eq!(compositor.session.workspace_count(), 2);
        assert_eq!(compositor.panes.len(), 2);
        // Only the active workspace's pane is laid out.
        assert_eq!(compositor.session.active().panes().len(), 1);
    }

    #[test]
    fn scrolling_moves_the_viewport_and_comes_back() {
        let mut compositor = compositor();
        let focus = compositor.session.focus();
        let rows = compositor.pane(focus).unwrap().terminal.rows();
        let filler: String = (0..rows * 2).map(|i| format!("line {i}\r\n")).collect();
        compositor.inject(filler.as_bytes());

        assert!(compositor.perform(Action::ScrollPage(-1)));
        assert!(compositor.pane(focus).unwrap().terminal.display_offset() > 0);
        assert!(compositor.perform(Action::ScrollToBottom));
        assert_eq!(compositor.pane(focus).unwrap().terminal.display_offset(), 0);
    }

    #[test]
    fn a_title_change_reaches_the_pane() {
        let mut compositor = compositor();
        compositor.inject(b"\x1b]0;hello\x07");
        let focus = compositor.session.focus();
        compositor.handle_terminal_events(focus);
        assert_eq!(compositor.pane(focus).unwrap().title, "hello");
    }

    #[test]
    fn osc_52_writes_to_the_compositor_clipboard() {
        let mut compositor = compositor();
        let payload = tos_term::graphics::encode_base64(b"from the app");
        compositor.inject(format!("\x1b]52;c;{payload}\x07").as_bytes());
        let focus = compositor.session.focus();
        compositor.handle_terminal_events(focus);
        assert_eq!(compositor.clipboard('c'), Some(&b"from the app"[..]));
    }

    /// Send an OSC 52 query and return what went back down the PTY.
    fn query_clipboard(compositor: &mut Compositor) -> String {
        compositor.inject(b"\x1b]52;c;?\x07");
        let focus = compositor.session.focus();
        compositor.handle_terminal_events(focus);
        let response = compositor.pane_mut(focus).unwrap().terminal.take_output();
        String::from_utf8(response).unwrap()
    }

    #[test]
    fn a_clipboard_query_is_refused_by_default() {
        let mut compositor = compositor();
        compositor.clipboard.insert('c', b"a password".to_vec());
        let text = query_clipboard(&mut compositor);
        // An empty selection, well formed: the program gets an answer instead
        // of waiting out a timeout, and none of the real one leaks.
        assert_eq!(text, "\x1b]52;c;\x1b\\", "got {text:?}");
        assert!(!text.contains(&tos_term::graphics::encode_base64(b"a password")));
    }

    #[test]
    fn a_clipboard_query_is_answered_once_the_user_allows_it() {
        let mut compositor = compositor_with(Config {
            allow_clipboard_read: true,
            ..Config::default()
        });
        compositor.clipboard.insert('c', b"stored".to_vec());
        let text = query_clipboard(&mut compositor);
        assert!(text.starts_with("\x1b]52;c;"), "got {text:?}");
        assert!(text.contains(&tos_term::graphics::encode_base64(b"stored")));
    }

    #[test]
    fn an_oversized_clipboard_write_is_refused() {
        let mut compositor = compositor();
        compositor.clipboard.insert('c', b"kept".to_vec());
        let payload = tos_term::graphics::encode_base64(&vec![b'x'; MAX_CLIPBOARD_BYTES + 1]);
        compositor.inject(format!("\x1b]52;c;{payload}\x07").as_bytes());
        let focus = compositor.session.focus();
        compositor.handle_terminal_events(focus);
        // Dropped whole, so what the user had is still what they have.
        assert_eq!(compositor.clipboard('c'), Some(&b"kept"[..]));
    }

    #[test]
    fn a_clipboard_write_at_the_cap_still_lands() {
        let mut compositor = compositor();
        let data = vec![b'x'; MAX_CLIPBOARD_BYTES];
        let payload = tos_term::graphics::encode_base64(&data);
        compositor.inject(format!("\x1b]52;c;{payload}\x07").as_bytes());
        let focus = compositor.session.focus();
        compositor.handle_terminal_events(focus);
        assert_eq!(compositor.clipboard('c'), Some(&data[..]));
    }

    #[test]
    fn a_made_up_selector_never_reaches_the_clipboard() {
        let mut compositor = compositor();
        let payload = tos_term::graphics::encode_base64(b"junk");
        compositor.inject(format!("\x1b]52;Z;{payload}\x07").as_bytes());
        let focus = compositor.session.focus();
        compositor.handle_terminal_events(focus);
        assert_eq!(compositor.clipboard('Z'), None);
    }

    #[test]
    fn keys_reach_the_focused_pane_and_bindings_do_not() {
        let mut compositor = compositor();
        let typing = KeyEvent::new(KeyCode::Char('x'), tos_input::Modifiers::NONE);
        compositor.handle_input(InputEvent::Key(typing));
        // A binding is consumed by the compositor instead.
        let split = KeyEvent::new(KeyCode::Char('d'), tos_input::Modifiers::SUPER);
        compositor.handle_input(InputEvent::Key(split));
        assert_eq!(compositor.panes.len(), 2);
    }

    #[test]
    fn the_ctrl_shift_bindings_win_over_the_encoder() {
        // Ctrl+shift+enter is a key the kitty protocol can encode, so the
        // keymap has to claim it before the encoder is ever asked.
        let mut compositor = compositor();
        let ctrl_shift = tos_input::Modifiers::CTRL.union(tos_input::Modifiers::SHIFT);
        compositor.handle_input(InputEvent::Key(KeyEvent::new(KeyCode::Enter, ctrl_shift)));
        assert_eq!(compositor.panes.len(), 2, "ctrl+shift+enter should split");

        compositor.handle_input(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('t'),
            ctrl_shift,
        )));
        assert_eq!(
            compositor.session.workspace_count(),
            2,
            "ctrl+shift+t should open a workspace"
        );
    }

    // ---- launcher -------------------------------------------------------

    /// An overlay of the compositor's own making, so a test does not depend on
    /// what happens to be installed on the machine running it.
    fn open_with(compositor: &mut Compositor, labels: &[&str]) {
        let items = labels
            .iter()
            .map(|label| crate::overlay::OverlayItem::new(*label))
            .collect();
        compositor.open_overlay(OverlayKind::Launcher, Overlay::new("run a program", items));
    }

    fn type_into_overlay(compositor: &mut Compositor, text: &str) {
        for c in text.chars() {
            compositor.handle_input(InputEvent::Key(KeyEvent::new(
                KeyCode::Char(c),
                tos_input::Modifiers::NONE,
            )));
        }
    }

    #[test]
    fn the_launcher_binding_opens_an_overlay_of_programs() {
        let mut compositor = compositor();
        compositor.handle_input(InputEvent::Key(KeyEvent::new(
            KeyCode::Char(' '),
            tos_input::Modifiers::SUPER,
        )));
        let overlay = compositor.overlay().expect("the launcher should be open");
        assert!(!overlay.items().is_empty(), "no programs on $PATH");
        // The list is whatever is on this machine, so check it by a property
        // rather than by name.
        assert!(overlay
            .items()
            .iter()
            .all(|item| tos_pty::which(&item.label).is_some()));
    }

    #[test]
    fn the_overlay_swallows_keys_the_pane_and_the_bindings_would_get() {
        let mut compositor = compositor();
        open_with(&mut compositor, &["ls", "vim"]);
        // Super+d splits when no overlay is open.
        compositor.handle_input(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('d'),
            tos_input::Modifiers::SUPER,
        )));
        assert_eq!(
            compositor.panes.len(),
            1,
            "a binding fired under the overlay"
        );
        // And the pane below sees nothing of what is typed into the query.
        type_into_overlay(&mut compositor, "vi");
        let focus = compositor.session.focus();
        assert_eq!(compositor.pane(focus).unwrap().pending_input(), 0);
        let overlay = compositor.overlay().unwrap();
        assert_eq!(overlay.query(), "vi");
        assert_eq!(overlay.selected_item().unwrap().label, "vim");
    }

    #[test]
    fn escape_leaves_the_panes_alone() {
        let mut compositor = compositor();
        open_with(&mut compositor, &["ls"]);
        assert!(compositor.handle_input(InputEvent::Key(KeyEvent::new(
            KeyCode::Escape,
            tos_input::Modifiers::NONE,
        ))));
        assert!(compositor.overlay().is_none());
        assert_eq!(compositor.panes.len(), 1);
    }

    #[test]
    fn choosing_a_program_opens_a_pane_running_it() {
        // cat is on every machine this could run on, and it stays up, so a
        // pane that is still alive afterwards is one where the exec worked.
        let mut compositor = compositor();
        let before = compositor.session.focus();
        open_with(&mut compositor, &["ls", "cat"]);
        type_into_overlay(&mut compositor, "cat");
        compositor.handle_input(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            tos_input::Modifiers::NONE,
        )));

        assert!(compositor.overlay().is_none(), "choosing should close it");
        assert_eq!(compositor.panes.len(), 2);
        let (&id, pane) = compositor
            .panes
            .iter_mut()
            .find(|(id, _)| **id != before)
            .expect("a new pane");
        assert!(pane.program.ends_with("cat"), "ran {:?}", pane.program);
        assert_eq!(pane.title, "cat");
        assert!(pane.pty.is_alive());
        assert!(compositor.session.active().panes().contains(&id));
    }

    #[test]
    fn choosing_a_program_that_has_gone_says_so_instead_of_spawning() {
        let mut compositor = compositor();
        open_with(&mut compositor, &["definitely-not-a-program-1a2b3c"]);
        compositor.handle_input(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            tos_input::Modifiers::NONE,
        )));
        assert_eq!(compositor.panes.len(), 1);
        assert_eq!(
            compositor.notifications.status_line().as_deref(),
            Some("not found: definitely-not-a-program-1a2b3c")
        );
    }

    // ---- notifications --------------------------------------------------

    /// Raise an application notification in a pane, the way OSC 9 does.
    fn notify_from(compositor: &mut Compositor, id: PaneId, body: &str) {
        compositor
            .pane_mut(id)
            .unwrap()
            .terminal
            .advance(format!("\x1b]9;{body}\x07").as_bytes());
        compositor.handle_terminal_events(id);
    }

    #[test]
    fn an_application_notification_is_attributed_to_its_pane() {
        let mut compositor = compositor();
        let focus = compositor.session.focus();
        notify_from(&mut compositor, focus, "build finished");
        assert_eq!(
            compositor.notifications.status_line().as_deref(),
            Some("pane 1: build finished")
        );
    }

    #[test]
    fn a_bell_says_which_pane_rang() {
        let mut compositor = compositor();
        let focus = compositor.session.focus();
        compositor
            .pane_mut(focus)
            .unwrap()
            .terminal
            .advance(b"\x07");
        compositor.handle_terminal_events(focus);
        assert_eq!(
            compositor.notifications.status_line().as_deref(),
            Some("pane 1: bell")
        );
    }

    #[test]
    fn a_second_notification_waits_instead_of_replacing_the_first() {
        let mut compositor = compositor();
        let focus = compositor.session.focus();
        notify_from(&mut compositor, focus, "first");
        notify_from(&mut compositor, focus, "second");
        // The first is still the one on screen, and the bar says something is
        // behind it rather than the first having never existed.
        assert_eq!(
            compositor.notifications.status_line().as_deref(),
            Some("pane 1: first (+1)")
        );
    }

    #[test]
    fn the_leader_key_does_not_wipe_a_notification() {
        let mut compositor = compositor();
        let focus = compositor.session.focus();
        notify_from(&mut compositor, focus, "still here");
        compositor.handle_input(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('a'),
            tos_input::Modifiers::CTRL,
        )));
        assert!(compositor.keymap.is_pending());
        // The indicator is drawn from the keymap rather than queued, so the
        // notification is untouched and has not spent its time behind it.
        compositor.tick();
        assert_eq!(
            compositor.notifications.status_line().as_deref(),
            Some("pane 1: still here")
        );
    }

    #[test]
    fn the_history_list_holds_what_went_past() {
        let mut compositor = compositor();
        let focus = compositor.session.focus();
        notify_from(&mut compositor, focus, "one");
        notify_from(&mut compositor, focus, "two");
        assert!(compositor.perform(Action::ShowNotifications));
        let overlay = compositor.overlay().expect("the list should be open");
        let labels: Vec<&str> = overlay.items().iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, ["clear", "pane 1: two", "pane 1: one"]);
        // Reading the list is seeing them, so the bar goes quiet.
        assert!(compositor.notifications.status_line().is_none());
    }

    #[test]
    fn choosing_a_notification_goes_to_the_pane_that_raised_it() {
        let mut compositor = compositor();
        let first = compositor.session.focus();
        compositor.perform(Action::Split(Axis::Columns));
        let second = compositor.session.focus();
        assert_ne!(first, second);
        notify_from(&mut compositor, first, "over here");

        compositor.perform(Action::ShowNotifications);
        // The first row clears the list; the second is the notification.
        for code in [KeyCode::Down, KeyCode::Enter] {
            compositor.handle_input(InputEvent::Key(KeyEvent::new(
                code,
                tos_input::Modifiers::NONE,
            )));
        }
        assert!(compositor.overlay().is_none());
        assert_eq!(compositor.session.focus(), first);
    }

    #[test]
    fn the_first_row_of_the_list_clears_it() {
        let mut compositor = compositor();
        let focus = compositor.session.focus();
        notify_from(&mut compositor, focus, "gone soon");
        compositor.perform(Action::ShowNotifications);
        compositor.handle_input(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            tos_input::Modifiers::NONE,
        )));
        assert_eq!(compositor.notifications.history().count(), 0);
    }

    #[test]
    fn without_a_status_bar_a_notification_is_drawn_over_the_panes() {
        // The bar is where messages live, so with no bar they have to live
        // somewhere else: a failure nobody sees looks like a dead key.
        let mut compositor = compositor_with(Config {
            status_bar: false,
            ..Config::default()
        });
        let mut framebuffer = tos_render::OwnedFramebuffer::new(640, 360);
        let accent = compositor.chrome.accent.pack();
        // The top right corner, which is where the banner draws and the one
        // corner nothing else draws in — see `notify::draw_banner`. The whole
        // frame would be the wrong place to look: the block cursor in a pane
        // is the accent colour too, and it sits at the other end of that row.
        let (_, ch) = compositor.cell_size();
        let corner = |framebuffer: &tos_render::OwnedFramebuffer| {
            (0..ch)
                .flat_map(|y| (320..640).map(move |x| (x, y)))
                .filter(|&(x, y)| framebuffer.pixel(x, y) == accent)
                .count()
        };
        {
            let mut surface = framebuffer.surface();
            compositor.render_frame(&mut surface, false);
        }
        assert_eq!(
            corner(&framebuffer),
            0,
            "nothing should be in the accent colour yet"
        );

        let focus = compositor.session.focus();
        notify_from(&mut compositor, focus, "split failed");
        {
            let mut surface = framebuffer.surface();
            compositor.render_frame(&mut surface, false);
        }
        assert!(corner(&framebuffer) > 0, "the notification was not drawn");
    }

    // ---- workspace rename -----------------------------------------------

    fn press_key(
        compositor: &mut Compositor,
        code: KeyCode,
        modifiers: tos_input::Modifiers,
    ) -> bool {
        compositor.handle_input(InputEvent::Key(KeyEvent::new(code, modifiers)))
    }

    /// Open the rename prompt, clear the name it starts on and type `name`.
    fn rename_to(compositor: &mut Compositor, name: &str) {
        press_key(compositor, KeyCode::Char(','), tos_input::Modifiers::SUPER);
        while !compositor.overlay().expect("the prompt").query().is_empty() {
            press_key(compositor, KeyCode::Backspace, tos_input::Modifiers::NONE);
        }
        type_into_overlay(compositor, name);
        press_key(compositor, KeyCode::Enter, tos_input::Modifiers::NONE);
    }

    #[test]
    fn the_rename_binding_opens_a_prompt_holding_the_current_name() {
        let mut compositor = compositor();
        assert!(press_key(
            &mut compositor,
            KeyCode::Char(','),
            tos_input::Modifiers::SUPER
        ));
        let overlay = compositor.overlay().expect("the prompt should be open");
        assert_eq!(overlay.query(), "1");
        assert!(overlay.items().is_empty(), "a prompt has no list");
    }

    #[test]
    fn a_typed_name_reaches_the_status_bar() {
        let mut compositor = compositor();
        rename_to(&mut compositor, "build");
        assert!(compositor.overlay().is_none(), "accepting should close it");
        assert_eq!(compositor.session.active().name, "build");
        let bar = compositor.status_bar();
        let first = &bar.pieces()[0];
        assert_eq!(first.text, " build ");
        assert_eq!(
            first.ink,
            crate::status::Ink::Active,
            "the active workspace is marked"
        );
        assert_eq!(first.hit, Some(crate::status::Hit::Workspace(1)));
    }

    #[test]
    fn a_renamed_workspace_is_wider_in_the_drawn_status_bar() {
        // The bar is the one thing the name is for, so this asks the pixels
        // rather than the items: a longer name highlights more of the row.
        fn accent_in_the_bar(compositor: &mut Compositor) -> usize {
            let mut framebuffer = tos_render::OwnedFramebuffer::new(640, 360);
            {
                let mut surface = framebuffer.surface();
                compositor.render_frame(&mut surface, false);
            }
            let (_, ch) = compositor.cell_size();
            let accent = compositor.chrome.accent.pack();
            let above = (compositor.grid_area().height * ch * 640) as usize;
            framebuffer
                .pixels()
                .iter()
                .skip(above)
                .filter(|&&px| px == accent)
                .count()
        }

        let mut compositor = compositor();
        let before = accent_in_the_bar(&mut compositor);
        rename_to(&mut compositor, "development");
        let after = accent_in_the_bar(&mut compositor);
        assert!(after > before, "{after} should be more than {before}");
    }

    #[test]
    fn cancelling_the_prompt_leaves_the_name_alone() {
        let mut compositor = compositor();
        compositor.perform(Action::RenameWorkspace);
        type_into_overlay(&mut compositor, "half typed");
        assert!(press_key(
            &mut compositor,
            KeyCode::Escape,
            tos_input::Modifiers::NONE
        ));
        assert!(compositor.overlay().is_none());
        assert_eq!(compositor.session.active().name, "1");
    }

    #[test]
    fn an_emptied_prompt_gives_the_workspace_its_number_back() {
        let mut compositor = compositor();
        rename_to(&mut compositor, "build");
        rename_to(&mut compositor, "");
        assert_eq!(compositor.session.active().name, "1");
        // And with the name forgotten the number follows the position again.
        compositor.perform(Action::NewWorkspace);
        compositor.perform(Action::SelectWorkspace(1));
        compositor.perform(Action::ClosePane);
        assert_eq!(compositor.status_bar().pieces()[0].text, " 1 ");
    }

    #[test]
    fn the_prompt_takes_the_keys_the_pane_would_have_had() {
        let mut compositor = compositor();
        compositor.perform(Action::RenameWorkspace);
        type_into_overlay(&mut compositor, "build");
        press_key(
            &mut compositor,
            KeyCode::Char('d'),
            tos_input::Modifiers::SUPER,
        );
        assert_eq!(
            compositor.panes.len(),
            1,
            "a binding fired under the prompt"
        );
        let focus = compositor.session.focus();
        assert_eq!(compositor.pane(focus).unwrap().pending_input(), 0);
    }

    #[test]
    fn the_overlay_is_drawn_over_the_panes() {
        let mut compositor = compositor();
        compositor.inject(b"tOS");
        let mut framebuffer = tos_render::OwnedFramebuffer::new(640, 360);
        {
            let mut surface = framebuffer.surface();
            compositor.render_frame(&mut surface, true);
        }
        // The status bar already highlights the active workspace in the accent
        // colour, so this counts rather than asks whether any is present.
        let accent = compositor.chrome.accent.pack();
        let count = |fb: &tos_render::OwnedFramebuffer| {
            fb.pixels().iter().filter(|&&px| px == accent).count()
        };
        let before = count(&framebuffer);

        open_with(&mut compositor, &["ls", "vim"]);
        {
            let mut surface = framebuffer.surface();
            compositor.render_frame(&mut surface, true);
        }
        // The query cursor and the selected row are both accent coloured, and
        // both are far bigger than anything the status bar draws.
        assert!(
            count(&framebuffer) > before,
            "the overlay should be on top of the panes"
        );
    }

    #[test]
    fn rendering_produces_pixels() {
        let mut compositor = compositor();
        compositor.inject(b"tOS");
        let mut framebuffer = tos_render::OwnedFramebuffer::new(640, 360);
        {
            let mut surface = framebuffer.surface();
            compositor.render_frame(&mut surface, true);
        }
        let background = compositor.chrome.background.pack();
        assert!(
            framebuffer.pixels().iter().any(|&px| px != background),
            "the frame should not be blank"
        );
    }

    /// Transmit a two frame animation into the focused pane and start it.
    fn inject_animation(compositor: &mut Compositor) {
        let pixels = tos_term::graphics::encode_base64(&[0x80u8; 8 * 16 * 4]);
        compositor.inject(format!("\x1b_Ga=T,f=32,s=8,v=16,i=1;{pixels}\x1b\\").as_bytes());
        compositor.inject(format!("\x1b_Ga=f,f=32,s=8,v=16,i=1,z=40;{pixels}\x1b\\").as_bytes());
        compositor.inject(b"\x1b_Ga=a,i=1,r=1,z=40,s=3\x1b\\");
    }

    #[test]
    fn a_running_animation_shortens_the_wait_for_the_next_frame() {
        let mut compositor = compositor();
        assert_eq!(compositor.frame_timeout_ms(), IDLE_TIMEOUT_MS);
        inject_animation(&mut compositor);
        // The first tick only starts the clock; after it the wait is a gap.
        compositor.tick();
        let timeout = compositor.frame_timeout_ms();
        assert!(timeout > 0 && timeout <= 40, "waited {timeout}ms");
    }

    #[test]
    fn a_new_animation_frame_asks_for_a_repaint() {
        let mut compositor = compositor();
        inject_animation(&mut compositor);
        // The first tick starts the animation's clock.
        compositor.tick();
        let mut framebuffer = tos_render::OwnedFramebuffer::new(640, 360);
        {
            let mut surface = framebuffer.surface();
            compositor.render_frame(&mut surface, true);
        }
        assert!(!compositor.needs_render());

        // Step past the first frame's gap the way the tick does, but with a
        // time of the test's choosing.
        let focus = compositor.session.focus();
        let due = Instant::now() + Duration::from_millis(40);
        assert!(compositor
            .pane_mut(focus)
            .unwrap()
            .terminal
            .advance_animations(due));
        assert!(compositor.needs_render());
    }

    // ---- copy mode ------------------------------------------------------

    /// Open copy mode the way a person does, on the binding.
    fn copy_mode(compositor: &mut Compositor) {
        press_key(compositor, KeyCode::Char('['), tos_input::Modifiers::SUPER);
    }

    /// Press a run of characters at whatever owns the keyboard.
    fn type_keys(compositor: &mut Compositor, keys: &str) {
        for ch in keys.chars() {
            press_key(compositor, KeyCode::Char(ch), tos_input::Modifiers::NONE);
        }
    }

    #[test]
    fn the_binding_opens_copy_mode_and_the_bindings_stop_firing() {
        let mut compositor = compositor();
        copy_mode(&mut compositor);
        assert!(compositor.copy_mode().is_some(), "the binding did nothing");
        // Every key belongs to the mode now, including the ones that would
        // otherwise split a pane.
        press_key(
            &mut compositor,
            KeyCode::Char('d'),
            tos_input::Modifiers::SUPER,
        );
        assert_eq!(compositor.panes.len(), 1, "a binding fired in copy mode");
        assert!(compositor.is_running());
        assert!(compositor.copy_mode().is_some());
    }

    #[test]
    fn the_motions_move_the_copy_cursor_rather_than_the_pane() {
        let mut compositor = compositor();
        compositor.inject(b"alpha beta\r\n");
        copy_mode(&mut compositor);
        let from = compositor.copy_mode().expect("copy mode").cursor();
        type_keys(&mut compositor, "kll");
        let to = compositor.copy_mode().expect("copy mode").cursor();
        assert_eq!(to, crate::selection::Anchor::new(from.line - 1, 2));
    }

    #[test]
    fn a_selection_made_with_the_keyboard_is_yanked_to_the_clipboard() {
        // The bug this mode exists for: leader [ then y used to copy exactly
        // one character, whatever happened in between.
        let mut compositor = compositor();
        compositor.inject(b"alpha beta\r\n");
        copy_mode(&mut compositor);
        type_keys(&mut compositor, "kve");
        let selected = compositor
            .panes
            .get(&compositor.session.focus())
            .and_then(|pane| pane.selected_text());
        assert_eq!(selected.as_deref(), Some("alpha"), "the highlight is wrong");
        type_keys(&mut compositor, "y");
        assert_eq!(compositor.clipboard(CLIPBOARD), Some(&b"alpha"[..]));
    }

    #[test]
    fn copying_leaves_the_mode_and_takes_the_highlight_with_it() {
        let mut compositor = compositor();
        compositor.inject(b"alpha beta\r\n");
        copy_mode(&mut compositor);
        type_keys(&mut compositor, "kvey");
        assert!(compositor.copy_mode().is_none(), "copy mode is still up");
        let pane = compositor.panes.get(&compositor.session.focus()).unwrap();
        assert!(
            pane.selection.is_none(),
            "a stale highlight was left behind"
        );
        assert!(
            !pane.selection_in_progress,
            "the pane still thinks it is being selected"
        );
        // And the keyboard is the pane's again.
        press_key(
            &mut compositor,
            KeyCode::Char('d'),
            tos_input::Modifiers::SUPER,
        );
        assert_eq!(compositor.panes.len(), 2);
    }

    #[test]
    fn escape_leaves_copy_mode_without_touching_the_clipboard() {
        let mut compositor = compositor();
        compositor.inject(b"alpha beta\r\n");
        copy_mode(&mut compositor);
        type_keys(&mut compositor, "kve");
        press_key(&mut compositor, KeyCode::Escape, tos_input::Modifiers::NONE);
        assert!(compositor.copy_mode().is_none());
        assert_eq!(compositor.clipboard(CLIPBOARD), None);
    }

    #[test]
    fn walking_above_the_viewport_scrolls_into_history_instead_of_stopping() {
        let mut compositor = compositor();
        let rows = compositor
            .panes
            .get(&compositor.session.focus())
            .map(|pane| pane.terminal.rows())
            .expect("a pane");
        for line in 0..rows * 2 {
            compositor.inject(format!("line {line}\r\n").as_bytes());
        }
        copy_mode(&mut compositor);
        for _ in 0..rows {
            type_keys(&mut compositor, "k");
        }
        let pane = compositor.panes.get(&compositor.session.focus()).unwrap();
        assert!(
            pane.terminal.display_offset() > 0,
            "the copy cursor stopped at the top of the screen"
        );
        // The viewport is following the cursor, so the cursor is still on
        // screen at the end of the walk.
        let copy = compositor.copy_mode().expect("copy mode");
        assert_eq!(copy.scroll_to_show(pane.terminal.grid()), 0);
        assert!(copy.display_cursor(pane.terminal.grid()).is_some());
    }

    #[test]
    fn a_selection_dragged_up_through_history_copies_what_it_covered() {
        let mut compositor = compositor();
        compositor.inject(b"first\r\n");
        let rows = compositor
            .panes
            .get(&compositor.session.focus())
            .map(|pane| pane.terminal.rows())
            .expect("a pane");
        for line in 0..rows {
            compositor.inject(format!("line {line}\r\n").as_bytes());
        }
        copy_mode(&mut compositor);
        type_keys(&mut compositor, "v");
        for _ in 0..rows * 2 {
            type_keys(&mut compositor, "k");
        }
        type_keys(&mut compositor, "y");
        let copied = compositor.clipboard(CLIPBOARD).expect("something copied");
        let text = String::from_utf8_lossy(copied);
        assert!(
            text.starts_with("first"),
            "the selection never reached history: {text:?}"
        );
    }

    #[test]
    fn a_mouse_press_takes_the_selection_back_from_copy_mode() {
        // Two owners of one selection is one too many, and the press is about
        // to start a selection of its own.
        let mut compositor = compositor();
        compositor.inject(b"alpha beta\r\n");
        copy_mode(&mut compositor);
        type_keys(&mut compositor, "kv");
        compositor.handle_input(InputEvent::Mouse(MouseEvent {
            button: Some(MouseButton::Left),
            action: MouseAction::Press,
            col: 2,
            row: 0,
            modifiers: tos_input::Modifiers::NONE,
        }));
        assert!(
            compositor.copy_mode().is_none(),
            "copy mode ignored the mouse"
        );
    }

    #[test]
    fn a_press_on_the_status_bar_ends_copy_mode_the_way_any_other_press_does() {
        // The bar is answered before any pane is looked at and returns from
        // there, so this press used to skip the rule entirely: the workspace
        // switched, the highlight went off screen with it, and copy mode sat
        // on a pane nobody could see swallowing every key until somebody
        // guessed escape.
        let mut compositor = compositor();
        compositor.perform(Action::NewWorkspace);
        compositor.inject(b"alpha beta\r\n");
        copy_mode(&mut compositor);
        type_keys(&mut compositor, "kv");
        let left_behind = compositor.session.focus();

        // The strip reads " 1  2 ", so the first workspace is cell one.
        assert!(click_bar(&mut compositor, 1), "the click did nothing");
        assert_eq!(compositor.session.active_index(), 0);
        assert!(
            compositor.copy_mode().is_none(),
            "copy mode outlived the click and is eating the keyboard"
        );
        let pane = compositor
            .panes
            .get(&left_behind)
            .expect("the pane it was in");
        assert!(pane.selection.is_none(), "a highlight was left behind");
        assert!(!pane.selection_in_progress);
        // And the bindings answer again, which is what "the keyboard is back"
        // means from the outside.
        press_key(
            &mut compositor,
            KeyCode::Char('d'),
            tos_input::Modifiers::SUPER,
        );
        assert_eq!(compositor.panes.len(), 3, "a binding was still swallowed");
    }

    #[test]
    fn a_mouse_moved_with_nothing_held_down_does_not_drag_the_copy_highlight() {
        let mut compositor = compositor();
        compositor.inject(b"alpha beta\r\n");
        copy_mode(&mut compositor);
        type_keys(&mut compositor, "kve");
        let focus = compositor.session.focus();
        let highlighted = compositor.panes[&focus].selection.expect("a highlight");
        assert_eq!(
            compositor.panes[&focus].selected_text().as_deref(),
            Some("alpha")
        );

        // A bare motion: the pointer crossing the pane with no button down,
        // which is what a hand resting on a mouse produces. Copy mode's
        // selection is one in progress, which is not the same claim as one
        // the pointer is dragging, and only the second may move it.
        compositor.handle_input(InputEvent::Mouse(MouseEvent {
            button: None,
            action: MouseAction::Motion,
            col: 20,
            row: 6,
            modifiers: tos_input::Modifiers::NONE,
        }));

        assert_eq!(
            compositor.panes[&focus].selection,
            Some(highlighted),
            "the pointer walked the highlight away from the copy cursor"
        );
        // The yank comes from the copy cursor either way, so the damage a
        // moved highlight does is that the two stop agreeing.
        type_keys(&mut compositor, "y");
        assert_eq!(compositor.clipboard(CLIPBOARD), Some(&b"alpha"[..]));
    }

    #[test]
    fn a_notification_raised_under_copy_mode_waits_for_the_slot_back() {
        let mut compositor = compositor();
        let focus = compositor.session.focus();
        copy_mode(&mut compositor);
        notify_from(&mut compositor, focus, "the build finished");
        // Copy mode has the slot the message would be drawn in, so nothing of
        // it is on screen; two ticks a long way apart are enough to retire it
        // if the queue's clock is running, and it must not be.
        let start = Instant::now();
        compositor.tick_at(start);
        compositor.tick_at(start + Duration::from_secs(10));
        assert!(compositor.copy_mode().is_some());

        press_key(&mut compositor, KeyCode::Escape, tos_input::Modifiers::NONE);
        assert_eq!(
            compositor.notifications.status_line().as_deref(),
            Some("pane 1: the build finished"),
            "the message spent its three seconds behind copy mode"
        );
    }

    // ---- the lock -------------------------------------------------------

    /// The account these tests are the session of, written into every shadow
    /// file below. Named rather than defaulted: `Config::default()` takes it
    /// from the environment, and a machine with `TOS_USER` set in the shell
    /// that ran `cargo test` would otherwise be asking these locks for a
    /// different line than the one the fixture wrote.
    const LOCK_ACCOUNT: &str = "tos";

    /// A shadow file of this test's own making, so that nothing here depends
    /// on whether the machine running it has a password of its own. The
    /// password is always "tos"; what varies is whether the account has one.
    ///
    /// Laid out the way the machine's own file is, with root above the
    /// person and carrying no password, so that a reader which took the
    /// first line or the first hash it found would be caught here.
    fn credential(name: &str, password: Option<&str>) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("tos-lock-compositor-{}-{name}", std::process::id()));
        let field = match password {
            Some(password) => tos_crypt::sha512crypt::hash(password.as_bytes(), b"tOScompositor"),
            None => "*".to_string(),
        };
        std::fs::write(
            &path,
            format!("root:*:::::::\n{LOCK_ACCOUNT}:{field}:::::::\n"),
        )
        .expect("credential file");
        path
    }

    fn compositor_with_password(name: &str) -> Compositor {
        compositor_with(Config {
            credential: credential(name, Some("tos")),
            credential_user: LOCK_ACCOUNT.into(),
            ..Config::default()
        })
    }

    /// Type a password into the lock and press enter.
    fn answer(compositor: &mut Compositor, password: &str) {
        type_into_overlay(compositor, password);
        press_key(compositor, KeyCode::Enter, tos_input::Modifiers::NONE);
    }

    fn lock_binding(compositor: &mut Compositor) -> bool {
        press_key(
            compositor,
            KeyCode::Char('l'),
            tos_input::Modifiers::SUPER.union(tos_input::Modifiers::SHIFT),
        )
    }

    #[test]
    fn the_binding_locks_and_the_password_unlocks() {
        let mut compositor = compositor_with_password("roundtrip");
        assert!(lock_binding(&mut compositor));
        assert!(compositor.is_locked());
        answer(&mut compositor, "tos");
        assert!(!compositor.is_locked());
        assert!(compositor.is_running(), "unlocking is not quitting");
    }

    #[test]
    fn a_wrong_password_leaves_the_screen_locked() {
        let mut compositor = compositor_with_password("wrong");
        compositor.lock_session();
        answer(&mut compositor, "not it");
        assert!(compositor.is_locked());
        assert_eq!(compositor.lock_screen().unwrap().attempts(), 1);
        // And the field is empty again, ready to be retyped.
        assert_eq!(compositor.lock_screen().unwrap().typed_len(), 0);
    }

    #[test]
    fn a_locked_screen_with_nothing_moving_on_it_asks_for_no_frames() {
        // #164. The caret in that box is drawn solid — `draw_field` has never
        // been given the blink phase — and nothing else on the screen moves,
        // so every frame this used to ask for repainted the whole display to
        // put back the image already on it. The readings, the clock and the
        // leases go on happening behind it and none of them is on the screen.
        let mut compositor = compositor_with_password("still");
        assert!(compositor.lock_session());
        let start = Instant::now();
        for interval in 1..=8 {
            let now = start + BLINK_INTERVAL * interval;
            assert!(
                !compositor.tick_at(now),
                "a locked screen asked for a frame {interval} intervals in"
            );
        }
    }

    #[test]
    fn a_lock_counting_down_asks_for_the_frame_that_moves_the_count() {
        // The one thing on that screen that moves by itself: "try again in
        // 12s" has to become 11, and the blink frame that used to carry it is
        // the frame above that no longer happens.
        let mut compositor = compositor_with_password("countdown");
        assert!(compositor.lock_session());
        answer(&mut compositor, "not it");
        let start = Instant::now();
        assert!(
            compositor.tick_at(start + BLINK_INTERVAL),
            "the countdown stopped counting"
        );
        // And it stops again once the wait is over, rather than leaving a
        // locked screen drawing forever because somebody mistyped once.
        assert!(
            !compositor.tick_at(start + Duration::from_secs(30)),
            "the screen went on asking after the wait ran out"
        );
    }

    #[test]
    fn a_session_still_blinks() {
        // The phase standing still is the lock's rule and not a new default.
        let mut compositor = compositor_with(Config::default());
        let before = compositor.blink_visible;
        assert!(compositor.tick_at(Instant::now() + BLINK_INTERVAL));
        assert_ne!(compositor.blink_visible, before, "the caret stopped");
    }

    #[test]
    fn the_caret_comes_back_lit_when_the_lock_is_answered() {
        // The phase is whatever it was when the screen went away, which for a
        // machine locked all night is an hour stale. The same thing an unblank
        // does, for the same reason: where the caret is, is the first thing
        // anybody looks for on a screen they have just got back.
        let mut compositor = compositor_with_password("lit");
        assert!(compositor.lock_session());
        compositor.blink_visible = false;
        answer(&mut compositor, "tos");
        assert!(!compositor.is_locked());
        assert!(compositor.blink_visible, "the caret came back dark");
    }

    #[test]
    fn with_no_credential_the_lock_refuses_and_says_why() {
        // The live ISO, and an installed machine whose owner declined a
        // password. The compositor never asks what kind of machine it is on.
        let mut compositor = compositor_with(Config {
            credential: credential("none", None),
            credential_user: LOCK_ACCOUNT.into(),
            ..Config::default()
        });
        assert!(lock_binding(&mut compositor));
        assert!(
            !compositor.is_locked(),
            "locked with nothing to unlock with"
        );
        let said = compositor.notifications.status_line().unwrap_or_default();
        assert!(
            said.starts_with("cannot lock: no password is set for tos"),
            "{said:?}"
        );
    }

    #[test]
    fn a_credential_that_does_not_parse_is_not_a_wrong_password() {
        // It is a refusal to engage. Treating it as a wrong password would
        // put up a screen that could never be opened.
        // `$y$` is what Debian's own passwd(1) writes, so it is the line
        // that will actually turn up on a machine somebody changed their
        // password on.
        let path = credential("yescrypt", None);
        std::fs::write(&path, format!("{LOCK_ACCOUNT}:$y$j9T$salt$digest:::::::\n"))
            .expect("credential file");
        let mut compositor = compositor_with(Config {
            credential: path,
            credential_user: LOCK_ACCOUNT.into(),
            ..Config::default()
        });
        compositor.lock_session();
        assert!(!compositor.is_locked());
        let said = compositor.notifications.status_line().unwrap_or_default();
        assert!(said.contains("not a password tOS can check"), "{said:?}");
    }

    // ---- the login boundary (#112) --------------------------------------

    /// A compositor on a machine's console: the one kind that is logged into.
    fn console_with(name: &str, password: Option<&str>) -> Compositor {
        compositor_with(Config {
            credential: credential(name, password),
            credential_user: LOCK_ACCOUNT.into(),
            gated: true,
            ..Config::default()
        })
    }

    fn panes_running(compositor: &Compositor) -> usize {
        compositor.panes.len()
    }

    #[test]
    fn a_machine_with_a_password_starts_at_a_login_screen() {
        let compositor = console_with("login-start", Some("tos"));
        assert!(compositor.is_locked(), "the session started unasked");
        assert_eq!(
            compositor.lock_screen().map(|screen| screen.purpose()),
            Some(lock::Purpose::Login),
            "a lock has a session behind it and this has none"
        );
        // And nothing is running behind it. A shell started before anybody
        // said who they were would make this a boundary in the drawing only.
        assert_eq!(panes_running(&compositor), 0);
        assert_eq!(
            compositor.lock_screen().map(|screen| screen.user()),
            Some(LOCK_ACCOUNT),
            "the screen has to say whose password it wants"
        );
    }

    #[test]
    fn answering_the_login_screen_starts_the_session() {
        let mut compositor = console_with("login-answer", Some("tos"));
        answer(&mut compositor, "tos");
        assert!(!compositor.is_locked());
        assert_eq!(panes_running(&compositor), 1);
        assert!(compositor.is_running());
    }

    #[test]
    fn a_wrong_password_at_the_login_screen_starts_nothing() {
        let mut compositor = console_with("login-wrong", Some("tos"));
        answer(&mut compositor, "not it");
        assert!(compositor.is_locked());
        assert_eq!(panes_running(&compositor), 0);
        assert_eq!(compositor.lock_screen().unwrap().attempts(), 1);
    }

    #[test]
    fn a_machine_with_no_password_is_not_asked_who_is_there() {
        // The live image, whose root carries `*`, and an installed machine
        // whose owner declined a password. A login screen with nothing to
        // check against is a brick, and the compositor arrives at that
        // without being told what live media is — the same rule the lock
        // obeys at the other end of the session.
        let compositor = console_with("login-none", None);
        assert!(!compositor.is_locked());
        assert_eq!(panes_running(&compositor), 1);
    }

    #[test]
    fn the_last_pane_closing_comes_back_to_the_login_screen() {
        // `exit` in the last pane. What it used to reach was a bare root
        // shell on the live image and a brand new session on an installed
        // one; what it reaches now is the boundary it came in through.
        let mut compositor = console_with("logout", Some("tos"));
        answer(&mut compositor, "tos");
        let focus = compositor.session.focus();
        compositor.close_pane(focus);

        assert!(
            compositor.is_running(),
            "the machine quit rather than asking"
        );
        assert!(compositor.is_locked());
        assert_eq!(
            compositor.lock_screen().map(|screen| screen.purpose()),
            Some(lock::Purpose::Login)
        );
        assert_eq!(panes_running(&compositor), 0);

        // And it is a way in, not a way to be stuck: the same password opens
        // it and the machine has a session again.
        answer(&mut compositor, "tos");
        assert!(!compositor.is_locked());
        assert_eq!(panes_running(&compositor), 1);
    }

    #[test]
    fn the_session_that_comes_back_is_not_the_one_that_left() {
        let mut compositor = console_with("logout-fresh", Some("tos"));
        answer(&mut compositor, "tos");
        press_key(
            &mut compositor,
            KeyCode::Char('d'),
            tos_input::Modifiers::SUPER,
        );
        assert_eq!(panes_running(&compositor), 2, "the split did not happen");
        compositor
            .clipboard
            .insert(CLIPBOARD, b"what the last person copied".to_vec());

        for _ in 0..2 {
            let focus = compositor.session.focus();
            compositor.close_pane(focus);
        }
        answer(&mut compositor, "tos");

        assert_eq!(panes_running(&compositor), 1, "the old layout came back");
        assert_eq!(compositor.session.workspace_count(), 1);
        assert!(
            compositor.clipboard.is_empty(),
            "the next person was handed what the last one copied"
        );
    }

    #[test]
    fn quitting_is_a_log_out_where_there_is_a_login_to_come_back_to() {
        let mut compositor = console_with("logout-binding", Some("tos"));
        answer(&mut compositor, "tos");
        assert!(press_key(
            &mut compositor,
            KeyCode::Char('q'),
            tos_input::Modifiers::SUPER
        ));
        assert!(
            compositor.is_running(),
            "quit handed the machine back rather than asking who is there"
        );
        assert!(compositor.is_locked());
        assert_eq!(panes_running(&compositor), 0);
    }

    #[test]
    fn quitting_still_quits_where_there_is_nobody_to_ask() {
        // The live image. Leaving is what it has always meant there, and the
        // init that started this starts another one — which is a session,
        // where what tos.rescue asks for is a shell.
        let mut compositor = console_with("quit-none", None);
        press_key(
            &mut compositor,
            KeyCode::Char('q'),
            tos_input::Modifiers::SUPER,
        );
        assert!(!compositor.is_running());
    }

    #[test]
    fn a_display_that_is_not_a_console_is_not_logged_into() {
        // The nested and headless backends: a window on a desktop that has
        // already asked. Gating them would mean every `cargo test` on a
        // machine whose root has a password started at a prompt.
        let compositor = compositor_with(Config {
            credential: credential("ungated", Some("tos")),
            credential_user: LOCK_ACCOUNT.into(),
            ..Config::default()
        });
        assert!(!compositor.is_locked());
        assert_eq!(panes_running(&compositor), 1);
    }

    #[test]
    fn a_login_screen_draws_with_no_session_under_it() {
        // Every locked frame clears the panel and paints the box; with no
        // panes at all there is nothing else to paint, and nothing here may
        // assume there is.
        let mut compositor = console_with("login-frame", Some("tos"));
        let mut pixels = vec![0u32; 640 * 360];
        let mut surface = Surface::new(&mut pixels, 640, 360, 640);
        compositor.render_frame(&mut surface, false);
        assert!(
            pixels.iter().any(|&p| p != 0),
            "the login screen drew nothing at all"
        );
    }

    /// A hand moving over the panel, in display pixels.
    fn moved_to(compositor: &mut Compositor, x: f64, y: f64) -> bool {
        compositor.handle_input(InputEvent::Pointer(tos_input::PointerEvent {
            x,
            y,
            button: None,
            action: MouseAction::Motion,
            modifiers: tos_input::Modifiers::NONE,
        }))
    }

    #[test]
    fn a_login_screen_shows_the_pointer() {
        // #122. The arrow is the only thing tOS has that says a mouse is
        // there — `Pointer::seen` is set by an event that arrived, never by a
        // device claiming to exist — and a login screen is the first screen
        // tOS shows anybody. No arrow there is indistinguishable from no
        // mouse there.
        let mut compositor = console_with("login-pointer", Some("tos"));
        assert_eq!(
            compositor.lock_screen().map(|screen| screen.purpose()),
            Some(lock::Purpose::Login)
        );
        assert!(
            compositor.pointer_rect().is_none(),
            "an arrow before any device had said anything"
        );

        assert!(
            moved_to(&mut compositor, 40.0, 40.0),
            "a motion that moved the arrow owes a frame"
        );
        assert_eq!(
            compositor.pointer_rect().map(|rect| (rect.x, rect.y)),
            Some((40, 40))
        );
    }

    #[test]
    fn a_lock_screen_still_takes_the_pointer_away() {
        // The half of #122 that deliberately did not change. A lock has a
        // session behind it and a hand in front of it that has not said whose
        // it is; a login has neither.
        let mut compositor = compositor_with_password("lock-pointer");
        assert!(moved_to(&mut compositor, 40.0, 40.0));
        assert!(compositor.pointer_rect().is_some());

        compositor.lock_session();
        assert_eq!(
            compositor.lock_screen().map(|screen| screen.purpose()),
            Some(lock::Purpose::Lock)
        );
        assert!(
            compositor.pointer_rect().is_none(),
            "the arrow stayed where the hand left it, on top of the password box"
        );
        assert!(
            !moved_to(&mut compositor, 200.0, 200.0),
            "a locked screen took a motion and asked for a frame for it"
        );
        assert!(compositor.pointer_rect().is_none());
    }

    #[test]
    fn a_click_at_the_login_screen_goes_nowhere() {
        // Letting the arrow through is letting a position through and nothing
        // else: this reaches `Pointer::moved_to` and never `route_mouse`.
        let mut compositor = console_with("login-click", Some("tos"));
        for (action, y) in [(MouseAction::Press, 20.0), (MouseAction::Drag, 60.0)] {
            compositor.handle_input(InputEvent::Pointer(tos_input::PointerEvent {
                x: 20.0,
                y,
                button: Some(MouseButton::Left),
                action,
                modifiers: tos_input::Modifiers::NONE,
            }));
        }
        assert!(compositor.is_locked(), "a click opened the session");
        assert_eq!(panes_running(&compositor), 0);
        assert!(compositor.mouse_grab.is_none());
    }

    #[test]
    fn a_login_screen_stops_asking_for_frames_once_the_pointer_stands_still() {
        // The counterpart of
        // `a_locked_screen_stops_asking_for_frames_on_the_pointer_s_account`.
        // That one settles because the arrow belongs nowhere and the locked
        // frame forgets where it was; this one has an arrow to draw, so it
        // settles only if the frame remembers the rectangle it drew it into.
        let mut compositor = console_with("login-pointer-idle", Some("tos"));
        assert!(moved_to(&mut compositor, 40.0, 40.0));
        let mut framebuffer = tos_render::OwnedFramebuffer::new(640, 360);
        {
            let mut surface = framebuffer.surface();
            compositor.render_frame(&mut surface, true);
        }
        assert!(compositor.is_locked());
        assert!(
            !compositor.needs_render(),
            "a login screen with a pointer on it repaints on every pass"
        );
    }

    #[test]
    fn no_binding_fires_while_the_screen_is_locked() {
        let mut compositor = compositor_with_password("bindings");
        compositor.lock_session();
        for (code, modifiers) in [
            (KeyCode::Char('d'), tos_input::Modifiers::SUPER),
            (KeyCode::Char('q'), tos_input::Modifiers::SUPER),
            (KeyCode::Char('x'), tos_input::Modifiers::SUPER),
            (
                KeyCode::Enter,
                tos_input::Modifiers::CTRL.union(tos_input::Modifiers::SHIFT),
            ),
        ] {
            press_key(&mut compositor, code, modifiers);
        }
        assert_eq!(compositor.panes.len(), 1, "a binding fired under the lock");
        assert!(compositor.is_running(), "super+q quit a locked session");
        assert!(compositor.is_locked());
    }

    #[test]
    fn the_leader_cannot_reach_a_binding_while_locked() {
        let mut compositor = compositor_with_password("leader");
        compositor.lock_session();
        press_key(
            &mut compositor,
            KeyCode::Char('a'),
            tos_input::Modifiers::CTRL,
        );
        assert!(
            !compositor.keymap.is_pending(),
            "the leader armed under the lock"
        );
        press_key(
            &mut compositor,
            KeyCode::Char('q'),
            tos_input::Modifiers::NONE,
        );
        assert!(compositor.is_running());
    }

    #[test]
    fn nothing_typed_at_the_lock_reaches_a_pane() {
        let mut compositor = compositor_with_password("typing");
        compositor.lock_session();
        type_into_overlay(&mut compositor, "tos");
        let focus = compositor.session.focus();
        assert_eq!(compositor.pane(focus).unwrap().pending_input(), 0);
    }

    /// The events `handle_key` would never have seen: routed straight through
    /// `handle_input`, which is why the gate has to be there and not one level
    /// further in.
    #[test]
    fn the_lock_gates_the_mouse_the_pointer_and_the_paste() {
        let mut compositor = compositor_with_password("input");
        compositor
            .clipboard
            .insert(PRIMARY, b"a command\n".to_vec());
        compositor
            .clipboard
            .insert(CLIPBOARD, b"another command\n".to_vec());
        compositor.lock_session();
        let focus = compositor.session.focus();

        // A middle click pastes primary into the focused pane when the screen
        // is not locked, and it never passes through `handle_key` at all.
        compositor.handle_input(InputEvent::Mouse(MouseEvent {
            button: Some(MouseButton::Middle),
            action: MouseAction::Press,
            col: 2,
            row: 2,
            modifiers: tos_input::Modifiers::NONE,
        }));
        // A pointer press and drag, which is how the mouse selects and copies
        // whatever is on the screen it is not supposed to be able to read.
        for (action, y) in [(MouseAction::Press, 20.0), (MouseAction::Drag, 60.0)] {
            compositor.handle_input(InputEvent::Pointer(tos_input::PointerEvent {
                x: 20.0,
                y,
                button: Some(MouseButton::Left),
                action,
                modifiers: tos_input::Modifiers::NONE,
            }));
        }
        // And a bracketed paste, which is how a host terminal types.
        compositor.handle_input(InputEvent::Paste("a pasted command\n".into()));
        // Focus notifications are input too.
        compositor.handle_input(InputEvent::FocusGained);

        assert!(compositor.is_locked());
        let pane = compositor.pane(focus).unwrap();
        assert_eq!(
            pane.pending_input(),
            0,
            "input reached the pane under the lock"
        );
        assert!(
            pane.selection.is_none(),
            "the mouse selected under the lock"
        );
        assert!(!pane.selection_in_progress);
        assert!(compositor.mouse_grab.is_none());
    }

    #[test]
    fn the_last_pane_dying_while_locked_does_not_end_the_session() {
        // A shell reaching its end of file is a way out of a locked screen
        // that does not go through a binding at all.
        let mut compositor = compositor_with_password("lastpane");
        compositor.lock_session();
        let focus = compositor.session.focus();
        compositor.close_pane(focus);
        assert!(compositor.is_running(), "the session ended under the lock");
        assert!(compositor.is_locked());
        // It ends when somebody has said who they are, and not before.
        answer(&mut compositor, "tos");
        assert!(!compositor.is_locked());
        assert!(!compositor.is_running());
    }

    #[test]
    fn resizing_the_display_while_locked_keeps_the_lock() {
        let mut compositor = compositor_with_password("resize");
        compositor.lock_session();
        compositor.resize((320, 240));
        assert!(compositor.is_locked());
        // Even down to a display with no room to draw the box in, which is
        // the one direction the lock is allowed to fail in: it stops being
        // legible, and it does not stop being a lock.
        compositor.resize((32, 24));
        let mut framebuffer = tos_render::OwnedFramebuffer::new(32, 24);
        {
            let mut surface = framebuffer.surface();
            compositor.render_frame(&mut surface, false);
        }
        assert!(compositor.is_locked());
        answer(&mut compositor, "tos");
        assert!(!compositor.is_locked());
    }

    #[test]
    fn a_notification_raised_while_locked_waits_instead_of_being_shown() {
        let mut compositor = compositor_with_password("notify");
        compositor.lock_session();
        let focus = compositor.session.focus();
        notify_from(&mut compositor, focus, "the build finished");
        // Nothing is drawn, so nothing spends its time on screen: the queue
        // stands still, and the message is still there afterwards.
        for _ in 0..4 {
            compositor.tick();
        }
        assert!(compositor.is_locked());
        answer(&mut compositor, "tos");
        assert_eq!(
            compositor.notifications.status_line().as_deref(),
            Some("pane 1: the build finished")
        );
    }

    #[test]
    fn unlocking_asks_for_the_whole_screen_back() {
        let mut compositor = compositor_with_password("redraw");
        compositor.lock_session();
        let mut framebuffer = tos_render::OwnedFramebuffer::new(640, 360);
        {
            let mut surface = framebuffer.surface();
            compositor.render_frame(&mut surface, true);
        }
        assert!(
            !compositor.needs_full_redraw,
            "a locked frame leaves nothing outstanding"
        );
        answer(&mut compositor, "tos");
        assert!(
            compositor.needs_full_redraw,
            "the session came back on damage that was thrown away"
        );
    }

    #[test]
    fn a_locked_screen_stops_asking_for_frames_on_the_pointer_s_account() {
        // `needs_render` answers the pointer's part of the question by
        // comparing where the arrow belongs against where it was last drawn,
        // and the lock says it belongs nowhere. A locked frame that did not
        // also forget the old rectangle would leave those two unable ever to
        // agree, which is a frame per pass of the loop for as long as nobody
        // is there to see one.
        let mut compositor = compositor_with_password("pointer-idle");
        compositor.handle_input(InputEvent::Pointer(tos_input::PointerEvent {
            x: 40.0,
            y: 40.0,
            button: None,
            action: MouseAction::Motion,
            modifiers: tos_input::Modifiers::NONE,
        }));
        let mut framebuffer = tos_render::OwnedFramebuffer::new(640, 360);
        {
            let mut surface = framebuffer.surface();
            compositor.render_frame(&mut surface, true);
        }
        assert!(
            !compositor.needs_render(),
            "a pointer standing still asks for a frame on every pass"
        );

        compositor.lock_session();
        {
            let mut surface = framebuffer.surface();
            compositor.render_frame(&mut surface, true);
        }
        assert!(compositor.is_locked());
        assert!(
            !compositor.needs_render(),
            "a locked session repaints on every pass"
        );
    }

    // ---- idle -----------------------------------------------------------

    /// A display that remembers what it was told about blanking.
    ///
    /// The DRM path cannot run here and the headless one has nothing to put
    /// to sleep, so what a test can check is the one thing the compositor is
    /// responsible for: that the display is told, once each way, at the right
    /// moment.
    struct Panel {
        framebuffer: tos_render::OwnedFramebuffer,
        told: Vec<bool>,
        /// A display that cannot do what it is asked, which is a thing
        /// hardware does.
        refuses: bool,
    }

    impl Panel {
        fn new() -> Panel {
            Panel {
                framebuffer: tos_render::OwnedFramebuffer::new(640, 360),
                told: Vec::new(),
                refuses: false,
            }
        }

        fn refusing() -> Panel {
            Panel {
                refuses: true,
                ..Panel::new()
            }
        }
    }

    impl Display for Panel {
        fn size(&self) -> (u32, u32) {
            (640, 360)
        }

        fn frame(&mut self, draw: &mut dyn FnMut(&mut Surface<'_>)) -> io::Result<()> {
            draw(&mut self.framebuffer.surface());
            Ok(())
        }

        fn blank(&mut self, blank: bool) -> io::Result<()> {
            self.told.push(blank);
            if self.refuses {
                return Err(io::Error::other("the panel is welded on"));
            }
            Ok(())
        }
    }

    /// A compositor whose deadlines are near enough to walk to, and the panel
    /// it sits in front of.
    fn idling(config: Config, lock_after: Option<u64>, blank_after: Option<u64>) -> Compositor {
        compositor_with(Config {
            idle_lock: lock_after.map(Duration::from_secs),
            idle_blank: blank_after.map(Duration::from_secs),
            ..config
        })
    }

    /// Where the session's idle time is measured from.
    fn started(compositor: &Compositor) -> Instant {
        compositor.last_activity
    }

    #[test]
    fn an_untouched_session_blanks_and_a_key_brings_it_back() {
        let mut compositor = idling(
            Config {
                credential: credential("idle-blank", None),
                credential_user: LOCK_ACCOUNT.into(),
                ..Config::default()
            },
            None,
            Some(60),
        );
        let start = started(&compositor);
        let mut panel = Panel::new();

        assert!(!compositor.apply_idle(start + Duration::from_secs(59), &mut panel));
        assert!(!compositor.is_blanked(), "blanked a second early");

        compositor.apply_idle(start + Duration::from_secs(60), &mut panel);
        assert!(compositor.is_blanked());
        assert_eq!(panel.told, vec![true]);
        // Staying idle does not tell it again.
        compositor.apply_idle(start + Duration::from_secs(600), &mut panel);
        assert_eq!(panel.told, vec![true]);

        compositor.handle_input(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('a'),
            tos_input::Modifiers::NONE,
        )));
        assert!(compositor.apply_idle(Instant::now(), &mut panel));
        assert!(!compositor.is_blanked());
        assert_eq!(panel.told, vec![true, false]);
        assert!(
            compositor.needs_full_redraw,
            "the screen came back without being repainted"
        );
    }

    #[test]
    fn the_key_that_wakes_the_screen_is_swallowed() {
        // super+space opens the launcher, which is a visible thing for a key
        // to have done. Typed at a dark screen it must do nothing at all.
        let mut compositor = idling(
            Config {
                credential: credential("idle-swallow", None),
                credential_user: LOCK_ACCOUNT.into(),
                ..Config::default()
            },
            None,
            Some(60),
        );
        let start = started(&compositor);
        let mut panel = Panel::new();
        compositor.apply_idle(start + Duration::from_secs(60), &mut panel);

        assert!(compositor.handle_input(InputEvent::Key(KeyEvent::new(
            KeyCode::Char(' '),
            tos_input::Modifiers::SUPER,
        ))));
        assert!(
            compositor.overlay().is_none(),
            "the key that woke the screen also ran a binding"
        );
    }

    #[test]
    fn a_screen_that_goes_dark_mid_drag_lets_go_of_what_the_mouse_was_holding() {
        // Everything a dark screen is sent, it gives to nobody — including
        // the release that would have ended a drag. A hand resting on the
        // button for the whole idle period is all it takes, and a grab that
        // survives the dark follows the pointer with no button held once the
        // screen comes back.
        let mut compositor = idling(
            Config {
                credential: credential("idle-grab", None),
                credential_user: LOCK_ACCOUNT.into(),
                ..Config::default()
            },
            None,
            Some(60),
        );
        compositor.handle_input(InputEvent::Pointer(tos_input::PointerEvent {
            x: 20.0,
            y: 20.0,
            button: Some(MouseButton::Left),
            action: MouseAction::Press,
            modifiers: tos_input::Modifiers::NONE,
        }));
        assert!(compositor.mouse_grab.is_some(), "the press grabbed nothing");
        let start = started(&compositor);
        let mut panel = Panel::new();

        compositor.apply_idle(start + Duration::from_secs(60), &mut panel);
        assert!(compositor.is_blanked());
        assert!(
            compositor.mouse_grab.is_none(),
            "the dark screen swallowed the release and kept the grab"
        );
        let focus = compositor.session.focus();
        assert!(!compositor.pane(focus).unwrap().selection_in_progress);
    }

    #[test]
    fn a_key_at_a_dark_locked_screen_is_not_the_first_of_the_password() {
        let mut compositor = idling(
            Config {
                credential: credential("idle-locked-dark", Some("tos")),
                credential_user: LOCK_ACCOUNT.into(),
                ..Config::default()
            },
            Some(60),
            Some(120),
        );
        let start = started(&compositor);
        let mut panel = Panel::new();
        compositor.apply_idle(start + Duration::from_secs(120), &mut panel);
        assert!(compositor.is_locked() && compositor.is_blanked());

        compositor.handle_input(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('t'),
            tos_input::Modifiers::NONE,
        )));
        assert_eq!(
            compositor.lock_screen().unwrap().typed_len(),
            0,
            "the key that woke the screen went into the password"
        );
        // The screen comes back, and the password typed at it works.
        compositor.apply_idle(Instant::now(), &mut panel);
        assert!(!compositor.is_blanked());
        answer(&mut compositor, "tos");
        assert!(!compositor.is_locked());
    }

    #[test]
    fn the_lock_comes_before_the_blank_when_both_are_due_at_once() {
        // Both deadlines land on the same pass, which is what happens when
        // they are given the same interval and when the loop was away. The
        // lock has to go up first: unblanking shows the last frame drawn, and
        // that frame must not be the session.
        let mut compositor = idling(
            Config {
                credential: credential("idle-together", Some("tos")),
                credential_user: LOCK_ACCOUNT.into(),
                ..Config::default()
            },
            Some(30),
            Some(30),
        );
        let start = started(&compositor);
        let mut panel = Panel::new();
        compositor.apply_idle(start + Duration::from_secs(30), &mut panel);
        assert!(compositor.is_locked(), "the screen went dark unlocked");
        assert!(compositor.is_blanked());
    }

    #[test]
    fn a_session_with_no_credential_blanks_and_does_not_lock() {
        // The live ISO. The failure this guards against is the one the design
        // refuses VT_LOCKSWITCH for: a machine that goes dark and then locks
        // with nothing able to open it.
        let mut compositor = idling(
            Config {
                credential: credential("idle-no-credential", None),
                credential_user: LOCK_ACCOUNT.into(),
                ..Config::default()
            },
            Some(30),
            Some(60),
        );
        let start = started(&compositor);
        let mut panel = Panel::new();
        for seconds in [30, 60, 90, 600] {
            compositor.apply_idle(start + Duration::from_secs(seconds), &mut panel);
        }
        assert!(!compositor.is_locked(), "locked with no way to unlock");
        assert!(
            compositor.is_blanked(),
            "a session with no password is not a session nobody left"
        );
        // And it says nothing about it. The binding answers the person who
        // pressed it; a deadline has nobody to answer.
        assert_eq!(compositor.notifications.status_line(), None);
        assert_eq!(panel.told, vec![true]);
    }

    #[test]
    fn a_deadline_that_is_never_never_comes() {
        let mut compositor = idling(
            Config {
                credential: credential("idle-off", Some("tos")),
                credential_user: LOCK_ACCOUNT.into(),
                ..Config::default()
            },
            None,
            None,
        );
        let start = started(&compositor);
        let mut panel = Panel::new();
        compositor.apply_idle(start + Duration::from_secs(86_400), &mut panel);
        assert!(!compositor.is_locked());
        assert!(!compositor.is_blanked());
        assert!(panel.told.is_empty());
        assert_eq!(compositor.next_idle_deadline(start), None);
    }

    #[test]
    fn the_wait_is_pulled_in_to_the_next_deadline() {
        let mut compositor = idling(
            Config {
                credential: credential("idle-timeout", None),
                credential_user: LOCK_ACCOUNT.into(),
                ..Config::default()
            },
            None,
            Some(10),
        );
        let start = started(&compositor);
        // Far from the deadline the wait is what it always was: the blink
        // phase still needs looking at.
        assert_eq!(compositor.frame_timeout_ms_at(start), IDLE_TIMEOUT_MS);
        // Near it, the deadline wins, so the screen goes dark on the ten
        // seconds rather than up to a tenth of a second afterwards.
        assert_eq!(
            compositor.frame_timeout_ms_at(start + Duration::from_millis(9_960)),
            40
        );
        // A deadline already past is not a wait of zero, which would be a
        // spin.
        assert_eq!(
            compositor.frame_timeout_ms_at(start + Duration::from_secs(11)),
            1
        );
        let mut panel = Panel::new();
        compositor.apply_idle(start + Duration::from_secs(10), &mut panel);
        // With the screen dark and nothing else to come, the loop stops
        // waking up to look at a screen nobody can see.
        assert_eq!(
            compositor.frame_timeout_ms_at(start + Duration::from_secs(10)),
            BLANKED_TIMEOUT_MS
        );
    }

    #[test]
    fn a_dark_screen_still_waits_for_the_lock_that_is_coming() {
        let mut compositor = idling(
            Config {
                credential: credential("idle-dark-wait", Some("tos")),
                credential_user: LOCK_ACCOUNT.into(),
                ..Config::default()
            },
            Some(60),
            Some(30),
        );
        let start = started(&compositor);
        let mut panel = Panel::new();
        compositor.apply_idle(start + Duration::from_secs(30), &mut panel);
        assert!(compositor.is_blanked() && !compositor.is_locked());
        assert_eq!(
            compositor.frame_timeout_ms_at(start + Duration::from_secs(30)),
            30_000,
            "a blanked session forgot the lock it still owes"
        );
        compositor.apply_idle(start + Duration::from_secs(60), &mut panel);
        assert!(
            compositor.is_locked(),
            "the lock deadline passed in the dark"
        );
        assert_eq!(panel.told, vec![true], "locking woke the screen up");
    }

    #[test]
    fn the_loop_itself_acts_on_the_deadlines() {
        // Every test above hands the clock in. This is the one that checks the
        // loop is wired to the deadlines at all, and it needs no clock of its
        // own: a millisecond has always gone by, because starting a pane takes
        // longer than that.
        let mut compositor = compositor_with(Config {
            credential: credential("idle-loop", Some("tos")),
            credential_user: LOCK_ACCOUNT.into(),
            idle_lock: Some(Duration::from_millis(1)),
            idle_blank: Some(Duration::from_millis(1)),
            ..Config::default()
        });
        let mut panel = Panel::new();
        compositor
            .run_once(&mut panel, &[], |_| Vec::new())
            .expect("a pass of the loop");
        assert!(compositor.is_locked(), "the loop ignored the lock deadline");
        assert!(
            compositor.is_blanked(),
            "the loop ignored the blank deadline"
        );
    }

    #[test]
    fn a_display_that_will_not_go_dark_is_said_and_not_died_of() {
        let mut compositor = idling(
            Config {
                credential: credential("idle-refused", None),
                credential_user: LOCK_ACCOUNT.into(),
                ..Config::default()
            },
            None,
            Some(60),
        );
        let start = started(&compositor);
        let mut panel = Panel::refusing();
        for seconds in [60, 61, 62, 600] {
            compositor.apply_idle(start + Duration::from_secs(seconds), &mut panel);
        }
        assert!(!compositor.is_blanked(), "a lit screen remembered as dark");
        assert!(compositor.is_running(), "a refused blank ended the session");
        assert_eq!(panel.told, vec![true], "asked a display that said no again");
        assert!(compositor
            .notifications
            .status_line()
            .unwrap_or_default()
            .contains("blanking the display"));
        // And the loop goes back to its ordinary wait rather than spinning on
        // a deadline that has passed and can never be met.
        assert_eq!(
            compositor.frame_timeout_ms_at(start + Duration::from_secs(600)),
            IDLE_TIMEOUT_MS
        );
    }

    #[test]
    fn output_from_a_pane_is_not_somebody_being_there() {
        // The `tail -f` case. A pane that goes on writing must not hold the
        // screen on, so nothing in the pump touches the idle clock.
        let mut compositor = idling(
            Config {
                credential: credential("idle-output", None),
                credential_user: LOCK_ACCOUNT.into(),
                ..Config::default()
            },
            None,
            Some(60),
        );
        let start = started(&compositor);
        compositor.inject(b"still here\r\n");
        compositor.pump_panes();
        compositor.tick();
        assert_eq!(started(&compositor), start, "a pane moved the idle clock");

        let mut panel = Panel::new();
        compositor.apply_idle(start + Duration::from_secs(60), &mut panel);
        assert!(compositor.is_blanked());
    }

    #[test]
    fn a_machine_with_no_sound_card_is_told_so_once() {
        let mut compositor = compositor();
        // The guard that makes the rest of this test safe to run: the config
        // above points the machine at a root that does not exist, so there is
        // nothing here that could turn the volume up on whoever is running
        // `cargo test`. If this ever stops holding, it fails here rather than
        // in the speakers.
        assert!(
            compositor.machine_mut().mixer().is_none(),
            "the test machine found a sound card"
        );

        assert!(compositor.perform(Action::VolumeUp), "said nothing at all");
        assert_eq!(
            compositor.notifications.status_line().as_deref(),
            Some("no sound card")
        );

        // Held down, or pressed again later: the answer has not changed, so
        // it is not repeated, and nothing asks for a frame on its account.
        for action in [Action::VolumeUp, Action::VolumeDown, Action::ToggleMute] {
            assert!(
                !compositor.perform(action.clone()),
                "{action:?} said it twice"
            );
        }
        assert_eq!(
            compositor.notifications.history().count(),
            1,
            "the queue filled up with the same sentence"
        );
    }

    #[test]
    fn the_volume_a_card_reports_is_what_reaches_the_bar() {
        // Muted wins over the level, because turning a muted card up is the
        // case where showing a percentage would look like it had worked.
        assert_eq!(
            volume_status(Volume {
                percent: 45,
                muted: false
            }),
            "volume 45%"
        );
        assert_eq!(
            volume_status(Volume {
                percent: 45,
                muted: true
            }),
            "muted"
        );
        assert_eq!(
            volume_status(Volume {
                percent: 0,
                muted: false
            }),
            "volume 0%"
        );
    }

    #[test]
    fn damage_is_cleared_after_a_frame() {
        let mut compositor = compositor();
        compositor.inject(b"hello");
        let mut framebuffer = tos_render::OwnedFramebuffer::new(640, 360);
        {
            let mut surface = framebuffer.surface();
            compositor.render_frame(&mut surface, true);
        }
        assert!(!compositor.needs_render());
    }

    // ---- bluetooth ------------------------------------------------------

    /// Run `tick` until something lands, which is what the frame loop does.
    ///
    /// Bounded, because a broken channel should fail the test rather than hang
    /// whoever is waiting for CI.
    fn tick_until(compositor: &mut Compositor, what: impl Fn(&Compositor) -> bool) {
        for _ in 0..1000 {
            compositor.tick();
            if what(compositor) {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("it never arrived");
    }

    #[test]
    fn the_bluetooth_binding_opens_the_controls() {
        // The config every test here uses points at a root with no hardware
        // under it, which is also the ordinary machine: most of them have no
        // adapter, and the menu has to say that rather than open empty.
        let mut compositor = compositor();
        compositor.handle_input(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('b'),
            tos_input::Modifiers::SUPER,
        )));
        let overlay = compositor.overlay().expect("the controls should be open");
        assert_eq!(overlay.title(), "bluetooth");
        let labels: Vec<&str> = overlay.items().iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, ["this machine has no Bluetooth"]);
    }

    #[test]
    fn choosing_a_row_that_is_only_there_to_be_read_does_nothing() {
        let mut compositor = compositor();
        assert!(compositor.perform(Action::ShowBluetooth));
        compositor.handle_input(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            tos_input::Modifiers::NONE,
        )));
        assert!(compositor.overlay().is_none(), "enter should close it");
        assert!(
            compositor.notifications.status_line().is_none(),
            "there was nothing to report"
        );
    }

    // ---- the status bar ---------------------------------------------------

    /// A session whose clock is in UT, so that a test asserting about what is
    /// on the bar is not asserting about where the machine running it is.
    fn compositor_showing(left: &[Segment], right: &[Segment]) -> Compositor {
        compositor_with(Config {
            status: status::Settings {
                left: left.to_vec(),
                right: right.to_vec(),
                zone: crate::clock::Zone::Utc,
                ..status::Settings::default()
            },
            ..Config::default()
        })
    }

    /// A timestamp exactly on a minute, so that "thirty seconds later" is
    /// unambiguously still the same one. 15:34 UT on the 4th of September
    /// 2025, which is a Thursday and of no significance whatever.
    const ON_THE_MINUTE: i64 = 1_757_000_040;

    #[test]
    fn the_bar_names_the_arrangement_only_once_it_is_not_the_tree() {
        let mut compositor = compositor_showing(&[Segment::Layout], &[]);
        // Splits says nothing: a word that is on the bar in every session
        // that ever runs tells nobody anything, and the segment appearing is
        // itself the news.
        assert_eq!(compositor.status_bar().text().trim(), "");

        compositor.perform(Action::NextLayout);
        assert_eq!(compositor.status_bar().text().trim(), "tall");
        compositor.perform(Action::NextLayout);
        assert_eq!(compositor.status_bar().text().trim(), "fat");
        compositor.perform(Action::PreviousLayout);
        compositor.perform(Action::PreviousLayout);
        assert_eq!(compositor.status_bar().text().trim(), "");
    }

    #[test]
    fn a_minute_turning_over_puts_a_new_time_on_the_bar_and_asks_for_a_frame() {
        let mut compositor = compositor_showing(&[Segment::Workspaces], &[Segment::Clock]);
        compositor.tick_clock(ON_THE_MINUTE);
        let before = compositor.status_bar().text();
        assert!(before.contains("15:34"), "{before}");

        // Half a minute later nothing on the bar has changed, so nothing asks
        // for a frame: a clock showing minutes must not repaint every second.
        assert!(
            !compositor.tick_clock(ON_THE_MINUTE + 30),
            "the same minute was treated as news"
        );
        assert_eq!(compositor.status_bar().text(), before);

        // And on the minute it does both.
        assert!(
            compositor.tick_clock(ON_THE_MINUTE + 60),
            "the minute turned over and nothing asked for a frame"
        );
        let after = compositor.status_bar().text();
        assert!(after.contains("15:35"), "{after}");
    }

    #[test]
    fn the_loop_wakes_up_often_enough_to_notice_the_minute() {
        // The other half of a clock that ticks. The trigger above only fires
        // if something calls `tick`, and what calls it is the frame loop
        // coming back — which it does for the machine poll whether or not
        // anything has happened, because that deadline is folded into the
        // wait. A second is the coarsest this may be and still land a `%M`
        // clock on the right side of a minute boundary.
        let compositor = compositor_showing(&[Segment::Workspaces], &[Segment::Clock]);
        let wait = compositor.frame_timeout_ms_at(Instant::now());
        assert!(
            wait > 0 && wait <= 1000,
            "the loop would sleep for {wait}ms"
        );
    }

    #[test]
    fn what_a_scan_found_arrives_through_the_tick_and_into_the_open_menu() {
        // The whole point of running the inquiry elsewhere: the answer has to
        // find its way back on to the thread that draws, without that thread
        // ever having waited for it.
        let mut compositor = compositor();
        assert!(compositor.perform(Action::ShowBluetooth));
        compositor
            .bluetooth
            .begin(crate::bluetooth::scan_that_found(vec![
                tos_system::bluetooth::Discovered {
                    address: "11:22:33:44:55:66".into(),
                    class: 0x240404,
                },
            ]));

        tick_until(&mut compositor, |compositor| {
            !compositor.bluetooth.is_scanning()
        });
        assert_eq!(
            compositor.notifications.status_line().as_deref(),
            Some("bluetooth: one device")
        );
        let overlay = compositor.overlay().expect("the menu should still be open");
        assert!(
            overlay
                .items()
                .iter()
                .any(|item| item.label == "11:22:33:44:55:66"),
            "the menu was not rebuilt: {:?}",
            overlay.items()
        );
    }

    // ---- the network menu -----------------------------------------------

    /// The interface name every test below uses.
    ///
    /// Not `eth0`. The sysfs half of these tests is a directory of text files,
    /// but the ioctls are not faked: [`crate::system::Machine`] holds a real
    /// `SystemKernel`, so anything that asks the kernel to change a link asks
    /// the kernel the test is running on. A name no machine has means those
    /// calls fail with `ENODEV` before they touch anything, which is both what
    /// makes the failure path assertable and what makes running the suite
    /// safe on a machine with an `eth0` on it.
    const FAKE_LINK: &str = "tosfake0";

    /// And the radio, for the same reason: no machine has one of these either,
    /// so every ioctl aimed at it fails with `ENODEV` before it touches
    /// anything.
    const FAKE_RADIO: &str = "tosfakewl0";

    /// A directory laid out like a machine with one wired interface in it,
    /// which cleans up after itself.
    struct FakeMachine {
        root: std::path::PathBuf,
    }

    impl FakeMachine {
        fn new(name: &str) -> FakeMachine {
            let root = std::env::temp_dir().join(format!(
                "tos-compositor-net-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).expect("temp dir");
            FakeMachine { root }
        }

        fn file(&self, path: &str, contents: &str) -> &FakeMachine {
            let full = self.root.join(path.trim_start_matches('/'));
            std::fs::create_dir_all(full.parent().expect("parent")).expect("dirs");
            std::fs::write(full, contents).expect("write");
            self
        }

        /// A wired interface that is administratively down.
        ///
        /// The `device/uevent` file is what tells a real interface from a
        /// bridge, and without it this is classified as virtual and never
        /// reaches the menu at all.
        fn with_wired_link(&self, mac: &str) -> &FakeMachine {
            let dir = format!("/sys/class/net/{FAKE_LINK}");
            self.file(&format!("{dir}/flags"), "0x1002\n")
                .file(&format!("{dir}/type"), "1\n")
                .file(&format!("{dir}/operstate"), "down\n")
                .file(&format!("{dir}/carrier"), "0\n")
                .file(&format!("{dir}/address"), &format!("{mac}\n"))
                .file(&format!("{dir}/device/uevent"), "")
        }

        /// A radio, administratively down and associated with nothing.
        ///
        /// `DEVTYPE=wlan` in `uevent` is one of the three things
        /// `net::kind_of` reads a wireless interface out of — the other two
        /// are a `wireless/` and a `phy80211/` directory — and it is the one
        /// that is a file, which is what this fixture can write. A separate
        /// name from [`FAKE_LINK`], so that a machine can have a cable and a
        /// radio in it and a test can say which menu it means.
        fn with_wireless_link(&self, mac: &str) -> &FakeMachine {
            let dir = format!("/sys/class/net/{FAKE_RADIO}");
            self.file(&format!("{dir}/flags"), "0x1002\n")
                .file(&format!("{dir}/type"), "1\n")
                .file(&format!("{dir}/operstate"), "down\n")
                .file(&format!("{dir}/carrier"), "0\n")
                .file(&format!("{dir}/address"), &format!("{mac}\n"))
                .file(&format!("{dir}/device/uevent"), "DRIVER=iwlwifi\n")
                .file(
                    &format!("{dir}/uevent"),
                    &format!("DEVTYPE=wlan\nINTERFACE={FAKE_RADIO}\n"),
                )
        }

        /// The same interface with the switch on and a cable in it.
        ///
        /// `0x1` is `IFF_UP`, which is what `admin_up` reads, and `carrier`
        /// is the file the kernel will not let anybody read on a link that is
        /// down — so a fixture that says both is a link somebody has already
        /// brought up and plugged in.
        fn with_carrying_link(&self, mac: &str) -> &FakeMachine {
            self.with_wired_link(mac);
            let dir = format!("/sys/class/net/{FAKE_LINK}");
            self.file(&format!("{dir}/flags"), "0x1003\n")
                .file(&format!("{dir}/operstate"), "up\n")
                .file(&format!("{dir}/carrier"), "1\n")
        }

        /// A compositor whose every reader is pointed at this directory.
        ///
        /// Not [`compositor_with`], which pins `system_root` at a path that
        /// does not exist so that no other test can see the developer's own
        /// hardware. That is exactly the right default and exactly wrong
        /// here, so the rest of what it fills in is repeated rather than
        /// loosened for everybody.
        fn compositor(&self) -> Compositor {
            let config = Config {
                command: Some(vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()]),
                bitmap_scale: Some(1),
                font: Some("/nonexistent-so-the-bitmap-font-is-used".into()),
                system_root: self.root.clone(),
                ..Config::default()
            };
            Compositor::new(config, (640, 360), None).expect("compositor")
        }
    }

    impl Drop for FakeMachine {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn a_machine_with_no_interfaces_says_so_rather_than_opening_an_empty_menu() {
        let mut compositor = compositor();
        compositor.perform_action(Action::ShowNetworks);
        assert!(compositor.overlay().is_none(), "an empty menu went up");
        assert_eq!(
            compositor.notifications.status_line().as_deref(),
            Some("no wired or wireless interfaces")
        );
    }

    #[test]
    fn a_clock_behind_a_dark_screen_keeps_time_without_asking_for_frames() {
        let mut compositor = compositor_showing(&[], &[Segment::Clock]);
        compositor.tick_clock(ON_THE_MINUTE);
        compositor.blanked = true;
        assert!(
            !compositor.tick_clock(ON_THE_MINUTE + 60),
            "a minute nobody can see is not a reason to paint"
        );
        // But it is up to date for the frame the screen comes back on.
        assert!(compositor.status_bar().text().contains("15:35"));
    }

    #[test]
    fn a_bar_with_no_clock_on_it_never_wakes_up_for_one() {
        let mut compositor = compositor_showing(&[Segment::Workspaces], &[Segment::Message]);
        assert!(!compositor.tick_clock(ON_THE_MINUTE));
        assert!(!compositor.tick_clock(ON_THE_MINUTE + 3600));
    }

    /// Press the left button on a cell of the status row.
    ///
    /// Through the device pointer, because what these tests name is a cell of
    /// the compositor's own grid and the pointer is the input that is one
    /// multiplication away from it. A host terminal's cells are a different
    /// grid — see the `Mouse` arm of `handle_input` — and saying `col` to a
    /// `MouseEvent` here would have been asking about that conversion instead
    /// of about the bar.
    fn click_bar(compositor: &mut Compositor, col: u32) -> bool {
        let row = compositor.status_row().expect("a status row");
        let (cw, ch) = compositor.cell_size();
        let x = (col * cw + cw / 2) as f64;
        let y = (row * ch + ch / 2) as f64;
        let event = |button, action| {
            InputEvent::Pointer(tos_input::PointerEvent {
                x,
                y,
                button,
                action,
                modifiers: tos_input::Modifiers::NONE,
            })
        };
        // The hand arrives before it presses. `handle_input` answers "a frame
        // is owed", and bringing the arrow to a place it has not been owes one
        // by itself — so a press sent cold reports `true` whatever the bar did
        // with it, and every `assert!(click_bar(..))` below would hold just as
        // well against a bar that dropped the click. The move spends that
        // frame, and the press is then answering for itself.
        compositor.handle_input(event(None, MouseAction::Motion));
        compositor.handle_input(event(Some(MouseButton::Left), MouseAction::Press))
    }

    #[test]
    fn clicking_a_workspace_on_the_bar_switches_to_it() {
        let mut compositor = compositor();
        compositor.perform(Action::NewWorkspace);
        compositor.perform(Action::NewWorkspace);
        assert_eq!(compositor.session.active_index(), 2);

        // The strip reads " 1  2  3 ", so the first workspace is cell one.
        assert!(click_bar(&mut compositor, 1), "the click did nothing");
        assert_eq!(compositor.session.active_index(), 0);
        // And the third is six cells along.
        assert!(click_bar(&mut compositor, 7));
        assert_eq!(compositor.session.active_index(), 2);
    }

    #[test]
    fn clicking_the_workspace_already_active_is_not_a_change() {
        // Every press used to be dropped here, so "nothing happened" is not
        // evidence on its own; this is the one press that should still be it.
        let mut compositor = compositor();
        assert!(!click_bar(&mut compositor, 1));
        assert_eq!(compositor.session.active_index(), 0);
    }

    #[test]
    fn a_press_on_the_bar_never_reaches_a_pane() {
        let mut compositor = compositor();
        let focus = compositor.session.focus();
        click_bar(&mut compositor, 1);
        assert!(
            compositor.panes[&focus].selection.is_none(),
            "the bar started a selection in a pane"
        );
    }

    #[test]
    fn clicking_an_unfocused_pane_on_the_strip_focuses_it() {
        // The pane strip is what gives an unfocused pane a name anywhere; this
        // is what makes the name worth clicking on.
        let mut compositor = compositor_showing(&[Segment::Panes], &[]);
        compositor.perform(Action::Split(Axis::Columns));
        let focus = compositor.session.focus();
        let panes = compositor.session.active().panes();
        let (index, other) = panes
            .iter()
            .enumerate()
            .find(|(_, id)| **id != focus)
            .map(|(index, id)| (index, *id))
            .expect("a pane that is not focused");
        // Every label is " n:shell ", which is nine cells.
        assert!(
            click_bar(&mut compositor, index as u32 * 9 + 1),
            "the label was not clickable"
        );
        assert_eq!(compositor.session.focus(), other);
    }

    #[test]
    fn an_unfocused_pane_is_named_on_the_strip_and_nowhere_else() {
        let mut compositor = compositor_showing(&[Segment::Panes], &[Segment::Title]);
        compositor.perform(Action::Split(Axis::Columns));
        let focus = compositor.session.focus();
        compositor.panes.get_mut(&focus).expect("a pane").title = "editing".to_string();
        let text = compositor.status_bar().text();
        // Both panes are on the strip, and the title segment says only one.
        assert!(text.contains("1:shell"), "{text}");
        assert!(text.contains("2:editing"), "{text}");
    }

    #[test]
    fn the_bar_can_be_hidden_and_brought_back_and_the_panes_follow() {
        let mut compositor = compositor();
        let with_bar = compositor.grid_area().height;
        let focus = compositor.session.focus();
        let rows = compositor.panes[&focus].terminal.rows();

        assert!(compositor.perform(Action::ToggleStatusBar));
        assert_eq!(compositor.status_row(), None, "the bar is still there");
        assert_eq!(compositor.grid_area().height, with_bar + 1);
        assert_eq!(
            compositor.panes[&focus].terminal.rows(),
            rows + 1,
            "the pane was not given the row back"
        );

        assert!(compositor.perform(Action::ToggleStatusBar));
        assert_eq!(compositor.status_row(), Some(with_bar));
        assert_eq!(compositor.panes[&focus].terminal.rows(), rows);
    }

    #[test]
    fn a_machine_with_none_of_the_hardware_shows_empty_slots_rather_than_lies() {
        // The test compositor's system root has no battery, no link, no card
        // and no adapter, which is also a perfectly ordinary machine.
        let compositor = compositor_showing(
            &[Segment::Workspaces],
            &[
                Segment::Battery,
                Segment::Network,
                Segment::Volume,
                Segment::Bluetooth,
                Segment::Clock,
            ],
        );
        let text = compositor.status_bar().text();
        for word in ["bat", "vol", "bt", "unknown", "none"] {
            assert!(!text.contains(word), "{text} claims {word}");
        }
        // And the rules that would have gone between them are not there
        // either: four missing segments must not leave four separators.
        let rules = compositor
            .status_bar()
            .pieces()
            .iter()
            .filter(|piece| piece.ink == status::Ink::Divider)
            .count();
        assert_eq!(rules, 0, "{text}");
    }

    #[test]
    fn a_reading_from_the_machine_reaches_the_bar() {
        let mut compositor = compositor_showing(&[], &[Segment::Battery, Segment::Volume]);
        compositor
            .machine_mut()
            .set_reading(crate::system::Reading {
                power: Some(tos_system::power::PowerState {
                    batteries: vec![tos_system::power::Battery {
                        name: "BAT0".into(),
                        present: true,
                        state: tos_system::power::ChargeState::Discharging,
                        percent: Some(41),
                        remaining: None,
                        full: None,
                        unit: None,
                        time_remaining: None,
                        power_watts: None,
                    }],
                    mains: Vec::new(),
                }),
                volume: Some(tos_system::audio::Volume {
                    percent: 70,
                    muted: false,
                }),
                ..crate::system::Reading::default()
            });
        let text = compositor.status_bar().text();
        assert!(text.contains("41%"), "{text}");
        assert!(text.contains("vol 70%"), "{text}");
    }

    #[test]
    fn a_layout_with_no_message_segment_still_shows_what_went_wrong() {
        // Otherwise arranging the bar to taste would be a way to make every
        // failure the compositor reports disappear.
        let mut compositor = compositor_showing(&[Segment::Workspaces], &[Segment::Clock]);
        compositor.notifications.status("copied");
        let mut framebuffer = tos_render::OwnedFramebuffer::new(640, 360);
        {
            let mut surface = framebuffer.surface();
            compositor.render_frame(&mut surface, false);
        }
        // The banner is drawn in the accent, over the top of the panes, which
        // is what `--no-status-bar` already does.
        let accent = compositor.chrome.accent.pack();
        let (_, ch) = compositor.cell_size();
        let first_row = (ch * 640) as usize;
        assert!(
            framebuffer.pixels()[..first_row].contains(&accent),
            "the message was drawn nowhere"
        );
    }

    #[test]
    fn the_network_menu_lists_a_link_with_what_state_it_is_in() {
        let fake = FakeMachine::new("list");
        fake.with_wired_link("aa:bb:cc:dd:ee:01");
        let mut compositor = fake.compositor();

        compositor.perform_action(Action::ShowNetworks);
        let overlay = compositor.overlay().expect("the network menu");
        assert_eq!(overlay.title(), "network");
        let row = overlay.items().first().expect("a row");
        assert_eq!(row.label, FAKE_LINK);
        assert!(row.detail.contains("wired"), "no kind: {}", row.detail);
        assert!(row.detail.contains("down"), "no state: {}", row.detail);
    }

    #[test]
    fn choosing_a_link_offers_what_can_be_done_to_it() {
        let fake = FakeMachine::new("actions");
        fake.with_wired_link("aa:bb:cc:dd:ee:01");
        let mut compositor = fake.compositor();

        compositor.choose(OverlayKind::Networks, Some(0), FAKE_LINK);
        let overlay = compositor.overlay().expect("the link menu");
        let labels: Vec<&str> = overlay
            .items()
            .iter()
            .map(|item| item.label.as_str())
            .collect();
        // Down, so bringing it up is offered and taking it down is not.
        assert_eq!(labels, vec![BRING_UP, REQUEST_ADDRESS]);
        assert_eq!(
            compositor.network_target.as_deref(),
            Some(FAKE_LINK),
            "the menu forgot which link it is about"
        );
    }

    #[test]
    fn a_link_that_went_away_between_the_two_menus_is_said_rather_than_acted_on() {
        let fake = FakeMachine::new("vanished");
        let mut compositor = fake.compositor();
        compositor.choose(OverlayKind::Networks, Some(0), FAKE_LINK);
        assert!(compositor.overlay().is_none());
        assert_eq!(
            compositor.notifications.status_line().as_deref(),
            Some("tosfake0 is no longer there")
        );
    }

    #[test]
    fn an_ioctl_the_kernel_refuses_is_put_on_the_status_line() {
        // The live ISO's whole situation, and an ordinary user's: the menu
        // opens, the row is there, and the kernel says no. What must not
        // happen is that it silently does nothing.
        let fake = FakeMachine::new("refused");
        fake.with_wired_link("aa:bb:cc:dd:ee:01");
        let mut compositor = fake.compositor();

        compositor.network_target = Some(FAKE_LINK.to_string());
        compositor.choose(OverlayKind::Link, Some(0), BRING_UP);

        let said = compositor
            .notifications
            .status_line()
            .expect("it said nothing at all");
        assert!(said.starts_with("tosfake0 up failed:"), "unhelpful: {said}");
    }

    #[test]
    fn a_link_with_no_hardware_address_is_not_asked_to_run_a_dhcp_client() {
        let fake = FakeMachine::new("nomac");
        fake.with_wired_link("00:00:00:00:00:00");
        let mut compositor = fake.compositor();

        compositor.network_target = Some(FAKE_LINK.to_string());
        compositor.choose(OverlayKind::Link, Some(1), REQUEST_ADDRESS);

        assert!(compositor.dhcp.is_none(), "a thread was started anyway");
        assert_eq!(
            compositor.notifications.status_line().as_deref(),
            Some("tosfake0 has no hardware address to ask from")
        );
    }

    #[test]
    fn a_dhcp_request_that_cannot_even_open_a_socket_comes_back_saying_so() {
        // The socket is bound to the device by name, so an interface that is
        // not there fails at the bind whether or not this has CAP_NET_RAW —
        // which is what makes the whole path, thread included, assertable
        // without any privilege and without touching real networking.
        let fake = FakeMachine::new("socket");
        fake.with_wired_link("aa:bb:cc:dd:ee:01");
        let mut compositor = fake.compositor();

        compositor.network_target = Some(FAKE_LINK.to_string());
        compositor.choose(OverlayKind::Link, Some(1), REQUEST_ADDRESS);
        assert!(compositor.dhcp.is_some(), "nothing was started");
        assert_eq!(
            compositor.notifications.status_line().as_deref(),
            Some("asking for an address on tosfake0")
        );

        // The answer is collected by the tick the frame loop already does.
        let deadline = Instant::now() + Duration::from_secs(5);
        while compositor.dhcp.is_some() && Instant::now() < deadline {
            compositor.tick();
        }
        assert!(compositor.dhcp.is_none(), "the answer never arrived");
        // The history rather than the status line: the line still holds the
        // "asking" notification, which has not been on screen long enough to
        // be retired, and the answer is queued behind it.
        let said: Vec<String> = compositor
            .notifications
            .history()
            .map(|notification| notification.text())
            .collect();
        assert!(
            said.iter().any(|line| line.starts_with("tosfake0: ")),
            "it never said what happened: {said:?}"
        );
    }

    #[test]
    fn a_link_nobody_is_there_to_bring_up_is_brought_up_by_the_machine() {
        // #124: the whole point is that no key is pressed. The fixture link
        // is not a real device, so the ioctl fails — and that failure is the
        // proof it was attempted, the same way it is for the menu row above.
        let fake = FakeMachine::new("autoup");
        fake.with_wired_link("aa:bb:cc:dd:ee:01");
        let mut compositor = fake.compositor();

        compositor.tick();

        let said = compositor
            .notifications
            .status_line()
            .expect("nothing was tried");
        assert!(said.starts_with("tosfake0 up failed:"), "unhelpful: {said}");
    }

    #[test]
    fn a_link_that_is_up_and_carrying_is_asked_for_an_address_by_the_machine() {
        let fake = FakeMachine::new("autoask");
        fake.with_carrying_link("aa:bb:cc:dd:ee:01");
        let mut compositor = fake.compositor();

        compositor.tick();

        assert!(compositor.dhcp.is_some(), "nobody asked for an address");
        assert_eq!(
            compositor.notifications.status_line().as_deref(),
            Some("asking for an address on tosfake0")
        );

        // Collect the answer rather than leaving the thread to outlive the
        // test; the bind fails, which is the socket test above.
        let deadline = Instant::now() + Duration::from_secs(5);
        while compositor.dhcp.is_some() && Instant::now() < deadline {
            compositor.tick();
        }
        assert!(compositor.dhcp.is_none(), "the answer never arrived");
    }

    #[test]
    fn a_link_is_only_asked_once_however_many_frames_go_by() {
        // A machine on a network with no DHCP server would otherwise spend
        // every second of its life in a fifteen second conversation with
        // nobody, and say so on the status line every time.
        let fake = FakeMachine::new("autoonce");
        fake.with_wired_link("aa:bb:cc:dd:ee:01");
        let mut compositor = fake.compositor();

        let start = Instant::now();
        for second in 0..5 {
            compositor.tick_at(start + Duration::from_secs(second));
        }

        let tries = compositor
            .notifications
            .history()
            .filter(|notification| notification.text().starts_with("tosfake0 up failed:"))
            .count();
        assert_eq!(tries, 1, "it kept asking a kernel that had said no");
    }

    #[test]
    fn a_blanked_machine_still_gets_itself_on_to_the_network() {
        // The one place in tOS that looks at a machine whose screen is dark,
        // because a machine with nobody at it is what this is for: a cable
        // plugged into a server ten minutes after it booted must not wait for
        // a keystroke nobody is coming to make.
        let fake = FakeMachine::new("autoblank");
        fake.with_wired_link("aa:bb:cc:dd:ee:01");
        let mut compositor = fake.compositor();
        compositor.blanked = true;

        let start = Instant::now();
        compositor.tick_at(start);
        let said = compositor
            .notifications
            .status_line()
            .expect("a dark screen meant a machine left off the network");
        assert!(said.starts_with("tosfake0 up failed:"), "unhelpful: {said}");
    }

    #[test]
    fn a_blanked_machine_is_looked_at_once_a_minute_rather_than_every_second() {
        let fake = FakeMachine::new("autoslow");
        fake.with_wired_link("aa:bb:cc:dd:ee:01");
        let mut compositor = fake.compositor();
        compositor.blanked = true;

        let start = Instant::now();
        compositor.tick_at(start);
        compositor.auto_looked_at = Some(start);

        compositor.tick_at(start + Duration::from_secs(59));
        assert_eq!(
            compositor.auto_looked_at,
            Some(start),
            "a dark screen was read every second after all"
        );
        compositor.tick_at(start + Duration::from_secs(60));
        assert_ne!(
            compositor.auto_looked_at,
            Some(start),
            "a dark screen was never read again"
        );
    }

    #[test]
    fn the_bar_takes_its_colours_from_the_configuration() {
        let bar = tos_term::Rgb::new(0x22, 0x00, 0x44);
        let mut compositor = compositor_with(Config {
            chrome: Chrome {
                status_background: Some(bar),
                ..Chrome::default()
            },
            ..Config::default()
        });
        let mut framebuffer = tos_render::OwnedFramebuffer::new(640, 360);
        {
            let mut surface = framebuffer.surface();
            compositor.render_frame(&mut surface, false);
        }
        let (_, ch) = compositor.cell_size();
        let above = (compositor.grid_area().height * ch * 640) as usize;
        assert!(
            framebuffer.pixels()[above..].contains(&bar.pack()),
            "the bar is not the colour it was asked to be"
        );
        // And nothing above it moved: this colour is the bar's alone.
        assert!(!framebuffer.pixels()[..above].contains(&bar.pack()));
    }

    #[test]
    fn a_link_detail_says_the_kind_then_the_network_then_the_address() {
        let interface = Interface {
            name: "wlan0".into(),
            kind: Kind::Wireless,
            state: tos_system::net::LinkState::Up,
            carrier: true,
            admin_up: true,
            mac: None,
            mtu: None,
            speed_mbps: None,
            addresses: vec![tos_system::net::Address::parse("192.168.1.5/24").expect("an address")],
            is_default: true,
            gateway: None,
            rx_bytes: 0,
            tx_bytes: 0,
            wireless: Some(tos_system::net::Wireless {
                ssid: Some("kitchen-table".into()),
                link_quality: None,
                signal_dbm: None,
            }),
        };
        assert_eq!(
            link_detail(&interface),
            "wireless  kitchen-table  192.168.1.5/24  default route"
        );

        // The same link with the cable out: the reason there is no address is
        // more use than the absence of one.
        let unplugged = Interface {
            carrier: false,
            addresses: Vec::new(),
            is_default: false,
            wireless: None,
            kind: Kind::Wired,
            ..interface
        };
        assert_eq!(link_detail(&unplugged), "wired  no carrier");
    }

    // ---- the wireless menus (#137) ---------------------------------------

    use std::cell::RefCell;
    use std::rc::Rc;
    use tos_system::net::wpa::{RecordingSupplicant, Supplicant};

    /// `SCAN_RESULTS` as the `mac80211_hwsim` witness in `docs/design/wifi.md`
    /// dumps it, with everything the rows have to cope with in it: a network
    /// on two bands, one that wants nothing, one that wants an identity and a
    /// certificate, and one that is not broadcasting its name.
    const IN_RANGE: &str = concat!(
        "bssid / frequency / signal level / flags / ssid\n",
        "02:00:00:00:01:00\t2412\t-30\t[WPA2-PSK-CCMP][ESS]\tkitchen-table\n",
        "02:00:00:00:02:00\t5180\t-52\t[ESS]\tcafe\n",
        "02:00:00:00:03:00\t5220\t-45\t[WPA2-PSK-CCMP][ESS]\tkitchen-table\n",
        "02:00:00:00:04:00\t2437\t-67\t[WPA2-EAP-CCMP][ESS]\toffice\n",
        "02:00:00:00:05:00\t2462\t-40\t[WPA2-PSK-CCMP][ESS]\t\n",
    );

    /// `STATUS` on a radio that is on `kitchen-table`.
    const ON_A_NETWORK: &str = concat!(
        "bssid=02:00:00:00:01:00\n",
        "freq=2412\n",
        "ssid=kitchen-table\n",
        "id=0\n",
        "wpa_state=COMPLETED\n",
    );

    /// And on one that is on nothing, which has no `id=` line at all.
    const ON_NOTHING: &str = "wpa_state=DISCONNECTED\n";

    /// `STATUS` half way through a join, which is where a wrong passphrase is
    /// found out.
    const HANDSHAKING: &str = "ssid=kitchen-table\nid=0\nwpa_state=4WAY_HANDSHAKE\n";

    const NO_NETWORKS: &str = "network id / ssid / bssid / flags\n";

    /// A supplicant the compositor's several short conversations all share.
    ///
    /// Every row that needs one opens a client and drops it again, so a table
    /// handed over once would be four tables and four empty transcripts. This
    /// is one table behind an `Rc`, which the opener hands out a fresh box of
    /// each time and the test reads the whole transcript out of at the end.
    struct SharedSupplicant(Rc<RefCell<RecordingSupplicant>>);

    impl Supplicant for SharedSupplicant {
        fn request(&mut self, command: &str) -> io::Result<String> {
            self.0.borrow_mut().request(command)
        }
    }

    /// Point a compositor's radio at `table`, and hand back the table.
    fn talking_to(
        compositor: &mut Compositor,
        table: RecordingSupplicant,
    ) -> Rc<RefCell<RecordingSupplicant>> {
        let shared = Rc::new(RefCell::new(table));
        let opener = shared.clone();
        compositor.wifi.set_opener(Box::new(move |_| {
            Ok(Box::new(SharedSupplicant(opener.clone())) as Box<dyn Supplicant>)
        }));
        shared
    }

    /// A machine where `apt remove wpasupplicant` has happened.
    fn with_no_supplicant(compositor: &mut Compositor) {
        compositor.wifi.set_opener(Box::new(|interface| {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("/run/wpa_supplicant/{interface}: no such file or directory"),
            ))
        }));
    }

    fn said(shared: &Rc<RefCell<RecordingSupplicant>>) -> Vec<String> {
        shared.borrow().transcript()
    }

    fn labels(compositor: &Compositor) -> Vec<String> {
        compositor
            .overlay()
            .expect("a menu")
            .items()
            .iter()
            .map(|item| item.label.clone())
            .collect()
    }

    /// A machine with a radio in it, talking to `table`.
    fn on_the_radio(
        fake: &FakeMachine,
        table: RecordingSupplicant,
    ) -> (Compositor, Rc<RefCell<RecordingSupplicant>>) {
        fake.with_wireless_link("aa:bb:cc:dd:ee:02");
        let mut compositor = fake.compositor();
        let shared = talking_to(&mut compositor, table);
        (compositor, shared)
    }

    /// A table that answers a scan and then the whole of a join of
    /// `kitchen-table`.
    fn joining_table() -> RecordingSupplicant {
        RecordingSupplicant::new()
            .answering("SCAN_RESULTS", IN_RANGE)
            .ok("SCAN")
            .answering("LIST_NETWORKS", NO_NETWORKS)
            .answering("ADD_NETWORK", "0")
            .ok("SET_NETWORK 0 ssid 6b69746368656e2d7461626c65")
            .ok("SET_NETWORK 0 psk \"correct horse battery staple\"")
            .ok("ENABLE_NETWORK 0")
            .ok("SELECT_NETWORK 0")
            .ok("SAVE_CONFIG")
    }

    /// Press the join row of the radio's link menu.
    fn open_the_list(compositor: &mut Compositor) {
        compositor.network_target = Some(FAKE_RADIO.to_string());
        compositor.choose(OverlayKind::Link, Some(0), JOIN);
    }

    /// Press enter on row `index` of the open menu.
    ///
    /// Through [`Compositor::overlay_outcome`] rather than straight into
    /// `choose`, because closing the box is half of what choosing a row does:
    /// a test that skipped it would be asserting about a menu the keystroke
    /// had already taken down.
    fn press(compositor: &mut Compositor, index: usize) {
        compositor.overlay_outcome(OverlayOutcome::Chosen(index));
    }

    /// Type a passphrase into the prompt that is up, and press enter.
    fn answer_the_prompt(compositor: &mut Compositor, passphrase: &str) {
        for character in passphrase.chars() {
            compositor.overlay_key(&KeyEvent::new(
                KeyCode::Char(character),
                tos_input::Modifiers::NONE,
            ));
        }
        compositor.overlay_key(&KeyEvent::new(KeyCode::Enter, tos_input::Modifiers::NONE));
    }

    #[test]
    fn a_radio_that_is_on_a_network_can_leave_it_or_forget_it_by_name() {
        let fake = FakeMachine::new("wifi-associated");
        let (mut compositor, _shared) = on_the_radio(
            &fake,
            RecordingSupplicant::new().answering("STATUS", ON_A_NETWORK),
        );

        compositor.choose(OverlayKind::Networks, Some(0), FAKE_RADIO);
        assert_eq!(
            labels(&compositor),
            vec![
                BRING_UP,
                REQUEST_ADDRESS,
                "leave kitchen-table",
                "forget kitchen-table",
                JOIN,
            ]
        );
    }

    #[test]
    fn a_radio_that_is_on_nothing_is_offered_the_list_and_nothing_else() {
        let fake = FakeMachine::new("wifi-idle");
        let (mut compositor, _shared) = on_the_radio(
            &fake,
            RecordingSupplicant::new().answering("STATUS", ON_NOTHING),
        );

        compositor.choose(OverlayKind::Networks, Some(0), FAKE_RADIO);
        assert_eq!(labels(&compositor), vec![BRING_UP, REQUEST_ADDRESS, JOIN]);
    }

    #[test]
    fn a_machine_with_no_supplicant_says_so_rather_than_offering_a_row_that_fails() {
        let fake = FakeMachine::new("wifi-nodaemon");
        fake.with_wireless_link("aa:bb:cc:dd:ee:02");
        let mut compositor = fake.compositor();
        with_no_supplicant(&mut compositor);

        compositor.choose(OverlayKind::Networks, Some(0), FAKE_RADIO);
        let (label, detail) = {
            let row = compositor
                .overlay()
                .expect("the link menu")
                .items()
                .last()
                .expect("a row");
            (row.label.clone(), row.detail.clone())
        };
        assert_eq!(label, "no supplicant on tosfakewl0");
        assert_eq!(detail, "is wpasupplicant installed?");

        // And pressing it says the one thing anybody can do about it.
        compositor.network_target = Some(FAKE_RADIO.to_string());
        compositor.choose(OverlayKind::Link, Some(2), &label);
        assert_eq!(
            compositor.notifications.status_line().as_deref(),
            Some("is wpasupplicant installed?")
        );
    }

    #[test]
    fn the_wired_rows_are_untouched_by_any_of_this() {
        let fake = FakeMachine::new("wifi-wired");
        fake.with_wired_link("aa:bb:cc:dd:ee:01");
        let mut compositor = fake.compositor();
        talking_to(&mut compositor, RecordingSupplicant::new());

        compositor.choose(OverlayKind::Networks, Some(0), FAKE_LINK);
        assert_eq!(labels(&compositor), vec![BRING_UP, REQUEST_ADDRESS]);
    }

    #[test]
    fn opening_the_list_asks_for_a_scan_and_shows_what_the_radio_already_heard() {
        let fake = FakeMachine::new("wifi-scan");
        let (mut compositor, shared) = on_the_radio(
            &fake,
            RecordingSupplicant::new()
                .answering("SCAN_RESULTS", IN_RANGE)
                .ok("SCAN"),
        );

        open_the_list(&mut compositor);

        let overlay = compositor.overlay().expect("the wireless menu");
        assert_eq!(overlay.title(), "wireless — scanning");
        let rows: Vec<(&str, &str)> = overlay
            .items()
            .iter()
            .map(|item| (item.label.as_str(), item.detail.as_str()))
            .collect();
        // Strongest first, the two kitchen-tables folded into the -30, and the
        // hidden network not a row at all.
        assert_eq!(
            rows,
            vec![
                ("kitchen-table", "-30 dBm  WPA2"),
                ("cafe", "-52 dBm  open"),
                ("office", "-67 dBm  enterprise — cannot join"),
            ]
        );
        assert_eq!(said(&shared), vec!["SCAN_RESULTS", "SCAN"]);
    }

    #[test]
    fn the_title_stops_saying_scanning_when_the_answer_changes() {
        let fake = FakeMachine::new("wifi-title");
        let more = format!("{IN_RANGE}02:00:00:00:06:00\t2412\t-70\t[ESS]\tnext-door\n");
        let (mut compositor, _shared) = on_the_radio(
            &fake,
            RecordingSupplicant::new()
                .answering("SCAN_RESULTS", IN_RANGE)
                .answering("SCAN_RESULTS", &more)
                .ok("SCAN"),
        );

        let opened = Instant::now();
        open_the_list(&mut compositor);
        // A second and a half on: the list asks the supplicant again once a
        // second and not once a frame, and `open_the_list` took its own
        // `Instant::now()` a moment after `opened`.
        compositor.tick_at(opened + Duration::from_millis(1500));

        let overlay = compositor.overlay().expect("the wireless menu");
        assert_eq!(overlay.title(), "wireless");
        assert_eq!(
            overlay.items().len(),
            4,
            "the new access point is not a row"
        );
    }

    #[test]
    fn escape_on_the_list_lets_the_scan_go() {
        let fake = FakeMachine::new("wifi-escape");
        let (mut compositor, _shared) = on_the_radio(
            &fake,
            RecordingSupplicant::new()
                .answering("SCAN_RESULTS", IN_RANGE)
                .ok("SCAN"),
        );

        open_the_list(&mut compositor);
        assert_eq!(compositor.wifi.scanning_on(), Some(FAKE_RADIO));
        compositor.overlay_key(&KeyEvent::new(KeyCode::Escape, tos_input::Modifiers::NONE));
        assert!(compositor.overlay().is_none());
        assert_eq!(compositor.wifi.scanning_on(), None);
    }

    #[test]
    fn a_network_with_a_key_asks_for_it_behind_bullets() {
        let fake = FakeMachine::new("wifi-prompt");
        let (mut compositor, _shared) = on_the_radio(&fake, joining_table());

        open_the_list(&mut compositor);
        press(&mut compositor, 0);

        let overlay = compositor.overlay().expect("the passphrase prompt");
        assert_eq!(overlay.title(), "kitchen-table — passphrase");
        assert_eq!(overlay.shown_query(), "");
        assert!(overlay.items().is_empty(), "a prompt has no list");
    }

    #[test]
    fn a_passphrase_sends_the_sequence_the_design_writes_down() {
        let fake = FakeMachine::new("wifi-join");
        let (mut compositor, shared) = on_the_radio(&fake, joining_table());

        open_the_list(&mut compositor);
        press(&mut compositor, 0);
        answer_the_prompt(&mut compositor, "correct horse battery staple");

        assert_eq!(
            said(&shared),
            vec![
                "SCAN_RESULTS",
                "SCAN",
                "LIST_NETWORKS",
                "ADD_NETWORK",
                "SET_NETWORK 0 ssid 6b69746368656e2d7461626c65",
                "SET_NETWORK 0 psk \"correct horse battery staple\"",
                "ENABLE_NETWORK 0",
                "SELECT_NETWORK 0",
                "SAVE_CONFIG",
            ]
        );
        assert_eq!(
            compositor.notifications.status_line().as_deref(),
            Some("joining kitchen-table")
        );
        assert!(compositor.overlay().is_none(), "the prompt is still up");
    }

    #[test]
    fn an_ssid_the_supplicant_already_knows_keeps_the_id_it_had() {
        let fake = FakeMachine::new("wifi-rejoin");
        let (mut compositor, shared) = on_the_radio(
            &fake,
            RecordingSupplicant::new()
                .answering("SCAN_RESULTS", IN_RANGE)
                .ok("SCAN")
                .answering(
                    "LIST_NETWORKS",
                    "network id / ssid / bssid / flags\n3\tkitchen-table\tany\t[DISABLED]\n",
                )
                .ok("SET_NETWORK 3 ssid 6b69746368656e2d7461626c65")
                .ok("SET_NETWORK 3 psk \"correct horse battery staple\"")
                .ok("ENABLE_NETWORK 3")
                .ok("SELECT_NETWORK 3")
                .ok("SAVE_CONFIG"),
        );

        open_the_list(&mut compositor);
        press(&mut compositor, 0);
        answer_the_prompt(&mut compositor, "correct horse battery staple");

        let transcript = said(&shared);
        assert!(
            !transcript.iter().any(|line| line == "ADD_NETWORK"),
            "the network is in the file twice now: {transcript:?}"
        );
        assert!(transcript.iter().any(|line| line == "SELECT_NETWORK 3"));
    }

    #[test]
    fn a_status_that_reaches_completed_is_a_join() {
        let fake = FakeMachine::new("wifi-joined");
        let (mut compositor, _shared) =
            on_the_radio(&fake, joining_table().answering("STATUS", ON_A_NETWORK));

        let started = Instant::now();
        open_the_list(&mut compositor);
        press(&mut compositor, 0);
        answer_the_prompt(&mut compositor, "correct horse battery staple");
        // "joining" has the line and has not been up long enough to be
        // retired; taking it off is what lets the next answer be read.
        compositor.notifications.dismiss();
        compositor.tick_at(started + Duration::from_secs(1));

        assert_eq!(
            compositor.notifications.status_line().as_deref(),
            Some("joined kitchen-table")
        );
    }

    #[test]
    fn a_network_that_goes_temp_disabled_is_a_wrong_passphrase_and_is_not_kept() {
        let fake = FakeMachine::new("wifi-wrongkey");
        let (mut compositor, shared) = on_the_radio(
            &fake,
            joining_table()
                .answering("STATUS", HANDSHAKING)
                .answering(
                    "LIST_NETWORKS",
                    "network id / ssid / bssid / flags\n0\tkitchen-table\tany\t[TEMP-DISABLED]\n",
                )
                .ok("REMOVE_NETWORK 0")
                .ok("SAVE_CONFIG"),
        );

        let started = Instant::now();
        open_the_list(&mut compositor);
        press(&mut compositor, 0);
        answer_the_prompt(&mut compositor, "correct horse battery staple");
        compositor.notifications.dismiss();
        compositor.tick_at(started + Duration::from_secs(1));

        assert_eq!(
            compositor.notifications.status_line().as_deref(),
            Some("kitchen-table: wrong passphrase")
        );
        let transcript = said(&shared);
        assert_eq!(
            &transcript[transcript.len() - 2..],
            ["REMOVE_NETWORK 0", "SAVE_CONFIG"],
            "a key that is wrong was left to be tried again at every boot: {transcript:?}"
        );
    }

    #[test]
    fn a_join_that_never_settles_is_called_off_and_the_network_is_left_alone() {
        let fake = FakeMachine::new("wifi-deadline");
        let (mut compositor, shared) =
            on_the_radio(&fake, joining_table().answering("STATUS", HANDSHAKING));

        let started = Instant::now();
        open_the_list(&mut compositor);
        press(&mut compositor, 0);
        answer_the_prompt(&mut compositor, "correct horse battery staple");
        compositor.notifications.dismiss();
        compositor.tick_at(started + Duration::from_secs(31));

        assert_eq!(
            compositor.notifications.status_line().as_deref(),
            Some("kitchen-table: could not join")
        );
        let transcript = said(&shared);
        assert!(
            !transcript.iter().any(|line| line == "REMOVE_NETWORK 0"),
            "a network that was only out of range was forgotten: {transcript:?}"
        );
    }

    #[test]
    fn an_open_network_is_joined_without_anybody_being_asked_for_a_key() {
        let fake = FakeMachine::new("wifi-open");
        let (mut compositor, shared) = on_the_radio(
            &fake,
            RecordingSupplicant::new()
                .answering("SCAN_RESULTS", IN_RANGE)
                .ok("SCAN")
                .answering("LIST_NETWORKS", NO_NETWORKS)
                .answering("ADD_NETWORK", "0")
                .ok("SET_NETWORK 0 ssid 63616665")
                .ok("SET_NETWORK 0 key_mgmt NONE")
                .ok("ENABLE_NETWORK 0")
                .ok("SELECT_NETWORK 0")
                .ok("SAVE_CONFIG"),
        );

        open_the_list(&mut compositor);
        press(&mut compositor, 1);

        assert!(compositor.overlay().is_none(), "it asked for a passphrase");
        assert_eq!(
            compositor.notifications.status_line().as_deref(),
            Some("joining cafe")
        );
        let transcript = said(&shared);
        assert!(transcript
            .iter()
            .any(|line| line == "SET_NETWORK 0 key_mgmt NONE"));
    }

    #[test]
    fn an_enterprise_network_says_why_it_cannot_be_joined() {
        let fake = FakeMachine::new("wifi-eap");
        let (mut compositor, shared) = on_the_radio(
            &fake,
            RecordingSupplicant::new()
                .answering("SCAN_RESULTS", IN_RANGE)
                .ok("SCAN"),
        );

        open_the_list(&mut compositor);
        press(&mut compositor, 2);

        assert_eq!(
            compositor.notifications.status_line().as_deref(),
            Some("cannot join office: enterprise networks are not supported")
        );
        assert_eq!(
            said(&shared),
            vec!["SCAN_RESULTS", "SCAN"],
            "a row that says it cannot be joined tried anyway"
        );
    }

    #[test]
    fn leaving_and_forgetting_each_send_their_two_commands() {
        let fake = FakeMachine::new("wifi-leave");
        let (mut compositor, shared) = on_the_radio(
            &fake,
            RecordingSupplicant::new()
                .answering("STATUS", ON_A_NETWORK)
                .ok("DISABLE_NETWORK 0")
                .ok("DISCONNECT")
                .ok("REMOVE_NETWORK 0")
                .ok("SAVE_CONFIG"),
        );

        compositor.network_target = Some(FAKE_RADIO.to_string());
        compositor.choose(OverlayKind::Link, Some(2), "leave kitchen-table");
        assert_eq!(
            compositor.notifications.status_line().as_deref(),
            Some("left kitchen-table")
        );

        compositor.network_target = Some(FAKE_RADIO.to_string());
        compositor.choose(OverlayKind::Link, Some(3), "forget kitchen-table");

        assert_eq!(
            said(&shared),
            vec![
                "STATUS",
                "DISABLE_NETWORK 0",
                "DISCONNECT",
                "STATUS",
                "REMOVE_NETWORK 0",
                "SAVE_CONFIG",
            ]
        );
    }

    #[test]
    fn a_passphrase_the_client_refuses_keeps_the_prompt_and_says_which_rule_it_broke() {
        let fake = FakeMachine::new("wifi-short");
        let (mut compositor, shared) = on_the_radio(&fake, joining_table());

        open_the_list(&mut compositor);
        press(&mut compositor, 0);
        answer_the_prompt(&mut compositor, "short");

        assert_eq!(
            compositor.notifications.status_line().as_deref(),
            Some("a passphrase is 8 to 63 characters, and this one is 5")
        );
        let overlay = compositor.overlay().expect("the prompt closed on a typo");
        assert_eq!(overlay.title(), "kitchen-table — passphrase");
        assert_eq!(overlay.query(), "short", "what was typed was thrown away");
        assert_eq!(overlay.shown_query(), "•••••");
        let transcript = said(&shared);
        assert!(
            !transcript.iter().any(|line| line.contains(" psk ")),
            "a passphrase that was refused here reached the daemon anyway: {transcript:?}"
        );
        assert!(compositor.wifi.joining().is_none());
    }
}
