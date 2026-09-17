//! The lock screen: a password field that owns the session until it is
//! answered.
//!
//! This sits beside [`crate::overlay`] rather than inside it, and the reason
//! is that an overlay is the opposite of a password field in all three of the
//! things it does. An overlay echoes what is typed; this masks it. An overlay
//! refilters a list on every keystroke; this has no list. An overlay cannot
//! submit when nothing matches, and escape closes it; this always submits and
//! has no way out at all. Reusing [`crate::overlay::Overlay`] would mean three
//! special cases in the one type whose value is that it has none.
//!
//! Nothing here draws to a display it is not given, reads a file it is not
//! pointed at, or asks what time it is: the compositor hands in the clock, the
//! way it already hands one to the terminals for their animations. That is
//! what lets a test drive the whole state machine — engage, wrong password,
//! wait, right password — with no display, no VT and no real credential.
//!
//! ```text
//!                         Action::Lock, with a credential
//!   Unlocked ────────────────────────────────────────────────► Locked
//!      ▲                                                     │   ▲
//!      │                the password verifies                │   │ it does not:
//!      └─────────────────────────────────────────────────────┘   │ clear the
//!                                                                │ field, wait,
//!                                                                └─ ask again
//! ```
//!
//! There is no other edge out of `Locked`. A wrong password, a pane that dies,
//! a display that cannot be painted and a terminal that changes size all leave
//! the lock exactly where it was.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tos_font::FontStack;
use tos_input::{KeyCode, KeyEvent, Modifiers};
use tos_render::{Rect, Surface};

use crate::chrome::{clip, draw_text, Chrome};
use crate::overlay::pad_to;
use crate::splash::Splash;

/// Where the credential lives when nobody says otherwise.
///
/// The machine's own file, and not tOS's. It used to be `/etc/tos/shadow`, on
/// the reasoning that a tOS password in Debian's file would silently become
/// the machine's login password too — which was right, and which turned out to
/// be the thing that was wanted: a password no `sshd`, no `su` and no `login`
/// could ever use is a password that only ever unlocks a screen (#111).
/// `docs/design/credentials.md` works through the change; the file it replaced
/// is described in `docs/design/screen-lock.md`.
pub const CREDENTIAL_PATH: &str = "/etc/shadow";

/// The environment variable that names the account a session belongs to.
///
/// Set by `iso/live-session` and by the `/etc/tos-session` the installer
/// writes, which are the two things that start a session. The compositor runs
/// as root on both, so the uid it happens to have is not the question being
/// asked: the live image is root's session and says so, and an installed
/// machine is the session of the person the installer was told about, whose
/// password is the one that machine has.
pub const SESSION_USER: &str = "TOS_USER";

/// Whose password the lock asks for, when nothing has said.
const DEFAULT_USER: &str = "root";

/// The account this session belongs to.
///
/// Read from the environment rather than from the uid, for the reason in
/// [`SESSION_USER`]. A session started by hand from a shell inherits whatever
/// that shell had, which on both images is the same variable; a session
/// started with neither falls back to `root`, whose line on a Debian root is
/// `*` and so locks nothing — the safe way round of the two, since the other
/// would be a lock over somebody else's password.
pub fn session_user() -> String {
    std::env::var(SESSION_USER)
        .ok()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| DEFAULT_USER.to_string())
}

/// The widest the box grows, however wide the display is.
const MAX_WIDTH: usize = 44;
/// Rows the box is: border, field, divider, message, border.
const BOX_ROWS: usize = 5;
/// What is written in front of the masked field.
const PROMPT: &str = "password: ";
/// Rows the picture leaves to everything that is not the box: one blank row
/// between the picture and the box, and one above and below the pair of them
/// so that neither is ever flush against an edge of the display.
const PICTURE_MARGIN_ROWS: usize = 3;
/// One of these per character typed. The length of a password is not a secret
/// worth hiding from the person who can see the hands that typed it, and a
/// field that shows nothing at all is how people end up convinced that a
/// compositor which has grabbed the keyboard has stopped reading it.
const MASK: char = '•';
/// The longest wait between attempts. Doubling from a second, this is reached
/// on the fourth wrong password.
const MAX_PENALTY: Duration = Duration::from_secs(8);

/// Why there is nothing to lock the screen against.
///
/// This is the whole of the "no credential, no lock" rule, and it is what
/// makes the live ISO behave without the compositor ever being taught what
/// live media is. A machine with no password does not get a lock that cannot
/// be opened; it gets told there is no password.
#[derive(Debug)]
pub enum NoCredential {
    /// There is no shadow file at all. The busybox world, and anything else
    /// that is not a Debian root.
    Missing(PathBuf),
    /// The file is there and could not be read.
    Unreadable(PathBuf, io::Error),
    /// The file is there and has no line for this account.
    Unknown(PathBuf, String),
    /// The account is there and has no password: `*`, `!`, or nothing at all
    /// in the field. The ordinary case on the live ISO, whose root is Debian's
    /// `*`, and on an installed machine whose owner declined a password.
    ///
    /// Distinct from [`NoCredential::Unusable`] because it is not a fault:
    /// nothing is wrong with that file, and what it says is that this account
    /// is not one anybody authenticates as.
    NoPassword(PathBuf, String),
    /// There is a password and it is not one this can check — a `$y$`
    /// yescrypt line, which is what Debian's own `passwd` writes. Not a wrong
    /// password: a lock that treated it as one would be a lock nobody could
    /// ever open.
    Unusable(PathBuf, String),
}

