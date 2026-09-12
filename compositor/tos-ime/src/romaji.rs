//! Romaji to kana: one table, read three ways, and a carry of a few keys.
//!
//! This is the first half of Japanese input, and it is deliberately the half
//! that can be settled without a screen, a keyboard or a dictionary. A key
//! goes in, kana come out, and everything it knows is in the table below —
//! which is why it is in tree rather than behind a conversion daemon, for the
//! reasoning that put a DEFLATE decoder and a PNG reader in `tos-term`.
//!
//! The table is hiragana only, and katakana is the same table read
//! differently. Hiragana U+3041..=U+3096 and katakana U+30A1..=U+30F6 are the
//! same list of kana in the same order, 0x60 apart, so a second table would be
//! a second thing to keep in step and a second thing to get wrong. Halfwidth
//! katakana is a third reading of it, and the only one that is not a shift,
//! because JIS X 0201 has no voiced kana: が is ｶ and a combining voiced mark.
//!
//! The state machine exists because a key is not always enough to decide. `k`
//! could still become か or きゃ; `n` is ん, but it is also the start of `na`,
//! and the rule everyone states specially — a lone `n` commits only once the
//! next key proves it was not `na` — is not special here. It falls out of one
//! rule that the table shape implies: **an entry that is also the prefix of a
//! longer entry cannot commit until the next key arrives**. `n` is that, and
//! so is `nn`, which is why `minna` is みんな and `konnyaku` is こんにゃく
//! rather than the みんあ and こんやく that a table with a bare `nn` produces:
//! the ten `nna`..`nnyo` entries say what a second `n` before a vowel meant,
//! instead of the machine having a second special case about the letter n.
//!
//! The carry holds what has been typed and not yet decided. It is one or two
//! characters in ordinary use (`ky`, `sh`, `n`) and at most three, which is
//! `xts` on the way to `xtsu`; it is romaji, not kana, so a caller can draw it
//! as it was typed and so switching script mid-word does not disturb it.
//!
//! What this deliberately does not do: no dictionary, no candidates, no I/O,
//! and no opinion about which key turned it on. It does not implement SKK's
//! capital-letter convention (a capital starts a conversion) because whether a
//! capital means that is a question for the layer that owns the modes, and
//! this one answers `KA` and `ka` identically. Digits are not in the table, so
//! they pass through halfwidth, which is what a user typing a number wants.

/// Which script a converter writes.
///
/// Katakana is not a mode of the input method — `docs/design/ime.md` has two
/// modes and this is not one of them — it is how the preedit is read, which is
/// what 無変換 and F7 change.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Kana {
    /// あ, and the table exactly as it is written.
    #[default]
    Hiragana,
    /// ア: the same entries shifted by 0x60.
    Katakana,
    /// ｱ: JIS X 0201 katakana, where a voiced kana becomes two characters and
    /// the fullwidth punctuation becomes its halfwidth counterpart.
    Halfwidth,
}

/// The small tsu a doubled consonant becomes. The table has it too, under
/// `xtsu` and `ltu`, because those are ways of typing it; this is the other
/// way in, and it is a rule about a pair of keys rather than a reading, so it
/// could not have been an entry.
const SOKUON: &str = "っ";

/// Romaji in, kana out, one key at a time.
///
/// Nothing here allocates a table or reads a file, so a converter is cheap
/// enough to keep one per pane if that ever turns out to be the right place
/// for it; today the design puts one on the compositor.
#[derive(Debug, Clone, Default)]
pub struct Converter {
    kana: Kana,
    /// Romaji that has been typed and has not yet decided what it is.
    carry: String,
}

impl Converter {
    /// A converter with an empty carry, writing `kana`.
    #[must_use]
    pub const fn new(kana: Kana) -> Self {
        Self {
            kana,
            carry: String::new(),
        }
    }

    /// Which script the next kana will be written in.
    #[must_use]
    pub const fn kana(&self) -> Kana {
        self.kana
    }

    /// Change the script. The carry is romaji, so it survives the change and
    /// the half-typed syllable lands in the script that is current when it
    /// finishes.
    pub const fn set_kana(&mut self, kana: Kana) {
        self.kana = kana;
    }

    /// What has been typed and not yet decided, as it was typed. A caller
    /// draws this at the end of the preedit; it is never sent anywhere.
    #[must_use]
    pub fn carry(&self) -> &str {
        &self.carry
    }

