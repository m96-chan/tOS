//! The SHA-512 crypt scheme — the `$6$` lines a Unix shadow file holds.
//!
//! The scheme is Ulrich Drepper's, specified at
//! <https://www.akkadia.org/drepper/SHA-crypt.txt>, and the step numbers in
//! the comments below are that document's. It is deliberately fiddly: the
//! password and the salt are folded into a digest in an order that depends on
//! the length of the password and on the first byte of an intermediate
//! digest, and then a loop runs five thousand times by default so that
//! guessing costs five thousand hashes instead of one. None of it is
//! cryptographically clever and all of it has to be exactly right, which is
//! why the reason this is safe to write by hand is not care but vectors:
//! `tests/vectors.rs` runs all seven of the ones the specification publishes.
//!
//! Everything here is a pure function over bytes. Nothing in this module
//! opens a file or asks the clock; [`crate::salt`] is the only part of the
//! crate that touches the machine.
//!
//! ## Reading a line off disk
//!
//! [`Hash::parse`] is the part that faces a file, so it is the part written
//! defensively. `/etc/shadow` is what stands between a locked screen and
//! the session behind it, and the failure that matters is not a panic — it is
//! a malformed line that quietly behaves like a valid one. So the parser
//! refuses anything it is not certain of rather than repairing it, and it
//! returns [`CryptError`] rather than `false`, so that a caller can tell "this
//! password is wrong" from "there is no usable credential here" and act
//! differently. The screen lock does: a wrong password keeps the prompt, a
//! broken credential file is a different message entirely.
//!
//! The strictness has a second reason. `crypt(3)` verifies by re-hashing and
//! comparing the *whole* output string, so a line whose rounds count is out of
//! range, or spelled with a leading zero, or whose salt runs past sixteen
//! characters, can never verify there — the value it re-hashes with is not the
//! value printed back. Rejecting those lines here therefore loses nothing that
//! any other implementation would have accepted, and it keeps tOS from being
//! the one implementation where a password has two spellings.

use crate::sha512::{Sha512, DIGEST_LEN};

/// The rounds a `$6$` line runs when it does not say.
pub const DEFAULT_ROUNDS: u32 = 5_000;

/// The fewest rounds the scheme allows. A smaller `rounds=` is raised to this.
pub const MIN_ROUNDS: u32 = 1_000;

/// The most rounds the scheme allows. A larger `rounds=` is lowered to this.
pub const MAX_ROUNDS: u32 = 999_999_999;

/// The longest salt the scheme uses. Anything past this is ignored, so tOS
/// writes exactly this many characters and refuses to read more.
pub const SALT_MAX: usize = 16;

/// The length of the encoded digest, in characters.
pub const DIGEST_CHARS: usize = 86;

/// The alphabet crypt has always used. Note that it is not the base64 of
/// RFC 4648: the ordering is different and there is no padding.
const ALPHABET: &[u8; 64] = b"./0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// The order the sixty-four digest bytes are read in when encoding, three at
/// a time, most significant first. It is a permutation and not a sequence,
/// and it is transcribed from the table in the specification; there is no
/// rule behind it to check it against, so [`tests::the_shuffle_is_a_permutation`]
/// checks the only property it must have.
const SHUFFLE: [[usize; 3]; 21] = [
    [0, 21, 42],
    [22, 43, 1],
    [44, 2, 23],
    [3, 24, 45],
    [25, 46, 4],
    [47, 5, 26],
    [6, 27, 48],
    [28, 49, 7],
    [50, 8, 29],
    [9, 30, 51],
    [31, 52, 10],
    [53, 11, 32],
    [12, 33, 54],
    [34, 55, 13],
    [56, 14, 35],
    [15, 36, 57],
    [37, 58, 16],
    [59, 17, 38],
    [18, 39, 60],
    [40, 61, 19],
    [62, 20, 41],
];