impl fmt::Display for NoCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NoCredential::Missing(path) => {
                write!(f, "there is no {}", path.display())
            }
            NoCredential::Unreadable(path, error) => {
                write!(f, "cannot read {}: {error}", path.display())
            }
            NoCredential::Unknown(path, user) => {
                write!(f, "{} has no line for {user}", path.display())
            }
            NoCredential::NoPassword(_, user) => {
                write!(f, "no password is set for {user}")
            }
            NoCredential::Unusable(path, why) => {
                write!(
                    f,
                    "{} is not a password tOS can check: {why}",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for NoCredential {}

/// Read the account's credential, and say why there is none rather than how
/// many ways there might have been.
///
/// `/etc/shadow`, in the format every other program on the machine reads it
/// in: one line per account, the account's name first and its password field
/// second. The installer writes the person's line there as a `$6$` crypt
/// hash, which is what `tos-crypt` produces and what this can check; Debian's
/// own lines are `*`, an account nothing authenticates as.
///
/// Comments are not a thing `/etc/shadow` has, so nothing is skipped but
/// blank lines. A `#` at the start of a line would be an account named `#`,
/// and treating it as a note is how a line could be commented out of a file
/// that no other reader on the machine believes in.
///
/// The mode of the file is not checked. It should be 0640 and dpkg and the
/// installer both write it that way, but refusing to lock a screen because
/// the hash is more readable than it ought to be would trade a lock that
/// works for one that does not, over a file only root can reach in the first
/// place.
pub fn read_credential(path: &Path, user: &str) -> Result<String, NoCredential> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(NoCredential::Missing(path.to_path_buf()))
        }
        Err(e) => return Err(NoCredential::Unreadable(path.to_path_buf(), e)),
    };
    let field = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .find_map(|line| {
            let (name, rest) = line.split_once(':')?;
            (name == user).then(|| rest.split(':').next().unwrap_or(""))
        })
        .ok_or_else(|| NoCredential::Unknown(path.to_path_buf(), user.to_string()))?;

    // `*` and `!` are how that file says "nobody authenticates as this", and
    // an empty field is how it says "anybody does, without being asked" —
    // which is not a credential either, and is certainly not one to hand a
    // lock. All three mean the same thing here: there is nothing to unlock
    // with, so nothing locks. A `!` in front of a real hash is an account
    // that has a password and has been disabled; the hash behind it is
    // deliberately not read.
    if field.is_empty() || field.starts_with('*') || field.starts_with('!') {
        return Err(NoCredential::NoPassword(
            path.to_path_buf(),
            user.to_string(),
        ));
    }

    // Parsing it now rather than at the first keypress is the point: the
    // decision that this machine can be locked is made before the screen goes
    // up, so a hash that does not parse can never become a locked session
    // waiting for a password that will never be accepted.
    match tos_crypt::Hash::parse(field) {
        Ok(_) => Ok(field.to_string()),
        Err(e) => Err(NoCredential::Unusable(path.to_path_buf(), e.to_string())),
    }
}

/// What the lock did with a key.
///
/// Every variant means the key was taken. There is no `Cancelled` here and
/// that absence is the type saying what a lock is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockOutcome {
    /// Taken, and nothing on screen changed.
    Consumed,
    /// Taken, and the lock needs repainting.
    Changed,
    /// The password verified. The session comes back.
    Unlocked,
}

/// The password prompt, and the state machine behind it.
/// Why this screen is up, which is the whole of the difference between a lock
/// and a login.
///
/// The two are the same screen asking the same account for the same password,
/// and the only thing that differs is what is behind it: a session that is
/// waiting, or no session at all. Having two types would be having two places
/// for the rate limit, the masking and the wrong-password wait to drift apart
/// — which is what #112 said would happen, in the issue that added the second
/// one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// The session is behind this and comes back when it opens.
    Lock,
    /// There is no session behind this. One starts when it opens.
    Login,
}

impl Purpose {
    /// What the box calls itself.
    fn title(self) -> &'static str {
        match self {
            Purpose::Lock => "locked",
            Purpose::Login => "log in",
        }
    }
}

/// How much of this screen a frame is putting on the surface (#164).
///
/// A locked screen used to be erased and drawn again for every frame it was
/// asked for, and it was asked for twice a second whether or not anything had
/// happened — so a machine nobody was at repainted its whole display all
/// night. It is retained now, like every other screen in tOS, and this is what
/// the frame says it is doing.
///
/// The split is not the box against the picture by taste. The box **paints its
/// own background** before it draws its border, so putting it down again over
/// the frame that is already there gives the same pixels as erasing first
/// would. The picture does not: it carries alpha and is composited over
/// whatever it lands on, so a second pass over its own result is a different
/// picture. That is the whole reason there are two of these and not a
/// rectangle of damage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Repaint {
    /// All of it, onto a surface that has just been erased to the background.
    /// What a frame does when it cannot know what is already there.
    Everything,
    /// The box alone, over a frame that still holds the rest of this screen.
    TheBox,
}

/// What is behind the box this frame, and whether this frame is putting it
/// there.
///
/// One argument rather than two because it is one question. The picture is
/// named whichever way [`Repaint`] falls: the box is laid out *around* it —
/// centred with it as one stack over a login screen — so a frame that left it
/// out to say "do not paint this" would move the box instead.
#[derive(Debug, Clone, Copy)]
pub struct Backdrop<'a> {
    /// The picture this screen is laid out around, where there is one.
    pub splash: Option<&'a Splash>,
    /// How much of the screen this frame is painting.
    pub repaint: Repaint,
}

