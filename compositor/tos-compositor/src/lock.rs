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

/// Where the credential lives when nobody says otherwise.
///
/// tOS's own file, not `/etc/shadow`: writing a tOS password into the file
/// Debian's PAM reads would silently make it the machine's login password too,
/// and the hashes already in that file are yescrypt, which this workspace has
/// no way to check. `docs/design/screen-lock.md` works through both.
pub const CREDENTIAL_PATH: &str = "/etc/tos/shadow";

/// The widest the box grows, however wide the display is.
const MAX_WIDTH: usize = 44;
/// Rows the box is: border, field, divider, message, border.
const BOX_ROWS: usize = 5;
/// What is written in front of the masked field.
const PROMPT: &str = "password: ";
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
    /// There is no credential file. The ordinary case on the live ISO, and on
    /// an installed machine whose owner declined a password.
    Missing(PathBuf),
    /// The file is there and could not be read.
    Unreadable(PathBuf, io::Error),
    /// The file is there and holds nothing this can check a password against.
    /// Not a wrong password — a lock that treated it as one would be a lock
    /// nobody could ever open.
    Unusable(PathBuf, String),
}

impl fmt::Display for NoCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NoCredential::Missing(path) => {
                write!(f, "no password is set in {}", path.display())
            }
            NoCredential::Unreadable(path, error) => {
                write!(f, "cannot read {}: {error}", path.display())
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

/// Read the credential, and say why there is none rather than how many ways
/// there might have been.
///
/// The file holds one line: the `$6$` crypt hash `tos-install` wrote, which is
/// an ordinary shadow-format hash any other tool on the machine can read.
/// Blank lines and anything after a `#` are skipped so that a person who opens
/// the file to see what it is can leave a note in it.
///
/// The mode of the file is not checked. It should be 0600 and the installer
/// writes it that way, but refusing to lock a screen because the hash is more
/// readable than it ought to be would trade a lock that works for one that
/// does not, over a file only root can reach in the first place.
pub fn read_credential(path: &Path) -> Result<String, NoCredential> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(NoCredential::Missing(path.to_path_buf()))
        }
        Err(e) => return Err(NoCredential::Unreadable(path.to_path_buf(), e)),
    };
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .ok_or_else(|| NoCredential::Unusable(path.to_path_buf(), "the file is empty".into()))?;
    // Parsing it now rather than at the first keypress is the point: the
    // decision that this machine can be locked is made before the screen goes
    // up, so a file that does not parse can never become a locked session
    // waiting for a password that will never be accepted.
    match tos_crypt::Hash::parse(line) {
        Ok(_) => Ok(line.to_string()),
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
pub struct LockScreen {
    /// The credential line, read once when the lock engaged.
    ///
    /// Once, rather than per attempt, so that the file being removed, renamed
    /// or made unreadable while the screen is locked cannot lock the owner out
    /// of their own session.
    hash: String,
    /// What has been typed, which nothing but [`LockScreen::submit`] reads.
    typed: String,
    /// Wrong answers so far, which is what the wait is computed from.
    attempts: u32,
    /// When the next attempt will be looked at. Typing is never held up; only
    /// checking is, because checking is the only thing a guess costs.
    ready_at: Option<Instant>,
}

impl LockScreen {
    /// A fresh lock over this credential.
    pub fn new(hash: String) -> Self {
        LockScreen {
            hash,
            typed: String::new(),
            attempts: 0,
            ready_at: None,
        }
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
    /// This draws the box and nothing else. Erasing the session is the
    /// caller's, because only the caller knows that the whole surface is its
    /// to erase.
    pub fn draw(
        &self,
        surface: &mut Surface<'_>,
        fonts: &mut FontStack,
        area: Rect,
        chrome: &Chrome,
        now: Instant,
    ) {
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
        let y0 = area.y + (((rows - BOX_ROWS) / 2) * ch as usize) as i32;

        // Nothing of the session is under this, but the box still paints its
        // own background so the border has something to sit on.
        surface.fill(
            Rect::new(x0, y0, box_cols as u32 * cw, BOX_ROWS as u32 * ch),
            chrome.background,
        );

        let border = chrome.divider_focused;
        let row_y = |row: usize| y0 + (row as u32 * ch) as i32;
        let rule = |left: char, right: char| {
            let mut text = left.to_string();
            pad_to(&mut text, box_cols - 1, '─');
            text.push(right);
            text
        };

        let mut top = "┌─ locked ".to_string();
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
        LockScreen::new(hash_of(password))
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
        let mut lock = LockScreen::new(hash_of(""));
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

    #[test]
    fn a_credential_file_is_read_back_as_written() {
        let hash = hash_of("hunter2");
        let path = credential_file("plain", &format!("{hash}\n"));
        assert_eq!(read_credential(&path).expect("a credential"), hash);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_file_with_a_note_in_it_still_reads() {
        let hash = hash_of("hunter2");
        let path = credential_file("commented", &format!("# the tOS lock password\n\n{hash}\n"));
        assert_eq!(read_credential(&path).expect("a credential"), hash);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_file_is_no_credential_rather_than_an_error() {
        let path = std::env::temp_dir().join("tos-lock-definitely-not-here-1a2b3c");
        let _ = std::fs::remove_file(&path);
        let why = read_credential(&path).expect_err("there is no file");
        assert!(matches!(why, NoCredential::Missing(_)), "{why}");
        assert!(why.to_string().contains("no password is set"), "{why}");
    }

    #[test]
    fn a_file_that_is_not_a_hash_is_refused_and_says_why() {
        // The failure that matters: this must not be reported as a wrong
        // password, because the screen would then never open.
        for (name, contents) in [
            ("empty", ""),
            ("blank", "\n\n# nothing but a note\n"),
            ("yescrypt", "$y$j9T$salt$digest\n"),
            ("nonsense", "not a hash at all\n"),
        ] {
            let path = credential_file(name, contents);
            let why = read_credential(&path).expect_err("not a usable credential");
            assert!(matches!(why, NoCredential::Unusable(..)), "{name}: {why}");
            let _ = std::fs::remove_file(&path);
        }
    }

    #[test]
    fn the_credential_is_read_once_and_not_per_attempt() {
        // A file that goes away while the screen is locked must not be able to
        // lock the owner out of their own session.
        let hash = hash_of("hunter2");
        let path = credential_file("removed", &format!("{hash}\n"));
        let mut lock = LockScreen::new(read_credential(&path).expect("a credential"));
        std::fs::remove_file(&path).expect("remove");
        assert_eq!(
            offer(&mut lock, "hunter2", Instant::now()),
            LockOutcome::Unlocked
        );
    }
}