/// Why a stored hash could not be read.
///
/// The variants exist so a caller can say something useful about a credential
/// file it cannot use. They are all "this line is not a `$6$` hash"; none of
/// them means "wrong password".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CryptError {
    /// The line does not begin with `$6$`. It may well be a perfectly good
    /// hash of some other scheme — `$y$` yescrypt is what Debian writes — but
    /// it is not one this can check.
    NotSha512,
    /// A field the line promised is not there: no `$` after the rounds, or no
    /// `$` between the salt and the digest.
    Truncated,
    /// `rounds=` followed by something that is not a plain decimal number
    /// within the range the scheme allows.
    BadRounds,
    /// A salt longer than sixteen characters, or one holding a byte that
    /// cannot appear in the file that holds the line.
    BadSalt,
    /// The digest is not exactly eighty-six characters of the crypt alphabet.
    BadDigest,
}

impl std::fmt::Display for CryptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            CryptError::NotSha512 => "not a $6$ SHA-512 crypt hash",
            CryptError::Truncated => "truncated crypt hash",
            CryptError::BadRounds => "crypt rounds out of range or not a number",
            CryptError::BadSalt => "corrupt crypt salt",
            CryptError::BadDigest => "corrupt crypt digest",
        };
        f.write_str(text)
    }
}

impl std::error::Error for CryptError {}

/// The three things a `$6$` line says, once it has been checked.
///
/// It borrows the line rather than copying out of it; a caller that read the
/// credential file already owns the string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hash<'a> {
    rounds: u32,
    explicit_rounds: bool,
    salt: &'a str,
    digest: &'a str,
}

impl<'a> Hash<'a> {
    /// Read a `$6$` line, checking every field.
    ///
    /// The line is one hash and nothing else: no trailing newline, and no
    /// surrounding colon-separated fields. Whitespace is not a character the
    /// crypt alphabet has, so a line that still has its terminator on it is an
    /// error rather than a quiet trim — a caller that cannot be bothered to
    /// say where the hash ends is a caller that has not checked what else is
    /// on the line.
    pub fn parse(line: &'a str) -> Result<Self, CryptError> {
        let rest = line.strip_prefix("$6$").ok_or(CryptError::NotSha512)?;

        let (rounds, explicit_rounds, rest) = match rest.strip_prefix("rounds=") {
            Some(after) => {
                let (digits, rest) = after.split_once('$').ok_or(CryptError::Truncated)?;
                (parse_rounds(digits)?, true, rest)
            }
            None => (DEFAULT_ROUNDS, false, rest),
        };

        let (salt, digest) = rest.split_once('$').ok_or(CryptError::Truncated)?;
        if salt.len() > SALT_MAX || !salt.bytes().all(is_salt_byte) {
            return Err(CryptError::BadSalt);
        }
        if digest.len() != DIGEST_CHARS || !digest.bytes().all(|b| ALPHABET.contains(&b)) {
            return Err(CryptError::BadDigest);
        }

        Ok(Hash {
            rounds,
            explicit_rounds,
            salt,
            digest,
        })
    }

    /// How many rounds this hash was computed with.
    ///
    /// Worth looking at before verifying against a line that came from
    /// somewhere other than tOS: the scheme permits up to `999_999_999`, and
    /// the cost of checking a password is linear in this. On a file that is
    /// mode 0600 and owned by root that is not an attack — whoever can write
    /// it can do worse — but it is a way for a hand-edited line to make an
    /// unlock appear to hang.
    pub fn rounds(self) -> u32 {
        self.rounds
    }

    /// Whether the line carried a `rounds=` field, as opposed to meaning the
    /// default by saying nothing.
    pub fn explicit_rounds(self) -> bool {
        self.explicit_rounds
    }

    /// The salt, without its delimiters.
    pub fn salt(self) -> &'a str {
        self.salt
    }

    /// The encoded digest, without its delimiter.
    pub fn digest(self) -> &'a str {
        self.digest
    }
}