pub struct LockScreen {
    /// The credential line, read once when the lock engaged.
    ///
    /// Once, rather than per attempt, so that the file being removed, renamed
    /// or made unreadable while the screen is locked cannot lock the owner out
    /// of their own session.
    hash: String,
    /// The account being asked for, which is whose session this is.
    ///
    /// Shown, because a screen that asks for a password without saying whose
    /// is a screen somebody types the wrong one into — and on a login screen
    /// it is the only thing on the panel that says what machine this is.
    user: String,
    /// Whether there is a session behind this.
    purpose: Purpose,
    /// What has been typed, which nothing but [`LockScreen::submit`] reads.
    typed: String,
    /// Wrong answers so far, which is what the wait is computed from.
    attempts: u32,
    /// When the next attempt will be looked at. Typing is never held up; only
    /// checking is, because checking is the only thing a guess costs.
    ready_at: Option<Instant>,
}

impl LockScreen {
    /// A fresh lock over this credential, with a session behind it.
    pub fn new(hash: String, user: String) -> Self {
        LockScreen::over(hash, user, Purpose::Lock)
    }

    /// The same screen with nothing behind it: the login boundary at the start
    /// of a session, and the one ending a session comes back to (#112).
    pub fn login(hash: String, user: String) -> Self {
        LockScreen::over(hash, user, Purpose::Login)
    }

    fn over(hash: String, user: String, purpose: Purpose) -> Self {
        LockScreen {
            hash,
            user,
            purpose,
            typed: String::new(),
            attempts: 0,
            ready_at: None,
        }
    }

    /// Whether a session is waiting behind this screen, or is yet to start.
    pub fn purpose(&self) -> Purpose {
        self.purpose
    }

    /// The account this is asking about.
    pub fn user(&self) -> &str {
        &self.user
    }

    /// How many wrong passwords have been offered.
    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    /// How many characters are in the field. The characters themselves are
    /// deliberately not reachable from outside.
    pub fn typed_len(&self) -> usize {
        self.typed.chars().count()
    }

    /// How much longer this refuses to check a password.
    pub fn wait_left(&self, now: Instant) -> Option<Duration> {
        self.ready_at
            .filter(|ready| *ready > now)
            .map(|ready| ready - now)
    }

    /// Offer a key to the lock.
    pub fn handle_key(&mut self, key: &KeyEvent, now: Instant) -> LockOutcome {
        // Releases and the modifiers themselves are swallowed like everything
        // else: while this is up, no key belongs to anyone but the lock.
        if !key.is_press() || matches!(key.code, KeyCode::ModifierKey(_)) {
            return LockOutcome::Consumed;
        }
        let modifiers = key.modifiers.effective();
        let ctrl = modifiers.contains(Modifiers::CTRL);

        match key.code {
            KeyCode::Enter => return self.submit(now),
            // Escape clears the line rather than closing anything. There is
            // nothing to close: that is what a lock is.
            KeyCode::Escape => return self.clear(),
            KeyCode::Char('u') if ctrl => return self.clear(),
            KeyCode::Backspace => {
                return match self.typed.pop() {
                    Some(_) => LockOutcome::Changed,
                    None => LockOutcome::Consumed,
                }
            }
            _ => {}
        }

        // Anything held with a modifier other than shift is somebody reaching
        // for a binding. No binding fires here, and it must not end up in the
        // password either.
        if !modifiers.without(Modifiers::SHIFT).is_empty() {
            return LockOutcome::Consumed;
        }
        match key.text {
            Some(c) if !c.is_control() => {
                self.typed.push(c);
                LockOutcome::Changed
            }
            _ => LockOutcome::Consumed,
        }
    }

    fn clear(&mut self) -> LockOutcome {
        if self.typed.is_empty() {
            return LockOutcome::Consumed;
        }
        self.typed.clear();
        LockOutcome::Changed
    }

    /// Check what was typed.
    ///
    /// An empty line is checked like any other. The installer is allowed to
    /// decide that an empty password is a password, and a lock that refused to
    /// submit one would be a lock that machine could never open; the cost is
    /// that a stray enter on an empty field spends a second, which is the same
    /// second a wrong password spends.
    fn submit(&mut self, now: Instant) -> LockOutcome {
        // The rate limit is on checking, not on typing. Hashing is about five
        // milliseconds, so an unthrottled prompt is a couple of hundred
        // guesses a second at the keyboard of a machine somebody walked up to.
        if self.wait_left(now).is_some() {
            return LockOutcome::Changed;
        }
        // A file that did not parse never got this far — `read_credential` is
        // what decides whether the screen locks at all — so an error here is
        // not reachable, and the only safe reading of one is "not the
        // password".
        if tos_crypt::verify_password(self.typed.as_bytes(), &self.hash) == Ok(true) {
            return LockOutcome::Unlocked;
        }
        self.typed.clear();
        self.attempts = self.attempts.saturating_add(1);
        self.ready_at = Some(now + self.penalty());
        LockOutcome::Changed
    }

    /// How long to wait after the wrong password this many attempts in.
    ///
    /// A second, then two, then four, then eight and no further. Doubling
    /// makes guessing expensive quickly; the cap is there because the person
    /// being kept waiting is, overwhelmingly, the owner who mistyped, and a
    /// lock that grows a punishment without limit is one that has stopped
    /// being able to tell the two apart.
    fn penalty(&self) -> Duration {
        let doublings = self.attempts.saturating_sub(1).min(3);
        Duration::from_secs(1 << doublings).min(MAX_PENALTY)
    }

