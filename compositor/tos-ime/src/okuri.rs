//! Okurigana: converting 「かきます」 to 「書きます」. See docs/design/ime.md
//! and issue #63.
//!
//! `dict.rs` converts 「かんじ」 to 「漢字」 and stops there, which is the
//! limit `docs/design/ime.md` states plainly: an IME that cannot write a verb
//! cannot write Japanese. The words it cannot write are all in the file
//! already — 15,995 okuri-ari entries, each a stem keyed by its kana and one
//! latin letter, `かk /書/` and `わたしm /私/` — and the letter is the first
//! romaji character of the okurigana. `かk` means 書 followed by something
//! beginning か, き, く, け or こ. Using them means knowing where the stem
//! ends, and that is the whole of this module.
//!
//! The format was checked against the real file rather than assumed, and it
//! held: every one of those 15,995 keys ends in a lowercase latin letter,
//! none of them carries the `[…]` okurigana block SKK's format allows, and
//! the letters used are the 21 that some kana can begin — `f`, `l`, `q`, `v`
//! and `x` never appear. One number moved: `docs/design/ime.md` and #63 say
//! 15,996, counted from skk-dev's own release; Debian's `skkdic`, which is
//! what `mkiso.sh` converts, has 15,995.
//!
//! # Where the stem ends: three ways, and which one tOS takes
//!
//! **Decision: tOS finds the boundary itself, by trying every one of them and
//! keeping the splits the dictionary confirms.** 「かきます」 is cut at each
//! kana in turn; each tail's first kana says which marker letter it would
//! have been typed with; the stem plus that letter is looked up. `かk` is an
//! entry, so 書 + きます is an answer. The other splits are asked too, and
//! whichever ones the dictionary knows about are offered as well, because
//! 変換 offers a list.
//!
//! **Rejected: SKK's shift-key convention**, where the user types `KaKimasu`
//! and the capital K marks the start of the okurigana. It is exact, it is
//! free, and it is what the dictionary format was designed around, so this
//! was the obvious choice and it is still the wrong one. Two reasons.
//!
//! The first is the user. tOS's IME turns on with a binding and converts with
//! 変換, which is what the keys on a JIS keyboard say they do; a user who
//! types `kakimasu` and presses 変換 is doing what every Japanese IME except
//! SKK has taught them for thirty years. Making the one usable path through
//! this feature depend on a convention almost nobody outside SKK knows would
//! mean an input method that works for the people who already have one.
//!
//! The second is the tree. `romaji.rs` answers `KA` and `ka` identically and
//! says why in its own module doc: whether a capital means "start converting"
//! is a question for the layer that owns the modes, and it declined to guess.
//! Taking the shift convention would overturn that — case would have to
//! survive the romaji table, a boundary mark would have to ride along with
//! the preedit through #61's key handling and #59's drawing, and two issues
//! belonging to other people would have to change shape to serve it.
//!
//! **Rejected: a grammar**, a conjugation table for the verb classes and the
//! adjective with a cost model to choose between splits. It is invisible to
//! the user, which is the right goal, and it is also the part of mozc that
//! justifies its 19 MB; `docs/design/ime.md` already declined that trade once
//! on size. The better argument is that it would be redundant. The okuri-ari
//! half *is* a conjugation table — `かk /書/` states exactly which okurigana
//! 書 takes — written by the people who maintain the dictionary, for 15,995
//! stems, and already on the ISO. A hand-written table would be a second,
//! smaller, less accurate copy of a file tOS is already reading.
//!
//! # What it still cannot write
//!
//! #63 asks for this list rather than for a caveat, so it is a list, and
//! every line of it was run against the `SKK-JISYO.L` the ISO ships rather
//! than reasoned about.
//!
//! **A sentence.** Conversion is single-segment by `docs/design/ime.md`'s
//! decision, and this does not change that.
//! 「にほんごをべんきょうします」 in one 変換 offers 似ほんご… and nothing
//! else, because を and し are particles rather than okurigana and no split
//! of the whole run is a word. The user converts 日本語, commits, converts
//! 勉強, commits. Splitting a run into bunsetsu is the task after this one,
//! and it is the one that needs the cost model this deliberately does not
//! have.
//!
//! **A word with a particle stuck to it.** 「きれいに」 offers 切れいに and
//! not 綺麗に: 綺麗 is in the okuri-nasi half as きれい and the に is not
//! okurigana, so neither half of the dictionary has anything to say about the
//! two together. Every 〜に, 〜の, 〜と, 〜は run is this. It is the same
//! limit as the sentence, met one word earlier than a user expects to meet
//! it.
//!
//! **Which homophone was meant.** 「いった」 offers 言った before 行った;
//! 「かった」 offers 勝った before 買った; 「しめます」 offers 占めます
//! before 閉めます. Nothing here chooses, because nothing here has any
//! evidence to choose with — they are the dictionary's order and the user
//! presses 変換 again. #64's history file is what makes the second time
//! right, and it is the larger part of the difference between this and an
//! IME people keep using.
//!
//! **A stem the dictionary reaches by a longer route than the word does.**
//! 「かいた」 offers 解た, 觧た and 買いた before 書いた, because かい is a
//! two-kana stem in `かいt /解/` and か is a one-kana stem in `かi /書/`, and
//! the longer stem wins on the rule above. 「わかりません」 puts 分かりません
//! fourth for the same reason. The rule is right 43 times in 52 and this is
//! what it costs the other nine.
//!
//! **A verb SKK never filed at that point.** The okuri-ari half is not
//! complete and not uniform — it carries 曲って beside 曲がって because SKK
//! users type both, and it has no entry at all for stems nobody added. A
//! conjugated form whose stem is missing converts to nothing, and looks to
//! the user exactly like a word that is not in the dictionary.
//!
//! **And it makes the okuri-nasi half noisier.** 「かんじ」 is 漢字 through
//! [`Dictionary::lookup`] and gains fourteen more conversions here, of which
//! 感じ and 観じ are real words and 兼んじ is not. Offering these after the
//! whole-word candidates, never before, is the whole of the mitigation; it is
//! a real cost of guessing the boundary instead of being told it.

