//! Reading an SKK dictionary. See docs/design/ime.md and issue #56.
//!
//! SKK's format is the whole reason tOS needs no conversion daemon. A
//! dictionary is a sorted text file — `かんれい /慣例/寒冷/管領/艦齢/` — and
//! `skkserv` is a daemon whose entire job is to hold that file in memory and
//! answer questions about it. tOS can open the file. `docs/design/ime.md`
//! argues that out against mozc and anthy and measures what each costs.
//!
//! So this module is the file. [`Dictionary::open`] reads it once into a
//! single `Vec<u8>` and builds a `Vec<u32>` holding the offset of every
//! entry's first byte — 159,795 entries and about 640 KB for `SKK-JISYO.L`,
//! against the 5.2 MB of text they point into — and [`Dictionary::lookup`]
//! binary searches that index. Nothing here reopens the file and nothing
//! rescans it: a lookup is a handful of comparisons against slices of the
//! bytes that were read at startup, which is what makes it affordable to do
//! on every 変換 keystroke.
//!
//! Where those bytes come from is a [`Source`], for the same reason
//! `tos-system` puts every read of the machine behind `Sysfs` and every
//! change to it behind a trait: the machine running these tests has no
//! Japanese dictionary on it, and a test that needed
//! `/usr/share/tos/SKK-JISYO.L` to exist would be a test that only runs on
//! the ISO. The compositor hands over a [`FileSource`] built from the path in
//! `[ime] dictionary`; the tests below hand over a [`BytesSource`] they wrote
//! inline, or a file in a temporary directory they made and delete again.
//!
//! The file is two halves, and they are two indexes. [`Dictionary::lookup`]
//! answers from the okuri-nasi half, where a reading is a whole word;
//! [`Dictionary::lookup_okuri`] answers from the okuri-ari half, where a
//! reading is a stem with a latin marker stuck to it — `かk /書/`. They could
//! not share one index: the okuri-ari section is sorted descending in SKK's
//! own encoding and every one of its readings ends in a letter no okuri-nasi
//! reading has, so one merged index would leave each search answering for the
//! other's entries. Which stem a reading should be cut into is not decided
//! here — that is `okuri.rs`, and this module only knows how to find a key.
//!
//! Two things this deliberately does not do, each because something else
//! owns them:
//!
//! - **EUC-JP.** The published dictionary is EUC-JP and tOS is UTF-8
//!   throughout, so `mkiso.sh` converts it once with `iconv` at image build
//!   time (#57). By the time this module sees bytes they are UTF-8, and a
//!   line that turns out not to be is dropped rather than decoded.
//! - **Numeric entries.** `だい# /第#1/` converts a digit run through a
//!   rewriting rule; the `#` is matched literally here, so `だい5` finds
//!   nothing. That is a candidate transformation rather than a lookup, and it
//!   belongs with the converter that will have the digits in hand.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// One thing a reading could mean.
///
/// The annotation is kept beside the word rather than thrown away, even
/// though the issue only asks that it be stripped. It is not part of what
/// gets committed to the pane — `慣例;custom` commits 慣例 — but it is often
/// the only thing that tells a user which of 慣例 and 寒冷 they are looking
/// at, and the candidate window is where SKK shows it. Dropping it here would
/// mean parsing the same line a second time later to get it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// What is committed: everything before the first `;`.
    pub word: String,
    /// What is shown beside it, if the entry carried one.
    pub annotation: Option<String>,
}

/// Where a dictionary's bytes come from.
///
/// One method, because opening a dictionary is one act. The seam is a trait
/// rather than an injectable root the way `Sysfs` is one, because there is no
/// tree here to re-root: there is a single file whose path comes from config,
/// and the question a test needs to answer is "what is in it", not "where is
/// it".
pub trait Source {
    /// The whole dictionary, in one read.
    ///
    /// Whole, and not a `Read` handed back for streaming, because the index
    /// below is offsets into these bytes and the bytes have to outlive it.
    /// This is what `tos-font` already does with the 4 MB VL Gothic face: one
    /// `fs::read` at startup and then no more I/O.
    fn read(&self) -> io::Result<Vec<u8>>;
}

/// A dictionary on disk, which is what the compositor has.
#[derive(Debug, Clone)]
pub struct FileSource {
    path: PathBuf,
}