    /// What the second line of the box says.
    fn message(&self, now: Instant) -> String {
        if self.attempts == 0 {
            return "type your password and press enter".into();
        }
        match self.wait_left(now) {
            // Rounded up, so the last part-second still reads as a wait: a
            // countdown that says 0 while the key does nothing is worse than
            // no countdown.
            Some(left) => {
                let seconds = (left.as_millis() as u64).div_ceil(1000).max(1);
                format!("wrong password; try again in {seconds}s")
            }
            None => "wrong password; try again".into(),
        }
    }

    /// Draw the lock centred in `area`, which is in pixels.
    ///
    /// `backdrop` says what is behind the box and how much of this screen the
    /// frame is putting on the surface.
    ///
    /// This draws the box and the picture that goes with it. Erasing the
    /// session is the caller's, because only the caller knows that the whole
    /// surface is its to erase.
    ///
    /// The picture is handed in rather than loaded here for the reason at the
    /// top of this file: nothing here reads a file it is not pointed at. Where
    /// it goes is this screen's purpose:
    ///
    /// | | the picture |
    /// |---|---|
    /// | [`Purpose::Login`] | centred above the box, laid out with it as one stack |
    /// | [`Purpose::Lock`] | in the bottom right corner, clear of the box |
    ///
    /// A login screen is the machine opening, which is what a frontispiece is
    /// for. A lock is somebody's own session waiting behind it, and they have
    /// already been told what this machine is — so the picture keeps to a
    /// corner, where it is something to rest the eye on rather than the thing
    /// between them and the field they came to type in.
    pub fn draw(
        &self,
        surface: &mut Surface<'_>,
        fonts: &mut FontStack,
        area: Rect,
        chrome: &Chrome,
        backdrop: Backdrop<'_>,
        now: Instant,
    ) {
        let Backdrop { splash, repaint } = backdrop;
        let metrics = fonts.metrics();
        let (cw, ch) = (metrics.cell_width.max(1), metrics.cell_height.max(1));
        let cols = (area.width / cw) as usize;
        let rows = (area.height / ch) as usize;
        // A screen too small for the box is still locked; it simply has
        // nowhere to say so, which is the one direction this is allowed to
        // fail in.
        let box_cols = cols.saturating_sub(2).min(MAX_WIDTH);
        if rows < BOX_ROWS + 2 || box_cols < PROMPT.len() + 6 {
            return;
        }
        let inner = box_cols - 2;
        let x0 = area.x + (((cols - box_cols) / 2) * cw as usize) as i32;

        // The picture and the box are centred as one thing, so the pair has
        // the same air above and below it that the box alone used to have.
        // A display with no room for the picture is the box on its own again,
        // in the place it has always been.
        let picture = splash
            .filter(|_| self.purpose == Purpose::Login)
            .and_then(|splash| {
                let over = rows.checked_sub(BOX_ROWS + PICTURE_MARGIN_ROWS)?;
                let room = (Splash::room_across(area.width), over as u32 * ch);
                splash.fit(room).map(|size| (splash, size))
            });
        let picture_rows = picture
            .map(|(_, (_, height))| height.div_ceil(ch) as usize)
            .unwrap_or(0);
        // The picture, a blank row, and the box — or, with no picture, the box.
        let stack_rows = if picture_rows == 0 {
            BOX_ROWS
        } else {
            picture_rows + 1 + BOX_ROWS
        };
        let y0 = area.y + (((rows - stack_rows) / 2 + stack_rows - BOX_ROWS) * ch as usize) as i32;

        // Above the box, with a blank row between them. The picture carries
        // its own alpha, so what surrounds it is the field the caller erased
        // to rather than a rectangle of its own.
        //
        // Only where the whole screen is being painted. The layout above is
        // worked out either way — the box sits under the picture and has to
        // stay where it was — but the pixels are not put down twice: alpha
        // composited over its own last result is a different picture, and a
        // retained frame already holds the right one.
        if let (Some((splash, (width, height))), Repaint::Everything) = (picture, repaint) {
            let x = area.x + ((area.width.saturating_sub(width)) / 2) as i32;
            splash.draw(
                surface,
                Rect::new(x, y0 - ch as i32 - height as i32, width, height),
            );
        }

        let box_rect = Rect::new(x0, y0, box_cols as u32 * cw, BOX_ROWS as u32 * ch);

        // And over a lock, in the bottom right corner instead, a row of margin
        // in from each edge. Nothing lays out around it: the box is where it
        // would have been on an empty field, and this is drawn before the box
        // so that on a display where the two do meet the box is the one left
        // whole — the same order the frontispiece already follows, for the
        // same reason that the box is the part somebody cannot do without.
        if self.purpose == Purpose::Lock && repaint == Repaint::Everything {
            if let Some((splash, (width, height))) = splash.and_then(|splash| {
                let room = Splash::room_in_corner((area.width, area.height));
                splash.fit(room).map(|size| (splash, size))
            }) {
                // One row of margin on both axes rather than a row down and a
                // cell across: the margin is a distance from the corner, and a
                // cell is half as wide as it is tall.
                let corner = Rect::new(
                    area.x + area.width.saturating_sub(width + ch) as i32,
                    area.y + area.height.saturating_sub(height + ch) as i32,
                    width,
                    height,
                );
                if corner.intersect(&box_rect).is_empty() {
                    splash.draw(surface, corner);
                }
            }
        }

        // Nothing of the session is under this, but the box still paints its
        // own background so the border has something to sit on.
        surface.fill(box_rect, chrome.background);

        let border = chrome.divider_focused;
        let row_y = |row: usize| y0 + (row as u32 * ch) as i32;
        let rule = |left: char, right: char| {
            let mut text = left.to_string();
            pad_to(&mut text, box_cols - 1, '─');
            text.push(right);
            text
        };

        // "┌─ log in ─ tos ──┐": what this is, and whose password it wants.
        // The name is clipped rather than allowed to push the box wider,
        // because the box's width is what the field under it was laid out
        // against.
        let mut top = format!(
            "┌─ {} ─ {} ",
            self.purpose.title(),
            clip(
                &self.user,
                inner.saturating_sub(self.purpose.title().len() + 6)
            )
        );
        pad_to(&mut top, box_cols - 1, '─');
        top.push('┐');
        for (row, text) in [
            (0, top),
            (2, rule('├', '┤')),
            (BOX_ROWS - 1, rule('└', '┘')),
        ] {
            draw_text(
                surface,
                fonts,
                x0,
                row_y(row),
                &text,
                border,
                Some(chrome.background),
                false,
            );
        }

        self.draw_field(surface, fonts, x0, row_y(1), inner, chrome);

        // The message line. Its text changes and its height does not: a box
        // that grew a row when the password was wrong would move the field out
        // from under the cursor at the worst possible moment.
        let y = row_y(3);
        let mut x = draw_text(
            surface,
            fonts,
            x0,
            y,
            "│ ",
            border,
            Some(chrome.background),
            false,
        );
        let mut message = clip(&self.message(now), inner.saturating_sub(1));
        pad_to(&mut message, inner - 1, ' ');
        let colour = if self.attempts == 0 {
            chrome.dim
        } else {
            chrome.accent
        };
        x = draw_text(
            surface,
            fonts,
            x,
            y,
            &message,
            colour,
            Some(chrome.background),
            false,
        );
        draw_text(
            surface,
            fonts,
            x,
            y,
            "│",
            border,
            Some(chrome.background),
            false,
        );
    }