use crate::dict::Dictionary;

/// One way a reading could be written, with the pieces it was made of.
///
/// The pieces are kept beside the finished word rather than thrown away.
/// [`Conversion::word`] is what commits to the pane, and it is the only field
/// the candidate window needs; [`Conversion::key`] and
/// [`Conversion::stem`] are the two halves of the dictionary line this came
/// from, which is exactly what [#64](https://github.com/m96-chan/tOS/issues/64)
/// has to write to its history file — `かk /書/` — and recomputing them there
/// would mean splitting the reading a second time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conversion {
    /// What commits: 「書きます」.
    pub word: String,
    /// The dictionary key it was found under, marker and all: `かk`.
    pub key: String,
    /// The candidate the dictionary gave, alone: 「書」.
    pub stem: String,
    /// The kana of the reading that were left over: 「きます」. These commit
    /// exactly as they were typed — the dictionary never sees them.
    pub okurigana: String,
    /// The entry's annotation, carried through for the candidate window the
    /// same way [`crate::dict::Candidate`] carries it.
    pub annotation: Option<String>,
}

/// Every way `reading` can be written using the okuri-ari half, best first.
///
/// These are offered **after** [`Dictionary::lookup`]'s candidates and never
/// before. A reading that is a whole word in the okuri-nasi half is a whole
/// word: 「かんじ」 is 漢字, and the fact that it also splits as か + んじ
/// into `かn /兼/` is noise that a user must not have to step over to reach
/// the word they typed.
///
/// # How the list is ordered
///
/// Two different things are being decided and they are not the same thing:
/// where the word ends, and how much of its tail is written in kana.
///
/// **The longest split the dictionary confirms decides where the word ends.**
/// 「おくります」 splits as お + くります, which `おk /起/置/` accepts and
/// which would commit 起くります, and as おく + ります, which `おくr /送/贈/`
/// accepts and which commits 送ります. The longer stem is right because it
/// explains more of the reading: two kana of it were found in the dictionary
/// rather than one. That is the whole justification, and it is a measurement
/// rather than a theory about Japanese — over 52 everyday inflected words
/// against the shipped `SKK-JISYO.L`, longest-first puts the intended word
/// first 42 times and in the first three 48 times, and shortest-first puts it
/// first 21 times and in the first three 30.
///
/// **The shortest split reaching the same kanji decides how it is spelled.**
/// 「まがって」 is 曲 at both splits: `まg /曲/枉/` gives 曲がって and
/// `まがt /曲/紛/擬/枉/` gives 曲って. SKK carries both because SKK users
/// type both, but 送り仮名の付け方 asks for 曲がる, 止まる, 分かる rather
/// than 曲る, 止る, 分る, and the fuller okurigana is always the shorter
/// stem. So a kanji is *ranked* by the longest split that found it and
/// *spelled* by the shortest. Both spellings stay in the list, in that order,
/// because the dictionary offers both and a user who wants 曲って should be
/// able to press 変換 once more rather than be told they cannot have it.
///
/// The second rule is worth its bookkeeping on the same 52 words: 43 first
/// and 50 in the first three against longest-first's 42 and 48, and — which
/// the counts understate — 曲がって, 止まります and 分かりません where
/// longest-first alone offers 曲って, 止ります and 分りません.
///
/// Ties after that are the dictionary's own candidate order, which for
/// `SKK-JISYO.L` is roughly commonest first.
///
/// Duplicates are dropped by the finished word, so 「もちます」 — which is
/// 持 through both `もc` and `もt`, because ち is typed `chi` or `ti` — is
/// offered once.
pub fn convert(dictionary: &Dictionary, reading: &str) -> Vec<Conversion> {
    let hits = split(dictionary, reading);
    // Scored once and not inside the comparator: the score of a hit depends
    // on every other hit with the same kanji, so computing it during the sort
    // would be a scan per comparison and — worse — a comparator that reads
    // the thing it is reordering.
    let scores: Vec<(usize, usize)> = hits.iter().map(|hit| score(&hits, &hit.stem)).collect();

    let mut order: Vec<usize> = (0..hits.len()).collect();
    // `sort_by` and not `sort_unstable_by`: the last tie-break is the order
    // the splits were walked in, which is the order [`markers`] lists them,
    // and a sort that reordered equal elements would make ち answer
    // differently depending on how many other splits happened to hit.
    order.sort_by(|&a, &b| {
        // Longest split first: where the word ends.
        scores[b]
            .0
            .cmp(&scores[a].0)
            // Then the dictionary's own candidate order, at that split.
            .then(scores[a].1.cmp(&scores[b].1))
            // Then shortest stem first: the fullest okurigana, which is the
            // spelling 送り仮名の付け方 asks for.
            .then(hits[a].stem_kana.cmp(&hits[b].stem_kana))
    });

    let mut conversions: Vec<Conversion> = Vec::new();
    for hit in order.into_iter().map(|at| &hits[at]) {
        let word = format!("{}{}", hit.stem, hit.okurigana);
        if conversions.iter().any(|seen| seen.word == word) {
            continue;
        }
        conversions.push(Conversion {
            word,
            key: hit.key.clone(),
            stem: hit.stem.clone(),
            okurigana: hit.okurigana.clone(),
            annotation: hit.annotation.clone(),
        });
    }
    conversions
}

