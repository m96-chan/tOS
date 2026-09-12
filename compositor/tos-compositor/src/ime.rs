//! Japanese input: the engine the compositor owns, and the context a pane has.
//!
//! `tos-ime` decides what `ka` means and what かんじ could be. Nothing in it
//! knows about a keyboard, a pane or a screen, which is why this module
//! exists: it is the half that knows which key arrived, which pane it was
//! meant for, and where on the display the half-typed word has to appear.
//!
//! The split is the one `docs/design/ime.md` argues for and it is not
//! arbitrary. The dictionary is megabytes and is loaded once, so it lives on
//! the compositor beside `fonts` and `keymap`. The mode and the preedit are
//! what a person perceives — is kana on, and what have I half-typed — and
//! those belong to the program being typed at, so they live on [`Pane`]
//! beside `selection` and `textures`. With a context per pane, moving focus
//! mid-preedit needs no rule at all: the preedit stays where it was going.
//! With one global context every available rule is wrong — committing text
//! the user did not commit, discarding text they did not discard, or carrying
//! it to a program they were not typing to.
//!
//! [`Pane`]: crate::pane::Pane

use std::io;
use std::path::{Path, PathBuf};

use tos_font::FontStack;
use tos_ime::dict::{Candidate, Dictionary, FileSource, Source};
use tos_ime::romaji::{to_halfwidth_katakana, to_katakana, Converter, Kana};
use tos_render::Surface;
use tos_session::{PaneId, Rect};
use tos_term::Terminal;

use crate::chrome::{self, BoxLine, BoxRect, Chrome};

/// The most candidates a window offers at once.
///
/// Nine because the number keys pick them and there are nine of those. A
/// tenth row would be a row nothing on the keyboard can choose, which is
/// worse than a list that says it has more by stopping.
pub const MAX_CANDIDATES: usize = 9;

/// Which script the preedit is written in, and so what 無変換 and F7 cycle
/// through.
///
/// This is not a mode of the input method. `docs/design/ime.md` has two of
/// those and katakana is not one of them: it is a conversion applied to the
/// preedit, which is what 無変換 does on the hardware and what F7 does
/// everywhere else. Making it a mode would make it a mode a user can get
/// stuck in.
fn next_script(kana: Kana) -> Kana {
    match kana {
        Kana::Hiragana => Kana::Katakana,
        Kana::Katakana => Kana::Halfwidth,
        // Back to where it started rather than stopping at halfwidth: a cycle
        // that does not come round is a one way door on a key with no label
        // saying so.
        Kana::Halfwidth => Kana::Hiragana,
    }
}

/// Rewrite settled kana into another script.
///
/// The preedit holds kana that are already settled, so changing script means
/// rewriting what is there rather than telling the converter to write the
/// next one differently — the converter's carry is romaji and looks after
/// itself.
fn in_script(hiragana: &str, kana: Kana) -> String {
    match kana {
        Kana::Hiragana => hiragana.to_string(),
        Kana::Katakana => to_katakana(hiragana),
        // Either script goes in, so the katakana step is already inside it.
        Kana::Halfwidth => to_halfwidth_katakana(hiragana),
    }
}

/// Which of the four states of `docs/design/ime.md` a pane is in.
///
/// Derived rather than stored, because three of the four are facts about the
/// preedit and the conversion and a stored copy would be a second place for
/// them to disagree. There is no fifth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// What tOS does with no IME at all: every key goes to the pane.
    Direct,
    /// Kana mode, with nothing typed yet.
    Kana,
    /// Kana typed and not yet given to the program.
    Preedit,
    /// A candidate list is open over the preedit.
    Converting,
}

/// A conversion in progress: what is being converted, what it could be, and
/// which of those the cursor is on.
#[derive(Debug, Clone)]
pub struct Conversion {
    /// The kana the candidates are replacements for. Kept so that Escape can
    /// put it back — the candidates are a proposal, and the reading is what
    /// the user actually typed.
    reading: String,
    candidates: Vec<Candidate>,
    cursor: usize,
}

impl Conversion {
    pub fn reading(&self) -> &str {
        &self.reading
    }

    pub fn candidates(&self) -> &[Candidate] {
        &self.candidates
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// The word the cursor is on, which is what Enter commits.
    pub fn chosen(&self) -> &str {
        &self.candidates[self.cursor].word
    }

    /// Walk to the next candidate, wrapping. 変換 pressed again does this,
    /// which is how every Japanese IME is driven and the reason the list
    /// needs no arrow keys to be usable.
    fn next(&mut self) {
        self.cursor = (self.cursor + 1) % self.candidates.len();
    }

    fn previous(&mut self) {
        self.cursor = (self.cursor + self.candidates.len() - 1) % self.candidates.len();
    }
}

/// The input method itself: one per session.
///
/// The dictionary is an `Option` and a missing one is not an error. A machine
/// with no `SKK-JISYO` still types kana, and a compositor that refused to
/// start because a data file was absent would be a compositor that cannot be
/// booted from `/init` on an image somebody trimmed.
pub struct Ime {
    dictionary: Option<Dictionary>,
    /// Where the dictionary came from, for anyone who wants to know which of
    /// the several paths won.
    source: Option<PathBuf>,
    /// The conversion in progress, and the pane it belongs to.
    ///
    /// The pane is carried the way [`crate::compositor::Compositor`] carries
    /// copy mode's, and for the same reason: a candidate list offers
    /// replacements for one particular preedit, and one that followed the
    /// focus would come back offering them for text it never saw. Only one
    /// pane can be converting, because only one pane is being typed at.
    conversion: Option<(PaneId, Conversion)>,
}

impl Default for Ime {
    fn default() -> Self {
        Ime::empty()
    }
}

impl Ime {
    /// An input method with no dictionary: kana work, conversion finds
    /// nothing.
    pub fn empty() -> Ime {
        Ime {
            dictionary: None,
            source: None,
            conversion: None,
        }
    }

