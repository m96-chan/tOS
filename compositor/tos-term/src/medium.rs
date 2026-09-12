//! Where a graphics command's bytes come from when they do not come inline.
//!
//! The protocol's `t=f`, `t=t` and `t=s` hand over a name — a path, or a
//! POSIX shared memory object — instead of base64, which is how a file
//! manager shows a picture without pushing megabytes of it through a PTY.
//!
//! Opening that name is an operating system question rather than a terminal
//! one. This crate has no dependencies and touches no filesystem, which is
//! what lets it be tested on any host; and the part of tOS that owns the
//! machine is the compositor, which is also the part that can say what it is
//! willing to open, and to delete, on a program's say-so. So the two are kept
//! apart by the seam below: `tos-compositor` installs the reader that opens
//! files, a test installs one that answers out of a map and remembers what it
//! was asked, and a terminal that was given neither refuses — which is the
//! honest answer for a terminal that has no way to read anything.
//!
//! The policy the real reader implements is argued in
//! `docs/design/graphics-file-transmission.md`.

use crate::graphics::Medium;

/// Reads what a non-direct transmission names.
pub trait MediumReader {
    /// Read what `name` names, at most `max_bytes` of it.
    ///
    /// `name` is the command's payload with its base64 already undone, so it
    /// is whatever bytes the program sent: not necessarily a path, not
    /// necessarily text, not necessarily anything. The error is a protocol
    /// response body rather than an `io::Error`, because a program that asked
    /// for a file it may not have has to be told so in the reply to its own
    /// command; there is nobody else to tell.
    ///
    /// The reader is also what deletes, because `t=t` and `t=s` are transfers
    /// that consume what they read. Whether a name may be unlinked at all is
    /// part of the same decision as whether it may be opened, and handing the
    /// terminal half of that decision would leave it holding a rule it has no
    /// way to check.
    fn read(
        &mut self,
        medium: Medium,
        name: &[u8],
        max_bytes: usize,
    ) -> Result<Vec<u8>, &'static str>;
}

/// The reader a terminal has until it is given one.
///
/// `ENOSUP` and not `EBADF`: nothing was wrong with the file, and a client
/// that can tell the two apart can fall back to sending the bytes inline
/// instead of retrying a path that was never going to be read.
pub struct NoMedia;

impl MediumReader for NoMedia {
    fn read(
        &mut self,
        _medium: Medium,
        _name: &[u8],
        _max_bytes: usize,
    ) -> Result<Vec<u8>, &'static str> {
        Err("ENOSUP:this terminal cannot read files")
    }
}