/// Which latin letters SKK could have filed an okurigana beginning `okurigana`
/// under.
///
/// Public because it is the one thing in this module that is a fact about
/// SKK's format rather than a choice tOS made, and because #64 needs it to
/// write an okuri-ari key back out to its history file.
///
/// Usually one letter, and two when the kana has two spellings in the romaji
/// tables SKK users type with: ち is `chi` or `ti`, so 待ち is filed under
/// both `まc` and `まt`, and both are in the shipped dictionary; じ is `ji`
/// or `zi`, and 混じる is under `まj` and `まz`. ふ is `hu` or `fu`, and `f`
/// is offered even though `SKK-JISYO.L` never uses it, because a lookup that
/// misses costs one binary search and a user's own dictionary was written by
/// whatever wrote it.
///
/// The small tsu takes the letter of the kana after it, because that is how
/// it is typed: 行った is `Itta`, so the marker is `t` and the entry is
/// `いt /行/`. A lone `っ` at the end of a reading is nothing yet.
///
/// Empty for anything that cannot begin an okurigana — a long vowel mark, a
/// small ゃ, a katakana, a latin letter, a kanji. `ん` is *not* in that list:
/// 読んだ is よ + んだ and the entry is `よn /読/`.
pub fn markers(okurigana: &str) -> &'static [u8] {
    let mut chars = okurigana.chars();
    let Some(kana) = chars.next() else {
        return b"";
    };
    if kana == 'っ' {
        return markers(chars.as_str());
    }
    match kana {
        'あ' => b"a",
        'い' => b"i",
        'う' => b"u",
        'え' => b"e",
        'お' => b"o",
        'か' | 'き' | 'く' | 'け' | 'こ' => b"k",
        'が' | 'ぎ' | 'ぐ' | 'げ' | 'ご' => b"g",
        'さ' | 'し' | 'す' | 'せ' | 'そ' => b"s",
        'ざ' | 'ず' | 'ぜ' | 'ぞ' => b"z",
        'じ' => b"jz",
        'た' | 'つ' | 'て' | 'と' => b"t",
        'ち' => b"ct",
        'だ' | 'ぢ' | 'づ' | 'で' | 'ど' => b"d",
        'な' | 'に' | 'ぬ' | 'ね' | 'の' | 'ん' => b"n",
        'は' | 'ひ' | 'へ' | 'ほ' => b"h",
        'ふ' => b"hf",
        'ば' | 'び' | 'ぶ' | 'べ' | 'ぼ' => b"b",
        'ぱ' | 'ぴ' | 'ぷ' | 'ぺ' | 'ぽ' => b"p",
        'ま' | 'み' | 'む' | 'め' | 'も' => b"m",
        'や' | 'ゆ' | 'よ' => b"y",
        'ら' | 'り' | 'る' | 'れ' | 'ろ' => b"r",
        'わ' | 'を' => b"w",
        _ => b"",
    }
}

