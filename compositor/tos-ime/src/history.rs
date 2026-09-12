//! What was chosen before, so it is offered first. See docs/design/ime.md and issue #64.
//!
//! A converter that offers 貴社 before 記者 to someone who has picked 記者
//! forty times is a converter people work around. `docs/design/ime.md` defers
//! this — "learning is deferred, not forgotten" — and names the shape it
//! wanted: a second file at `$XDG_DATA_HOME/tos/ime-history`, in the same SKK
//! format, searched ahead of the system dictionary and merged over it.
//!
//! This module is that file. It is deliberately the *smaller* half of the
//! pair: [`dict`](crate::dict) owns 5.2 MB of somebody else's curation and is
//! read once; this owns a few hundred lines of the user's own and is written
//! constantly. So it reuses [`dict::Candidate`] and [`dict::Source`] rather
//! than growing parallel shapes, and adds the one thing a dictionary never
//! needed: a [`Sink`].
//!
//! Three decisions, each with what it rejected:
//!
//! **The merge is most-recently-chosen first.** Committing a candidate moves
//! it to the front of its reading, and [`History::merge`] puts the remembered
//! ones ahead of the dictionary's own order. This is SKK's own convention,
//! and it is the one that recovers: a user who has picked 記者 forty times
//! and then changes jobs gets 貴社 first on the *next* conversion.
//! *Rejected: frequency.* A count would need somewhere to live, and the SKK
//! line has exactly one spare field — the annotation, which is the field the
//! candidate window shows the user — so storing counts means either a format
//! only tOS can read or a number displayed beside every word. It also adapts
//! forty times more slowly than the case that motivated the issue.
//! *Rejected: frequency with decay.* That is a half-life, and a half-life is
//! a tuning constant with nothing in this repository able to measure it.
//!
//! **A remembered candidate moves; it does not appear twice.** When the
//! dictionary also offers a remembered word, [`History::merge`] emits the
//! *dictionary's* `Candidate` — annotation and all — at the history's
//! position, and drops it from the tail. The history's own copy is used only
//! for words the dictionary no longer has.
//!
//! **Writing is an append of one line per commit, compacted on a rule.** Each
//! commit appends `きしゃ /記者/` and nothing else: one `write` to a file
//! opened with `O_APPEND`, a few dozen bytes, no read and no rewrite. Loading
//! replays the journal in order, so the last mention of a word wins — which
//! is exactly most-recently-chosen-first, for free.
//! *Rejected: rewriting the file on every commit.* That is O(file) per
//! keystroke-ish event and, worse, a rewrite interrupted by power loss
//! destroys the whole history rather than one line.
//! *Rejected: flushing on exit.* The issue is explicit about why, and so is
//! the design: a compositor that can be PID 1 does not reliably get an exit.
//! Nothing here defers anything to a shutdown path.
//! The journal's one cost is that it grows, so [`History`] rewrites it whole
//! — once — when it has grown past twice what it needs (see
//! [`COMPACT_SLACK`]), which is amortised O(1) per commit.
//!
//! **Nothing here can fail loudly.** The live ISO starts the compositor from
//! `/init` with almost no environment and no home directory, which is the
//! same reason `docs/design/ime.md` gives for the system dictionary path
//! having to be the one that always works. So: a missing file is an empty
//! history, an unreadable one is an empty history, a corrupt one is whatever
//! survived, and a read-only filesystem is a history that learns for this
//! session and stops trying to write. [`History::open`] returns a `History`
//! and not a `Result`, and [`History::remember`] returns nothing at all.
//! Learning degrades to no learning; it never degrades to no compositor, and
//! it never costs a commit.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::dict::{Candidate, Source};

/// The most readings kept, and so the most ever written.
///
/// A heavy day of Japanese is a few thousand conversions over far fewer
/// distinct readings; 4096 of them at the ~48 bytes an entry costs is under
/// 200 KB, against the 5.2 MB of `SKK-JISYO.L` already resident. The cap
/// exists because an uncapped history is a file that only grows, on a machine
/// whose whole root filesystem may be a squashfs image with a small writable
/// overlay. Past the cap the least recently used reading is dropped: what a
/// user has not typed in four thousand readings is not what they are about to
/// type.
pub const MAX_READINGS: usize = 4096;

/// The most candidates remembered for one reading.
///
/// The candidate window numbers nine at a time (`1-9`, per the state machine
/// in `docs/design/ime.md`), so sixteen is already more than one page of
/// second-guessing. Past that the dictionary's own order supplies the rest,
/// which is what it is for.
pub const MAX_CANDIDATES: usize = 16;

/// How far the journal is allowed to run ahead of the entries it encodes
/// before it is rewritten whole.
///
/// Compaction happens when `lines > 2 * readings + COMPACT_SLACK`. The factor
/// of two bounds the wasted bytes at half the file; the slack means a fresh
/// history — zero readings, and the case where a rewrite would be most
/// pointless — writes 512 lines before it ever pays for one.
pub const COMPACT_SLACK: usize = 512;

/// The comment a compacted file opens with, so that what tOS writes is a file
/// tOS (and SKK) can read as a dictionary rather than a private log.
const HEADER: &str = ";; okuri-nasi entries. Written by tOS on commit; see docs/design/ime.md.\n";

/// Where a history's bytes go.
///
/// The write half of [`dict::Source`], and shaped the same way for the same
/// reason: the machine running these tests has no `$XDG_DATA_HOME`, and a
/// test that wrote to the real one would be a test that edits the developer's
/// own input history. Two methods rather than one because the two writes are
/// genuinely different acts — an append is the common case and must be cheap,
/// a replace is compaction and must be atomic — and collapsing them into one
/// `write(bytes, append: bool)` would only move the match inside the
/// implementation.
pub trait Sink {
    /// Add `line` to the end. Called once per commit that changed the order.
    fn append(&self, line: &[u8]) -> io::Result<()>;

    /// Replace the whole file with `bytes`. Called only by compaction, and
    /// required to be all-or-nothing: a compaction interrupted halfway must
    /// leave the old file, not half a new one.
    fn replace(&self, bytes: &[u8]) -> io::Result<()>;
}

/// The history on disk, which is what the compositor has.
#[derive(Debug, Clone)]
pub struct FileSink {
    path: PathBuf,
}

