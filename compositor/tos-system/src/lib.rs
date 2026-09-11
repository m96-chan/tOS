//! Reading and changing the state of the machine tOS runs on.
//!
//! There is no D-Bus here, and no system daemon to ask. Power, network, sound
//! and Bluetooth are read out of sysfs and changed with ioctls and syscalls,
//! the same way the rest of tOS talks to the kernel.
//!
//! Everything that touches the machine goes through a seam, so the logic above
//! it can be tested on a machine that has none of this: [`Sysfs`] for reading,
//! and each module's own trait for acting. `installer/src/exec.rs` uses the
//! same shape for the same reason.

pub mod audio;
pub mod bluetooth;
pub mod net;
pub mod power;
pub mod sysfs;

pub use sysfs::Sysfs;
