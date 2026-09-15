//! The tOS compositor.
//!
//! The binary is a thin wrapper around this library, which exists so that the
//! session, pane and rendering behaviour can be driven from integration tests
//! without a display.

pub mod account;
pub mod bluetooth;
pub mod chrome;
pub mod clock;
pub mod compositor;
pub mod config;
pub mod config_file;
pub mod copymode;
pub mod imagefile;
pub mod ime;
pub mod launcher;
pub mod lock;
pub mod notify;
pub mod overlay;
pub mod pane;
pub mod pointer;
pub mod power;
pub mod selection;
pub mod splash;
pub mod status;
pub mod system;

pub use compositor::{Compositor, OverlayKind};
pub use config::{parse_args, usage, Backend, Config, ConfigSource};
pub use config_file::{startup, Startup};
pub use copymode::{CopyMode, CopyOutcome};
pub use imagefile::ImageFiles;
pub use ime::{Ime, ImeContext};
pub use lock::{LockOutcome, LockScreen, NoCredential};
pub use notify::{Notification, Notifications, Source};
pub use overlay::{Overlay, OverlayItem, OverlayOutcome, Placement};
pub use pane::Pane;
pub use pointer::Pointer;
pub use selection::{Anchor, Selection, SelectionMode};
pub use splash::Splash;
pub use status::{Bar, Hit, Segment};