/// `rounds=` accepts a plain decimal number in range and nothing else.
///
/// In particular a leading zero is refused even though it parses: `crypt(3)`
/// prints the number back without it, so `rounds=05000` is a line that
/// implementation can never verify, and accepting it here would give one
/// password two hashes that both work.
fn parse_rounds(digits: &str) -> Result<u32, CryptError> {
    // MAX_ROUNDS has nine digits, so this bound both rejects the absurd and
    // keeps the parse below from ever overflowing.
    if digits.is_empty() || digits.len() > 9 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(CryptError::BadRounds);
    }
    if digits.starts_with('0') {
        return Err(CryptError::BadRounds);
    }
    let rounds: u32 = digits.parse().map_err(|_| CryptError::BadRounds)?;
    if !(MIN_ROUNDS..=MAX_ROUNDS).contains(&rounds) {
        return Err(CryptError::BadRounds);
    }
    Ok(rounds)
}

/// Which bytes a stored salt may hold.
///
/// The scheme itself only stops at `$`, but the salt has to survive being
/// written to a file and read back: `:` is the field separator of every
/// shadow-style file there is, and a control character or a byte above ASCII
/// would make the line unprintable and its length ambiguous. Nothing that
/// generates one of these hashes produces such a salt, so refusing them costs
/// nothing.
fn is_salt_byte(b: u8) -> bool {
    b.is_ascii_graphic() && b != b'$' && b != b':'
}

/// Hash a password with the default number of rounds.
///
/// The salt is cut down by [`trim_salt`] first, so the result is always a
/// complete `$6$` line that [`Hash::parse`] will read back. There is no
/// `rounds=` field on it, which is what the default means.
pub fn hash(password: &[u8], salt: &[u8]) -> String {
    format_hash(password, salt, DEFAULT_ROUNDS, false)
}

/// Hash a password with an explicit number of rounds.
///
/// A count outside the scheme's range is pulled into it, and the line says the
/// number that was actually used rather than the one that was asked for —
/// `rounds=10` produces a `rounds=1000` line, which is the seventh published
/// vector.
pub fn hash_with_rounds(password: &[u8], salt: &[u8], rounds: u32) -> String {
    format_hash(password, salt, clamp_rounds(rounds), true)
}

/// A round count the scheme will accept, which is the one it would have used
/// anyway: `crypt(3)` silently pulls the number into range rather than
/// refusing it.
fn clamp_rounds(rounds: u32) -> u32 {
    rounds.clamp(MIN_ROUNDS, MAX_ROUNDS)
}

fn format_hash(password: &[u8], salt: &[u8], rounds: u32, explicit_rounds: bool) -> String {
    let salt = trim_salt(salt);
    let digest = encode(&digest(password, salt, rounds));
    let mut out = String::with_capacity(32 + salt.len() + digest.len());
    out.push_str("$6$");
    if explicit_rounds {
        out.push_str("rounds=");
        out.push_str(&rounds.to_string());
        out.push('$');
    }
    // `trim_salt` has already thrown away anything that is not printable
    // ASCII, so each byte is its own character and this is not a lossy
    // conversion.
    out.extend(salt.iter().map(|&b| b as char));
    out.push('$');
    out.push_str(&digest);
    out
}

/// Check a password against a stored `$6$` line.
///
/// `Ok(false)` means the password is wrong. An `Err` means the line is not a
/// hash this can check at all, which is a different thing and deserves a
/// different answer from the caller.
pub fn verify(password: &[u8], stored: &str) -> Result<bool, CryptError> {
    let parsed = Hash::parse(stored)?;
    let computed = encode(&digest(password, parsed.salt.as_bytes(), parsed.rounds));
    Ok(constant_time_eq(
        computed.as_bytes(),
        parsed.digest.as_bytes(),
    ))
}

