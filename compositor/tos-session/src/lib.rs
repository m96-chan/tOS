//! tOS session model.
//!
//! Panes, workspaces, focus and key bindings: the part of the compositor that
//! a terminal multiplexer would normally provide, owned by tOS itself so that
//! no tmux layer is needed for the core desktop model.

pub mod describe;
pub mod keys;
pub mod layout;
pub mod session;

pub use describe::{cheat_sheet, BindingHelp};
pub use keys::{Action, Binding, Keymap, Resolution};
pub use layout::{Axis, Direction, Layout, PaneId, Rect};
pub use session::{Session, Workspace, WorkspaceId};