    /// Whether anything is waiting. A caller with an empty converter and an
    /// empty preedit has nothing to draw and nothing to lose.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.carry.is_empty()
    }

    /// Throw the carry away, for the Escape that abandons a preedit.
    pub fn clear(&mut self) {
        self.carry.clear();
    }

    /// Take one character off the carry, and say whether there was one.
    ///
    /// Backspace has to consume the carry before it consumes kana, or a user
    /// who typed `ky` and changed their mind would delete the syllable in
    /// front of it while `ky` sat there invisible to them. `false` means the
    /// carry was empty and the caller's own text is what backspace meant.
    pub fn backspace(&mut self) -> bool {
        self.carry.pop().is_some()
    }

    /// Feed one key, and get back the kana it settled — usually nothing.
    ///
    /// Case is not part of any entry, so `KA` and `ka` are the same two keys.
    /// A character the table can never use is returned as itself rather than
    /// held: holding it would mean a carry that never drains and a preedit
    /// that stops responding.
    #[must_use = "the kana this settled are the only copy of them"]
    pub fn push(&mut self, ch: char) -> String {
        self.carry.push(ch.to_ascii_lowercase());
        let mut out = String::new();
        while self.step(&mut out) {}
        out
    }

    /// Settle everything that is left, because no further key is coming.
    ///
    /// This is what makes a trailing `n` ん: with no next key to prove it was
    /// `na`, the entry it already matches wins. Romaji that has not reached an
    /// entry at all — the `ky` of an abandoned `kyo` — comes back as the
    /// letters that were typed, since the alternative is silently eating keys
    /// the user can see they pressed.
    #[must_use = "the kana this settled are the only copy of them"]
    pub fn flush(&mut self) -> String {
        let mut out = String::new();
        while !self.carry.is_empty() {
            if let Some(kana) = lookup(&self.carry) {
                self.emit(kana, &mut out);
                self.carry.clear();
            } else {
                self.resolve_dead_end(&mut out);
            }
        }
        out
    }

    /// One decision. Returns whether the carry changed in a way that could
    /// allow another, which is only ever after a dead end has been resolved.
    fn step(&mut self, out: &mut String) -> bool {
        if self.carry.is_empty() {
            return false;
        }
        if let Some(kana) = lookup(&self.carry) {
            // The lone n, and every other entry that is a prefix of a longer
            // one: matching is not enough, the next key decides.
            if extends(&self.carry) {
                return false;
            }
            self.emit(kana, out);
            self.carry.clear();
            return false;
        }
        // Still on the way to something.
        if extends(&self.carry) {
            return false;
        }
        self.resolve_dead_end(out);
        true
    }

    /// The carry is no longer on the way to any entry, so the front of it has
    /// to become something and get out of the way.
    fn resolve_dead_end(&mut self, out: &mut String) {
        let Some(first) = self.carry.chars().next() else {
            return;
        };
        // The longest prefix that is an entry commits. This is `n` in `nk`,
        // and it is the whole of the lone-n rule's second half.
        for len in (1..self.carry.len()).rev() {
            if !self.carry.is_char_boundary(len) {
                continue;
            }
            if let Some(kana) = lookup(&self.carry[..len]) {
                self.emit(kana, out);
                self.carry.replace_range(..len, "");
                return;
            }
        }
        // A doubled consonant is a small tsu and a retry of the second one.
        // `n` is excluded because `nn` is ん — that entry is checked above and
        // never reaches here — and the vowels because `aa` is two あ.
        if is_geminable(first) && self.carry[first.len_utf8()..].starts_with(first) {
            self.emit(SOKUON, out);
            self.carry.replace_range(..first.len_utf8(), "");
            return;
        }
        // Nothing this character can be. Hand it back unchanged.
        out.push(first);
        self.carry.replace_range(..first.len_utf8(), "");
    }

    /// Write one table entry in the script this converter is set to.
    fn emit(&self, hiragana: &str, out: &mut String) {
        match self.kana {
            Kana::Hiragana => out.push_str(hiragana),
            Kana::Katakana => {
                for ch in hiragana.chars() {
                    out.push(katakana_char(ch));
                }
            }
            Kana::Halfwidth => {
                for ch in hiragana.chars() {
                    push_halfwidth(katakana_char(ch), out);
                }
            }
        }
    }
}

/// Convert a whole run of romaji, as if it were typed and then finished.
///
/// The compositor types one key at a time and wants [`Converter`]; this is for
/// the caller that already has the letters, and for tests, where it keeps the
/// expectation on one line.
#[must_use]
pub fn convert(romaji: &str, kana: Kana) -> String {
    let mut converter = Converter::new(kana);
    let mut out = String::new();
    for ch in romaji.chars() {
        out.push_str(&converter.push(ch));
    }
    out.push_str(&converter.flush());
    out
}

/// Read hiragana as katakana. Anything that is not hiragana — the punctuation
/// the table also produces, and any text a caller passes through — is left
/// exactly as it is.
#[must_use]
pub fn to_katakana(hiragana: &str) -> String {
    hiragana.chars().map(katakana_char).collect()
}

/// Read kana as JIS X 0201 halfwidth katakana. Accepts either script, since
/// the first thing it does is make katakana of what it was given.
#[must_use]
pub fn to_halfwidth_katakana(kana: &str) -> String {
    let mut out = String::new();
    for ch in kana.chars() {
        push_halfwidth(katakana_char(ch), &mut out);
    }
    out
}

/// The one place hiragana becomes katakana. `゛` and `゜` (U+309B, U+309C) are
/// not in the shifted range on purpose: they are already common to both
/// scripts, and shifting them would land on `・`.
const fn katakana_char(ch: char) -> char {
    match ch {
        'ぁ'..='ゖ' | 'ゝ' | 'ゞ' => match char::from_u32(ch as u32 + 0x60) {
            Some(katakana) => katakana,
            None => ch,
        },
        _ => ch,
    }
}

