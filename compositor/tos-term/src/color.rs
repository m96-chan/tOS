//! Color model for terminal cells.
//!
//! tOS is true-color native. The 256-entry indexed palette exists only for
//! compatibility with applications that still speak SGR 38;5;n.

/// A 24 bit color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const BLACK: Rgb = Rgb::new(0, 0, 0);
    pub const WHITE: Rgb = Rgb::new(0xff, 0xff, 0xff);

    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Rgb { r, g, b }
    }

    /// Pack into 0x00RRGGBB, the layout used by the XRGB8888 framebuffer.
    pub const fn pack(self) -> u32 {
        (self.r as u32) << 16 | (self.g as u32) << 8 | self.b as u32
    }

    /// Linear interpolation, `t` in 0..=255 where 255 yields `other`.
    pub fn blend(self, other: Rgb, t: u8) -> Rgb {
        let mix = |a: u8, b: u8| -> u8 {
            let a = a as u32;
            let b = b as u32;
            let t = t as u32;
            ((a * (255 - t) + b * t) / 255) as u8
        };
        Rgb::new(mix(self.r, other.r), mix(self.g, other.g), mix(self.b, other.b))
    }

    /// Scale every channel by `num/den`, used for the SGR "dim" attribute.
    pub fn scale(self, num: u32, den: u32) -> Rgb {
        let s = |c: u8| ((c as u32 * num) / den).min(255) as u8;
        Rgb::new(s(self.r), s(self.g), s(self.b))
    }
}

/// A color as stored in a cell: either a palette reference or a literal RGB.
///
/// `Default` is resolved late, at paint time, so that changing the theme does
/// not require rewriting the grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[derive(Default)]
pub enum Color {
    #[default]
    Default,
    Indexed(u8),
    Rgb(Rgb),
}


/// The 256 color palette plus the default foreground/background/cursor colors.
#[derive(Debug, Clone)]
pub struct Palette {
    colors: [Rgb; 256],
    pub foreground: Rgb,
    pub background: Rgb,
    pub cursor: Rgb,
    pub cursor_text: Rgb,
}

impl Palette {
    /// Build the standard xterm palette: 16 system colors, a 6x6x6 color cube
    /// and a 24 step gray ramp.
    pub fn new() -> Self {
        let mut colors = [Rgb::BLACK; 256];

        const SYSTEM: [Rgb; 16] = [
            Rgb::new(0x1c, 0x1c, 0x1c), // 0 black (slightly lifted, easier on OLED panels)
            Rgb::new(0xcc, 0x57, 0x57), // 1 red
            Rgb::new(0x5f, 0xaf, 0x5f), // 2 green
            Rgb::new(0xc7, 0xa1, 0x4f), // 3 yellow
            Rgb::new(0x5f, 0x87, 0xd7), // 4 blue
            Rgb::new(0xa8, 0x72, 0xc7), // 5 magenta
            Rgb::new(0x5f, 0xb0, 0xb0), // 6 cyan
            Rgb::new(0xd0, 0xd0, 0xd0), // 7 white
            Rgb::new(0x60, 0x60, 0x60), // 8 bright black
            Rgb::new(0xff, 0x7b, 0x7b), // 9 bright red
            Rgb::new(0x87, 0xdf, 0x87), // 10 bright green
            Rgb::new(0xff, 0xd7, 0x7b), // 11 bright yellow
            Rgb::new(0x87, 0xb7, 0xff), // 12 bright blue
            Rgb::new(0xd7, 0xa1, 0xff), // 13 bright magenta
            Rgb::new(0x87, 0xe7, 0xe7), // 14 bright cyan
            Rgb::new(0xff, 0xff, 0xff), // 15 bright white
        ];
        colors[..16].copy_from_slice(&SYSTEM);

        const CUBE: [u8; 6] = [0, 0x5f, 0x87, 0xaf, 0xd7, 0xff];
        let mut i = 16;
        for r in CUBE {
            for g in CUBE {
                for b in CUBE {
                    colors[i] = Rgb::new(r, g, b);
                    i += 1;
                }
            }
        }
        for step in 0..24u8 {
            let v = 8 + step * 10;
            colors[i] = Rgb::new(v, v, v);
            i += 1;
        }
        debug_assert_eq!(i, 256);

        Palette {
            colors,
            foreground: Rgb::new(0xd0, 0xd0, 0xd0),
            background: Rgb::new(0x10, 0x10, 0x12),
            cursor: Rgb::new(0x87, 0xb7, 0xff),
            cursor_text: Rgb::new(0x10, 0x10, 0x12),
        }
    }

    pub fn index(&self, i: u8) -> Rgb {
        self.colors[i as usize]
    }

    /// OSC 4 lets applications redefine palette entries.
    pub fn set_index(&mut self, i: u8, color: Rgb) {
        self.colors[i as usize] = color;
    }

    /// Resolve a cell color to RGB. `is_fg` selects which default to use.
    pub fn resolve(&self, color: Color, is_fg: bool) -> Rgb {
        match color {
            Color::Default => {
                if is_fg {
                    self.foreground
                } else {
                    self.background
                }
            }
            Color::Indexed(i) => self.index(i),
            Color::Rgb(rgb) => rgb,
        }
    }

    /// Map an indexed color to its bright variant, for `bold-is-bright`.
    pub fn brighten(color: Color) -> Color {
        match color {
            Color::Indexed(i) if i < 8 => Color::Indexed(i + 8),
            other => other,
        }
    }
}

impl Default for Palette {
    fn default() -> Self {
        Palette::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_cube_and_ramp() {
        let p = Palette::new();
        // First cube entry is pure black, last is pure white.
        assert_eq!(p.index(16), Rgb::new(0, 0, 0));
        assert_eq!(p.index(231), Rgb::new(0xff, 0xff, 0xff));
        // Gray ramp start/end.
        assert_eq!(p.index(232), Rgb::new(8, 8, 8));
        assert_eq!(p.index(255), Rgb::new(238, 238, 238));
    }

    #[test]
    fn pack_is_xrgb() {
        assert_eq!(Rgb::new(0x12, 0x34, 0x56).pack(), 0x0012_3456);
    }

    #[test]
    fn brighten_only_affects_low_indices() {
        assert_eq!(Palette::brighten(Color::Indexed(1)), Color::Indexed(9));
        assert_eq!(Palette::brighten(Color::Indexed(9)), Color::Indexed(9));
        assert_eq!(Palette::brighten(Color::Default), Color::Default);
    }
}