/// One split the dictionary confirmed.
struct Hit {
    /// How many kana of the reading the stem took. The score, and the thing
    /// the two rules in [`convert`] disagree about.
    stem_kana: usize,
    /// Where this candidate sat in its entry's list.
    rank: usize,
    key: String,
    stem: String,
    okurigana: String,
    annotation: Option<String>,
}

/// Cut `reading` at every kana and keep the cuts the dictionary agrees with.
///
/// The cut before the first kana is not tried, because a kanji needs at least
/// one kana of reading, and neither is the one past the last, because an
/// okurigana of nothing is an okuri-nasi entry and [`Dictionary::lookup`] has
/// already answered for those. So a reading of n kana costs n-1 cuts and at
/// most twice that many binary searches over 16,000 entries — a few dozen
/// comparisons against a `Vec<u32>` that was built at startup, which is what
/// makes it affordable to do this on every 変換 rather than caching anything.
fn split(dictionary: &Dictionary, reading: &str) -> Vec<Hit> {
    let mut hits = Vec::new();
    // One buffer for every key tried, rather than a `format!` per marker per
    // cut. The stem is re-pushed each time because it grows by one kana each
    // cut; truncating would be the same work with an index to get wrong.
    let mut key = String::new();

    for (stem_kana, (at, _)) in reading.char_indices().enumerate().skip(1) {
        let (stem, okurigana) = reading.split_at(at);
        for &marker in markers(okurigana) {
            key.clear();
            key.push_str(stem);
            key.push(char::from(marker));
            for (rank, candidate) in dictionary.lookup_okuri(&key).into_iter().enumerate() {
                hits.push(Hit {
                    stem_kana,
                    rank,
                    key: key.clone(),
                    stem: candidate.word,
                    okurigana: okurigana.to_string(),
                    annotation: candidate.annotation,
                });
            }
        }
    }
    hits
}