impl FileSource {
    pub fn new(path: impl Into<PathBuf>) -> FileSource {
        FileSource { path: path.into() }
    }

    /// Which file this is, so a compositor that tried the several paths
    /// `docs/design/ime.md` lists can say which one it settled on.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Source for FileSource {
    fn read(&self) -> io::Result<Vec<u8>> {
        fs::read(&self.path)
    }
}

/// A dictionary somebody already has the bytes of.
///
/// This is how a test writes four entries and looks one of them up with no
/// filesystem anywhere in it. The clone in `read` is not worth avoiding: a
/// dictionary small enough to write inline is small enough to copy, and the
/// alternative — a `Source` that consumes itself — cannot be a trait object.
#[derive(Debug, Clone)]
pub struct BytesSource {
    bytes: Vec<u8>,
}

impl BytesSource {
    pub fn new(bytes: impl Into<Vec<u8>>) -> BytesSource {
        BytesSource {
            bytes: bytes.into(),
        }
    }
}

impl Source for BytesSource {
    fn read(&self) -> io::Result<Vec<u8>> {
        Ok(self.bytes.clone())
    }
}

/// A dictionary, held open.
pub struct Dictionary {
    /// The file, verbatim. Every candidate a lookup produces is cut out of
    /// here.
    bytes: Vec<u8>,
    /// The offset of the first byte of each okuri-nasi entry, in the order
    /// their readings sort. See [`index`] for why that is not simply the
    /// order they appear in the file.
    entries: Vec<u32>,
    /// The same for the okuri-ari half, whose readings are a stem and a
    /// marker — `かk`. Separate because they sort into a different place and
    /// answer a different question; see [`index`].
    okuri: Vec<u32>,
}

impl Dictionary {
    /// Read a dictionary through `source`.
    ///
    /// The error is the source's own: a dictionary that was configured and is
    /// not there is something the user asked for that did not happen, and
    /// they are owed the reason. A caller that would rather carry on without
    /// Japanese input can fall back to [`Dictionary::from_bytes`] of nothing,
    /// which answers every lookup with no candidates.
    pub fn open(source: &dyn Source) -> io::Result<Dictionary> {
        Ok(Dictionary::from_bytes(source.read()?))
    }

    /// A dictionary from bytes already in hand.
    pub fn from_bytes(bytes: Vec<u8>) -> Dictionary {
        let (entries, okuri) = index(&bytes);
        Dictionary {
            bytes,
            entries,
            okuri,
        }
    }

    /// How many whole-word readings can be looked up. Not how many candidates
    /// there are, and not how many lines the file has: comment lines and the
    /// okuri-ari half are counted by [`Dictionary::okuri_len`] instead.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// How many okuri-ari keys can be looked up — 15,995 in Debian's `skkdic`
    /// copy of `SKK-JISYO.L`, which is the one `mkiso.sh` converts.
    pub fn okuri_len(&self) -> usize {
        self.okuri.len()
    }

    /// What `reading` could mean, best guess first.
    ///
    /// The order is the entry's own, which for `SKK-JISYO.L` is the order its
    /// maintainers put the candidates in — roughly commonest first. Nothing
    /// here reorders by use; the history file that would do that is deferred
    /// in `docs/design/ime.md` and changes no signature in this module.
    ///
    /// Empty when the reading is absent and when its entry is malformed. Also
    /// empty for anything inflected: 「かきます」 is not a reading in this
    /// half and never will be. [`okuri::convert`](crate::okuri::convert) is
    /// what answers those, by cutting the reading up and asking
    /// [`Dictionary::lookup_okuri`] about the pieces.
    pub fn lookup(&self, reading: &str) -> Vec<Candidate> {
        find(&self.bytes, &self.entries, reading.as_bytes())
    }

    /// What an okuri-ari key could mean, best guess first.
    ///
    /// The key is the whole reading the file is keyed by, marker included —
    /// `かk`, not `か` and `'k'` as two arguments. That is on purpose: this
    /// module's job is to find a line in a file, and the marker is part of
    /// what the line is called. Working out that 「かきます」 should be asked
    /// about as `かk` needs a table of kana and a rule for where a word ends,
    /// and both of those are Japanese rather than file format, so they live
    /// in `okuri.rs`.
    ///
    /// The candidate is the stem alone — `かk /書/` answers `書`, not
    /// `書きます`. Sticking the okurigana back on needs the reading that was
    /// cut up, which the caller has and this does not.
    pub fn lookup_okuri(&self, key: &str) -> Vec<Candidate> {
        find(&self.bytes, &self.okuri, key.as_bytes())
    }
}