    /// The masked field, with a block cursor after it.
    fn draw_field(
        &self,
        surface: &mut Surface<'_>,
        fonts: &mut FontStack,
        x0: i32,
        y: i32,
        inner: usize,
        chrome: &Chrome,
    ) {
        let cw = fonts.metrics().cell_width.max(1);
        let border = chrome.divider_focused;
        let mut x = draw_text(
            surface,
            fonts,
            x0,
            y,
            "│ ",
            border,
            Some(chrome.background),
            false,
        );
        x = draw_text(
            surface,
            fonts,
            x,
            y,
            PROMPT,
            chrome.dim,
            Some(chrome.background),
            false,
        );
        // Room for the prompt, a cell of margin at each end and the cursor. A
        // password longer than the field shows its last characters, which is
        // the end being typed.
        let room = inner.saturating_sub(PROMPT.len() + 2);
        let shown = self.typed_len().min(room);
        let mask = MASK.to_string().repeat(shown);
        x = draw_text(
            surface,
            fonts,
            x,
            y,
            &mask,
            chrome.foreground,
            Some(chrome.background),
            false,
        );
        surface.fill(
            Rect::new(x, y, cw, fonts.metrics().cell_height.max(1)),
            chrome.accent,
        );
        x += cw as i32;

        let used = 1 + PROMPT.len() + shown + 1;
        let mut tail = " ".repeat(inner.saturating_sub(used.min(inner)));
        tail.push('│');
        draw_text(
            surface,
            fonts,
            x,
            y,
            &tail,
            border,
            Some(chrome.background),
            false,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A credential file of this crate's own making, so the tests depend on
    /// nothing the machine running them happens to have.
    fn credential_file(name: &str, contents: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("tos-lock-{}-{name}", std::process::id()));
        let mut file = std::fs::File::create(&path).expect("credential file");
        file.write_all(contents.as_bytes()).expect("write");
        path
    }

    fn hash_of(password: &str) -> String {
        // A fixed salt: the point of the test is the state machine, and a hash
        // that is the same every run is one a failure can be read from.
        tos_crypt::sha512crypt::hash(password.as_bytes(), b"tOSlocktest")
    }

    fn lock(password: &str) -> LockScreen {
        LockScreen::new(hash_of(password), "tos".into())
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, Modifiers::NONE)
    }

    fn typed(text: &str) -> Vec<KeyEvent> {
        text.chars().map(|c| press(KeyCode::Char(c))).collect()
    }

    /// Type `password` and press enter, at a time of the caller's choosing.
    fn offer(lock: &mut LockScreen, password: &str, now: Instant) -> LockOutcome {
        for key in typed(password) {
            lock.handle_key(&key, now);
        }
        lock.handle_key(&press(KeyCode::Enter), now)
    }

    #[test]
    fn the_right_password_unlocks() {
        let mut lock = lock("hunter2");
        assert_eq!(
            offer(&mut lock, "hunter2", Instant::now()),
            LockOutcome::Unlocked
        );
    }

    #[test]
    fn a_wrong_password_clears_the_field_and_stays() {
        let mut lock = lock("hunter2");
        let now = Instant::now();
        assert_eq!(offer(&mut lock, "hunter3", now), LockOutcome::Changed);
        assert_eq!(lock.attempts(), 1);
        assert_eq!(lock.typed_len(), 0, "the field should be empty to retype");
    }