/// How good a kanji is, over every cut that reached it: the longest cut, and
/// the best place it took in that cut's candidate list.
///
/// Per kanji rather than per hit, because the 曲 of `まg` and the 曲 of
/// `まがt` are one word written two ways rather than two words, and the
/// evidence for it being the word that was typed is the best evidence there
/// is for it anywhere.
///
/// A linear scan per hit, which is quadratic in the number of hits. The
/// number of hits is what a candidate window can show: a dozen or two for an
/// everyday word, and 24 was the worst case over a thousand readings taken at
/// random from the shipped `SKK-JISYO.L`. A map keyed by the kanji would be a
/// hash table built and thrown away per keystroke to save a few hundred
/// string comparisons.
fn score(hits: &[Hit], stem: &str) -> (usize, usize) {
    let mut best = (0usize, usize::MAX);
    for hit in hits.iter().filter(|hit| hit.stem == stem) {
        if hit.stem_kana > best.0 {
            best = (hit.stem_kana, hit.rank);
        } else if hit.stem_kana == best.0 {
            best.1 = best.1.min(hit.rank);
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dict::BytesSource;

    /// Real lines from `SKK-JISYO.L`, cut down to the stems these tests turn
    /// on and left in the descending order the file has them in. Nothing here
    /// is invented: every entry and every candidate order is the shipped
    /// dictionary's, so a test that passes here is a test about the file that
    /// is on the ISO.
    const SAMPLE: &str = concat!(
        ";; -*- mode: fundamental; coding: utf-8 -*-\n",
        ";; okuri-ari entries.\n",
        "わたしm /私/\n",
        "もt /持/盛/以/\n",
        "もc /持/保/盛/\n",
        "まがt /曲/紛/擬;<rare>/枉/\n",
        "まg /曲/枉/\n",
        "いt /言/行/入/\n",
        "おくr /送/贈/\n",
        "おk /起/置/於/追/\n",
        "かk /書/掛/欠/駆/\n",
        "かi /買/書/描/\n",
        "ふr /降/振;振り向く/\n",
        "よn /読/呼/\n",
        ";; okuri-nasi entries.\n",
        "かんじ /漢字/幹事/\n",
        "とうきょう /東京/\n",
    );

    fn sample() -> Dictionary {
        Dictionary::open(&BytesSource::new(SAMPLE)).expect("bytes always read")
    }

    fn words(reading: &str) -> Vec<String> {
        convert(&sample(), reading)
            .into_iter()
            .map(|conversion| conversion.word)
            .collect()
    }

    #[test]
    fn an_inflected_verb_is_written_by_cutting_the_reading_where_a_stem_ends() {
        // The whole point of #63, in two assertions: かきます is not a
        // reading anywhere in the file, and 書きます comes out anyway.
        assert!(sample().lookup("かきます").is_empty());
        assert_eq!(
            words("かきます").first().map(String::as_str),
            Some("書きます")
        );
    }

    #[test]
    fn the_okurigana_commits_as_it_was_typed_and_never_goes_near_the_dictionary() {
        let conversion = convert(&sample(), "かきます")
            .into_iter()
            .next()
            .expect("かk is in the sample");
        assert_eq!(conversion.stem, "書");
        assert_eq!(conversion.okurigana, "きます");
        assert_eq!(conversion.word, "書きます");
        assert_eq!(conversion.key, "かk");
    }

    #[test]
    fn the_longest_stem_the_dictionary_confirms_is_offered_before_a_shorter_one() {
        // おくります cuts as お + くります, which `おk /起/` accepts, and as
        // おく + ります, which `おくr /送/` accepts. Two kana of the reading
        // explained beats one.
        let found = words("おくります");
        assert_eq!(found.first().map(String::as_str), Some("送ります"));
        assert!(
            found.iter().position(|word| word == "送ります")
                < found.iter().position(|word| word == "起くります"),
            "the shorter stem should still be offered, further down: {found:?}"
        );
    }

    #[test]
    fn the_fullest_okurigana_is_the_spelling_offered_first_for_the_same_kanji() {
        // 曲 is reachable at both cuts. 送り仮名の付け方 wants 曲がって, and
        // SKK carries 曲って as well because SKK users type it.
        let found = words("まがって");
        assert_eq!(found.first().map(String::as_str), Some("曲がって"));
        assert!(
            found.contains(&"曲って".to_string()),
            "the shorter spelling is not thrown away: {found:?}"
        );
        assert!(
            found.iter().position(|word| word == "曲がって")
                < found.iter().position(|word| word == "曲って"),
            "{found:?}"
        );
    }

    #[test]
    fn a_kanji_is_ranked_by_its_best_cut_and_not_by_the_one_it_is_spelled_from() {
        // 紛 is only at the two-kana cut; 曲 is at both, and is spelled from
        // the one-kana cut. Ranking 曲 by where it is spelled would put
        // 紛って — a word nobody typed — in front of 曲がって.
        let found = words("まがって");
        assert!(
            found.iter().position(|word| word == "曲がって")
                < found.iter().position(|word| word == "紛って"),
            "{found:?}"
        );
    }

    #[test]
    fn a_small_tsu_takes_its_marker_from_the_kana_after_it() {
        // 行った is typed `Itta`, so SKK filed it under い + t and not under
        // anything spelled with っ.
        assert_eq!(markers("った"), b"t");
        assert_eq!(words("いった"), vec!["言った", "行った", "入った"]);
    }

    #[test]
    fn a_kana_with_two_romaji_spellings_is_looked_up_under_both_and_answers_once() {
        // ち is `chi` or `ti`, so 持ち is in the file twice, under もc and もt.
        assert_eq!(markers("ちます"), b"ct");
        let found = words("もちます");
        assert_eq!(found.iter().filter(|word| *word == "持ちます").count(), 1);
        assert_eq!(found.first().map(String::as_str), Some("持ちます"));
        // Both entries' candidates are offered, merged rather than one of the
        // two lookups winning and the other being dropped.
        assert!(found.contains(&"保ちます".to_string()), "{found:?}");
        assert!(found.contains(&"以ちます".to_string()), "{found:?}");
    }

    #[test]
    fn a_syllabic_n_can_begin_an_okurigana_because_that_is_where_read_is_filed() {
        assert_eq!(markers("んだ"), b"n");
        assert_eq!(words("よんだ"), vec!["読んだ", "呼んだ"]);
    }

    #[test]
    fn the_marker_is_the_first_romaji_letter_of_the_okurigana() {
        for (okurigana, expected) in [
            ("きます", &b"k"[..]),
            ("がって", b"g"),
            ("します", b"s"),
            ("じます", b"jz"),
            ("べる", b"b"),
            ("ぷり", b"p"),
            ("いた", b"i"),
            ("えて", b"e"),
            ("ります", b"r"),
            ("ふ", b"hf"),
            ("む", b"m"),
            ("を", b"w"),
            ("づけ", b"d"),
            ("っこい", b"k"),
        ] {
            assert_eq!(markers(okurigana), expected, "{okurigana}");
        }
    }

    #[test]
    fn nothing_that_cannot_begin_an_okurigana_is_a_place_to_cut() {
        // A long vowel mark, a small ゃ, katakana, latin, a digit and a
        // kanji: none of them is a kana SKK could have taken a marker from,
        // so a reading is not cut in front of one.
        for okurigana in ["ー", "ゃく", "カ", "k", "書", "", "っ", "1"] {
            assert_eq!(markers(okurigana), b"", "{okurigana:?}");
        }
    }

    #[test]
    fn a_whole_word_from_the_okuri_nasi_half_is_not_answered_here() {
        // かんじ is 漢字 through `Dictionary::lookup`, and this module must
        // not be a second thing that says so.
        assert!(words("かんじ").is_empty());
        assert!(words("とうきょう").is_empty());
    }

    #[test]
    fn a_reading_too_short_to_cut_converts_to_nothing() {
        assert!(words("").is_empty());
        assert!(words("か").is_empty());
    }

    #[test]
    fn a_reading_no_cut_of_which_is_in_the_dictionary_converts_to_nothing() {
        assert!(words("ぬけている").is_empty());
    }

    #[test]
    fn an_inflection_much_longer_than_its_stem_is_still_one_cut() {
        // 書かせられたくなかった: nine kana of okurigana on one kana of stem,
        // which costs nothing because the okurigana is never looked up.
        assert_eq!(
            words("かかせられたくなかった").first().map(String::as_str),
            Some("書かせられたくなかった")
        );
    }

    #[test]
    fn an_annotation_travels_with_the_conversion_and_is_not_part_of_the_word() {
        let found: Vec<_> = convert(&sample(), "ふります")
            .into_iter()
            .map(|conversion| (conversion.word, conversion.annotation))
            .collect();
        assert_eq!(
            found,
            vec![
                ("降ります".to_string(), None),
                ("振ります".to_string(), Some("振り向く".to_string())),
            ]
        );
    }

    #[test]
    fn a_conversion_names_the_dictionary_line_it_came_from() {
        // What #64 has to write to its history file: the key on the left of
        // the line and the candidate on the right, `かi /書/`.
        let conversion = convert(&sample(), "かいた")
            .into_iter()
            .find(|conversion| conversion.word == "書いた")
            .expect("かi has 書 in it");
        assert_eq!(conversion.key, "かi");
        assert_eq!(conversion.stem, "書");
        assert_eq!(conversion.okurigana, "いた");
    }

    #[test]
    fn an_empty_dictionary_converts_everything_to_nothing() {
        let dictionary = Dictionary::from_bytes(Vec::new());
        assert!(convert(&dictionary, "かきます").is_empty());
    }
}