impl FileSink {
    pub fn new(path: impl Into<PathBuf>) -> FileSink {
        FileSink { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Sink for FileSink {
    fn append(&self, line: &[u8]) -> io::Result<()> {
        let open = || {
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
        };
        // The directory is made only when opening says it is missing, rather
        // than by a `create_dir_all` in front of every append. `$XDG_DATA_HOME
        // /tos` is absent exactly once in a history's life and present for
        // every commit after it, and the common path should not pay a syscall
        // for the rare one.
        let mut file = match open() {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if let Some(parent) = self.path.parent() {
                    fs::create_dir_all(parent)?;
                }
                open()?
            }
            other => other?,
        };
        file.write_all(line)
    }

    fn replace(&self, bytes: &[u8]) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        // Write beside it and rename, because `rename` within a directory is
        // the one filesystem operation that is atomic: a machine that loses
        // power during compaction comes back with either the whole old
        // history or the whole new one. Writing in place would give it a
        // truncated file, and truncation is indistinguishable from "the user
        // has never converted anything".
        let temp = self.path.with_file_name(match self.path.file_name() {
            Some(name) => {
                let mut name = name.to_os_string();
                name.push(".new");
                name
            }
            None => return Err(io::Error::new(io::ErrorKind::InvalidInput, "no file name")),
        });
        fs::write(&temp, bytes)?;
        match fs::rename(&temp, &self.path) {
            Ok(()) => Ok(()),
            Err(error) => {
                // Do not leave the half-written sibling behind for the next
                // run to trip over.
                let _ = fs::remove_file(&temp);
                Err(error)
            }
        }
    }
}

/// A history that is never written anywhere.
///
/// This is not a disabled history: the entries still move, so conversion
/// still learns for as long as the session lasts. It is what the compositor
/// gets when `/init` started it with no `HOME` and no `XDG_DATA_HOME`, and
/// what a read-only filesystem degrades to after the first failed write.
#[derive(Debug, Clone, Copy, Default)]
pub struct NullSink;

impl Sink for NullSink {
    fn append(&self, _line: &[u8]) -> io::Result<()> {
        Ok(())
    }

    fn replace(&self, _bytes: &[u8]) -> io::Result<()> {
        Ok(())
    }
}

/// A history somebody wants to read back rather than store.
///
/// The counterpart to [`dict::BytesSource`]: it behaves like a file — an
/// append adds to the end, a replace discards everything — so a test can
/// assert both *what* would have been written and *how much*, and can feed
/// what came out of one history into the next one's [`Source`].
#[derive(Debug, Default)]
pub struct MemorySink {
    bytes: std::cell::RefCell<Vec<u8>>,
    /// Counted rather than derived, because "one commit wrote one line" and
    /// "the file has one line" stop being the same claim as soon as
    /// compaction exists.
    writes: std::cell::Cell<usize>,
}

impl MemorySink {
    pub fn new() -> MemorySink {
        MemorySink::default()
    }

    /// Everything that would be on disk now.
    pub fn contents(&self) -> Vec<u8> {
        self.bytes.borrow().clone()
    }

    /// The same, as lines, for tests that want to count them.
    pub fn lines(&self) -> Vec<String> {
        String::from_utf8_lossy(&self.bytes.borrow())
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// How many times the sink was touched at all, appends and replaces
    /// together.
    pub fn writes(&self) -> usize {
        self.writes.get()
    }
}

impl Sink for MemorySink {
    fn append(&self, line: &[u8]) -> io::Result<()> {
        self.writes.set(self.writes.get() + 1);
        self.bytes.borrow_mut().extend_from_slice(line);
        Ok(())
    }

    fn replace(&self, bytes: &[u8]) -> io::Result<()> {
        self.writes.set(self.writes.get() + 1);
        *self.bytes.borrow_mut() = bytes.to_vec();
        Ok(())
    }
}

/// One reading and what has been chosen for it, most recent first.
#[derive(Debug, Clone)]
struct Entry {
    reading: String,
    candidates: Vec<Candidate>,
    /// When this reading was last committed, counted in commits rather than
    /// seconds. A clock would be wrong here twice over: the ISO boots with no
    /// RTC set, and the only question ever asked of this number is "which of
    /// these is oldest", which needs an order and not a time.
    used: u64,
}

/// What has been chosen before.
///
/// Entries are kept sorted by reading and binary searched, the same shape
/// [`dict::Dictionary`] uses and for the same reason: [`History::merge`] runs
/// on every 変換 keystroke, and a linear scan of four thousand strings per
/// keypress is a cost with no reason to exist. Recency lives in a counter on
/// the entry rather than in the order of the vector, so that remembering a
/// word does not memmove the whole store.
pub struct History {
    entries: Vec<Entry>,
    sink: Box<dyn Sink>,
    /// Commits so far, the source of [`Entry::used`].
    clock: u64,
    /// How many lines the sink is believed to hold. Tracked rather than
    /// measured, because measuring means reading the file back, and reading
    /// the file back on every commit is the cost the append was chosen to
    /// avoid.
    lines: usize,
    /// Set when the loaded file did not end in a newline — a torn write, or a
    /// hand edit. The next line written is prefixed with one, so the append
    /// does not glue itself onto half an entry and corrupt a second line.
    needs_newline: bool,
    /// Cleared for good by the first failed write. See [`History::persists`].
    persists: bool,
    /// Why, once, so the compositor can say so in a log line instead of
    /// guessing.
    last_error: Option<io::Error>,
}

impl History {
    /// A history that remembers nothing yet and writes nowhere.
    ///
    /// The `/init` case: no home directory, so no file, so no [`Sink`] — but
    /// conversion still reorders within the session, which is the most that
    /// can honestly be offered on a machine with nowhere to put the answer.
    pub fn in_memory() -> History {
        History {
            entries: Vec::new(),
            sink: Box::new(NullSink),
            clock: 0,
            lines: 0,
            needs_newline: false,
            persists: false,
            last_error: None,
        }
    }

