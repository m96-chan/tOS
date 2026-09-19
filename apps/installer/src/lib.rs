//! The tOS installer.
//!
//! Installing tOS is the first real use of the platform as a platform: the
//! installer is an ordinary TUI application running in a pane, talking to the
//! compositor through the same escape sequences as anything else.
//!
//! The library holds everything that can be reasoned about without touching a
//! disk — device enumeration, the plan, the step sequence, the UI — so that
//! the destructive parts have a seam that tests can sit in.

pub mod app;
pub mod disk;
pub mod exec;
pub mod install;
pub mod motd;
pub mod plan;
pub mod ui;

pub use app::{App, Command, Stage};
pub use disk::{Disk, DiskSource};
pub use exec::{Backend, DryRun, Recorder};
pub use install::{Installer, Progress, StepOutcome};
pub use plan::{Plan, Step};