/// Write one katakana character as halfwidth.
///
/// A voiced kana becomes two characters here, which is the whole reason this
/// is a function and not a third table: JIS X 0201 has ｶ and `ﾞ` and no ｶﾞ.
/// Kana that JIS X 0201 never had — ヰ, ヱ, the small ヮ and ヵ — stay
/// fullwidth, because a wrong halfwidth guess is worse than a wide character.
fn push_halfwidth(ch: char, out: &mut String) {
    let halfwidth = match ch {
        'ァ' => "ｧ",
        'ア' => "ｱ",
        'ィ' => "ｨ",
        'イ' => "ｲ",
        'ゥ' => "ｩ",
        'ウ' => "ｳ",
        'ェ' => "ｪ",
        'エ' => "ｴ",
        'ォ' => "ｫ",
        'オ' => "ｵ",
        'カ' => "ｶ",
        'ガ' => "ｶﾞ",
        'キ' => "ｷ",
        'ギ' => "ｷﾞ",
        'ク' => "ｸ",
        'グ' => "ｸﾞ",
        'ケ' => "ｹ",
        'ゲ' => "ｹﾞ",
        'コ' => "ｺ",
        'ゴ' => "ｺﾞ",
        'サ' => "ｻ",
        'ザ' => "ｻﾞ",
        'シ' => "ｼ",
        'ジ' => "ｼﾞ",
        'ス' => "ｽ",
        'ズ' => "ｽﾞ",
        'セ' => "ｾ",
        'ゼ' => "ｾﾞ",
        'ソ' => "ｿ",
        'ゾ' => "ｿﾞ",
        'タ' => "ﾀ",
        'ダ' => "ﾀﾞ",
        'チ' => "ﾁ",
        'ヂ' => "ﾁﾞ",
        'ッ' => "ｯ",
        'ツ' => "ﾂ",
        'ヅ' => "ﾂﾞ",
        'テ' => "ﾃ",
        'デ' => "ﾃﾞ",
        'ト' => "ﾄ",
        'ド' => "ﾄﾞ",
        'ナ' => "ﾅ",
        'ニ' => "ﾆ",
        'ヌ' => "ﾇ",
        'ネ' => "ﾈ",
        'ノ' => "ﾉ",
        'ハ' => "ﾊ",
        'バ' => "ﾊﾞ",
        'パ' => "ﾊﾟ",
        'ヒ' => "ﾋ",
        'ビ' => "ﾋﾞ",
        'ピ' => "ﾋﾟ",
        'フ' => "ﾌ",
        'ブ' => "ﾌﾞ",
        'プ' => "ﾌﾟ",
        'ヘ' => "ﾍ",
        'ベ' => "ﾍﾞ",
        'ペ' => "ﾍﾟ",
        'ホ' => "ﾎ",
        'ボ' => "ﾎﾞ",
        'ポ' => "ﾎﾟ",
        'マ' => "ﾏ",
        'ミ' => "ﾐ",
        'ム' => "ﾑ",
        'メ' => "ﾒ",
        'モ' => "ﾓ",
        'ャ' => "ｬ",
        'ヤ' => "ﾔ",
        'ュ' => "ｭ",
        'ユ' => "ﾕ",
        'ョ' => "ｮ",
        'ヨ' => "ﾖ",
        'ラ' => "ﾗ",
        'リ' => "ﾘ",
        'ル' => "ﾙ",
        'レ' => "ﾚ",
        'ロ' => "ﾛ",
        'ワ' => "ﾜ",
        'ヲ' => "ｦ",
        'ン' => "ﾝ",
        'ヴ' => "ｳﾞ",
        '゛' => "ﾞ",
        '゜' => "ﾟ",
        'ー' => "ｰ",
        '。' => "｡",
        '、' => "､",
        '・' => "･",
        '「' => "｢",
        '」' => "｣",
        // The mechanical half of the fullwidth block, which is where every
        // punctuation entry that has no Japanese form of its own came from.
        '！'..='～' => {
            if let Some(ascii) = char::from_u32(ch as u32 - 0xFEE0) {
                out.push(ascii);
            } else {
                out.push(ch);
            }
            return;
        }
        _ => {
            out.push(ch);
            return;
        }
    };
    out.push_str(halfwidth);
}

/// Whether a doubled `ch` is a small tsu rather than two of something.
fn is_geminable(ch: char) -> bool {
    ch.is_ascii_alphabetic() && !matches!(ch, 'a' | 'e' | 'i' | 'o' | 'u' | 'n')
}

/// The kana one complete piece of romaji makes, or nothing.
fn lookup(romaji: &str) -> Option<&'static str> {
    TABLE
        .binary_search_by(|(entry, _)| (*entry).cmp(romaji))
        .ok()
        .map(|index| TABLE[index].1)
}