    /// Read what was chosen before through `source`, and write what is chosen
    /// next through `sink`.
    ///
    /// No `io::Result`, unlike [`dict::Dictionary::open`], and the difference
    /// is deliberate. A dictionary that was configured and is not there is
    /// something the user asked for that did not happen, and they are owed
    /// the error. A history that is not there is the ordinary state of every
    /// first run on every machine, and there is nobody to tell. The reason is
    /// kept in [`History::last_error`] for a compositor that wants to log it.
    pub fn open(source: &dyn Source, sink: Box<dyn Sink>) -> History {
        let mut history = History {
            sink,
            persists: true,
            ..History::in_memory()
        };
        match source.read() {
            Ok(bytes) => history.replay(&bytes),
            // Not there, not readable, a directory where a file should be:
            // all the same answer. The sink is left alone, because a file
            // that cannot be read today may still be writable — and if it is
            // not, the first append will say so.
            Err(error) => history.last_error = Some(error),
        }
        history
    }

    /// A history from bytes already in hand, mostly so tests and the
    /// compositor's own fallbacks do not need a [`Source`] for four lines.
    pub fn from_bytes(bytes: &[u8], sink: Box<dyn Sink>) -> History {
        let mut history = History {
            sink,
            persists: true,
            ..History::in_memory()
        };
        history.replay(bytes);
        history
    }

    /// The history at the place `docs/design/ime.md` names, or an in-memory
    /// one when the environment has no home in it.
    ///
    /// The one call the compositor makes at startup. Everything under it is
    /// [`default_path`] and the two seams, so nothing about this function is
    /// what a test has to go through.
    pub fn open_default() -> History {
        match default_path() {
            Some(path) => History::open(
                &crate::dict::FileSource::new(&path),
                Box::new(FileSink::new(&path)),
            ),
            None => History::in_memory(),
        }
    }

    /// How many readings are remembered.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether writes are still being attempted.
    ///
    /// False from construction for [`History::in_memory`], and false for good
    /// after a write fails — a read-only overlay does not become writable
    /// because a user kept typing, and retrying a guaranteed `EROFS` on every
    /// commit is a syscall spent to learn nothing.
    pub fn persists(&self) -> bool {
        self.persists
    }

    /// The last thing that went wrong reading or writing, if anything did.
    pub fn last_error(&self) -> Option<&io::Error> {
        self.last_error.as_ref()
    }

    /// What has been chosen for `reading`, most recent first. Empty for a
    /// reading never converted.
    pub fn remembered(&self, reading: &str) -> &[Candidate] {
        match self.find(reading) {
            Ok(at) => &self.entries[at].candidates,
            Err(_) => &[],
        }
    }

    /// The dictionary's answer, reordered by what was chosen before.
    ///
    /// Remembered candidates come first in the order they were last chosen,
    /// then everything else in the dictionary's own order. A remembered word
    /// the dictionary also offers is emitted as the *dictionary's*
    /// `Candidate`, so its annotation survives, and it is removed from the
    /// tail: it moves, it does not appear twice.
    ///
    /// `from_dictionary` is taken by value so that the overwhelmingly common
    /// case — a reading with no history — is a move and not a copy of a
    /// candidate list.
    pub fn merge(&self, reading: &str, from_dictionary: Vec<Candidate>) -> Vec<Candidate> {
        let remembered = self.remembered(reading);
        if remembered.is_empty() {
            return from_dictionary;
        }
        let mut rest: Vec<Option<Candidate>> = from_dictionary.into_iter().map(Some).collect();
        let mut merged = Vec::with_capacity(remembered.len() + rest.len());
        for candidate in remembered {
            // Linear, because `remembered` is at most MAX_CANDIDATES and a
            // dictionary entry is a handful: a hash map would cost more to
            // build than this costs to scan, every single time.
            let found = rest
                .iter_mut()
                .find(|slot| slot.as_ref().is_some_and(|c| c.word == candidate.word))
                .and_then(Option::take);
            merged.push(found.unwrap_or_else(|| candidate.clone()));
        }
        merged.extend(rest.into_iter().flatten());
        merged
    }

    /// Record that `chosen` was committed for `reading`, and write it.
    ///
    /// Called on commit, per the issue — not on exit, because a compositor
    /// that can be PID 1 does not reliably get one. Returns nothing: there is
    /// no failure here the compositor could act on, and a commit must not
    /// depend on a filesystem.
    pub fn remember(&mut self, reading: &str, chosen: &Candidate) {
        if !writable_reading(reading) || !writable_word(&chosen.word) {
            // A reading with a space in it or a word with a `/` in it cannot
            // be written as an SKK line, and a thing that cannot be written
            // must not be remembered either — otherwise this session's order
            // and the next session's would silently disagree. Neither can
            // come out of `dict`, which splits on exactly those bytes; this is
            // a guard against a caller that built a `Candidate` by hand.
            return;
        }
        let chosen = Candidate {
            word: chosen.word.clone(),
            annotation: chosen
                .annotation
                .as_deref()
                .filter(|note| writable_annotation(note))
                .map(str::to_string),
        };
        if !self.promote(reading, &chosen) {
            // Already the first candidate for this reading, so `merge` would
            // answer exactly what it answered before: nothing to write. This
            // is the repetition case the issue is about — a user committing
            // 記者 for the fortieth time writes no bytes at all. The entry's
            // recency was still refreshed in memory, so the on-disk order is
            // the order of *changes*, which is a staler eviction hint than
            // memory has and never a wrong one.
            return;
        }
        self.write(line_for(reading, std::slice::from_ref(&chosen)).as_bytes());
        self.compact_if_bloated();
    }

    /// Rewrite the journal now, whatever its size.
    ///
    /// Exposed because it is the one operation a caller might want at a
    /// moment this module cannot know about — a compositor with an idle
    /// second in hand, say. Nothing requires it: [`History::remember`]
    /// compacts on its own rule, and a history that is never compacted is
    /// correct, only larger.
    pub fn compact(&mut self) {
        let bytes = self.compacted();
        self.lines = self.entries.len() + 1;
        self.needs_newline = false;
        if self.persists {
            if let Err(error) = self.sink.replace(&bytes) {
                self.fail(error);
            }
        }
    }