    /// Read a dictionary through the seam `tos-ime` provides, so that a test
    /// can hand over four entries written inline and never touch a disk.
    pub fn load(&mut self, source: &dyn Source) -> io::Result<()> {
        self.dictionary = Some(Dictionary::open(source)?);
        Ok(())
    }

    /// Open the first dictionary that is there, in the order
    /// `docs/design/ime.md` sets out.
    ///
    /// Returns what went wrong when a *configured* path could not be read,
    /// because a setting that silently does nothing is indistinguishable from
    /// one that is broken. A search that finds nothing is not a problem and
    /// says nothing: most machines have no dictionary and still want kana.
    pub fn open(configured: Option<&Path>) -> (Ime, Option<String>) {
        let mut ime = Ime::empty();
        if let Some(path) = configured {
            let source = FileSource::new(path);
            return match ime.load(&source) {
                Ok(()) => {
                    ime.source = Some(path.to_path_buf());
                    (ime, None)
                }
                Err(e) => (ime, Some(format!("{}: {e}", path.display()))),
            };
        }
        for path in search_path() {
            if ime.load(&FileSource::new(&path)).is_ok() {
                ime.source = Some(path);
                break;
            }
        }
        (ime, None)
    }

    /// Which file the dictionary was read from, if one was.
    pub fn dictionary_path(&self) -> Option<&Path> {
        self.source.as_deref()
    }

    pub fn has_dictionary(&self) -> bool {
        self.dictionary.is_some()
    }

    /// The conversion open over `pane`, if that pane is the one converting.
    pub fn conversion(&self, pane: PaneId) -> Option<&Conversion> {
        self.conversion
            .as_ref()
            .filter(|(owner, _)| *owner == pane)
            .map(|(_, conversion)| conversion)
    }

    fn conversion_mut(&mut self, pane: PaneId) -> Option<&mut Conversion> {
        self.conversion
            .as_mut()
            .filter(|(owner, _)| *owner == pane)
            .map(|(_, conversion)| conversion)
    }

    /// Forget any conversion belonging to `pane`. A pane that dies, or whose
    /// preedit is abandoned, takes its candidate list with it.
    pub fn end_conversion(&mut self, pane: PaneId) {
        if self.conversion(pane).is_some() {
            self.conversion = None;
        }
    }

    /// Look a reading up and open a candidate list over it, or say there was
    /// nothing to offer.
    fn begin_conversion(&mut self, pane: PaneId, reading: &str) -> bool {
        let candidates = match &self.dictionary {
            Some(dictionary) => dictionary.lookup(reading),
            None => Vec::new(),
        };
        if candidates.is_empty() {
            return false;
        }
        self.conversion = Some((
            pane,
            Conversion {
                reading: reading.to_string(),
                candidates,
                cursor: 0,
            },
        ));
        true
    }
}

/// Where a dictionary is looked for, best first.
///
/// The user's own comes first because somebody who has curated one for years
/// means it. The system copy comes last because it is the one that is always
/// there: an installed machine starts the compositor from `/init`, where
/// there is no home directory and often no environment at all, which is the
/// same reasoning `config_file::search_path` gives for its own order.
pub fn search_path() -> Vec<PathBuf> {
    let data_home = std::env::var("XDG_DATA_HOME").ok();
    let home = std::env::var("HOME").ok();
    search_path_from(data_home.as_deref(), home.as_deref())
}

fn search_path_from(data_home: Option<&str>, home: Option<&str>) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    // The basedir spec says an empty variable counts as unset, and empty is
    // exactly what a stripped-down init environment tends to hand over.
    match data_home.filter(|v| !v.is_empty()) {
        Some(dir) => paths.push(Path::new(dir).join("tos").join("SKK-JISYO")),
        None => {
            if let Some(home) = home.filter(|v| !v.is_empty()) {
                paths.push(
                    Path::new(home)
                        .join(".local/share")
                        .join("tos")
                        .join("SKK-JISYO"),
                );
            }
        }
    }
    // What `iso/mkiso.sh` writes today, beside the VL Gothic copy it already
    // makes.
    paths.push(PathBuf::from("/usr/share/tos/SKK-JISYO.L"));
    // Where Debian's `skkdic` package puts it, which is what takes over once
    // tOS has a rootfs and the initramfs copy goes away.
    paths.push(PathBuf::from("/usr/share/skk/SKK-JISYO.L"));
    paths
}

