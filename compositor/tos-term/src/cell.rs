//! The terminal cell, the fundamental layout primitive of tOS.
//!
//! A cell carries a glyph, colors, attributes and an optional reference to a
//! graphics surface, matching the conceptual model in the tOS README.

use crate::color::Color;

/// Style flags packed into a single word.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Flags(u16);

impl Flags {
    pub const NONE: Flags = Flags(0);
    pub const BOLD: Flags = Flags(1 << 0);
    pub const DIM: Flags = Flags(1 << 1);
    pub const ITALIC: Flags = Flags(1 << 2);
    pub const BLINK: Flags = Flags(1 << 3);
    pub const REVERSE: Flags = Flags(1 << 4);
    pub const HIDDEN: Flags = Flags(1 << 5);
    pub const STRIKEOUT: Flags = Flags(1 << 6);
    pub const OVERLINE: Flags = Flags(1 << 7);
    /// Leading cell of a double width glyph.
    pub const WIDE: Flags = Flags(1 << 8);
    /// Placeholder cell that follows a `WIDE` cell.
    pub const WIDE_SPACER: Flags = Flags(1 << 9);
    /// Blank left at the right margin because a double width glyph would not
    /// fit beside it and moved to the next line whole. It is padding rather
    /// than a space anybody typed, so anything reading the row back as text
    /// leaves it out — the row is marked wrapped, and on a wrapped row the
    /// trailing blanks are otherwise content.
    pub const WRAP_PAD: Flags = Flags(1 << 10);

    pub const fn contains(self, other: Flags) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn union(self, other: Flags) -> Flags {
        Flags(self.0 | other.0)
    }

    pub const fn without(self, other: Flags) -> Flags {
        Flags(self.0 & !other.0)
    }

    pub fn insert(&mut self, other: Flags) {
        self.0 |= other.0;
    }

    pub fn remove(&mut self, other: Flags) {
        self.0 &= !other.0;
    }

    pub fn set(&mut self, other: Flags, on: bool) {
        if on {
            self.insert(other)
        } else {
            self.remove(other)
        }
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

/// Underline rendering styles (SGR 4:x).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Underline {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

impl Underline {
    pub fn from_sgr_param(p: u16) -> Underline {
        match p {
            0 => Underline::None,
            1 => Underline::Single,
            2 => Underline::Double,
            3 => Underline::Curly,
            4 => Underline::Dotted,
            5 => Underline::Dashed,
            _ => Underline::Single,
        }
    }

    pub fn is_none(self) -> bool {
        matches!(self, Underline::None)
    }
}

/// Reference to a graphics placement covering this cell.
///
/// The image data itself lives in the compositor's surface store; cells only
/// remember which surface and which piece of it they show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GraphicsRef {
    pub placement: u32,
    /// Offset of this cell inside the placement, in cells.
    pub col: u16,
    pub row: u16,
}

/// Everything about a cell except which character it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Attrs {
    pub fg: Color,
    pub bg: Color,
    pub underline_color: Color,
    pub flags: Flags,
    pub underline: Underline,
    /// Index into the terminal's hyperlink table (OSC 8).
    pub hyperlink: Option<u16>,
    pub graphics: Option<GraphicsRef>,
}

impl Attrs {
    pub fn reset(&mut self) {
        *self = Attrs::default();
    }
}

/// One terminal cell.
#[derive(Debug, Clone, PartialEq)]
pub struct Cell {
    pub ch: char,
    /// Combining characters applied on top of `ch`, if any.
    pub zerowidth: Option<Box<Vec<char>>>,
    pub attrs: Attrs,
}

impl Default for Cell {
    fn default() -> Self {
        Cell {
            ch: ' ',
            zerowidth: None,
            attrs: Attrs::default(),
        }
    }
}

impl Cell {
    pub fn new(ch: char, attrs: Attrs) -> Self {
        Cell {
            ch,
            zerowidth: None,
            attrs,
        }
    }

    /// A blank cell that keeps the current background color, which is what
    /// erase operations must produce (background color erase).
    pub fn blank(attrs: &Attrs) -> Self {
        Cell {
            ch: ' ',
            zerowidth: None,
            attrs: Attrs {
                bg: attrs.bg,
                ..Attrs::default()
            },
        }
    }

    pub fn clear(&mut self, attrs: &Attrs) {
        *self = Cell::blank(attrs);
    }

    pub fn is_empty(&self) -> bool {
        self.ch == ' ' && self.zerowidth.is_none() && self.attrs.graphics.is_none()
    }

    /// A cell holds at most this many combining marks. Beyond it the extras
    /// are dropped, because a stream of marks at one position would otherwise
    /// grow a single cell without bound.
    pub const MAX_ZEROWIDTH: usize = 8;

    pub fn push_zerowidth(&mut self, c: char) {
        let marks = self.zerowidth.get_or_insert_with(Default::default);
        if marks.len() < Cell::MAX_ZEROWIDTH {
            marks.push(c);
        }
    }

    /// Number of columns this cell occupies.
    pub fn width(&self) -> usize {
        if self.attrs.flags.contains(Flags::WIDE) {
            2
        } else {
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::Rgb;

    #[test]
    fn flags_set_and_clear() {
        let mut f = Flags::NONE;
        f.insert(Flags::BOLD);
        f.insert(Flags::ITALIC);
        assert!(f.contains(Flags::BOLD));
        f.set(Flags::BOLD, false);
        assert!(!f.contains(Flags::BOLD));
        assert!(f.contains(Flags::ITALIC));
    }

    #[test]
    fn combining_marks_are_capped() {
        let mut cell = Cell::new('a', Attrs::default());
        for _ in 0..100 {
            cell.push_zerowidth('\u{0301}');
        }
        assert_eq!(cell.zerowidth.unwrap().len(), Cell::MAX_ZEROWIDTH);
    }

    #[test]
    fn blank_keeps_background_only() {
        let attrs = Attrs {
            fg: Color::Rgb(Rgb::WHITE),
            bg: Color::Indexed(4),
            flags: Flags::BOLD,
            ..Attrs::default()
        };
        let cell = Cell::blank(&attrs);
        assert_eq!(cell.attrs.bg, Color::Indexed(4));
        assert_eq!(cell.attrs.fg, Color::Default);
        assert!(cell.attrs.flags.is_empty());
    }
}