    /// What the whole history looks like as a file.
    ///
    /// Oldest reading first, which is the order that *replays* to the order
    /// held now: [`History::replay`] walks the file forwards and each line
    /// promotes what it names. Sorting by reading instead would make this a
    /// file stock SKK could binary search, and would throw away every entry's
    /// recency on the next load; `dict::index` sorts its own index anyway, so
    /// tOS can read this file as a dictionary whatever order it is in.
    /// Within a line the candidates are most-recent first, and `replay`
    /// promotes them back-to-front so they end up that way again.
    fn compacted(&self) -> Vec<u8> {
        let mut order: Vec<&Entry> = self.entries.iter().collect();
        order.sort_by_key(|entry| entry.used);
        let mut text = String::from(HEADER);
        for entry in order {
            text.push_str(&line_for(&entry.reading, &entry.candidates));
        }
        text.into_bytes()
    }

    /// Put `chosen` at the front of `reading`, answering whether that changed
    /// what [`History::merge`] would say.
    fn promote(&mut self, reading: &str, chosen: &Candidate) -> bool {
        self.clock += 1;
        let used = self.clock;
        match self.find(reading) {
            Ok(at) => {
                let entry = &mut self.entries[at];
                entry.used = used;
                let already = entry
                    .candidates
                    .first()
                    .is_some_and(|c| c.word == chosen.word);
                entry.candidates.retain(|c| c.word != chosen.word);
                entry.candidates.insert(0, chosen.clone());
                entry.candidates.truncate(MAX_CANDIDATES);
                // A repeat of the head is not a change even when the
                // annotation differs: what commits is the word.
                !already
            }
            Err(at) => {
                self.entries.insert(
                    at,
                    Entry {
                        reading: reading.to_string(),
                        candidates: vec![chosen.clone()],
                        used,
                    },
                );
                self.evict();
                true
            }
        }
    }

    /// Drop the least recently used reading once the store is over its cap.
    ///
    /// One at a time, because entries only ever arrive one at a time, so the
    /// cap can only ever be exceeded by one. The scan is O(readings) and runs
    /// only on the commit that overflowed.
    fn evict(&mut self) {
        if self.entries.len() <= MAX_READINGS {
            return;
        }
        let oldest = self
            .entries
            .iter()
            .enumerate()
            .min_by_key(|(_, entry)| entry.used)
            .map(|(at, _)| at);
        if let Some(at) = oldest {
            self.entries.remove(at);
        }
    }

    /// Where `reading` is, or where it would go.
    fn find(&self, reading: &str) -> Result<usize, usize> {
        self.entries
            .binary_search_by(|entry| entry.reading.as_str().cmp(reading))
    }

    /// Append `line`, and stop persisting if that fails.
    fn write(&mut self, line: &[u8]) {
        if !self.persists {
            return;
        }
        let result = if self.needs_newline {
            // The loaded file ended mid-entry. One newline in front of this
            // append turns the damaged tail into its own line, which `replay`
            // drops on the next load, instead of letting it swallow the
            // beginning of this one.
            let mut repaired = Vec::with_capacity(line.len() + 1);
            repaired.push(b'\n');
            repaired.extend_from_slice(line);
            self.sink.append(&repaired)
        } else {
            self.sink.append(line)
        };
        match result {
            Ok(()) => {
                self.lines += 1 + usize::from(self.needs_newline);
                self.needs_newline = false;
            }
            Err(error) => self.fail(error),
        }
    }

    /// A write failed. Keep learning, stop writing, remember why.
    fn fail(&mut self, error: io::Error) {
        self.persists = false;
        self.last_error = Some(error);
    }

    fn compact_if_bloated(&mut self) {
        // Twice what the entries need, plus the slack that keeps a young
        // history from rewriting itself over and over while it is still
        // cheaper to append. Amortised, every line written is rewritten at
        // most once.
        if self.lines > 2 * self.entries.len() + COMPACT_SLACK {
            self.compact();
        }
    }