/// Binary search one index for `key` and cut up the line it names.
///
/// Shared by both halves because the halves differ in what their keys mean
/// and not at all in how they are found — the alternative, two searches, is
/// two places for the `partition_point` subtlety below to be got wrong.
///
/// `partition_point` rather than `binary_search_by`, because the latter may
/// land on any one of several equal elements. SKK's halves should each have
/// no duplicate readings, but a hand-merged file can, and answering with a
/// different one of them depending on how big the dictionary happens to be is
/// the kind of bug that is found years later.
fn find(bytes: &[u8], entries: &[u32], key: &[u8]) -> Vec<Candidate> {
    let at = entries.partition_point(|&offset| reading_at(bytes, offset) < key);
    let Some(&offset) = entries.get(at) else {
        return Vec::new();
    };
    if reading_at(bytes, offset) != key {
        return Vec::new();
    }
    candidates(line_at(bytes, offset))
}

/// Not derived: the first field is the entire dictionary, and a `{:?}` that
/// printed five megabytes of Japanese is not a debugging aid.
impl std::fmt::Debug for Dictionary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dictionary")
            .field("entries", &self.entries.len())
            .field("okuri", &self.okuri.len())
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

/// Find every entry and put its offset in sorted order, in the index for the
/// half it came from: okuri-nasi first, okuri-ari second.
///
/// Two things here do not follow from the format.
///
/// **The two halves get two indexes.** A dictionary is two sections, each
/// announced by a comment: `;; okuri-ari entries.` and then
/// `;; okuri-nasi entries.`. Entries in the first carry an okurigana marker —
/// `わたしm /私/`, `かk /書/` — and they are kept apart from the second for
/// two reasons that are each sufficient. The okuri-ari section is sorted
/// *descending* in SKK's own encoding, so merging it into an ascending index
/// would mean re-sorting 176,000 entries instead of two runs of 16,000 and
/// 160,000. And every okuri-ari reading ends in a latin letter that no
/// okuri-nasi reading has, so a merged index would put `かk` among readings
/// beginning `か…` and leave a search for a whole word stepping over stems it
/// can never mean.
///
/// A file with no section comments at all — a test's four lines, or a
/// fragment somebody keeps of their own — is taken as all okuri-nasi, which
/// is what such a file almost always is and the only assumption that makes
/// the small case work without ceremony.
///
/// Lines beginning `>` — `>あk /飽/`, SKK's suffix entries — are indexed like
/// any other, because they are well formed and nothing here has an opinion
/// about what a reading means. No split `okuri.rs` makes produces a key
/// starting with `>`, so they simply never match.
///
/// **The index is sorted, and that is not redundant.** `SKK-JISYO.L` really
/// is sorted, but it is sorted in EUC-JP, and `iconv` does not preserve that
/// order. `ー` is JIS row 1 and `あ` is row 4, so EUC-JP puts `ー` (`A1 BC`)
/// first; in UTF-8 `あ` is `E3 81 82` and `ー` is `E3 83 BC`, so the pair is
/// the other way round, and every reading with a long vowel in it — `こーひー`
/// and its kind — is out of place by the time tOS reads the file. Binary
/// searching that would silently answer "no candidates" for words that are
/// present, which is worse than a crash because nobody reports it. Sorting
/// costs nothing worth measuring next to the multi-megabyte read that got us
/// here, and `sort_by` is the stable, run-detecting sort: a file already in
/// order — which is every file except at the handful of points where the two
/// encodings disagree — is a merge over a few long runs rather than a full
/// n log n. Stable also means that if a reading really does appear twice, the
/// one earlier in the file stays first, so the answer does not depend on the
/// sort.
fn index(bytes: &[u8]) -> (Vec<u32>, Vec<u32>) {
    let mut entries: Vec<u32> = Vec::new();
    let mut okuri: Vec<u32> = Vec::new();
    // True until a section comment says otherwise; see the note above about
    // dictionaries that have no section comments.
    let mut okuri_nasi = true;
    let mut pos = 0usize;

    while pos < bytes.len() {
        let end = bytes[pos..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(bytes.len(), |i| pos + i);
        let line = trim_cr(&bytes[pos..end]);

        if line.first() == Some(&b';') {
            // Matched at the start of the line, with the semicolons and
            // spaces taken off first, rather than anywhere in it: a header
            // line that happens to mention okurigana must not switch sections.
            let text = line
                .iter()
                .position(|&b| b != b';' && b != b' ')
                .map_or(&line[line.len()..], |i| &line[i..]);
            if text.starts_with(b"okuri-ari") {
                okuri_nasi = false;
            } else if text.starts_with(b"okuri-nasi") {
                okuri_nasi = true;
            }
        } else if pos <= u32::MAX as usize && is_entry(line) {
            // The offset is a u32 because 159,795 of them is 640 KB and
            // 1.2 MB would be waste. A dictionary over 4 GiB is not a
            // dictionary, and the rest of such a file is left unindexed
            // rather than widening every entry to pay for a case that does
            // not happen.
            if okuri_nasi {
                entries.push(pos as u32);
            } else {
                okuri.push(pos as u32);
            }
        }

        pos = end + 1;
    }

    entries.sort_by(|&a, &b| reading_at(bytes, a).cmp(reading_at(bytes, b)));
    okuri.sort_by(|&a, &b| reading_at(bytes, a).cmp(reading_at(bytes, b)));
    (entries, okuri)
}

/// Whether a line is an entry: a non-empty reading, a space, and a candidate
/// list starting where SKK says it does.
///
/// Anything else is walked past rather than indexed. A truncated download
/// ends in half a line and a hand-edited file has a line somebody was in the
/// middle of; neither is a reason to refuse the other 159,794 entries, and a
/// fragment in the index is a fragment that sorts somewhere and answers some
/// lookup with nonsense.
fn is_entry(line: &[u8]) -> bool {
    let Some(space) = line.iter().position(|&b| b == b' ') else {
        return false;
    };
    space > 0 && line.get(space + 1) == Some(&b'/')
}

/// The line starting at `offset`, without its newline or a CRLF's carriage
/// return. The `\r` matters: it would otherwise be the last byte of the last
/// candidate on the line, and of the reading on a line that has nothing else.
fn line_at(bytes: &[u8], offset: u32) -> &[u8] {
    let start = offset as usize;
    let Some(rest) = bytes.get(start..) else {
        return &[];
    };
    let end = rest.iter().position(|&b| b == b'\n').unwrap_or(rest.len());
    trim_cr(&rest[..end])
}

/// The reading an entry is filed under: everything before the first space.
fn reading_at(bytes: &[u8], offset: u32) -> &[u8] {
    let line = line_at(bytes, offset);
    line.iter()
        .position(|&b| b == b' ')
        .map_or(line, |i| &line[..i])
}

fn trim_cr(line: &[u8]) -> &[u8] {
    match line.last() {
        Some(b'\r') => &line[..line.len() - 1],
        _ => line,
    }
}

/// Cut an entry's `/a/b/c/` into candidates.
///
/// Empty pieces are dropped rather than becoming empty candidates, which
/// covers the leading and trailing slashes every entry has and the `//` a
/// careless edit leaves behind. A piece that is not UTF-8 is dropped for a
/// related reason: the file is UTF-8 by the time tOS sees it, so a piece that
/// is not means this line is damaged, and one damaged line should cost its
/// own candidates and nothing else.
///
/// `(concat "...")` candidates — how SKK escapes a `/` or a `;` inside a word
/// — come through verbatim. Evaluating a Lisp expression to type one
/// character is not machinery this module is going to grow while there are a
/// few hundred such entries in 159,795.
fn candidates(line: &[u8]) -> Vec<Candidate> {
    let Some(space) = line.iter().position(|&b| b == b' ') else {
        return Vec::new();
    };
    line[space + 1..]
        .split(|&b| b == b'/')
        .filter(|piece| !piece.is_empty())
        .filter_map(|piece| std::str::from_utf8(piece).ok())
        .filter_map(candidate)
        .collect()
}

/// Split one candidate from its annotation.
fn candidate(text: &str) -> Option<Candidate> {
    let (word, annotation) = match text.split_once(';') {
        Some((word, note)) => (word, (!note.is_empty()).then(|| note.to_string())),
        None => (text, None),
    };
    if word.is_empty() {
        // `;なんとか` is an annotation with nothing annotated. There is no
        // word to commit, so there is no candidate to offer.
        return None;
    }
    Some(Candidate {
        word: word.to_string(),
        annotation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// The shape of the real thing, small enough to read: a header, both
    /// section comments, and the okuri-ari half SKK puts first.
    const SAMPLE: &str = concat!(
        ";; -*- mode: fundamental; coding: utf-8 -*-\n",
        ";; Large size dictionary for SKK system\n",
        ";; okuri-ari entries.\n",
        "わたしm /私/\n",
        "かきr /書/\n",
        ";; okuri-nasi entries.\n",
        "かんじ /漢字/幹事/監事/\n",
        "かんれい /慣例/寒冷/管領/艦齢/\n",
        "けいけん /経験;experience/敬虔/\n",
        "とうきょう /東京/\n",
    );

    fn sample() -> Dictionary {
        Dictionary::open(&BytesSource::new(SAMPLE)).expect("bytes always read")
    }

    fn words(dict: &Dictionary, reading: &str) -> Vec<String> {
        dict.lookup(reading)
            .into_iter()
            .map(|candidate| candidate.word)
            .collect()
    }

    /// A directory that cleans up after itself, so the tests that want a real
    /// path do not leave one behind. The same shape `tos-system`'s `sysfs`
    /// tests use, for the same reason.
    struct Scratch {
        root: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let root =
                std::env::temp_dir().join(format!("tos-ime-dict-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).unwrap();
            Scratch { root }
        }

        fn file(&self, name: &str, contents: &str) -> PathBuf {
            let path = self.root.join(name);
            fs::write(&path, contents).unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn a_reading_answers_with_its_candidates_in_the_order_the_file_has_them() {
        assert_eq!(
            words(&sample(), "かんれい"),
            vec!["慣例", "寒冷", "管領", "艦齢"]
        );
    }

    #[test]
    fn an_annotation_is_kept_beside_the_word_and_not_inside_it() {
        assert_eq!(
            sample().lookup("けいけん"),
            vec![
                Candidate {
                    word: "経験".to_string(),
                    annotation: Some("experience".to_string()),
                },
                Candidate {
                    word: "敬虔".to_string(),
                    annotation: None,
                },
            ]
        );
    }

    #[test]
    fn a_reading_that_is_not_in_the_dictionary_answers_with_nothing() {
        assert!(sample().lookup("ぬけている").is_empty());
    }

    #[test]
    fn the_header_and_the_section_comments_are_not_entries() {
        // Four okuri-nasi entries, and nothing for the two header lines, the
        // two section comments or the two okuri-ari entries.
        assert_eq!(sample().len(), 4);
    }

    #[test]
    fn a_whole_word_lookup_never_answers_from_the_okuri_ari_half() {
        let dict = sample();
        assert!(dict.lookup("わたしm").is_empty());
        assert!(dict.lookup("わたし").is_empty());
        assert!(dict.lookup("かきr").is_empty());
        // And the okuri-nasi half on the far side of the marker is unharmed.
        assert_eq!(words(&dict, "かんじ"), vec!["漢字", "幹事", "監事"]);
    }

    #[test]
    fn an_okuri_lookup_never_answers_from_the_okuri_nasi_half() {
        let dict = sample();
        assert!(dict.lookup_okuri("かんじ").is_empty());
        assert!(dict.lookup_okuri("とうきょう").is_empty());
    }

    #[test]
    fn an_okuri_ari_entry_is_found_by_the_key_the_file_gives_it_marker_and_all() {
        let dict = sample();
        assert_eq!(
            dict.lookup_okuri("わたしm"),
            vec![Candidate {
                word: "私".to_string(),
                annotation: None,
            }]
        );
        // The stem alone, without the okurigana: putting きます back on is the
        // caller's job, because the caller is the one holding the reading.
        assert_eq!(
            dict.lookup_okuri("かきr")
                .into_iter()
                .map(|candidate| candidate.word)
                .collect::<Vec<_>>(),
            vec!["書"]
        );
        assert!(dict.lookup_okuri("かき").is_empty());
    }

    #[test]
    fn the_two_halves_are_counted_separately() {
        let dict = sample();
        assert_eq!(dict.len(), 4);
        assert_eq!(dict.okuri_len(), 2);
    }

    #[test]
    fn the_okuri_ari_half_is_searchable_although_the_file_holds_it_descending() {
        // SKK writes this section largest reading first — the shipped file
        // opens `をs`, `ゐr`, `われらg` — so a binary search over it as it
        // lies finds nothing. These four are in the file's own order.
        let dict = Dictionary::from_bytes(
            concat!(
                ";; okuri-ari entries.\n",
                "をs /惜/\n",
                "われw /我/\n",
                "かk /書/掛/\n",
                "あk /open/\n",
                ";; okuri-nasi entries.\n",
                "あい /愛/\n",
            )
            .as_bytes()
            .to_vec(),
        );
        assert_eq!(dict.okuri_len(), 4);
        for (key, first) in [
            ("をs", "惜"),
            ("われw", "我"),
            ("かk", "書"),
            ("あk", "open"),
        ] {
            assert_eq!(
                dict.lookup_okuri(key).first().map(|c| c.word.as_str()),
                Some(first),
                "{key} should be findable wherever the file put it"
            );
        }
        assert!(dict.lookup_okuri("んz").is_empty());
    }

    #[test]
    fn a_file_with_no_section_comments_is_read_as_all_okuri_nasi() {
        let dict = Dictionary::from_bytes("あい /愛/藍/\nうみ /海/\n".as_bytes().to_vec());
        assert_eq!(dict.len(), 2);
        assert_eq!(dict.okuri_len(), 0);
        assert_eq!(words(&dict, "あい"), vec!["愛", "藍"]);
    }

    #[test]
    fn a_dictionary_that_is_empty_answers_every_lookup_with_nothing() {
        let dict = Dictionary::from_bytes(Vec::new());
        assert!(dict.is_empty());
        assert_eq!(dict.len(), 0);
        assert!(dict.lookup("かんじ").is_empty());
    }

    #[test]
    fn a_truncated_dictionary_loses_the_last_line_rather_than_panicking() {
        // Cut mid-entry with no newline to end it: what a download that
        // stopped halfway leaves on disk.
        let dict = Dictionary::from_bytes(
            ";; okuri-nasi entries.\nあい /愛/\nかんじ /漢"
                .as_bytes()
                .to_vec(),
        );
        assert_eq!(words(&dict, "あい"), vec!["愛"]);
        // The half-line is still an entry by the format's own rules — reading,
        // space, slash — so it answers with what survived rather than being
        // discarded. What matters is that it did not take the process with it.
        assert_eq!(words(&dict, "かんじ"), vec!["漢"]);
    }

    #[test]
    fn lines_that_are_not_entries_never_reach_the_index() {
        let dict = Dictionary::from_bytes(
            [
                ";; okuri-nasi entries.\n",
                "\n",                // blank
                "なにか\n",          // a reading and nothing else
                " /みだし/\n",       // no reading
                "こわれた みだし\n", // no candidate list
                "あい /愛/\n",
            ]
            .concat()
            .into_bytes(),
        );
        assert_eq!(dict.len(), 1);
        assert_eq!(words(&dict, "あい"), vec!["愛"]);
        assert!(dict.lookup("なにか").is_empty());
        assert!(dict.lookup("こわれた").is_empty());
    }

    #[test]
    fn a_candidate_that_is_not_utf8_is_dropped_and_its_neighbours_are_not() {
        // The middle candidate is a lone EUC-JP pair, which is what a
        // dictionary that escaped `iconv` would be made of throughout.
        let mut bytes = "あい /愛/".as_bytes().to_vec();
        bytes.extend_from_slice(&[0xb0, 0xa6]);
        bytes.extend_from_slice("/藍/\n".as_bytes());
        let dict = Dictionary::from_bytes(bytes);
        assert_eq!(words(&dict, "あい"), vec!["愛", "藍"]);
    }

    #[test]
    fn an_annotation_with_no_word_in_front_of_it_is_not_a_candidate() {
        let dict = Dictionary::from_bytes("あい //愛/;orphan/藍;indigo/\n".as_bytes().to_vec());
        assert_eq!(words(&dict, "あい"), vec!["愛", "藍"]);
    }

    #[test]
    fn a_carriage_return_is_not_part_of_the_reading_or_the_last_candidate() {
        let dict = Dictionary::from_bytes(
            ";; okuri-nasi entries.\r\nあい /愛/藍\r\n"
                .as_bytes()
                .to_vec(),
        );
        assert_eq!(words(&dict, "あい"), vec!["愛", "藍"]);
    }

    #[test]
    fn a_reading_that_iconv_moved_out_of_order_is_still_found() {
        // SKK-JISYO.L is sorted in EUC-JP, where `ー` (A1 BC, JIS row 1) comes
        // before `あ` (A4 A2, row 4). In UTF-8 it is the other way round — `あ`
        // is E3 81 82 and `ー` is E3 83 BC — so `iconv` hands the three lines
        // over in exactly this order, which is not UTF-8 order. Binary
        // searching them where they lie loses こい and こいぬ entirely.
        let dict = Dictionary::from_bytes(
            concat!(
                ";; okuri-nasi entries.\n",
                "こーひー /珈琲/\n",
                "こい /恋/鯉/\n",
                "こいぬ /子犬/\n",
            )
            .as_bytes()
            .to_vec(),
        );
        assert_eq!(words(&dict, "こーひー"), vec!["珈琲"]);
        assert_eq!(words(&dict, "こい"), vec!["恋", "鯉"]);
        assert_eq!(words(&dict, "こいぬ"), vec!["子犬"]);
    }

    /// A six-kana reading for `n`, counting in kana digits so that every
    /// reading is distinct and they come out already in order.
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

    /// A dictionary within an order of magnitude of a real one, so that
    /// finding a reading means finding it among tens of thousands of
    /// neighbours rather than among four.
    fn many(count: u32) -> Vec<u8> {
        let mut text = String::from(";; okuri-nasi entries.\n");
        for n in 0..count {
            text.push_str(&format!("{} /第{n}/\n", reading_of(n)));
        }
        text.into_bytes()
    }

    #[test]
    fn a_reading_is_found_among_fifty_thousand_others() {
        let dict = Dictionary::from_bytes(many(50_000));
        assert_eq!(dict.len(), 50_000);
        // The first, the last, and one in the middle: a search that is off by
        // one at either end of the index still finds the middle.
        assert_eq!(words(&dict, &reading_of(0)), vec!["第0"]);
        assert_eq!(words(&dict, &reading_of(24_989)), vec!["第24989"]);
        assert_eq!(words(&dict, &reading_of(49_999)), vec!["第49999"]);
        // A reading that sorts inside the range and is not there.
        assert!(dict.lookup(&format!("{}あ", reading_of(24_989))).is_empty());
        // And ones that sort off either end of it.
        assert!(dict.lookup("あ").is_empty());
        assert!(dict.lookup("んんんんんん").is_empty());
    }

    /// A source that counts, so that "the file is read once" is a test rather
    /// than a comment.
    struct Counting {
        bytes: Vec<u8>,
        reads: Cell<usize>,
    }

    impl Source for Counting {
        fn read(&self) -> io::Result<Vec<u8>> {
            self.reads.set(self.reads.get() + 1);
            Ok(self.bytes.clone())
        }
    }

    #[test]
    fn the_dictionary_is_read_once_and_not_once_per_lookup() {
        let source = Counting {
            bytes: SAMPLE.as_bytes().to_vec(),
            reads: Cell::new(0),
        };
        let dict = Dictionary::open(&source).expect("bytes always read");
        for _ in 0..100 {
            assert_eq!(words(&dict, "かんじ"), vec!["漢字", "幹事", "監事"]);
        }
        assert_eq!(source.reads.get(), 1, "the dictionary was reopened");
    }

    #[test]
    fn a_test_can_point_the_seam_at_a_file_it_owns() {
        let scratch = Scratch::new("own-file");
        let path = scratch.file("SKK-JISYO.test", SAMPLE);
        let source = FileSource::new(&path);
        assert_eq!(source.path(), path);
        let dict = Dictionary::open(&source).expect("the file was just written");
        assert_eq!(words(&dict, "とうきょう"), vec!["東京"]);
    }

    #[test]
    fn a_dictionary_that_is_not_there_is_an_error_rather_than_an_empty_one() {
        let scratch = Scratch::new("missing");
        let source = FileSource::new(scratch.root.join("SKK-JISYO.absent"));
        let error = Dictionary::open(&source).expect_err("nothing was written");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }
}
