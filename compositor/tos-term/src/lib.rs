//! The tOS terminal model.
//!
//! This crate is the part of the compositor that knows what a terminal *is*:
//! a grid of cells, a cursor, a set of modes, and a parser that turns a byte
//! stream into changes to those. It has no dependencies and no knowledge of
//! display hardware, PTYs or input devices, which is what makes it testable
//! on any host while the rest of tOS targets Linux directly.

pub mod cell;
pub mod color;
pub mod graphics;
pub mod grid;
pub mod inflate;
pub mod modes;
pub mod parser;
pub mod png;
pub mod term;
pub mod width;

pub use cell::{Attrs, Cell, Flags, GraphicsRef, Underline};
pub use color::{Color, Palette, Rgb};
pub use grid::{Grid, Region, Row};
pub use modes::{
    CursorShape, CursorStyle, KeyboardFlags, Modes, MouseEncoding, MouseState, MouseTracking,
};
pub use parser::{Params, Parser, Perform};
pub use term::{Cursor, Damage, TermEvent, Terminal, TerminalConfig};
pub use width::{char_width, str_width};
