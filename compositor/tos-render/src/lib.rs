//! tOS CPU renderer.

pub mod damage;
pub mod surface;
pub mod terminal;
pub mod texture;

pub use damage::Damage;
pub use surface::{OwnedFramebuffer, Rect, Surface};
pub use terminal::{render, RenderOptions, Selection};
pub use texture::{Texture, TextureCache, TextureKey};