/// Compare two byte strings without letting the time taken say where they
/// first differ.
///
/// What this buys: the lock takes the same time to reject every wrong
/// password, so an attacker cannot recover the stored digest one character at
/// a time by timing rejections and keeping whichever guess was slowest. That
/// attack is real against the obvious `==`, which stops at the first
/// difference.
///
/// What it does not buy, and it is worth being blunt about how much that is:
///
/// - It says nothing about the time spent *computing* the digest, which is the
///   overwhelming majority of the work and which varies with the length of the
///   password typed, because the scheme's inner loop feeds the password in on
///   most of its rounds. The length of a password is not a secret this can
///   keep.
/// - It does not hide the lengths. They are compared first, and not in
///   constant time — but both sides are always eighty-six characters here, one
///   because the encoder produced it and the other because the parser insisted
///   on it.
/// - It is a property of this loop, not of the machine. Cache behaviour,
///   branch prediction and whatever the optimiser decides are outside what
///   source code can promise; [`std::hint::black_box`] discourages the
///   optimiser from turning the accumulation back into an early return, but it
///   is a hint and not a guarantee.
/// - It protects nothing at all from someone who can read the file. The
///   stored digest is on the same disk as the session it guards, mode 0600 and
///   owned by root. This is a defence against someone at the keyboard of a
///   locked machine, and the thing that actually makes their guessing
///   expensive is the rounds loop, not this comparison.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut differences = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        differences |= x ^ y;
    }
    std::hint::black_box(differences) == 0
}

/// The salt as the scheme sees it: at most sixteen bytes, stopping at the
/// first one that cannot be in a salt.
///
/// `crypt(3)` stops only at `$`, because that is the only byte that would
/// confuse its own parser. Stopping at everything [`is_salt_byte`] rejects is
/// a little stricter, and it buys the guarantee that a line this module writes
/// is always a line [`Hash::parse`] will read back. It cannot change the
/// answer for any hash that could have been stored in the first place, since
/// verifying uses the salt the parser took off the line and never comes
/// through here.
fn trim_salt(salt: &[u8]) -> &[u8] {
    let end = salt
        .iter()
        .position(|&b| !is_salt_byte(b))
        .unwrap_or(salt.len());
    &salt[..end.min(SALT_MAX)]
}

/// The digest itself: steps 1 to 21 of the specification.
///
/// `salt` must already be trimmed, and `rounds` must already be in range;
/// both are true of everything in this module that calls it.
fn digest(password: &[u8], salt: &[u8], rounds: u32) -> [u8; DIGEST_LEN] {
    // Steps 4 to 8: digest B is the password, the salt and the password
    // again. It is the only place the three appear in that order, and
    // everything below is built out of it.
    let mut ctx = Sha512::new();
    ctx.update(password);
    ctx.update(salt);
    ctx.update(password);
    let b = ctx.finalize();

    // Steps 1 to 3, then 9 to 12: digest A is the password and the salt,
    // followed by as much of B as the password is long, followed by one of B
    // or the password for each bit of the password's length. The last part is
    // what makes two passwords of different lengths take different paths
    // through the same arithmetic.
    let mut ctx = Sha512::new();
    ctx.update(password);
    ctx.update(salt);
    let mut remaining = password.len();
    while remaining > DIGEST_LEN {
        ctx.update(&b);
        remaining -= DIGEST_LEN;
    }
    ctx.update(&b[..remaining]);
    let mut length = password.len();
    while length > 0 {
        if length & 1 == 1 {
            ctx.update(&b);
        } else {
            ctx.update(password);
        }
        length >>= 1;
    }
    let mut a = ctx.finalize();

    // Steps 13 to 16: P is the password repeated into a sequence as long as
    // the password, out of a digest of the password repeated as many times as
    // it is long. Steps 17 to 20 do the same for the salt, except that the
    // repeat count comes from the first byte of A — so the work is not quite
    // the same for two different passwords even with the same salt.
    let mut ctx = Sha512::new();
    for _ in 0..password.len() {
        ctx.update(password);
    }
    let p = stretch(&ctx.finalize(), password.len());

    let mut ctx = Sha512::new();
    for _ in 0..16 + u32::from(a[0]) {
        ctx.update(salt);
    }
    let s = stretch(&ctx.finalize(), salt.len());

    // Step 21: the loop that costs the time. Each round hashes the previous
    // round's digest together with P and S in an order that depends on the
    // round number, so the rounds cannot be collapsed or run out of order.
    for round in 0..rounds {
        let mut ctx = Sha512::new();
        if round % 2 == 1 {
            ctx.update(&p);
        } else {
            ctx.update(&a);
        }
        if round % 3 != 0 {
            ctx.update(&s);
        }
        if round % 7 != 0 {
            ctx.update(&p);
        }
        if round % 2 == 1 {
            ctx.update(&a);
        } else {
            ctx.update(&p);
        }
        a = ctx.finalize();
    }

    a
}