    #[test]
    fn the_right_password_still_works_after_a_wrong_one() {
        // There is no state a wrong answer leaves behind except the wait.
        let mut lock = lock("hunter2");
        let now = Instant::now();
        offer(&mut lock, "wrong", now);
        let later = now + Duration::from_secs(30);
        assert_eq!(offer(&mut lock, "hunter2", later), LockOutcome::Unlocked);
    }

    #[test]
    fn checking_is_held_off_after_a_wrong_password() {
        let mut lock = lock("hunter2");
        let now = Instant::now();
        offer(&mut lock, "wrong", now);
        // The right password, offered immediately, is not even looked at. What
        // was typed stays in the field, so the enter that comes after the wait
        // is the same attempt again rather than a second one typed on top.
        assert_eq!(
            offer(&mut lock, "hunter2", now + Duration::from_millis(100)),
            LockOutcome::Changed,
        );
        assert_eq!(lock.typed_len(), "hunter2".len());
        assert_eq!(
            lock.handle_key(&press(KeyCode::Enter), now + Duration::from_secs(2)),
            LockOutcome::Unlocked,
        );
    }

    #[test]
    fn typing_is_never_held_off() {
        // The wait is on checking a guess, which is what a guess costs. A
        // field that stopped taking characters would just look broken.
        let mut lock = lock("hunter2");
        let now = Instant::now();
        offer(&mut lock, "wrong", now);
        for key in typed("hun") {
            assert_eq!(lock.handle_key(&key, now), LockOutcome::Changed);
        }
        assert_eq!(lock.typed_len(), 3);
    }

    #[test]
    fn the_wait_doubles_and_then_stops_growing() {
        let mut lock = lock("hunter2");
        let mut now = Instant::now();
        let mut seen = Vec::new();
        for _ in 0..6 {
            offer(&mut lock, "wrong", now);
            seen.push(lock.wait_left(now).expect("a wait").as_secs());
            now += Duration::from_secs(60);
        }
        assert_eq!(seen, [1, 2, 4, 8, 8, 8]);
    }

    #[test]
    fn a_refused_attempt_does_not_extend_the_wait() {
        // Otherwise a held enter key would lock the owner out for good.
        let mut lock = lock("hunter2");
        let now = Instant::now();
        offer(&mut lock, "wrong", now);
        let ready = lock.ready_at.expect("a wait");
        for _ in 0..20 {
            lock.handle_key(&press(KeyCode::Enter), now);
        }
        assert_eq!(lock.ready_at, Some(ready));
        assert_eq!(lock.attempts(), 1, "a refused attempt is not an attempt");
    }

    #[test]
    fn escape_clears_the_field_rather_than_the_lock() {
        let mut lock = lock("hunter2");
        let now = Instant::now();
        for key in typed("half") {
            lock.handle_key(&key, now);
        }
        assert_eq!(
            lock.handle_key(&press(KeyCode::Escape), now),
            LockOutcome::Changed
        );
        assert_eq!(lock.typed_len(), 0);
        // And there is still no way out but the password.
        assert_eq!(offer(&mut lock, "hunter2", now), LockOutcome::Unlocked);
    }

    #[test]
    fn backspace_takes_one_character_and_stops_at_the_start() {
        let mut lock = lock("hunter2");
        let now = Instant::now();
        for key in typed("abc") {
            lock.handle_key(&key, now);
        }
        assert_eq!(
            lock.handle_key(&press(KeyCode::Backspace), now),
            LockOutcome::Changed
        );
        assert_eq!(lock.typed_len(), 2);
        for _ in 0..5 {
            lock.handle_key(&press(KeyCode::Backspace), now);
        }
        assert_eq!(
            lock.handle_key(&press(KeyCode::Backspace), now),
            LockOutcome::Consumed
        );
    }

    #[test]
    fn a_binding_attempt_is_swallowed_rather_than_typed() {
        let mut lock = lock("d");
        let now = Instant::now();
        let split = KeyEvent::new(KeyCode::Char('d'), Modifiers::SUPER);
        assert_eq!(lock.handle_key(&split, now), LockOutcome::Consumed);
        assert_eq!(lock.typed_len(), 0, "super+d went into the password");
    }

    #[test]
    fn an_empty_password_is_a_password() {
        // Whether the installer allows one is its decision; if it does, the
        // machine it set up has to be openable.
        let mut lock = LockScreen::new(hash_of(""), "tos".into());
        assert_eq!(
            lock.handle_key(&press(KeyCode::Enter), Instant::now()),
            LockOutcome::Unlocked
        );
    }

    #[test]
    fn a_password_with_a_kanji_in_it_is_checked_as_typed() {
        // The field takes whatever the keymap produces, and the hash is over
        // bytes, so this only has to not be truncated on the way through.
        let mut lock = lock("かぎ");
        assert_eq!(
            offer(&mut lock, "かぎ", Instant::now()),
            LockOutcome::Unlocked
        );
    }

    #[test]
    fn the_message_says_what_is_happening() {
        let mut lock = lock("hunter2");
        let now = Instant::now();
        assert_eq!(lock.message(now), "type your password and press enter");
        offer(&mut lock, "wrong", now);
        assert_eq!(lock.message(now), "wrong password; try again in 1s");
        assert_eq!(
            lock.message(now + Duration::from_secs(2)),
            "wrong password; try again"
        );
    }

