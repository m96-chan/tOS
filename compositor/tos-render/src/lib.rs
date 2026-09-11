//! tOS CPU renderer.

pub mod surface;
pub mod terminal;

pub use surface::{OwnedFramebuffer, Rect, Surface};
pub use terminal::{render, RenderOptions, Selection};