/// One pane's input context: the mode, the preedit, and the rows the preedit
/// covered when it was last painted.
#[derive(Debug, Default)]
pub struct ImeContext {
    /// Kana mode, per pane, because someone with vim in one pane and a chat
    /// client in another wants it off in the first and on in the second.
    enabled: bool,
    /// Romaji that has been typed and has not yet decided what it is. The
    /// carry lives here rather than on the engine because it is half of the
    /// preedit: a carry left on the engine would follow the focus to a pane
    /// that never typed it.
    converter: Converter,
    /// Kana settled so far, in the script they were settled in.
    preedit: String,
    /// The same kana in hiragana, which is what the dictionary is keyed by.
    /// Kept beside the preedit rather than converted back from it, because
    /// halfwidth katakana does not round-trip: ｶﾞ is two characters and
    /// nothing says which of them the が came from.
    reading: String,
    /// Which script the settled kana are written in.
    script: Kana,
    /// Rows of this pane's own grid the IME painted over last frame.
    ///
    /// Rows, not pixels. A pane resized under an open preedit has moved its
    /// own cursor, and a stored pixel rectangle would damage rows that are no
    /// longer the ones it covered; rows are clamped by `Damage` itself and
    /// the rectangle to paint is recomputed from the cursor every frame.
    painted: Option<(usize, usize)>,
}

impl ImeContext {
    /// Whether kana mode is on for this pane.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// The kana typed so far, followed by the romaji that has not settled.
    ///
    /// The carry is part of what is drawn or it is invisible: someone who has
    /// typed `ky` and sees nothing has no way to know the keyboard heard
    /// them.
    pub fn display(&self) -> String {
        format!("{}{}", self.preedit, self.converter.carry())
    }

    /// Whether anything is being held that a program has not been given.
    pub fn is_composing(&self) -> bool {
        !self.preedit.is_empty() || !self.converter.is_empty()
    }

    /// Which of the four states this pane is in.
    pub fn state(&self, converting: bool) -> State {
        if !self.enabled {
            State::Direct
        } else if converting {
            State::Converting
        } else if self.is_composing() {
            State::Preedit
        } else {
            State::Kana
        }
    }

    /// Turn kana mode on or off, and say which it now is.
    ///
    /// Turning it off abandons whatever was half-typed rather than committing
    /// it. The user pressed a key that means "stop doing this", and a toggle
    /// that posted text into a program on the way out would be a toggle
    /// nobody could use to get out of trouble.
    pub fn toggle(&mut self) -> bool {
        self.enabled = !self.enabled;
        self.abandon();
        self.enabled
    }

    /// Throw away everything uncommitted. Nothing was ever sent, so there is
    /// nothing to take back.
    pub fn abandon(&mut self) {
        self.converter.clear();
        self.preedit.clear();
        self.reading.clear();
        self.script = Kana::Hiragana;
    }

    /// Settle whatever the converter is still holding.
    ///
    /// `flush` is what makes a trailing `n` into ん; without it the last
    /// syllable of こんばん would be committed as a bare `n`, and a lookup of
    /// かんj would find nothing.
    fn settle(&mut self) {
        let settled = self.converter.flush();
        self.reading.push_str(&settled);
        self.preedit.push_str(&in_script(&settled, self.script));
    }

    /// Settle the trailing carry and take the whole preedit out.
    fn take(&mut self) -> String {
        self.settle();
        let text = std::mem::take(&mut self.preedit);
        self.reading.clear();
        self.script = Kana::Hiragana;
        text
    }

    /// Feed one character of romaji.
    fn push(&mut self, ch: char) {
        let settled = self.converter.push(ch);
        self.reading.push_str(&settled);
        self.preedit.push_str(&in_script(&settled, self.script));
    }

    /// Delete one character of what is held, and say whether there was one.
    ///
    /// The carry goes first: someone who typed `ky` and changed their mind
    /// would otherwise delete the syllable in front of it while `ky` sat
    /// there invisible to them.
    fn backspace(&mut self) -> bool {
        if self.converter.backspace() {
            return true;
        }
        if self.reading.pop().is_some() {
            self.preedit.pop();
            // Halfwidth katakana is the one script where a kana is not one
            // character, so the preedit and the reading come apart. Rewriting
            // the preedit from the reading is cheap and is the only way they
            // cannot drift.
            self.preedit = in_script(&self.reading, self.script);
            return true;
        }
        false
    }

    /// Rewrite the preedit in the next script along.
    fn cycle_script(&mut self) {
        // Settle the carry first, or a script change part way through `kya`
        // would leave `ky` to land in whichever script is current when it
        // finishes, which is not the one the user just asked for.
        self.settle();
        self.script = next_script(self.script);
        self.preedit = in_script(&self.reading, self.script);
    }

    /// Rewrite the preedit in one particular script, for the key that names
    /// one rather than cycling.
    fn set_script(&mut self, script: Kana) {
        self.settle();
        self.script = script;
        self.preedit = in_script(&self.reading, self.script);
    }

    /// Replace the preedit with a chosen candidate, ready to be committed.
    fn set_preedit(&mut self, text: &str) {
        self.preedit = text.to_string();
    }

    /// Put the unconverted kana back, for the Escape that leaves a candidate
    /// list.
    fn restore_kana(&mut self) {
        self.preedit = in_script(&self.reading, self.script);
    }