    /// Rebuild the store by replaying the file from the beginning.
    ///
    /// Every line is a commit that happened, in the order it happened, so
    /// replaying them in file order reconstructs exactly the order they left
    /// behind — including for a compacted file, whose multi-candidate lines
    /// are replayed back-to-front so the first candidate ends up first.
    ///
    /// Nothing here rejects a file. A line that is not UTF-8, or has no
    /// space, or no candidates, is skipped and the rest is kept: the whole
    /// point of appending one line at a time is that damage is confined to
    /// the line it happened in, and refusing the other four thousand because
    /// the last one was torn would throw away the thing this module exists to
    /// keep.
    fn replay(&mut self, bytes: &[u8]) {
        self.lines = bytes.iter().filter(|&&b| b == b'\n').count()
            + usize::from(!bytes.is_empty() && !bytes.ends_with(b"\n"));
        self.needs_newline = !bytes.is_empty() && !bytes.ends_with(b"\n");

        for raw in bytes.split(|&b| b == b'\n') {
            let line = match raw.last() {
                Some(b'\r') => &raw[..raw.len() - 1],
                _ => raw,
            };
            if line.is_empty() || line.first() == Some(&b';') {
                continue;
            }
            let Ok(text) = std::str::from_utf8(line) else {
                continue;
            };
            let Some((reading, rest)) = text.split_once(' ') else {
                continue;
            };
            if !writable_reading(reading) || !rest.starts_with('/') {
                continue;
            }
            let candidates: Vec<Candidate> = rest
                .split('/')
                .filter(|piece| !piece.is_empty())
                .filter_map(parse_candidate)
                .collect();
            // Back to front: the earliest promotion ends up furthest from the
            // head, so a line's own left-to-right order is preserved.
            for candidate in candidates.iter().rev() {
                self.promote(reading, candidate);
            }
        }
    }
}

/// Not derived: the sink is a trait object and the entries are the user's own
/// typing, which is not something to print into a log by accident.
impl std::fmt::Debug for History {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("History")
            .field("readings", &self.entries.len())
            .field("lines", &self.lines)
            .field("persists", &self.persists)
            .finish()
    }
}

/// One SKK line: `きしゃ /記者/貴社;your company/`.
fn line_for(reading: &str, candidates: &[Candidate]) -> String {
    let mut line = String::with_capacity(reading.len() + 8 * candidates.len());
    line.push_str(reading);
    line.push_str(" /");
    for candidate in candidates {
        line.push_str(&candidate.word);
        if let Some(note) = &candidate.annotation {
            line.push(';');
            line.push_str(note);
        }
        line.push('/');
    }
    line.push('\n');
    line
}

/// The same split `dict` makes on a candidate: word, then annotation.
///
/// Duplicated rather than shared because `dict`'s copy is private, and it is
/// four lines. See the note in the report for #64: a `pub fn` there would let
/// this module drop it.
fn parse_candidate(text: &str) -> Option<Candidate> {
    let (word, annotation) = match text.split_once(';') {
        Some((word, note)) => (word, (!note.is_empty()).then(|| note.to_string())),
        None => (text, None),
    };
    (!word.is_empty()).then(|| Candidate {
        word: word.to_string(),
        annotation,
    })
}

/// A reading that can be the first field of an SKK line.
fn writable_reading(reading: &str) -> bool {
    !reading.is_empty() && !reading.contains([' ', '/', '\n', '\r'])
}

/// A word that can sit between two slashes and come back unchanged. `;` is in
/// the list because it would come back as an annotation boundary.
fn writable_word(word: &str) -> bool {
    !word.is_empty() && !word.contains(['/', ';', '\n', '\r'])
}

/// An annotation that can sit after the `;`. A second `;` is harmless — the
/// split takes the first — so only the line and candidate separators matter.
fn writable_annotation(note: &str) -> bool {
    !note.contains(['/', '\n', '\r'])
}

/// `$XDG_DATA_HOME/tos/ime-history`, or nothing.
///
/// Reads the environment, and is the only thing in this module that does; the
/// decision it makes is [`path_from`], which does not, so the tests never
/// touch a real home directory.
pub fn default_path() -> Option<PathBuf> {
    path_from(
        std::env::var_os("XDG_DATA_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

/// Where the history goes, given what the environment says.
///
/// `None` when there is nowhere: `/init` starts the compositor with almost no
/// environment, and `docs/design/ime.md` says exactly this about why the
/// *system* dictionary path has to be the one that always works. A relative
/// `XDG_DATA_HOME` is ignored rather than resolved, because the XDG
/// specification says it must be, and because resolving it against a working
/// directory the compositor did not choose would put a history file in
/// whatever directory `/init` happened to be in.
pub fn path_from(data_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    let base: PathBuf = match data_home {
        Some(value) if !value.is_empty() && Path::new(value).is_absolute() => PathBuf::from(value),
        _ => {
            let home = home.filter(|value| !value.is_empty() && Path::new(value).is_absolute())?;
            let mut base = PathBuf::from(home);
            // The specification's own default when XDG_DATA_HOME is unset.
            base.push(".local");
            base.push("share");
            base
        }
    };
    Some(base.join("tos").join("ime-history"))
}

/// So callers holding an owned `OsString` do not have to spell the deref.
pub fn path_from_owned(data_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    path_from(data_home.as_deref(), home.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dict::{BytesSource, Dictionary};
    use std::rc::Rc;

    fn word(word: &str) -> Candidate {
        Candidate {
            word: word.to_string(),
            annotation: None,
        }
    }

    fn annotated(word: &str, note: &str) -> Candidate {
        Candidate {
            word: word.to_string(),
            annotation: Some(note.to_string()),
        }
    }

    fn words(candidates: &[Candidate]) -> Vec<String> {
        candidates.iter().map(|c| c.word.clone()).collect()
    }

    /// The dictionary the issue's own example comes from: 貴社 before 記者.
    fn dictionary() -> Dictionary {
        Dictionary::open(&BytesSource::new(
            ";; okuri-nasi entries.\n\
             きしゃ /貴社;your company/記者;reporter/汽車/帰社/\n\
             かんれい /慣例/寒冷/管領/艦齢/\n",
        ))
        .expect("bytes always read")
    }

    /// A history writing into something a test can read back.
    fn with_memory() -> (History, Rc<MemorySink>) {
        let sink = Rc::new(MemorySink::new());
        (
            History::from_bytes(b"", Box::new(SharedSink(sink.clone()))),
            sink,
        )
    }

    /// A sink two owners share: the history writes through it, the test reads
    /// it. `Rc` rather than a borrow because `History` owns its sink boxed.
    struct SharedSink(Rc<MemorySink>);

    impl Sink for SharedSink {
        fn append(&self, line: &[u8]) -> io::Result<()> {
            self.0.append(line)
        }

        fn replace(&self, bytes: &[u8]) -> io::Result<()> {
            self.0.replace(bytes)
        }
    }

    /// Everything fails, the way a squashfs root does.
    struct ReadOnlySink {
        attempts: std::cell::Cell<usize>,
    }

    impl ReadOnlySink {
        fn new() -> Rc<ReadOnlySink> {
            Rc::new(ReadOnlySink {
                attempts: std::cell::Cell::new(0),
            })
        }
    }

    impl Sink for Rc<ReadOnlySink> {
        fn append(&self, _line: &[u8]) -> io::Result<()> {
            self.attempts.set(self.attempts.get() + 1);
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        }

        fn replace(&self, _bytes: &[u8]) -> io::Result<()> {
            self.attempts.set(self.attempts.get() + 1);
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        }
    }

    /// A source that fails, the way an unreadable file does.
    struct UnreadableSource;

    impl Source for UnreadableSource {
        fn read(&self) -> io::Result<Vec<u8>> {
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        }
    }

    /// A directory that cleans up after itself, the same shape `dict`'s tests
    /// and `tos-system`'s use.
    struct Scratch {
        root: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let root =
                std::env::temp_dir().join(format!("tos-ime-history-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).unwrap();
            Scratch { root }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn a_reading_never_converted_is_answered_in_the_dictionarys_own_order() {
        let (history, _sink) = with_memory();
        let merged = history.merge("きしゃ", dictionary().lookup("きしゃ"));
        assert_eq!(words(&merged), vec!["貴社", "記者", "汽車", "帰社"]);
    }

    #[test]
    fn the_word_that_was_chosen_is_offered_first_the_next_time() {
        let (mut history, _sink) = with_memory();
        history.remember("きしゃ", &word("記者"));
        let merged = history.merge("きしゃ", dictionary().lookup("きしゃ"));
        assert_eq!(words(&merged), vec!["記者", "貴社", "汽車", "帰社"]);
    }

    #[test]
    fn a_remembered_word_moves_to_the_front_and_is_not_listed_twice() {
        let (mut history, _sink) = with_memory();
        history.remember("きしゃ", &word("汽車"));
        let merged = history.merge("きしゃ", dictionary().lookup("きしゃ"));
        assert_eq!(words(&merged), vec!["汽車", "貴社", "記者", "帰社"]);
        assert_eq!(merged.len(), 4, "nothing was duplicated: {merged:?}");
    }

    #[test]
    fn a_remembered_word_keeps_the_dictionarys_annotation_rather_than_losing_it() {
        let (mut history, _sink) = with_memory();
        // Remembered with no annotation at all, which is what a caller that
        // built the candidate from the committed text would hand over.
        history.remember("きしゃ", &word("記者"));
        let merged = history.merge("きしゃ", dictionary().lookup("きしゃ"));
        assert_eq!(merged[0], annotated("記者", "reporter"));
    }

    #[test]
    fn the_most_recently_chosen_word_comes_before_the_one_chosen_before_it() {
        let (mut history, _sink) = with_memory();
        history.remember("きしゃ", &word("記者"));
        history.remember("きしゃ", &word("帰社"));
        let merged = history.merge("きしゃ", dictionary().lookup("きしゃ"));
        // Most recent first: 帰社, then 記者, then the dictionary's remainder
        // in its own order.
        assert_eq!(words(&merged), vec!["帰社", "記者", "貴社", "汽車"]);
    }

    #[test]
    fn a_word_the_dictionary_no_longer_offers_is_still_offered_first() {
        // The user learnt 記者 against one dictionary and then the dictionary
        // was replaced with one that has never heard of it. Their own word
        // must not vanish, and the new dictionary's words must all still be
        // there behind it.
        let history = History::from_bytes("きしゃ /記者/\n".as_bytes(), Box::new(NullSink));
        let thinner =
            Dictionary::open(&BytesSource::new("きしゃ /貴社/汽車/\n")).expect("bytes always read");
        let merged = history.merge("きしゃ", thinner.lookup("きしゃ"));
        assert_eq!(words(&merged), vec!["記者", "貴社", "汽車"]);
    }

    #[test]
    fn a_word_no_dictionary_offers_at_all_is_answered_from_the_history_alone() {
        let history = History::from_bytes("きしゃ /記者/\n".as_bytes(), Box::new(NullSink));
        assert_eq!(words(&history.merge("きしゃ", Vec::new())), vec!["記者"]);
    }

    #[test]
    fn one_readings_history_does_not_reorder_another_readings_candidates() {
        let (mut history, _sink) = with_memory();
        history.remember("きしゃ", &word("記者"));
        let merged = history.merge("かんれい", dictionary().lookup("かんれい"));
        assert_eq!(words(&merged), vec!["慣例", "寒冷", "管領", "艦齢"]);
    }

    #[test]
    fn a_commit_is_written_at_the_moment_it_happens_and_not_at_exit() {
        let (mut history, sink) = with_memory();
        history.remember("きしゃ", &word("記者"));
        // No flush, no drop, no shutdown: the bytes are already there.
        assert_eq!(sink.lines(), vec!["きしゃ /記者/".to_string()]);
    }

    #[test]
    fn an_annotation_survives_the_round_trip_through_the_file() {
        let (mut history, sink) = with_memory();
        history.remember("けいけん", &annotated("経験", "experience"));
        assert_eq!(sink.lines(), vec!["けいけん /経験;experience/".to_string()]);
        let reloaded = History::from_bytes(&sink.contents(), Box::new(NullSink));
        assert_eq!(
            reloaded.remembered("けいけん"),
            [annotated("経験", "experience")]
        );
    }

    #[test]
    fn the_same_word_committed_twice_is_written_once() {
        let (mut history, sink) = with_memory();
        history.remember("きしゃ", &word("記者"));
        for _ in 0..40 {
            history.remember("きしゃ", &word("記者"));
        }
        // Forty repeats of a word that is already first change nothing that
        // `merge` could report, so they cost no bytes.
        assert_eq!(sink.lines(), vec!["きしゃ /記者/".to_string()]);
        assert_eq!(sink.writes(), 1);
        assert_eq!(words(history.remembered("きしゃ")), vec!["記者"]);
    }

    #[test]
    fn alternating_between_two_words_writes_each_time_because_the_order_changed() {
        let (mut history, sink) = with_memory();
        for _ in 0..3 {
            history.remember("きしゃ", &word("記者"));
            history.remember("きしゃ", &word("貴社"));
        }
        assert_eq!(sink.writes(), 6);
        assert_eq!(words(history.remembered("きしゃ")), vec!["貴社", "記者"]);
    }

    #[test]
    fn the_journal_replays_to_the_order_the_commits_happened_in() {
        let history = History::from_bytes(
            "きしゃ /記者/\nきしゃ /貴社/\nきしゃ /記者/\n".as_bytes(),
            Box::new(NullSink),
        );
        // Three commits, last one wins the front, and 記者 appears once.
        assert_eq!(words(history.remembered("きしゃ")), vec!["記者", "貴社"]);
    }

    #[test]
    fn a_history_reloaded_from_what_it_wrote_answers_exactly_as_it_did() {
        let (mut history, sink) = with_memory();
        for (reading, chosen) in [
            ("きしゃ", "記者"),
            ("かんれい", "寒冷"),
            ("きしゃ", "汽車"),
            ("きしゃ", "記者"),
        ] {
            history.remember(reading, &word(chosen));
        }
        let before = history.merge("きしゃ", dictionary().lookup("きしゃ"));
        let reloaded = History::from_bytes(&sink.contents(), Box::new(NullSink));
        assert_eq!(
            reloaded.merge("きしゃ", dictionary().lookup("きしゃ")),
            before
        );
        assert_eq!(words(reloaded.remembered("かんれい")), vec!["寒冷"]);
    }

    #[test]
    fn a_compacted_history_reloads_to_the_same_answers_as_the_journal_it_replaced() {
        let (mut history, sink) = with_memory();
        for (reading, chosen) in [
            ("きしゃ", "記者"),
            ("かんれい", "寒冷"),
            ("きしゃ", "汽車"),
            ("かんれい", "慣例"),
            ("きしゃ", "記者"),
        ] {
            history.remember(reading, &word(chosen));
        }
        let before_kisha = words(history.remembered("きしゃ"));
        let before_kanrei = words(history.remembered("かんれい"));
        history.compact();
        let text = String::from_utf8(sink.contents()).unwrap();
        assert!(
            text.starts_with(";;"),
            "a compacted file is a readable dictionary: {text}"
        );
        let reloaded = History::from_bytes(sink.contents().as_slice(), Box::new(NullSink));
        assert_eq!(words(reloaded.remembered("きしゃ")), before_kisha);
        assert_eq!(words(reloaded.remembered("かんれい")), before_kanrei);
    }

    #[test]
    fn a_compacted_file_is_something_the_dictionary_reader_can_open() {
        // Not a private log format: `dict::Dictionary` sorts its own index,
        // so it reads this file whatever order the entries are in.
        let (mut history, sink) = with_memory();
        history.remember("きしゃ", &word("記者"));
        history.remember("かんれい", &word("寒冷"));
        history.compact();
        let as_dictionary = Dictionary::from_bytes(sink.contents());
        assert_eq!(as_dictionary.len(), 2);
        assert_eq!(words(&as_dictionary.lookup("きしゃ")), vec!["記者"]);
        assert_eq!(words(&as_dictionary.lookup("かんれい")), vec!["寒冷"]);
    }

    #[test]
    fn a_corrupt_history_file_costs_its_damaged_lines_and_nothing_else() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice("きしゃ /記者/\n".as_bytes());
        bytes.extend_from_slice("\n".as_bytes()); // blank
        bytes.extend_from_slice("これはこわれている\n".as_bytes()); // no candidates
        bytes.extend_from_slice(" /みだしなし/\n".as_bytes()); // no reading
        bytes.extend_from_slice("かんれい みだし\n".as_bytes()); // no slash
        bytes.extend_from_slice(&[0xb0, 0xa6, b'\n']); // not UTF-8: raw EUC-JP
        bytes.extend_from_slice("とうきょう /東京/\n".as_bytes());
        let history = History::from_bytes(&bytes, Box::new(NullSink));
        assert_eq!(history.len(), 2, "{history:?}");
        assert_eq!(words(history.remembered("きしゃ")), vec!["記者"]);
        assert_eq!(words(history.remembered("とうきょう")), vec!["東京"]);
        assert!(history.remembered("かんれい").is_empty());
    }

    #[test]
    fn a_history_torn_mid_line_keeps_what_came_before_it() {
        // What a machine that lost power during an append leaves behind.
        let history =
            History::from_bytes("きしゃ /記者/\nかんれい /寒".as_bytes(), Box::new(NullSink));
        assert_eq!(words(history.remembered("きしゃ")), vec!["記者"]);
        // The torn line is still an entry by the format's own rules, so what
        // survived of it survives. What matters is that the good line did.
        assert_eq!(words(history.remembered("かんれい")), vec!["寒"]);
    }

    #[test]
    fn an_append_after_a_torn_last_line_does_not_glue_itself_onto_it() {
        let sink = Rc::new(MemorySink::new());
        // No trailing newline: the previous process died mid-write.
        sink.append("きしゃ /記者/\nかんれい /寒".as_bytes())
            .unwrap();
        let mut history = History::from_bytes(&sink.contents(), Box::new(SharedSink(sink.clone())));
        history.remember("とうきょう", &word("東京"));
        assert_eq!(
            sink.lines(),
            vec![
                "きしゃ /記者/".to_string(),
                "かんれい /寒".to_string(),
                "とうきょう /東京/".to_string(),
            ]
        );
        // And the repaired file reloads with all three readings intact.
        let reloaded = History::from_bytes(&sink.contents(), Box::new(NullSink));
        assert_eq!(reloaded.len(), 3);
        assert_eq!(words(reloaded.remembered("とうきょう")), vec!["東京"]);
    }

    #[test]
    fn a_history_file_that_is_not_there_is_an_empty_history_and_not_an_error() {
        let scratch = Scratch::new("absent");
        let path = scratch.root.join("ime-history");
        let history = History::open(
            &crate::dict::FileSource::new(&path),
            Box::new(FileSink::new(&path)),
        );
        assert!(history.is_empty());
        assert!(history.persists(), "an absent file is still writable");
        assert_eq!(
            history.last_error().map(io::Error::kind),
            Some(io::ErrorKind::NotFound)
        );
    }

    #[test]
    fn a_history_file_that_cannot_be_read_still_learns_for_this_session() {
        let (sink, history) = {
            let sink = Rc::new(MemorySink::new());
            let history = History::open(&UnreadableSource, Box::new(SharedSink(sink.clone())));
            (sink, history)
        };
        let mut history = history;
        assert!(history.is_empty());
        assert_eq!(
            history.last_error().map(io::Error::kind),
            Some(io::ErrorKind::PermissionDenied)
        );
        history.remember("きしゃ", &word("記者"));
        assert_eq!(words(history.remembered("きしゃ")), vec!["記者"]);
        assert_eq!(sink.lines(), vec!["きしゃ /記者/".to_string()]);
    }

    #[test]
    fn a_read_only_filesystem_learns_for_the_session_and_stops_trying_to_write() {
        let sink = ReadOnlySink::new();
        let mut history = History::from_bytes(b"", Box::new(sink.clone()));
        for chosen in ["記者", "貴社", "汽車"] {
            history.remember("きしゃ", &word(chosen));
        }
        // Learning still happened, in this session, in memory.
        assert_eq!(
            words(history.remembered("きしゃ")),
            vec!["汽車", "貴社", "記者"]
        );
        // But the failed write was attempted exactly once. Retrying a
        // guaranteed EROFS on every commit spends a syscall to learn nothing.
        assert_eq!(sink.attempts.get(), 1);
        assert!(!history.persists());
        assert_eq!(
            history.last_error().map(io::Error::kind),
            Some(io::ErrorKind::PermissionDenied)
        );
    }

    #[test]
    fn a_history_with_nowhere_to_write_still_reorders_within_the_session() {
        let mut history = History::in_memory();
        assert!(!history.persists());
        history.remember("きしゃ", &word("記者"));
        let merged = history.merge("きしゃ", dictionary().lookup("きしゃ"));
        assert_eq!(words(&merged), vec!["記者", "貴社", "汽車", "帰社"]);
    }

    #[test]
    fn a_candidate_that_could_not_be_written_back_is_not_remembered_either() {
        let (mut history, sink) = with_memory();
        // None of these can survive the file format, so none of them may
        // change what this session answers: the two orders would diverge.
        history.remember("きしゃ", &word("記/者"));
        history.remember("き しゃ", &word("記者"));
        history.remember("", &word("記者"));
        history.remember("きしゃ", &word(""));
        assert!(history.is_empty());
        assert!(sink.lines().is_empty());
    }

    #[test]
    fn an_annotation_that_would_break_the_line_is_dropped_and_the_word_is_kept() {
        let (mut history, sink) = with_memory();
        history.remember("きしゃ", &annotated("記者", "a/slash"));
        assert_eq!(sink.lines(), vec!["きしゃ /記者/".to_string()]);
        assert_eq!(history.remembered("きしゃ"), [word("記者")]);
    }

    #[test]
    fn a_reading_remembers_at_most_a_page_and_a_half_of_candidates() {
        let (mut history, _sink) = with_memory();
        for n in 0..MAX_CANDIDATES + 10 {
            history.remember("あ", &word(&format!("第{n}")));
        }
        assert_eq!(history.remembered("あ").len(), MAX_CANDIDATES);
        // The newest is kept and the oldest is the one dropped.
        assert_eq!(
            history.remembered("あ")[0],
            word(&format!("第{}", MAX_CANDIDATES + 9))
        );
        assert!(!history.remembered("あ").iter().any(|c| c.word == "第0"));
    }

    /// A six-kana reading for `n`, the same trick `dict`'s large test uses so
    /// that every reading is distinct.
    fn reading_of(n: u32) -> String {
        const KANA: [&str; 10] = ["あ", "い", "う", "え", "お", "か", "き", "く", "け", "こ"];
        let mut reading = String::new();
        let mut digits = n;
        for _ in 0..6 {
            reading.insert_str(0, KANA[(digits % 10) as usize]);
            digits /= 10;
        }
        reading
    }

    #[test]
    fn a_history_that_has_grown_large_stops_growing_rather_than_filling_the_disk() {
        let (mut history, sink) = with_memory();
        let commits = MAX_READINGS as u32 * 3;
        for n in 0..commits {
            history.remember(&reading_of(n), &word(&format!("第{n}")));
        }
        // Memory is capped.
        assert_eq!(history.len(), MAX_READINGS);
        // The oldest readings were evicted and the newest were kept.
        assert!(history.remembered(&reading_of(0)).is_empty());
        assert_eq!(
            words(history.remembered(&reading_of(commits - 1))),
            vec![format!("第{}", commits - 1)]
        );
        // And so is the file: compaction kept it near what the entries need
        // rather than near the number of commits.
        let lines = sink.lines().len();
        assert!(
            lines <= 2 * MAX_READINGS + COMPACT_SLACK + 1,
            "{lines} lines on disk after {commits} commits"
        );
        // Compaction is amortised: far fewer rewrites than commits.
        assert!(
            sink.writes() < commits as usize + 64,
            "{} writes for {commits} commits",
            sink.writes()
        );
        // What is on disk still answers, and answers the same.
        let reloaded = History::from_bytes(&sink.contents(), Box::new(NullSink));
        assert_eq!(reloaded.len(), history.len());
        assert_eq!(
            words(reloaded.remembered(&reading_of(commits - 1))),
            vec![format!("第{}", commits - 1)]
        );
    }

    #[test]
    fn a_reading_is_found_among_thousands_of_others() {
        let (mut history, _sink) = with_memory();
        for n in 0..MAX_READINGS as u32 {
            history.remember(&reading_of(n), &word(&format!("第{n}")));
        }
        // First, middle and last of a store at its cap: a binary search that
        // is off by one at either end still finds the middle.
        for n in [0u32, (MAX_READINGS / 2) as u32, MAX_READINGS as u32 - 1] {
            assert_eq!(
                words(history.remembered(&reading_of(n))),
                vec![format!("第{n}")],
                "{}",
                reading_of(n)
            );
        }
        assert!(history.remembered("んんんんんん").is_empty());
        assert!(history.remembered("あ").is_empty());
    }

    #[test]
    fn the_history_is_written_through_a_real_file_when_it_has_one() {
        let scratch = Scratch::new("real-file");
        let path = scratch.root.join("tos").join("ime-history");
        {
            let mut history = History::open(
                &crate::dict::FileSource::new(&path),
                Box::new(FileSink::new(&path)),
            );
            history.remember("きしゃ", &word("記者"));
        }
        // The directory did not exist and was made on the way.
        assert_eq!(fs::read_to_string(&path).unwrap(), "きしゃ /記者/\n");
        // A second process picks up where the first left off.
        let mut history = History::open(
            &crate::dict::FileSource::new(&path),
            Box::new(FileSink::new(&path)),
        );
        assert_eq!(words(history.remembered("きしゃ")), vec!["記者"]);
        history.remember("きしゃ", &word("貴社"));
        history.compact();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("きしゃ /貴社/記者/"), "{text}");
        assert!(
            !scratch.root.join("tos").join("ime-history.new").exists(),
            "the temporary file was left behind"
        );
    }

    #[test]
    fn the_path_is_under_xdg_data_home_when_the_environment_has_one() {
        assert_eq!(
            path_from(Some(OsStr::new("/home/someone/.local/share")), None),
            Some(PathBuf::from("/home/someone/.local/share/tos/ime-history"))
        );
    }

    #[test]
    fn the_path_falls_back_to_the_specifications_default_under_home() {
        assert_eq!(
            path_from(None, Some(OsStr::new("/home/someone"))),
            Some(PathBuf::from("/home/someone/.local/share/tos/ime-history"))
        );
    }

    #[test]
    fn an_environment_with_no_home_in_it_has_nowhere_to_put_a_history() {
        // What `/init` hands the compositor on the live ISO.
        assert_eq!(path_from(None, None), None);
        assert_eq!(path_from(Some(OsStr::new("")), Some(OsStr::new(""))), None);
        // A relative XDG_DATA_HOME is invalid per the specification, and HOME
        // is not there to fall back to.
        assert_eq!(path_from(Some(OsStr::new("share")), None), None);
        assert_eq!(
            path_from_owned(
                Some(OsString::from("share")),
                Some(OsString::from("/home/someone"))
            ),
            Some(PathBuf::from("/home/someone/.local/share/tos/ime-history"))
        );
    }
}
