//! tOS platform backends.
//!
//! Everything hardware specific lives here, behind the [`Display`] trait, so
//! the compositor itself stays portable across PCs, SBCs and phones.

pub mod display;
pub mod headless;
pub mod nested;
pub mod tty;

#[cfg(target_os = "linux")]
pub mod drm;
#[cfg(target_os = "linux")]
pub mod vt;

pub use display::{suggested_font_size, Display};
pub use headless::HeadlessDisplay;
pub use nested::NestedDisplay;

#[cfg(target_os = "linux")]
pub use drm::DrmDisplay;
#[cfg(target_os = "linux")]
pub use vt::VirtualTerminal;