/// Whether some entry is `romaji` plus at least one more character — whether,
/// in other words, another key could still change what this becomes.
///
/// Entries beginning with `romaji` are contiguous in a sorted table and an
/// exact match sorts first among them, so this is two lookups and no scan.
fn extends(romaji: &str) -> bool {
    let start = TABLE.partition_point(|(entry, _)| *entry < romaji);
    let longer = if TABLE.get(start).is_some_and(|(entry, _)| *entry == romaji) {
        start + 1
    } else {
        start
    };
    TABLE
        .get(longer)
        .is_some_and(|(entry, _)| entry.starts_with(romaji))
}

/// Romaji to hiragana, sorted by romaji so that it can be binary searched.
///
/// Sorted rather than grouped by row, because the order is the index. A
/// `HashMap` would have to be built at run time out of this same list, paying
/// an allocation and then a hash of a two-character string per keystroke to
/// save a handful of comparisons over three hundred entries; the array is
/// `const` and costs nothing. What the sorting loses is the grouping a reader
/// wants, and what it must not lose is the ordering `lookup` and `extends`
/// both depend on — so that is asserted by a test rather than trusted.
///
/// Two conventions are in here at once, because both get typed: the kunrei
/// spellings (`si`, `ti`, `tu`, `hu`, `zi`) and the Hepburn ones (`shi`,
/// `chi`, `tsu`, `fu`, `ji`). `du` is づ and `di` is ぢ, which is what those
/// keys mean to someone typing 「つづく」; `dzu` is absent, because it is a
/// way of writing づ in Latin script rather than a way of typing it, and the
/// keys it would need already spell it. Backtick is deliberately absent: on a
/// JIS keyboard 半角/全角
/// arrives as `KEY_GRAVE`, so it is the key most likely to be the toggle, and
/// a table that claimed it would type `｀` at whoever bound it.
#[rustfmt::skip]
const TABLE: &[(&str, &str)] = &[
    ("!", "！"),
    ("\"", "＂"),
    ("#", "＃"),
    ("$", "＄"),
    ("%", "％"),
    ("&", "＆"),
    ("(", "（"),
    (")", "）"),
    ("*", "＊"),
    ("+", "＋"),
    (",", "、"),
    ("-", "ー"),
    (".", "。"),
    ("/", "・"),
    (":", "："),
    (";", "；"),
    ("<", "＜"),
    ("=", "＝"),
    (">", "＞"),
    ("?", "？"),
    ("@", "＠"),
    ("[", "「"),
    ("\\", "＼"),
    ("]", "」"),
    ("^", "＾"),
    ("_", "＿"),
    ("a", "あ"),
    ("ba", "ば"),
    ("be", "べ"),
    ("bi", "び"),
    ("bo", "ぼ"),
    ("bu", "ぶ"),
    ("bya", "びゃ"),
    ("bye", "びぇ"),
    ("byi", "びぃ"),
    ("byo", "びょ"),
    ("byu", "びゅ"),
    ("cha", "ちゃ"),
    ("che", "ちぇ"),
    ("chi", "ち"),
    ("cho", "ちょ"),
    ("chu", "ちゅ"),
    ("cya", "ちゃ"),
    ("cye", "ちぇ"),
    ("cyi", "ちぃ"),
    ("cyo", "ちょ"),
    ("cyu", "ちゅ"),
    ("da", "だ"),
    ("de", "で"),
    ("dha", "でゃ"),
    ("dhe", "でぇ"),
    ("dhi", "でぃ"),
    ("dho", "でょ"),
    ("dhu", "でゅ"),
    ("di", "ぢ"),
    ("do", "ど"),
    ("du", "づ"),
    ("dwa", "どぁ"),
    ("dwe", "どぇ"),
    ("dwi", "どぃ"),
    ("dwo", "どぉ"),
    ("dwu", "どぅ"),
    ("dya", "ぢゃ"),
    ("dye", "ぢぇ"),
    ("dyi", "ぢぃ"),
    ("dyo", "ぢょ"),
    ("dyu", "ぢゅ"),
    ("e", "え"),
    ("fa", "ふぁ"),
    ("fe", "ふぇ"),
    ("fi", "ふぃ"),
    ("fo", "ふぉ"),
    ("fu", "ふ"),
    ("fya", "ふゃ"),
    ("fye", "ふぇ"),
    ("fyi", "ふぃ"),
    ("fyo", "ふょ"),
    ("fyu", "ふゅ"),
    ("ga", "が"),
    ("ge", "げ"),
    ("gi", "ぎ"),
    ("go", "ご"),
    ("gu", "ぐ"),
    ("gwa", "ぐぁ"),
    ("gwe", "ぐぇ"),
    ("gwi", "ぐぃ"),
    ("gwo", "ぐぉ"),
    ("gwu", "ぐぅ"),
    ("gya", "ぎゃ"),
    ("gye", "ぎぇ"),
    ("gyi", "ぎぃ"),
    ("gyo", "ぎょ"),
    ("gyu", "ぎゅ"),
    ("ha", "は"),
    ("he", "へ"),
    ("hi", "ひ"),
    ("ho", "ほ"),
    ("hu", "ふ"),
    ("hya", "ひゃ"),
    ("hye", "ひぇ"),
    ("hyi", "ひぃ"),
    ("hyo", "ひょ"),
    ("hyu", "ひゅ"),
    ("i", "い"),
    ("ja", "じゃ"),
    ("je", "じぇ"),
    ("ji", "じ"),
    ("jo", "じょ"),
    ("ju", "じゅ"),
    ("jya", "じゃ"),
    ("jye", "じぇ"),
    ("jyi", "じぃ"),
    ("jyo", "じょ"),
    ("jyu", "じゅ"),
    ("ka", "か"),
    ("ke", "け"),
    ("ki", "き"),
    ("ko", "こ"),
    ("ku", "く"),
    ("kwa", "くぁ"),
    ("kwe", "くぇ"),
    ("kwi", "くぃ"),
    ("kwo", "くぉ"),
    ("kwu", "くぅ"),
    ("kya", "きゃ"),
    ("kye", "きぇ"),
    ("kyi", "きぃ"),
    ("kyo", "きょ"),
    ("kyu", "きゅ"),
    ("la", "ぁ"),
    ("le", "ぇ"),
    ("li", "ぃ"),
    ("lka", "ゕ"),
    ("lke", "ゖ"),
    ("lo", "ぉ"),
    ("ltsu", "っ"),
    ("ltu", "っ"),
    ("lu", "ぅ"),
    ("lwa", "ゎ"),
    ("lya", "ゃ"),
    ("lye", "ぇ"),
    ("lyi", "ぃ"),
    ("lyo", "ょ"),
    ("lyu", "ゅ"),
    ("ma", "ま"),
    ("me", "め"),
    ("mi", "み"),
    ("mo", "も"),
    ("mu", "む"),
    ("mya", "みゃ"),
    ("mye", "みぇ"),
    ("myi", "みぃ"),
    ("myo", "みょ"),
    ("myu", "みゅ"),
    ("n", "ん"),
    ("n'", "ん"),
    ("na", "な"),
    ("ne", "ね"),
    ("ni", "に"),
    ("nn", "ん"),
    ("nna", "んな"),
    ("nne", "んね"),
    ("nni", "んに"),
    ("nno", "んの"),
    ("nnu", "んぬ"),
    ("nnya", "んにゃ"),
    ("nnye", "んにぇ"),
    ("nnyi", "んにぃ"),
    ("nnyo", "んにょ"),
    ("nnyu", "んにゅ"),
    ("no", "の"),
    ("nu", "ぬ"),
    ("nya", "にゃ"),
    ("nye", "にぇ"),
    ("nyi", "にぃ"),
    ("nyo", "にょ"),
    ("nyu", "にゅ"),
    ("o", "お"),
    ("pa", "ぱ"),
    ("pe", "ぺ"),
    ("pi", "ぴ"),
    ("po", "ぽ"),
    ("pu", "ぷ"),
    ("pya", "ぴゃ"),
    ("pye", "ぴぇ"),
    ("pyi", "ぴぃ"),
    ("pyo", "ぴょ"),
    ("pyu", "ぴゅ"),
    ("qa", "くぁ"),
    ("qe", "くぇ"),
    ("qi", "くぃ"),
    ("qo", "くぉ"),
    ("qu", "く"),
    ("qya", "くゃ"),
    ("qye", "くぇ"),
    ("qyi", "くぃ"),
    ("qyo", "くょ"),
    ("qyu", "くゅ"),
    ("ra", "ら"),
    ("re", "れ"),
    ("ri", "り"),
    ("ro", "ろ"),
    ("ru", "る"),
    ("rya", "りゃ"),
    ("rye", "りぇ"),
    ("ryi", "りぃ"),
    ("ryo", "りょ"),
    ("ryu", "りゅ"),
    ("sa", "さ"),
    ("se", "せ"),
    ("sha", "しゃ"),
    ("she", "しぇ"),
    ("shi", "し"),
    ("sho", "しょ"),
    ("shu", "しゅ"),
    ("si", "し"),
    ("so", "そ"),
    ("su", "す"),
    ("swa", "すぁ"),
    ("swe", "すぇ"),
    ("swi", "すぃ"),
    ("swo", "すぉ"),
    ("swu", "すぅ"),
    ("sya", "しゃ"),
    ("sye", "しぇ"),
    ("syi", "しぃ"),
    ("syo", "しょ"),
    ("syu", "しゅ"),
    ("ta", "た"),
    ("te", "て"),
    ("tha", "てゃ"),
    ("the", "てぇ"),
    ("thi", "てぃ"),
    ("tho", "てょ"),
    ("thu", "てゅ"),
    ("ti", "ち"),
    ("to", "と"),
    ("tsa", "つぁ"),
    ("tse", "つぇ"),
    ("tsi", "つぃ"),
    ("tso", "つぉ"),
    ("tsu", "つ"),
    ("tu", "つ"),
    ("twa", "とぁ"),
    ("twe", "とぇ"),
    ("twi", "とぃ"),
    ("two", "とぉ"),
    ("twu", "とぅ"),
    ("tya", "ちゃ"),
    ("tye", "ちぇ"),
    ("tyi", "ちぃ"),
    ("tyo", "ちょ"),
    ("tyu", "ちゅ"),
    ("u", "う"),
    ("va", "ゔぁ"),
    ("ve", "ゔぇ"),
    ("vi", "ゔぃ"),
    ("vo", "ゔぉ"),
    ("vu", "ゔ"),
    ("vya", "ゔゃ"),
    ("vye", "ゔぇ"),
    ("vyi", "ゔぃ"),
    ("vyo", "ゔょ"),
    ("vyu", "ゔゅ"),
    ("wa", "わ"),
    ("we", "ゑ"),
    ("wha", "うぁ"),
    ("whe", "うぇ"),
    ("whi", "うぃ"),
    ("who", "うぉ"),
    ("whu", "う"),
    ("wi", "ゐ"),
    ("wo", "を"),
    ("wu", "う"),
    ("xa", "ぁ"),
    ("xe", "ぇ"),
    ("xi", "ぃ"),
    ("xka", "ゕ"),
    ("xke", "ゖ"),
    ("xn", "ん"),
    ("xo", "ぉ"),
    ("xtsu", "っ"),
    ("xtu", "っ"),
    ("xu", "ぅ"),
    ("xwa", "ゎ"),
    ("xya", "ゃ"),
    ("xye", "ぇ"),
    ("xyi", "ぃ"),
    ("xyo", "ょ"),
    ("xyu", "ゅ"),
    ("ya", "や"),
    ("ye", "いぇ"),
    ("yo", "よ"),
    ("yu", "ゆ"),
    ("za", "ざ"),
    ("ze", "ぜ"),
    ("zi", "じ"),
    ("zo", "ぞ"),
    ("zu", "ず"),
    ("zya", "じゃ"),
    ("zye", "じぇ"),
    ("zyi", "じぃ"),
    ("zyo", "じょ"),
    ("zyu", "じゅ"),
    ("{", "｛"),
    ("|", "｜"),
    ("}", "｝"),
    ("~", "～"),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Typing a whole word in hiragana, which is what most of these check.
    fn hiragana(romaji: &str) -> String {
        convert(romaji, Kana::Hiragana)
    }

    #[test]
    fn ka_is_one_kana_and_kya_is_one_syllable() {
        assert_eq!(hiragana("ka"), "か");
        assert_eq!(hiragana("kya"), "きゃ");
        assert_eq!(hiragana("kyou"), "きょう");
        assert_eq!(hiragana("aiueo"), "あいうえお");
    }

    #[test]
    fn a_doubled_consonant_is_a_small_tsu() {
        assert_eq!(hiragana("kka"), "っか");
        assert_eq!(hiragana("matte"), "まって");
        assert_eq!(hiragana("gakkou"), "がっこう");
        assert_eq!(hiragana("nippon"), "にっぽん");
    }

    #[test]
    fn gemination_survives_the_two_letter_spellings() {
        // `ssh` and `cch` are the cases where the doubled letter is not the
        // consonant the kana is named after.
        assert_eq!(hiragana("zasshi"), "ざっし");
        assert_eq!(hiragana("issho"), "いっしょ");
        assert_eq!(hiragana("kocchi"), "こっち");
        assert_eq!(hiragana("nattou"), "なっとう");
    }

    #[test]
    fn a_doubled_vowel_is_two_vowels_and_a_doubled_n_is_not_a_small_tsu() {
        assert_eq!(hiragana("aa"), "ああ");
        assert_eq!(hiragana("oo"), "おお");
        assert_eq!(hiragana("nn"), "ん");
    }

    #[test]
    fn a_lone_n_is_not_committed_until_the_next_key_proves_it() {
        let mut converter = Converter::new(Kana::Hiragana);
        assert_eq!(converter.push('n'), "");
        assert_eq!(converter.carry(), "n");
        // `na` was still possible, so nothing was written; now it is decided.
        assert_eq!(converter.push('a'), "な");
        assert!(converter.is_empty());
    }

    #[test]
    fn a_lone_n_is_committed_by_the_key_that_rules_na_out() {
        let mut converter = Converter::new(Kana::Hiragana);
        assert_eq!(converter.push('n'), "");
        assert_eq!(converter.push('k'), "ん");
        // The key that decided it is kept: it is the start of the next kana.
        assert_eq!(converter.carry(), "k");
        assert_eq!(converter.push('a'), "か");
        assert_eq!(hiragana("kanji"), "かんじ");
    }

    #[test]
    fn a_lone_n_at_the_end_of_input_is_committed_by_the_flush() {
        let mut converter = Converter::new(Kana::Hiragana);
        assert_eq!(converter.push('n'), "");
        assert_eq!(converter.flush(), "ん");
        assert!(converter.is_empty());
        assert_eq!(hiragana("nihon"), "にほん");
    }

    #[test]
    fn nn_is_one_n_but_a_vowel_after_it_makes_it_a_na_row_kana() {
        assert_eq!(hiragana("nn"), "ん");
        assert_eq!(hiragana("minna"), "みんな");
        assert_eq!(hiragana("konnichiwa"), "こんにちわ");
        assert_eq!(hiragana("konnyaku"), "こんにゃく");
        assert_eq!(hiragana("sannen"), "さんねん");
    }

    #[test]
    fn nya_is_one_syllable_and_not_an_n_and_a_ya() {
        assert_eq!(hiragana("nya"), "にゃ");
        assert_eq!(hiragana("ninja"), "にんじゃ");
    }

    #[test]
    fn an_apostrophe_ends_an_n_that_the_next_vowel_would_have_taken() {
        assert_eq!(hiragana("sen'i"), "せんい");
        assert_eq!(hiragana("seni"), "せに");
    }

    #[test]
    fn the_kunrei_and_hepburn_spellings_reach_the_same_kana() {
        for (kunrei, hepburn, kana) in [
            ("si", "shi", "し"),
            ("ti", "chi", "ち"),
            ("tu", "tsu", "つ"),
            ("hu", "fu", "ふ"),
            ("zi", "ji", "じ"),
            ("sya", "sha", "しゃ"),
        ] {
            assert_eq!(hiragana(kunrei), kana, "{kunrei} is not {kana}");
            assert_eq!(hiragana(hepburn), kana, "{hepburn} is not {kana}");
        }
        assert_eq!(hiragana("du"), "づ");
        assert_eq!(hiragana("di"), "ぢ");
        assert_eq!(hiragana("wo"), "を");
    }

    #[test]
    fn the_loanword_rows_are_a_consonant_and_a_small_kana() {
        assert_eq!(hiragana("fa"), "ふぁ");
        assert_eq!(hiragana("va"), "ゔぁ");
        assert_eq!(hiragana("tsa"), "つぁ");
        assert_eq!(hiragana("she"), "しぇ");
        assert_eq!(hiragana("che"), "ちぇ");
        assert_eq!(hiragana("je"), "じぇ");
        assert_eq!(hiragana("wi"), "ゐ");
        assert_eq!(hiragana("whi"), "うぃ");
        assert_eq!(hiragana("kwa"), "くぁ");
        assert_eq!(hiragana("twu"), "とぅ");
        // `th` and `dh` take the small y rather than the small vowel, because
        // `two` and `dwu` already spell とぅ and どぅ and nothing else spells
        // でゅ: 「ティ」 is thi and 「デュオ」 is dhuo.
        assert_eq!(hiragana("thi"), "てぃ");
        assert_eq!(hiragana("thu"), "てゅ");
        assert_eq!(hiragana("dhi"), "でぃ");
        assert_eq!(hiragana("dhuo"), "でゅお");
    }

    #[test]
    fn the_small_kana_are_reachable_on_both_of_their_prefixes() {
        assert_eq!(hiragana("xtsu"), "っ");
        assert_eq!(hiragana("xtu"), "っ");
        assert_eq!(hiragana("ltsu"), "っ");
        assert_eq!(hiragana("ltu"), "っ");
        assert_eq!(hiragana("xa"), "ぁ");
        assert_eq!(hiragana("la"), "ぁ");
        assert_eq!(hiragana("xya"), "ゃ");
        assert_eq!(hiragana("xke"), "ゖ");
    }

    #[test]
    fn uppercase_types_the_same_kana_as_lowercase() {
        assert_eq!(hiragana("KA"), "か");
        assert_eq!(hiragana("KyA"), "きゃ");
        assert_eq!(hiragana("NIHON"), "にほん");
    }

    #[test]
    fn punctuation_is_the_japanese_form_of_the_key_that_was_pressed() {
        assert_eq!(hiragana("."), "。");
        assert_eq!(hiragana(","), "、");
        assert_eq!(hiragana("/"), "・");
        assert_eq!(hiragana("-"), "ー");
        assert_eq!(hiragana("[]"), "「」");
        assert_eq!(hiragana("!?"), "！？");
        assert_eq!(hiragana("ka."), "か。");
        // Everything without a Japanese form of its own is the fullwidth of
        // the key, which is exactly 0xFEE0 above the ASCII.
        assert_eq!(hiragana("~;:"), "～；：");
    }

    #[test]
    fn halfwidth_reads_the_punctuation_back_at_the_other_width() {
        assert_eq!(convert(".", Kana::Halfwidth), "｡");
        assert_eq!(convert(",", Kana::Halfwidth), "､");
        assert_eq!(convert("/", Kana::Halfwidth), "･");
        assert_eq!(convert("-", Kana::Halfwidth), "ｰ");
        assert_eq!(convert("[]", Kana::Halfwidth), "｢｣");
        assert_eq!(convert("!?", Kana::Halfwidth), "!?");
        assert_eq!(convert("~;:", Kana::Halfwidth), "~;:");
    }

    #[test]
    fn katakana_is_the_same_table_read_differently() {
        for word in ["kanji", "kyou", "gakkou", "nippon", "minna", "ka."] {
            assert_eq!(
                convert(word, Kana::Katakana),
                to_katakana(&hiragana(word)),
                "{word} did not read the same in both scripts",
            );
        }
        assert_eq!(convert("ko-hi-", Kana::Katakana), "コーヒー");
    }

    #[test]
    fn halfwidth_katakana_splits_a_voiced_kana_into_two_characters() {
        assert_eq!(convert("ganbatte", Kana::Halfwidth), "ｶﾞﾝﾊﾞｯﾃ");
        assert_eq!(convert("ko-hi-", Kana::Halfwidth), "ｺｰﾋｰ");
        assert_eq!(to_halfwidth_katakana("ぱ"), "ﾊﾟ");
        assert_eq!(to_halfwidth_katakana("ヴ"), "ｳﾞ");
        // JIS X 0201 never had these, so they stay wide rather than be guessed.
        assert_eq!(to_halfwidth_katakana("ゐゑ"), "ヰヱ");
    }

    #[test]
    fn katakana_leaves_alone_what_is_not_hiragana() {
        assert_eq!(to_katakana("あ、A。"), "ア、A。");
        // Shifting the voiced marks would land them on a middle dot.
        assert_eq!(to_katakana("゛゜"), "゛゜");
    }

    #[test]
    fn a_half_typed_syllable_is_visible_to_the_caller_and_comes_back_as_letters() {
        let mut converter = Converter::new(Kana::Hiragana);
        assert_eq!(converter.push('k'), "");
        assert_eq!(converter.push('y'), "");
        assert_eq!(converter.carry(), "ky");
        assert_eq!(converter.flush(), "ky");
        assert!(converter.is_empty());
        // A small tsu is written the moment the second key arrives, so an
        // abandoned `kk` keeps it and hands back the letter that made it.
        assert_eq!(hiragana("kk"), "っk");
    }

    #[test]
    fn backspace_eats_the_carry_before_it_eats_anything_else() {
        let mut converter = Converter::new(Kana::Hiragana);
        assert_eq!(converter.push('k'), "");
        assert_eq!(converter.push('y'), "");
        assert!(converter.backspace());
        assert_eq!(converter.carry(), "k");
        assert!(converter.backspace());
        // Nothing left to take, so the caller's own text is what was meant.
        assert!(!converter.backspace());
        assert_eq!(converter.push('a'), "あ");
    }

    #[test]
    fn clearing_abandons_the_carry_without_writing_it() {
        let mut converter = Converter::new(Kana::Hiragana);
        assert_eq!(converter.push('n'), "");
        converter.clear();
        assert!(converter.is_empty());
        assert_eq!(converter.flush(), "");
    }

    #[test]
    fn the_script_can_change_under_a_half_typed_syllable() {
        let mut converter = Converter::new(Kana::Hiragana);
        assert_eq!(converter.push('k'), "");
        converter.set_kana(Kana::Katakana);
        assert_eq!(converter.kana(), Kana::Katakana);
        // The carry is romaji, so it survives and lands in the new script.
        assert_eq!(converter.push('a'), "カ");
    }

    #[test]
    fn a_key_the_table_can_never_use_is_handed_back_unchanged() {
        assert_eq!(hiragana("1"), "1");
        assert_eq!(hiragana("ka1"), "か1");
        assert_eq!(hiragana(" "), " ");
        // `q` is the start of several entries, so it waits and then gives up.
        assert_eq!(hiragana("q"), "q");
    }

    #[test]
    fn every_entry_is_reachable_by_typing_the_romaji_that_names_it() {
        for (romaji, kana) in TABLE {
            assert_eq!(
                &hiragana(romaji),
                kana,
                "typing {romaji} did not produce its own entry",
            );
        }
    }

    #[test]
    fn every_entry_reads_as_katakana_without_a_second_table() {
        for (romaji, kana) in TABLE {
            assert_eq!(
                convert(romaji, Kana::Katakana),
                to_katakana(kana),
                "{romaji} disagreed between the two readings",
            );
            assert_eq!(
                convert(romaji, Kana::Halfwidth),
                to_halfwidth_katakana(kana),
                "{romaji} disagreed at halfwidth",
            );
        }
    }

    #[test]
    fn the_table_is_sorted_and_has_no_entry_twice() {
        // The binary search and the contiguity `extends` relies on are both
        // this property, so it is asserted rather than assumed.
        for pair in TABLE.windows(2) {
            assert!(
                pair[0].0 < pair[1].0,
                "{} and {} are out of order",
                pair[0].0,
                pair[1].0,
            );
        }
        assert!(TABLE.len() > 250, "the table lost most of itself");
        assert!(TABLE
            .iter()
            .all(|(romaji, kana)| !romaji.is_empty() && !kana.is_empty()));
    }

    #[test]
    fn a_word_is_the_same_whether_it_arrives_at_once_or_key_by_key() {
        let mut converter = Converter::new(Kana::Hiragana);
        let mut out = String::new();
        for ch in "konnichiwa".chars() {
            out.push_str(&converter.push(ch));
        }
        out.push_str(&converter.flush());
        assert_eq!(out, hiragana("konnichiwa"));
    }
}
