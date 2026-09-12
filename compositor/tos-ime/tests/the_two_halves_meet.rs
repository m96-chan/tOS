//! The seam between the two halves, which neither half's own tests cross.
//!
//! `romaji` is decided by its tests and `dict` is decided by its tests, and
//! both passed while nothing checked that the kana one produces is the kana
//! the other is keyed by. That is the shape of bug this repository has been
//! bitten by before: two units that agree with their own tests and not with
//! each other.

use tos_ime::dict::{BytesSource, Dictionary};
use tos_ime::romaji::{convert, Kana};

/// A dictionary in the format the ISO ships, headers and all.
fn dictionary() -> Dictionary {
    Dictionary::open(&BytesSource::new(
        ";; -*- mode: fundamental; coding: euc-jp -*-\n\
         ;; okuri-ari entries.\n\
         をs /惜/\n\
         ;; okuri-nasi entries.\n\
         かんじ /漢字/感じ/幹事/\n\
         かんれい /慣例/寒冷/管領/艦齢/\n\
         こーひー /珈琲/\n\
         にほんご /日本語/\n",
    ))
    .expect("the dictionary should open")
}

fn words(dictionary: &Dictionary, reading: &str) -> Vec<String> {
    dictionary
        .lookup(reading)
        .into_iter()
        .map(|candidate| candidate.word)
        .collect()
}

#[test]
fn romaji_typed_at_the_keyboard_finds_the_word_in_the_dictionary() {
    let dictionary = dictionary();
    for (typed, expected_kana, first_word) in [
        ("kanji", "かんじ", "漢字"),
        ("kanrei", "かんれい", "慣例"),
        ("nihongo", "にほんご", "日本語"),
    ] {
        let kana = convert(typed, Kana::Hiragana);
        assert_eq!(kana, expected_kana, "{typed} should type {expected_kana}");
        let found = words(&dictionary, &kana);
        assert_eq!(
            found.first().map(String::as_str),
            Some(first_word),
            "typing {typed} gave {kana}, which looked up as {found:?}"
        );
    }
}

#[test]
fn a_reading_with_a_long_vowel_survives_both_halves() {
    // iconv moves ー past あ, so the shipped file is not sorted as UTF-8 and a
    // binary search over it as it lies answers "no candidates" for this word.
    let dictionary = dictionary();
    let kana = convert("ko-hi-", Kana::Hiragana);
    assert_eq!(
        kana, "こーひー",
        "the long vowel mark has to come out of romaji"
    );
    assert_eq!(words(&dictionary, &kana), vec!["珈琲".to_string()]);
}

#[test]
fn the_okuri_ari_half_is_not_offered_as_a_reading() {
    // The first non-comment line of the shipped SKK-JISYO.L is `をs /惜/`.
    // Indexing it would make a latin-suffixed reading answerable.
    let dictionary = dictionary();
    assert!(words(&dictionary, "をs").is_empty());
    assert!(words(&dictionary, "を").is_empty());
}

#[test]
fn an_annotation_is_not_part_of_the_word_that_commits() {
    let dictionary = Dictionary::open(&BytesSource::new(
        ";; okuri-nasi entries.\nきしゃ /貴社;your company/記者;reporter/\n",
    ))
    .expect("the dictionary should open");
    assert_eq!(words(&dictionary, "きしゃ"), vec!["貴社", "記者"]);
    let annotations: Vec<_> = dictionary
        .lookup("きしゃ")
        .into_iter()
        .map(|candidate| candidate.annotation)
        .collect();
    assert_eq!(
        annotations,
        vec![
            Some("your company".to_string()),
            Some("reporter".to_string())
        ]
    );
}
