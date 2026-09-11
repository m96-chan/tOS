//! Where a new salt comes from.
//!
//! This is the one part of the crate that is not a pure function over bytes,
//! and it is deliberately the smallest part: opening `/dev/urandom` and
//! reading sixteen bytes. The rest of the crate never calls it, so every
//! digest in the tests is reproducible.

use std::fs::File;
use std::io::{self, Read};

use crate::sha512crypt::SALT_MAX;

/// The alphabet a generated salt is drawn from, which is the alphabet the
/// encoded digest uses.
const ALPHABET: &[u8; 64] = b"./0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// Draw a sixteen-character salt from `/dev/urandom`.
///
/// Sixteen characters out of a sixty-four character alphabet is ninety-six
/// bits, which is what the scheme allows and far more than the salt has to do:
/// a salt is not a secret, it only has to be unlikely to have been used
/// before, so that a table computed against one machine's hash is worth
/// nothing against another's.
///
/// Each byte is masked to its low six bits rather than reduced modulo
/// sixty-four. Those are the same thing here — sixty-four divides two hundred
/// and fifty-six exactly — so there is no modulo bias to reject samples for,
/// and the loop cannot run twice.
///
/// `/dev/urandom` rather than `getrandom(2)` because reaching the syscall
/// would mean either a `libc` dependency in a crate that has none or a raw
/// `syscall` by number, and the difference between them is only visible in the
/// first second of boot, before the kernel's pool is seeded. tOS asks for a
/// salt when someone is typing a password into the installer, which is a long
/// way past that.
pub fn random_salt() -> io::Result<String> {
    let mut bytes = [0u8; SALT_MAX];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes
        .iter()
        .map(|b| ALPHABET[usize::from(b & 0x3f)] as char)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sha512crypt::Hash;
    use std::collections::HashSet;

    /// A salt has to be the right length, has to be made of characters a
    /// stored line can carry, and has to be different every time. The last of
    /// those cannot really be tested — only contradicted — so this asks for a
    /// few and insists they are not all the same.
    #[test]
    fn a_generated_salt_is_usable_and_not_a_constant() {
        let mut seen = HashSet::new();
        for _ in 0..8 {
            let salt = random_salt().expect("/dev/urandom must be readable");
            assert_eq!(salt.len(), SALT_MAX);
            assert!(
                salt.bytes().all(|b| ALPHABET.contains(&b)),
                "salt {salt:?} is not in the crypt alphabet"
            );
            seen.insert(salt);
        }
        assert!(seen.len() > 1, "every salt came out the same");
    }

    /// The end tOS actually uses: a salt straight out of `/dev/urandom`, a
    /// password hashed with it, and a line that reads back and verifies.
    #[test]
    fn a_generated_salt_survives_the_line() {
        let line = crate::hash_password(b"correct horse").expect("must hash");
        let parsed = Hash::parse(&line).expect("a hash this crate wrote must parse");
        assert_eq!(parsed.salt().len(), SALT_MAX);
        assert_eq!(
            crate::sha512crypt::verify(b"correct horse", &line),
            Ok(true)
        );
        assert_eq!(
            crate::sha512crypt::verify(b"correct hors", &line),
            Ok(false)
        );
    }
}
