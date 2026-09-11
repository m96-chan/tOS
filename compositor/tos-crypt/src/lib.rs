//! Hashing and checking a tOS password.
//!
//! tOS has to be able to ask "is this the password?" in two places that have
//! nothing else in common: `tos-install` writes the credential when it puts
//! the system on a disk, and the compositor reads it back to unlock a locked
//! screen. There is no library in the workspace either of them is willing to
//! take the answer from. The `libc` crate declares no `crypt(3)` on any
//! target; musl's, which is what the ISO links, cannot read the yescrypt
//! hashes Debian writes; and glibc's lives in libxcrypt, which means a
//! link-time dependency on the one build where it would cost anything and
//! still no yescrypt. `docs/design/screen-lock.md` works through that at
//! length and lands on this: the credential is tOS's own file, hashed with
//! SHA-512 crypt, implemented here.
//!
//! So this crate is SHA-512 ([`sha512`]) and the `$6$` scheme on top of it
//! ([`sha512crypt`]), both of them pure functions over bytes with no I/O and
//! no dependencies, the way `tos-term`'s DEFLATE and PNG decoders are. What
//! makes writing them by hand reasonable rather than reckless is that both
//! halves are published together with test vectors, and `tests/vectors.rs`
//! runs all of them: NIST's for the hash, and the seven in Ulrich Drepper's
//! specification for the scheme. A hash tOS writes is an ordinary `$6$` line,
//! so `/etc/tos/shadow` stays readable by any other tool on the machine and
//! this choice stays reversible.
//!
//! The two things a caller needs:
//!
//! ```no_run
//! # fn main() -> std::io::Result<()> {
//! // The installer, having asked for a password twice and got the same
//! // answer, writes this line to /etc/tos/shadow with mode 0600.
//! let line = tos_crypt::hash_password(b"correct horse battery staple")?;
//!
//! // The lock, having read that line back, checks what was typed against it.
//! match tos_crypt::verify_password(b"correct horse battery staple", &line) {
//!     Ok(true) => println!("unlock"),
//!     Ok(false) => println!("wrong password"),
//!     Err(err) => println!("no usable credential: {err}"),
//! }
//! # Ok(())
//! # }
//! ```
//!
//! That third arm is the point of the error type. A credential file that does
//! not parse is not a wrong password, and a lock that treated it as one would
//! be a lock nobody could ever open; the design says it should refuse to
//! engage at all and say why.
//!
//! What this crate does not do: it never writes a file, never chooses a path,
//! and never decides whether an empty password is allowed — those belong to
//! the installer and the lock. It also makes no attempt to wipe the password
//! bytes it was handed, which are the caller's memory and, on a machine where
//! `tos` is root and owns the keyboard, not the boundary that is doing any
//! work.

pub mod salt;
pub mod sha512;
pub mod sha512crypt;

pub use salt::random_salt;
pub use sha512::{sha512, Sha512};
pub use sha512crypt::{verify as verify_password, CryptError, Hash};

/// Hash a password with a fresh salt, ready to be written to a credential
/// file.
///
/// The salt comes from `/dev/urandom` and the rounds are the scheme's default,
/// so the line carries no `rounds=` field. Use
/// [`sha512crypt::hash_with_rounds`] to say otherwise; anything tOS writes
/// today verifies against either spelling afterwards.
pub fn hash_password(password: &[u8]) -> std::io::Result<String> {
    let salt = random_salt()?;
    Ok(sha512crypt::hash(password, salt.as_bytes()))
}