    /// A shadow file the way the machine's own is laid out: Debian's system
    /// accounts, none of which has a password, and the person's line among
    /// them.
    fn shadow(user: &str, field: &str) -> String {
        format!(
            "root:*:::::::\n\
             daemon:*:20709:0:99999:7:::\n\
             {user}:{field}:::::::\n\
             _apt:*:20709:0:99999:7:::\n"
        )
    }

    #[test]
    fn the_box_says_which_screen_this_is_and_whose_password_it_wants() {
        // The two screens are one type, and the title is the whole of what a
        // person sees of the difference: a session waiting behind this, or no
        // session yet. The name is there because a prompt that does not say
        // whose password it wants is one people type the wrong one into.
        let mut pixels = vec![0u32; 640 * 360];
        let mut fonts = FontStack::new(Box::new(tos_font::BitmapFont::new(1)));
        for (screen, word) in [
            (LockScreen::new(hash_of("hunter2"), "tos".into()), "locked"),
            (
                LockScreen::login(hash_of("hunter2"), "tos".into()),
                "log in",
            ),
        ] {
            assert_eq!(screen.user(), "tos");
            let mut surface = Surface::new(&mut pixels, 640, 360, 640);
            screen.draw(
                &mut surface,
                &mut fonts,
                Rect::new(0, 0, 640, 360),
                &Chrome::default(),
                Backdrop {
                    splash: None,
                    repaint: Repaint::Everything,
                },
                Instant::now(),
            );
            assert!(
                pixels.iter().any(|&p| p != 0),
                "{word}: the screen drew nothing"
            );
            pixels.fill(0);
        }
    }

    /// How many distinct colours a frame holds. A box is a handful of them
    /// and a picture is thousands, which is the whole of how these tests tell
    /// one from the other without knowing what the picture is.
    fn colours(pixels: &[u32]) -> usize {
        pixels
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len()
    }

    /// The first row with anything on it.
    fn first_drawn_row(pixels: &[u32], width: usize) -> Option<usize> {
        pixels
            .chunks_exact(width)
            .position(|row| row.iter().any(|&p| p != 0))
    }

    /// One quarter of a frame, named by the corner it is in. Which quarter a
    /// picture's thousands of colours land in is how these tests tell where it
    /// was drawn without knowing what it is a picture of.
    fn quadrant(pixels: &[u32], width: usize, right: bool, bottom: bool) -> Vec<u32> {
        let height = pixels.len() / width;
        let rows = if bottom {
            height / 2..height
        } else {
            0..height / 2
        };
        let cols = if right {
            width / 2..width
        } else {
            0..width / 2
        };
        rows.flat_map(|row| pixels[row * width..][cols.clone()].to_vec())
            .collect()
    }

    /// Draw one screen on a frame of its own and hand back the pixels.
    fn frame(screen: &LockScreen, picture: &Splash, size: (usize, usize)) -> Vec<u32> {
        let (width, height) = size;
        let mut pixels = vec![0u32; width * height];
        let mut fonts = FontStack::new(Box::new(tos_font::BitmapFont::new(1)));
        let mut surface = Surface::new(&mut pixels, width as u32, height as u32, width as u32);
        screen.draw(
            &mut surface,
            &mut fonts,
            Rect::new(0, 0, width as u32, height as u32),
            &Chrome::default(),
            Backdrop {
                splash: Some(picture),
                repaint: Repaint::Everything,
            },
            Instant::now(),
        );
        pixels
    }

    #[test]
    fn the_login_picture_is_above_the_box_and_the_locks_is_in_the_corner() {
        // #132 gave the login screen a frontispiece and deliberately left the
        // lock without one: what somebody at a locked screen came for is the
        // field, and a picture over it would be decoration in the way of that.
        // A lock has a picture again now, and that argument is why it is in a
        // corner rather than over the box — the placement is the whole of the
        // difference, so it is the thing asserted here.
        let size = (640usize, 360usize);
        let login = frame(
            &LockScreen::login(hash_of("hunter2"), "tos".into()),
            &Splash::built_in().expect("the login picture tOS ships"),
            size,
        );
        let lock = frame(
            &LockScreen::new(hash_of("hunter2"), "tos".into()),
            &Splash::built_in_lock().expect("the lock picture tOS ships"),
            size,
        );
        for (word, pixels) in [("login", &login), ("locked", &lock)] {
            let drawn = colours(pixels);
            assert!(
                drawn > 1000,
                "the {word} screen drew {drawn} colours, which is no picture"
            );
        }

        // The frontispiece starts above where the box alone would.
        let login_top = first_drawn_row(&login, size.0);
        let lock_top = first_drawn_row(&lock, size.0);
        assert!(
            login_top < lock_top,
            "the login picture should start above the box ({login_top:?}, {lock_top:?})"
        );

        // And the lock's picture is in one corner and no other.
        let bottom_right = colours(&quadrant(&lock, size.0, true, true));
        assert!(
            bottom_right > 1000,
            "the lock drew {bottom_right} colours in the bottom right, which is no picture"
        );
        for (word, right, bottom) in [
            ("bottom left", false, true),
            ("top right", true, false),
            ("top left", false, false),
        ] {
            let drawn = colours(&quadrant(&lock, size.0, right, bottom));
            assert!(
                drawn < 16,
                "the lock drew {drawn} colours in the {word}, which is a picture"
            );
        }
    }