    /// The rows this pane's IME painted last frame, taken.
    pub fn take_painted(&mut self) -> Option<(usize, usize)> {
        self.painted.take()
    }

    pub fn set_painted(&mut self, rows: Option<(usize, usize)>) {
        self.painted = rows;
    }
}

/// What one key did to a pane's context, so the caller knows what to do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImeOutcome {
    /// The key was not the IME's. The existing path runs untouched, which in
    /// Direct mode is every key and is why Direct costs nothing.
    Passthrough,
    /// The key changed what is on screen and nothing else.
    Consumed,
    /// These bytes are now the program's.
    Commit(String),
}

/// Offer one key to a pane's input context.
///
/// A free function rather than a method on either half, because it is the one
/// operation that needs both: the context holds what has been typed and the
/// engine holds what it could mean.
///
/// The caller has already established that this key belongs to the focused
/// pane — it is the `Resolution::Passthrough` arm of `Keymap::resolve` and
/// nothing else — and that it is a press.
pub fn handle_key(
    ime: &mut Ime,
    ctx: &mut ImeContext,
    pane: PaneId,
    key: &tos_input::KeyEvent,
) -> ImeOutcome {
    use tos_input::{ImeKey, KeyCode, Modifiers};

    // Direct mode is not a branch the key passes through, it is the absence
    // of one. Nothing below runs, nothing is allocated, and what reaches the
    // PTY is byte for byte what reached it before this module existed.
    if !ctx.enabled {
        return ImeOutcome::Passthrough;
    }

    let converting = ime.conversion(pane).is_some();
    // A key with ctrl or alt on it is not text and is not the IME's. The
    // preedit is left standing rather than committed or thrown away: the user
    // asked for neither, and a ctrl+c that posted a half-typed word into a
    // shell on its way past would be the IME putting words in their mouth.
    let held = key.modifiers.effective().without(Modifiers::SHIFT);
    if !held.is_empty() {
        return ImeOutcome::Passthrough;
    }

    match key.code {
        // 変換, and the space bar, which is what everyone without a JIS
        // keyboard converts with. Pressed again it walks the list, which is
        // how every Japanese IME is driven.
        KeyCode::Ime(ImeKey::Convert) | KeyCode::Char(' ') if ctx.is_composing() => {
            if let Some(conversion) = ime.conversion_mut(pane) {
                conversion.next();
                let chosen = conversion.chosen().to_string();
                ctx.set_preedit(&chosen);
                return ImeOutcome::Consumed;
            }
            ctx.settle();
            let reading = ctx.reading.clone();
            if ime.begin_conversion(pane, &reading) {
                let chosen = ime
                    .conversion(pane)
                    .expect("just opened")
                    .chosen()
                    .to_string();
                ctx.set_preedit(&chosen);
            }
            // Consumed either way. A 変換 that found nothing has still
            // settled the carry, and handing the key to the program as well
            // would type a space into the middle of a word.
            ImeOutcome::Consumed
        }
        // With nothing typed, a space is a space: the pane is owed it, and an
        // IME that swallowed the space bar in kana mode would be one that
        // cannot type a sentence. 変換 with nothing to convert means nothing,
        // and `encode_key` gives it no bytes, so the pane loses nothing
        // either way.
        KeyCode::Char(' ') | KeyCode::Ime(ImeKey::Convert) => ImeOutcome::Passthrough,
        // 無変換: take what was typed, unconverted. During a conversion that
        // means the kana rather than the candidate, which is the one thing
        // the candidate list cannot otherwise give back and commit in one
        // key.
        KeyCode::Ime(ImeKey::NonConvert) if converting => {
            ime.end_conversion(pane);
            ctx.restore_kana();
            ImeOutcome::Commit(ctx.take())
        }
        // With no conversion open it cycles the script, which is what the key
        // does on the hardware and what F7 and F8 do everywhere else.
        KeyCode::Ime(ImeKey::NonConvert) | KeyCode::Function(7) if ctx.is_composing() => {
            ctx.cycle_script();
            ImeOutcome::Consumed
        }
        // F8 is halfwidth katakana wherever there is an F8, and it is one key
        // rather than three presses of the cycle.
        KeyCode::Function(8) if ctx.is_composing() => {
            ctx.set_script(Kana::Halfwidth);
            ImeOutcome::Consumed
        }
        // The number keys pick from the list, which is faster than walking to
        // the fourth candidate and is what the numbers down the side are for.
        KeyCode::Char(digit @ '1'..='9') if converting => {
            let index = digit as usize - '1' as usize;
            let Some(conversion) = ime.conversion_mut(pane) else {
                return ImeOutcome::Passthrough;
            };
            let Some(candidate) = conversion.candidates.get(index) else {
                // A digit past the end of the list is not a choice. Swallowed
                // rather than typed, because a 7 landing in the shell because
                // there were only four candidates is not what the finger
                // meant.
                return ImeOutcome::Consumed;
            };
            let word = candidate.word.clone();
            ime.end_conversion(pane);
            ctx.set_preedit(&word);
            ImeOutcome::Commit(ctx.take())
        }
        // Up and down walk the list the way a list is walked. 変換 does the
        // same thing forwards and is what a Japanese typist reaches for; the
        // arrows are for everyone else.
        KeyCode::Down | KeyCode::Tab if converting => {
            if let Some(conversion) = ime.conversion_mut(pane) {
                conversion.next();
                let chosen = conversion.chosen().to_string();
                ctx.set_preedit(&chosen);
            }
            ImeOutcome::Consumed
        }
        KeyCode::Up if converting => {
            if let Some(conversion) = ime.conversion_mut(pane) {
                conversion.previous();
                let chosen = conversion.chosen().to_string();
                ctx.set_preedit(&chosen);
            }
            ImeOutcome::Consumed
        }
        KeyCode::Enter if ctx.is_composing() => {
            ime.end_conversion(pane);
            ImeOutcome::Commit(ctx.take())
        }
        // Escape backs out one step at a time: out of the candidate list to
        // the kana, and out of the kana to nothing. With nothing held it is
        // the program's — vim is one Escape away from unusable otherwise.
        KeyCode::Escape if converting => {
            ime.end_conversion(pane);
            ctx.restore_kana();
            ImeOutcome::Consumed
        }
        KeyCode::Escape if ctx.is_composing() => {
            ctx.abandon();
            ImeOutcome::Consumed
        }
        // Backspace during a conversion is Escape: the candidate list is a
        // proposal, and the first thing a backspace means is "not that one".
        KeyCode::Backspace if converting => {
            ime.end_conversion(pane);
            ctx.restore_kana();
            ImeOutcome::Consumed
        }
        KeyCode::Backspace => {
            if ctx.backspace() {
                ImeOutcome::Consumed
            } else {
                // Nothing was held, so the backspace is about the program's
                // own text and not about ours.
                ImeOutcome::Passthrough
            }
        }
        KeyCode::Char(_) => {
            let Some(ch) = key.text else {
                return ImeOutcome::Passthrough;
            };
            // Typing during a conversion abandons the conversion and extends
            // the preedit. It deliberately does not narrow the list the way
            // typing into an overlay does — that is the opposite operation,
            // and it is the third reason a candidate window is not an
            // `Overlay`.
            if converting {
                ime.end_conversion(pane);
                ctx.restore_kana();
            }
            ctx.push(ch);
            ImeOutcome::Consumed
        }
        // Everything else — the arrows outside a conversion, Home, the
        // function keys — belongs to the program. An IME that ate them would
        // be an IME that has to be turned off to use an editor.
        _ => ImeOutcome::Passthrough,
    }
}

