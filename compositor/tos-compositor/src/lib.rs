//! The tOS compositor.
//!
//! The binary is a thin wrapper around this library, which exists so that the
//! session, pane and rendering behaviour can be driven from integration tests
//! without a display.

pub mod chrome;
pub mod compositor;
pub mod config;
pub mod config_file;
pub mod launcher;
pub mod notify;
pub mod overlay;
pub mod pane;
pub mod selection;

pub use compositor::{Compositor, OverlayKind};
pub use config::{parse_args, usage, Backend, Config, ConfigSource};
pub use config_file::{startup, Startup};
pub use notify::{Notification, Notifications, Source};
pub use overlay::{Overlay, OverlayItem, OverlayOutcome};
pub use pane::Pane;
pub use selection::{Anchor, Selection, SelectionMode};