    #[test]
    fn a_lock_picture_that_would_reach_the_box_gives_way_to_it() {
        // The corner is laid out against the corner rather than around the
        // box, so on a display short enough the two meet. The box is the part
        // somebody cannot do without, so it is the picture that goes.
        let size = (400usize, 120usize);
        let pixels = frame(
            &LockScreen::new(hash_of("hunter2"), "tos".into()),
            &Splash::built_in_lock().expect("the lock picture tOS ships"),
            size,
        );
        assert!(
            pixels.iter().any(|&p| p != 0),
            "the box went with the picture"
        );
        let drawn = colours(&pixels);
        assert!(
            drawn < 16,
            "{drawn} colours on a display the picture should have left"
        );
    }

    #[test]
    fn a_display_with_no_room_for_the_picture_still_draws_the_box() {
        // The box is the part that has to survive: a screen that cannot show
        // the picture can still be logged into, and a picture drawn where
        // there was no room for it would be drawn over the field.
        let splash = Splash::built_in().expect("the picture tOS ships");
        let (width, height) = (320usize, 120usize);
        let mut pixels = vec![0u32; width * height];
        let mut fonts = FontStack::new(Box::new(tos_font::BitmapFont::new(1)));
        let mut surface = Surface::new(&mut pixels, width as u32, height as u32, width as u32);
        LockScreen::login(hash_of("hunter2"), "tos".into()).draw(
            &mut surface,
            &mut fonts,
            Rect::new(0, 0, width as u32, height as u32),
            &Chrome::default(),
            Backdrop {
                splash: Some(&splash),
                repaint: Repaint::Everything,
            },
            Instant::now(),
        );
        assert!(
            pixels.iter().any(|&p| p != 0),
            "the box went with the picture"
        );
        let drawn = colours(&pixels);
        assert!(drawn < 16, "{drawn} colours on a display too small for one");
    }

    #[test]
    fn the_account_line_is_found_among_the_others() {
        let hash = hash_of("hunter2");
        let path = credential_file("shadow", &shadow("tos", &hash));
        assert_eq!(read_credential(&path, "tos").expect("a credential"), hash);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_aging_fields_after_the_hash_are_not_part_of_it() {
        // Debian's own lines carry a day number and four more fields, and a
        // reader that took the rest of the line would hand the hasher a
        // credential no password could ever match.
        let hash = hash_of("hunter2");
        let path = credential_file(
            "aged",
            &format!("root:*:20709:0:99999:7:::\ntos:{hash}:20709:0:99999:7:::\n"),
        );
        assert_eq!(read_credential(&path, "tos").expect("a credential"), hash);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn another_accounts_password_is_not_this_sessions() {
        // The live image is root's session, and root's line is `*`. A reader
        // that took the first usable hash in the file would lock a live
        // session against whatever account happened to have one.
        let path = credential_file("other", &shadow("tos", &hash_of("hunter2")));
        let why = read_credential(&path, "root").expect_err("root has no password");
        assert!(matches!(why, NoCredential::NoPassword(..)), "{why}");
        assert!(
            why.to_string().contains("no password is set for root"),
            "{why}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_file_is_no_credential_rather_than_an_error() {
        let path = std::env::temp_dir().join("tos-lock-definitely-not-here-1a2b3c");
        let _ = std::fs::remove_file(&path);
        let why = read_credential(&path, "tos").expect_err("there is no file");
        assert!(matches!(why, NoCredential::Missing(_)), "{why}");
        assert!(why.to_string().contains("there is no"), "{why}");
    }

    #[test]
    fn an_account_with_no_line_is_not_an_account_with_no_password() {
        let path = credential_file("stranger", &shadow("tos", &hash_of("hunter2")));
        let why = read_credential(&path, "yusuke").expect_err("no such account");
        assert!(matches!(why, NoCredential::Unknown(..)), "{why}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_account_nothing_authenticates_as_locks_nothing() {
        // Every one of these is a file that is exactly right and an account
        // that has no password: the live image's root, an installed machine
        // whose owner declined one, and a disabled account. None of them is a
        // fault to report.
        for (name, field) in [
            ("star", "*"),
            ("bang", "!"),
            ("disabled", "!$6$salt$digest"),
            ("empty", ""),
        ] {
            let path = credential_file(name, &shadow("tos", field));
            let why = read_credential(&path, "tos").expect_err("no password");
            assert!(matches!(why, NoCredential::NoPassword(..)), "{name}: {why}");
            let _ = std::fs::remove_file(&path);
        }
    }

    #[test]
    fn a_password_that_is_not_a_hash_is_refused_and_says_why() {
        // The failure that matters: this must not be reported as a wrong
        // password, because the screen would then never open. `$y$` is what
        // Debian's own passwd(1) writes, so it is the one that will actually
        // turn up.
        for (name, field) in [
            ("yescrypt", "$y$j9T$salt$digest"),
            ("nonsense", "not a hash at all"),
        ] {
            let path = credential_file(name, &shadow("tos", field));
            let why = read_credential(&path, "tos").expect_err("not a usable credential");
            assert!(matches!(why, NoCredential::Unusable(..)), "{name}: {why}");
            let _ = std::fs::remove_file(&path);
        }
    }

    #[test]
    fn the_credential_is_read_once_and_not_per_attempt() {
        // A file that goes away while the screen is locked must not be able to
        // lock the owner out of their own session.
        let hash = hash_of("hunter2");
        let path = credential_file("removed", &shadow("tos", &hash));
        let mut lock = LockScreen::new(
            read_credential(&path, "tos").expect("a credential"),
            "tos".into(),
        );
        std::fs::remove_file(&path).expect("remove");
        assert_eq!(
            offer(&mut lock, "hunter2", Instant::now()),
            LockOutcome::Unlocked
        );
    }
}