/// Where the IME painted, in rows of the pane's own grid.
///
/// Returned by [`draw`] so the caller can hand it to `Damage::mark_range`
/// next frame. `render()` skips any row that is not dirty and a pane has no
/// idea the preedit is there, so without this the frame after a commit
/// repaints nothing and the committed glyphs stay on screen twice.
pub type PaintedRows = Option<(usize, usize)>;

/// Draw the preedit over the pane's cursor row, and the candidate window at
/// the cursor.
///
/// `area` is the pane's rectangle in cells, in the screen's own grid.
/// Everything is clipped to it: a preedit spilling into the neighbour would
/// be drawn over a program that has no idea.
pub fn draw(
    surface: &mut Surface<'_>,
    fonts: &mut FontStack,
    chrome: &Chrome,
    area: Rect,
    terminal: &Terminal,
    ctx: &ImeContext,
    conversion: Option<&Conversion>,
) -> PaintedRows {
    let text = ctx.display();
    if text.is_empty() {
        return None;
    }
    let metrics = fonts.metrics();
    let (cw, ch) = (metrics.cell_width.max(1), metrics.cell_height.max(1));
    let cursor = terminal.cursor();
    // Recomputed from the cursor every frame rather than stored, because a
    // pane resized under an open preedit has moved its cursor and a
    // remembered rectangle would be over the wrong cells.
    let col = cursor.x.min(area.width.saturating_sub(1) as usize);
    let row = cursor.y.min(area.height.saturating_sub(1) as usize);
    let room = (area.width as usize).saturating_sub(col);
    if room == 0 || area.height == 0 {
        return None;
    }

    let x = ((area.x as usize + col) as u32 * cw) as i32;
    let y = ((area.y as usize + row) as u32 * ch) as i32;
    // The tail rather than the head: the end of a preedit is where the cursor
    // is and where the next key lands, so a word longer than the room left is
    // cut at the front, the way a shell cuts a long line.
    let shown = clip_tail(&text, room);
    chrome::draw_text(
        surface,
        fonts,
        x,
        y,
        &shown,
        chrome.accent_text,
        Some(chrome.accent),
        false,
    );

    let mut top = row;
    let mut bottom = row + 1;
    if let Some(conversion) = conversion {
        if let Some((first, last)) =
            draw_candidates(surface, fonts, chrome, area, col, row, conversion, cw, ch)
        {
            top = top.min(first);
            bottom = bottom.max(last);
        }
    }
    Some((top, bottom))
}