/// Repeat a digest until it is `len` bytes long, cutting the last copy short.
fn stretch(digest: &[u8; DIGEST_LEN], len: usize) -> Vec<u8> {
    digest.iter().copied().cycle().take(len).collect()
}

/// Encode the final digest the way crypt does: three bytes at a time in the
/// shuffled order, six bits at a time, least significant first.
///
/// The last group has only one byte left, so it produces two characters
/// instead of four and the digest is eighty-six characters rather than
/// eighty-eight.
fn encode(digest: &[u8; DIGEST_LEN]) -> String {
    let mut out = String::with_capacity(DIGEST_CHARS);
    for [high, mid, low] in SHUFFLE {
        let group = (u32::from(digest[high]) << 16)
            | (u32::from(digest[mid]) << 8)
            | u32::from(digest[low]);
        push_group(&mut out, group, 4);
    }
    push_group(&mut out, u32::from(digest[63]), 2);
    out
}

fn push_group(out: &mut String, mut group: u32, chars: usize) {
    for _ in 0..chars {
        out.push(ALPHABET[(group & 0x3f) as usize] as char);
        group >>= 6;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shuffle is copied from a table in a text file, so the thing to
    /// check is that nothing was copied twice or left out: every byte of the
    /// digest but the last must appear exactly once.
    #[test]
    fn the_shuffle_is_a_permutation() {
        let mut seen = [false; DIGEST_LEN];
        for index in SHUFFLE.iter().flatten() {
            assert!(!seen[*index], "byte {index} appears twice");
            seen[*index] = true;
        }
        for (index, seen) in seen.iter().enumerate() {
            assert_eq!(*seen, index != 63, "byte {index}");
        }
    }

    #[test]
    fn a_hash_round_trips_through_the_parser() {
        let line = hash(b"hunter2", b"0123456789abcdef");
        let parsed = Hash::parse(&line).expect("a hash this crate wrote must parse");
        assert_eq!(parsed.salt(), "0123456789abcdef");
        assert_eq!(parsed.rounds(), DEFAULT_ROUNDS);
        assert!(!parsed.explicit_rounds());
        assert_eq!(verify(b"hunter2", &line), Ok(true));
        assert_eq!(verify(b"hunter3", &line), Ok(false));
        assert_eq!(verify(b"", &line), Ok(false));
    }

    /// A rounds count tOS wrote has to still verify after someone edits the
    /// file, and a line that says the default explicitly has to mean the same
    /// as one that leaves it out.
    #[test]
    fn rounds_survive_a_round_trip() {
        for rounds in [MIN_ROUNDS, 1_234, DEFAULT_ROUNDS, 7_777] {
            let line = hash_with_rounds(b"pass phrase", b"NaClNaClNaClNaCl", rounds);
            let parsed = Hash::parse(&line).expect("must parse");
            assert_eq!(parsed.rounds(), rounds);
            assert!(parsed.explicit_rounds());
            assert!(verify(b"pass phrase", &line).expect("must parse"));
            assert!(!verify(b"pass phras", &line).expect("must parse"));
        }
        let implicit = hash(b"pass phrase", b"NaClNaClNaClNaCl");
        let explicit = hash_with_rounds(b"pass phrase", b"NaClNaClNaClNaCl", DEFAULT_ROUNDS);
        assert_eq!(
            Hash::parse(&implicit).unwrap().digest(),
            Hash::parse(&explicit).unwrap().digest(),
        );
    }

    /// Out-of-range rounds are pulled into range rather than refused, which
    /// is what `crypt(3)` does. Only the clamp is checked here and not a
    /// digest computed with it: the top of the range is a billion rounds, and
    /// a test that waited for one would be a test nobody runs. The seventh
    /// published vector checks the bottom of the clamp against a real digest.
    #[test]
    fn absurd_rounds_are_pulled_into_range() {
        assert_eq!(clamp_rounds(0), MIN_ROUNDS);
        assert_eq!(clamp_rounds(1), MIN_ROUNDS);
        assert_eq!(clamp_rounds(MIN_ROUNDS - 1), MIN_ROUNDS);
        assert_eq!(clamp_rounds(MIN_ROUNDS), MIN_ROUNDS);
        assert_eq!(clamp_rounds(DEFAULT_ROUNDS), DEFAULT_ROUNDS);
        assert_eq!(clamp_rounds(MAX_ROUNDS), MAX_ROUNDS);
        assert_eq!(clamp_rounds(MAX_ROUNDS + 1), MAX_ROUNDS);
        assert_eq!(clamp_rounds(u32::MAX), MAX_ROUNDS);

        // And the line says the number that was used, not the one asked for.
        let low = hash_with_rounds(b"x", b"salt", 1);
        assert!(low.starts_with("$6$rounds=1000$salt$"), "{low}");
        assert_eq!(verify(b"x", &low), Ok(true));
    }

    /// Over-long salts are cut to sixteen bytes, which is what makes the
    /// published vectors with long salts come out right, and a salt holding
    /// anything a line cannot carry is cut there, so that hashing can never
    /// produce a line this crate would refuse to read back.
    #[test]
    fn a_salt_is_trimmed_to_something_a_line_can_hold() {
        assert_eq!(trim_salt(b"short"), b"short");
        assert_eq!(trim_salt(b"0123456789abcdefghij"), b"0123456789abcdef");
        assert_eq!(trim_salt(b"abc$def"), b"abc");
        assert_eq!(trim_salt(b"abc:def"), b"abc");
        assert_eq!(trim_salt(b"abc def"), b"abc");
        assert_eq!(trim_salt("abcé".as_bytes()), b"abc");
        assert_eq!(trim_salt(b"$"), b"");
        for salt in [
            &b"abc$def"[..],
            b"abc:def",
            b"abc def",
            b"0123456789abcdefghij",
            b"",
            "\u{3042}".as_bytes(),
        ] {
            let line = hash(b"x", salt);
            let parsed = Hash::parse(&line).expect("a hash this crate wrote must parse");
            assert_eq!(parsed.salt().as_bytes(), trim_salt(salt));
            assert_eq!(verify(b"x", &line), Ok(true));
        }
    }

    /// Everything a line off disk can be wrong in. None of these may panic
    /// and none of them may verify; each has to come back as an error, so the
    /// caller can say "there is no credential here" rather than "wrong
    /// password".
    #[test]
    fn a_malformed_line_is_an_error() {
        let good = hash(b"secret", b"0123456789abcdef");
        let digest = Hash::parse(&good).unwrap().digest().to_string();
        let salt = "0123456789abcdef";

        let cases: &[(&str, CryptError)] = &[
            // Not this scheme, or not a hash at all.
            ("", CryptError::NotSha512),
            ("$", CryptError::NotSha512),
            ("$6", CryptError::NotSha512),
            ("$5$salt$tooshort", CryptError::NotSha512),
            ("$y$j9T$F5Jx5fExrKuPp53xLKQ..1$hash", CryptError::NotSha512),
            ("x$6$salt$hash", CryptError::NotSha512),
            // Fields that stop early.
            ("$6$", CryptError::Truncated),
            ("$6$salt", CryptError::Truncated),
            ("$6$rounds=5000", CryptError::Truncated),
            ("$6$rounds=5000$salt", CryptError::Truncated),
            // Rounds that are not a number in range.
            ("$6$rounds=$salt$hash", CryptError::BadRounds),
            ("$6$rounds=abc$salt$hash", CryptError::BadRounds),
            ("$6$rounds= 5000$salt$hash", CryptError::BadRounds),
            ("$6$rounds=+5000$salt$hash", CryptError::BadRounds),
            ("$6$rounds=-5000$salt$hash", CryptError::BadRounds),
            ("$6$rounds=5000.0$salt$hash", CryptError::BadRounds),
            ("$6$rounds=05000$salt$hash", CryptError::BadRounds),
            ("$6$rounds=999$salt$hash", CryptError::BadRounds),
            ("$6$rounds=1000000000$salt$hash", CryptError::BadRounds),
            (
                "$6$rounds=99999999999999999999$salt$hash",
                CryptError::BadRounds,
            ),
            // A second rounds field reads as a salt, and then the digest has
            // a `$` in it. Wrong for a reason, but wrong.
            (
                "$6$rounds=5000$rounds=5000$salt$hash",
                CryptError::BadDigest,
            ),
        ];
        for (line, expected) in cases {
            assert_eq!(Hash::parse(line), Err(*expected), "{line:?}");
            assert_eq!(verify(b"secret", line), Err(*expected), "{line:?}");
        }

        // A salt that is too long, or holds a byte the file format cannot
        // carry. The digest is a real one so that only the salt is wrong.
        for bad_salt in [
            "0123456789abcdefg",
            "salt with spaces",
            "salt:colon",
            "salt\tta b",
            "salté",
        ] {
            let line = format!("$6${bad_salt}${digest}");
            assert_eq!(Hash::parse(&line), Err(CryptError::BadSalt), "{line:?}");
        }

        // A digest of the wrong length, or with a character outside the
        // alphabet — which includes a line that still has its newline on it,
        // and one with an extra field after it.
        for bad_digest in [
            String::new(),
            digest[1..].to_string(),
            format!("{digest}x"),
            format!("{digest}\n"),
            format!("{}$", &digest[1..]),
            format!("{}+", &digest[1..]),
            format!("{}=", &digest[1..]),
            format!("*{}", &digest[1..]),
        ] {
            let line = format!("$6${salt}${bad_digest}");
            assert_eq!(Hash::parse(&line), Err(CryptError::BadDigest), "{line:?}");
            assert_eq!(verify(b"secret", &line), Err(CryptError::BadDigest));
        }
    }

    /// A digest that is the right shape but not the right value is a wrong
    /// password, not a corrupt file. The lock tells those two apart.
    #[test]
    fn a_wrong_digest_is_not_an_error() {
        let good = hash(b"secret", b"0123456789abcdef");
        let flipped = format!(
            "{}{}",
            &good[..good.len() - 1],
            if good.ends_with('.') { "/" } else { "." }
        );
        assert_eq!(verify(b"secret", &flipped), Ok(false));
    }

    /// An empty password hashes and verifies like any other. Whether the
    /// installer allows one is the installer's decision; this layer has no
    /// opinion and must not fall over either way.
    #[test]
    fn an_empty_password_is_just_a_password() {
        let line = hash(b"", b"0123456789abcdef");
        assert_eq!(verify(b"", &line), Ok(true));
        assert_eq!(verify(b" ", &line), Ok(false));
    }

    /// The password is bytes, not text: whatever the keyboard produced goes
    /// in unmodified, including a NUL, which `crypt(3)` could not have carried
    /// through a C string.
    #[test]
    fn a_password_is_bytes() {
        let line = hash("пароль🔒".as_bytes(), b"0123456789abcdef");
        assert_eq!(verify("пароль🔒".as_bytes(), &line), Ok(true));
        assert_eq!(verify("пароль".as_bytes(), &line), Ok(false));

        let with_nul = hash(b"one\0two", b"0123456789abcdef");
        assert_eq!(verify(b"one\0two", &with_nul), Ok(true));
        assert_eq!(verify(b"one", &with_nul), Ok(false));
    }

    #[test]
    fn constant_time_eq_still_answers_the_question() {
        assert!(constant_time_eq(b"", b""));
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"Abc"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"abc", b""));
    }
}