/// Draw the candidate window, and say which rows of the pane it covered.
#[allow(clippy::too_many_arguments)]
fn draw_candidates(
    surface: &mut Surface<'_>,
    fonts: &mut FontStack,
    chrome: &Chrome,
    area: Rect,
    col: usize,
    row: usize,
    conversion: &Conversion,
    cw: u32,
    ch: u32,
) -> Option<(usize, usize)> {
    let labels: Vec<String> = conversion
        .candidates
        .iter()
        .take(MAX_CANDIDATES)
        .enumerate()
        .map(|(index, candidate)| match &candidate.annotation {
            // The annotation is often the only thing that says which of 慣例
            // and 寒冷 is being looked at, which is why `tos-ime` kept it.
            Some(note) => format!("{} {}  {note}", index + 1, candidate.word),
            None => format!("{} {}", index + 1, candidate.word),
        })
        .collect();
    let widest = labels
        .iter()
        .map(|label| tos_term::str_width(label))
        .max()
        .unwrap_or(0);
    // Two borders, and never wider than the pane it belongs to — but also
    // never so narrow that the reading in the top border is cut to something
    // the user did not type. `draw_box` writes the title as `┌─ {title} ` and
    // clips it to `cols - 6`, so a four kana reading over a two kana candidate
    // used to come out as 「か」 above a list of 慣例/寒冷/管領/艦齢: a window
    // saying the wrong word about the very thing it is offering to replace.
    // The candidates decide the width until the reading needs more.
    let title = tos_term::str_width(conversion.reading());
    let cols = (widest + 2).max(title + 6).min(area.width as usize);
    let rows = (labels.len() + 2).min(area.height as usize);
    if cols < 3 || rows < 3 {
        // Nothing honest to draw in a pane this small. The preedit is still
        // on screen, so the conversion is not invisible — it is just not
        // offering a list nobody could read.
        return None;
    }

    // Below the cursor, and above it when there is no room below. This is the
    // rule every IME uses and the only one that does not cover the text being
    // converted.
    let below = row + 1;
    let top = if below + rows <= area.height as usize {
        below
    } else {
        row.saturating_sub(rows)
    };
    // Pushed left until it fits rather than clipped, so the last candidate is
    // as readable as the first.
    let left = col.min((area.width as usize).saturating_sub(cols));

    let lines: Vec<BoxLine<'_>> = labels
        .iter()
        .enumerate()
        .map(|(index, label)| BoxLine::Text {
            text: label,
            fg: if index == conversion.cursor {
                chrome.accent_text
            } else {
                chrome.foreground
            },
            bg: if index == conversion.cursor {
                Some(chrome.accent)
            } else {
                None
            },
            bold: false,
        })
        .collect();
    chrome::draw_box(
        surface,
        fonts,
        BoxRect::new(
            ((area.x as usize + left) as u32 * cw) as i32,
            ((area.y as usize + top) as u32 * ch) as i32,
            cols,
            rows,
        ),
        Some(conversion.reading()),
        &lines,
        chrome,
    );
    Some((top, top + rows))
}

/// Cut `text` to the last `cols` cells, saying so when something was cut.
///
/// [`chrome::clip_marked`] keeps the front, which is right for a title and
/// wrong for a preedit: the front of a preedit is the part the user has
/// finished with.
fn clip_tail(text: &str, cols: usize) -> String {
    if tos_term::str_width(text) <= cols || cols == 0 {
        return chrome::clip(text, cols);
    }
    let mut kept: Vec<char> = Vec::new();
    // One cell of the budget goes to the marker that says text was cut.
    let mut used = 1;
    for ch in text.chars().rev() {
        let width = tos_term::char_width(ch).max(1) as usize;
        if used + width > cols {
            break;
        }
        kept.push(ch);
        used += width;
    }
    let mut out = String::from("…");
    out.extend(kept.into_iter().rev());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tos_ime::dict::BytesSource;
    use tos_input::{ImeKey, KeyCode, KeyEvent, Modifiers};

    fn dictionary() -> BytesSource {
        BytesSource::new(
            ";; okuri-nasi entries.\n\
             かんじ /漢字/感じ;feeling/幹事/\n\
             きしゃ /貴社/記者/汽車/帰社/喜捨/기사/騎射/貴사/記社/寄射/\n\
             にほんご /日本語/\n",
        )
    }

    fn ime() -> Ime {
        let mut ime = Ime::empty();
        ime.load(&dictionary()).expect("the dictionary should open");
        ime
    }

    fn pane() -> PaneId {
        PaneId(1)
    }

    fn press(ctx: &mut ImeContext, ime: &mut Ime, code: KeyCode) -> ImeOutcome {
        handle_key(ime, ctx, pane(), &KeyEvent::new(code, Modifiers::NONE))
    }

    fn type_romaji(ctx: &mut ImeContext, ime: &mut Ime, text: &str) {
        for ch in text.chars() {
            press(ctx, ime, KeyCode::Char(ch));
        }
    }

    #[test]
    fn a_context_starts_in_direct_mode_and_takes_no_key() {
        let mut ime = ime();
        let mut ctx = ImeContext::default();
        assert_eq!(ctx.state(false), State::Direct);
        assert_eq!(
            press(&mut ctx, &mut ime, KeyCode::Char('a')),
            ImeOutcome::Passthrough
        );
        assert!(ctx.display().is_empty());
    }

    #[test]
    fn romaji_becomes_kana_in_the_preedit_and_the_carry_is_visible() {
        let mut ime = ime();
        let mut ctx = ImeContext::default();
        ctx.toggle();
        assert_eq!(ctx.state(false), State::Kana);
        type_romaji(&mut ctx, &mut ime, "kanj");
        // The `k` and the `j` are still undecided; drawing only the settled
        // kana would leave the user typing into nothing.
        assert_eq!(ctx.display(), "かんj");
        assert_eq!(ctx.state(false), State::Preedit);
    }

    #[test]
    fn enter_commits_the_kana_and_settles_a_trailing_n() {
        let mut ime = ime();
        let mut ctx = ImeContext::default();
        ctx.toggle();
        type_romaji(&mut ctx, &mut ime, "konban");
        assert_eq!(
            press(&mut ctx, &mut ime, KeyCode::Enter),
            ImeOutcome::Commit("こんばん".into())
        );
        assert!(!ctx.is_composing());
    }

    #[test]
    fn convert_walks_the_candidates_and_enter_commits_the_one_under_the_cursor() {
        let mut ime = ime();
        let mut ctx = ImeContext::default();
        ctx.toggle();
        type_romaji(&mut ctx, &mut ime, "kanji");
        press(&mut ctx, &mut ime, KeyCode::Ime(ImeKey::Convert));
        assert_eq!(ctx.state(true), State::Converting);
        assert_eq!(ctx.display(), "漢字");
        press(&mut ctx, &mut ime, KeyCode::Ime(ImeKey::Convert));
        assert_eq!(ctx.display(), "感じ");
        assert_eq!(
            press(&mut ctx, &mut ime, KeyCode::Enter),
            ImeOutcome::Commit("感じ".into())
        );
        assert!(ime.conversion(pane()).is_none());
    }

    #[test]
    fn the_space_bar_converts_when_something_is_typed_and_is_a_space_when_not() {
        let mut ime = ime();
        let mut ctx = ImeContext::default();
        ctx.toggle();
        // Nothing typed: the pane is owed its space.
        assert_eq!(
            press(&mut ctx, &mut ime, KeyCode::Char(' ')),
            ImeOutcome::Passthrough
        );
        type_romaji(&mut ctx, &mut ime, "kanji");
        assert_eq!(
            press(&mut ctx, &mut ime, KeyCode::Char(' ')),
            ImeOutcome::Consumed
        );
        assert_eq!(ctx.display(), "漢字");
    }

    #[test]
    fn a_number_key_picks_a_candidate_and_commits_it() {
        let mut ime = ime();
        let mut ctx = ImeContext::default();
        ctx.toggle();
        type_romaji(&mut ctx, &mut ime, "kanji");
        press(&mut ctx, &mut ime, KeyCode::Ime(ImeKey::Convert));
        assert_eq!(
            press(&mut ctx, &mut ime, KeyCode::Char('3')),
            ImeOutcome::Commit("幹事".into())
        );
    }

    #[test]
    fn escape_leaves_the_candidates_for_the_kana_and_then_leaves_the_kana() {
        let mut ime = ime();
        let mut ctx = ImeContext::default();
        ctx.toggle();
        type_romaji(&mut ctx, &mut ime, "kanji");
        press(&mut ctx, &mut ime, KeyCode::Ime(ImeKey::Convert));
        press(&mut ctx, &mut ime, KeyCode::Escape);
        assert!(ime.conversion(pane()).is_none());
        assert_eq!(ctx.display(), "かんじ");
        press(&mut ctx, &mut ime, KeyCode::Escape);
        assert!(!ctx.is_composing());
        // And with nothing held it is the program's: vim needs this key.
        assert_eq!(
            press(&mut ctx, &mut ime, KeyCode::Escape),
            ImeOutcome::Passthrough
        );
    }

    #[test]
    fn typing_during_a_conversion_abandons_it_rather_than_narrowing_the_list() {
        let mut ime = ime();
        let mut ctx = ImeContext::default();
        ctx.toggle();
        type_romaji(&mut ctx, &mut ime, "kanji");
        press(&mut ctx, &mut ime, KeyCode::Ime(ImeKey::Convert));
        type_romaji(&mut ctx, &mut ime, "ya");
        assert!(ime.conversion(pane()).is_none());
        assert_eq!(ctx.display(), "かんじや");
    }

    #[test]
    fn backspace_eats_the_carry_then_the_kana_then_belongs_to_the_program() {
        let mut ime = ime();
        let mut ctx = ImeContext::default();
        ctx.toggle();
        type_romaji(&mut ctx, &mut ime, "kanjy");
        assert_eq!(ctx.display(), "かんjy");
        // The carry first, one letter at a time, as it was typed.
        assert_eq!(
            press(&mut ctx, &mut ime, KeyCode::Backspace),
            ImeOutcome::Consumed
        );
        assert_eq!(ctx.display(), "かんj");
        press(&mut ctx, &mut ime, KeyCode::Backspace);
        assert_eq!(ctx.display(), "かん");
        // Then the kana, one kana at a time and not one romaji key at a time.
        for _ in 0..2 {
            press(&mut ctx, &mut ime, KeyCode::Backspace);
        }
        assert!(!ctx.is_composing());
        assert_eq!(
            press(&mut ctx, &mut ime, KeyCode::Backspace),
            ImeOutcome::Passthrough
        );
    }

    #[test]
    fn muhenkan_cycles_the_script_without_becoming_a_mode() {
        let mut ime = ime();
        let mut ctx = ImeContext::default();
        ctx.toggle();
        type_romaji(&mut ctx, &mut ime, "kanji");
        press(&mut ctx, &mut ime, KeyCode::Ime(ImeKey::NonConvert));
        assert_eq!(ctx.display(), "カンジ");
        press(&mut ctx, &mut ime, KeyCode::Ime(ImeKey::NonConvert));
        assert_eq!(ctx.display(), "ｶﾝｼﾞ");
        // Round, not a one way door.
        press(&mut ctx, &mut ime, KeyCode::Ime(ImeKey::NonConvert));
        assert_eq!(ctx.display(), "かんじ");
        // And the next word starts in hiragana again, because the script went
        // with the preedit rather than becoming a mode.
        press(&mut ctx, &mut ime, KeyCode::Enter);
        type_romaji(&mut ctx, &mut ime, "kanji");
        assert_eq!(ctx.display(), "かんじ");
    }

    #[test]
    fn a_modified_key_is_not_the_imes_and_leaves_the_preedit_standing() {
        let mut ime = ime();
        let mut ctx = ImeContext::default();
        ctx.toggle();
        type_romaji(&mut ctx, &mut ime, "kanji");
        let outcome = handle_key(
            &mut ime,
            &mut ctx,
            pane(),
            &KeyEvent::new(KeyCode::Char('c'), Modifiers::CTRL),
        );
        assert_eq!(outcome, ImeOutcome::Passthrough);
        assert_eq!(ctx.display(), "かんじ");
    }

    #[test]
    fn a_reading_with_no_entry_still_settles_its_carry() {
        let mut ime = ime();
        let mut ctx = ImeContext::default();
        ctx.toggle();
        type_romaji(&mut ctx, &mut ime, "zzzn");
        press(&mut ctx, &mut ime, KeyCode::Ime(ImeKey::Convert));
        assert!(ime.conversion(pane()).is_none());
        assert_eq!(ctx.display(), "っっzん");
    }

    #[test]
    fn conversion_without_a_dictionary_leaves_the_kana_alone() {
        let mut ime = Ime::empty();
        let mut ctx = ImeContext::default();
        ctx.toggle();
        type_romaji(&mut ctx, &mut ime, "kanji");
        press(&mut ctx, &mut ime, KeyCode::Ime(ImeKey::Convert));
        assert!(ime.conversion(pane()).is_none());
        assert_eq!(ctx.display(), "かんじ");
        assert_eq!(
            press(&mut ctx, &mut ime, KeyCode::Enter),
            ImeOutcome::Commit("かんじ".into())
        );
    }

    #[test]
    fn a_conversion_belongs_to_the_pane_that_opened_it() {
        let mut ime = ime();
        let mut ctx = ImeContext::default();
        ctx.toggle();
        type_romaji(&mut ctx, &mut ime, "kanji");
        press(&mut ctx, &mut ime, KeyCode::Ime(ImeKey::Convert));
        assert!(ime.conversion(pane()).is_some());
        // Another pane is not converting, whatever this one is doing.
        assert!(ime.conversion(PaneId(2)).is_none());
    }

    #[test]
    fn turning_kana_off_abandons_what_was_half_typed_rather_than_committing_it() {
        let mut ime = ime();
        let mut ctx = ImeContext::default();
        ctx.toggle();
        type_romaji(&mut ctx, &mut ime, "kanji");
        assert!(!ctx.toggle());
        assert!(!ctx.is_composing());
        assert_eq!(ctx.state(false), State::Direct);
    }

    #[test]
    fn the_dictionary_search_prefers_the_users_own_and_ends_with_the_system_copy() {
        let paths = search_path_from(Some("/home/someone/.local/share"), None);
        assert_eq!(
            paths.first().map(|p| p.display().to_string()),
            Some("/home/someone/.local/share/tos/SKK-JISYO".to_string())
        );
        assert_eq!(
            paths.last().map(|p| p.display().to_string()),
            Some("/usr/share/skk/SKK-JISYO.L".to_string())
        );
        assert!(paths.contains(&PathBuf::from("/usr/share/tos/SKK-JISYO.L")));
        // An empty variable counts as unset, which is what an init
        // environment hands over.
        let from_home = search_path_from(Some(""), Some("/home/someone"));
        assert_eq!(
            from_home.first().map(|p| p.display().to_string()),
            Some("/home/someone/.local/share/tos/SKK-JISYO".to_string())
        );
    }

    #[test]
    fn a_dictionary_that_is_not_there_is_not_a_reason_to_stop() {
        let (ime, problem) = Ime::open(Some(Path::new("/nonexistent/SKK-JISYO")));
        assert!(!ime.has_dictionary());
        assert!(problem.is_some(), "a configured path that failed says so");
        // And a search that finds nothing says nothing at all.
        let (ime, problem) = Ime::open(None);
        assert!(problem.is_none());
        let _ = ime;
    }

    #[test]
    fn a_preedit_longer_than_the_room_keeps_the_end_that_is_being_typed() {
        assert_eq!(clip_tail("にほんご", 8), "にほんご");
        assert_eq!(clip_tail("にほんご", 6), "…んご");
        assert_eq!(clip_tail("abcdef", 3), "…ef");
        assert_eq!(clip_tail("abc", 0), "");
    }
}
