//! Character cell width.
//!
//! A pragmatic `wcwidth`: combining marks occupy no cell, East Asian wide and
//! fullwidth characters occupy two, everything else occupies one. The tables
//! cover the ranges that actually show up in terminal use; they are sorted so
//! lookups are a binary search.

type Range = (u32, u32);

const ZERO_WIDTH: &[Range] = &[
    (0x0300, 0x036F),
    (0x0483, 0x0489),
    (0x0591, 0x05BD),
    (0x05BF, 0x05BF),
    (0x05C1, 0x05C2),
    (0x05C4, 0x05C5),
    (0x05C7, 0x05C7),
    (0x0610, 0x061A),
    (0x064B, 0x065F),
    (0x0670, 0x0670),
    (0x06D6, 0x06DC),
    (0x06DF, 0x06E4),
    (0x06E7, 0x06E8),
    (0x06EA, 0x06ED),
    (0x0711, 0x0711),
    (0x0730, 0x074A),
    (0x07A6, 0x07B0),
    (0x07EB, 0x07F3),
    (0x0816, 0x0819),
    (0x081B, 0x0823),
    (0x0825, 0x0827),
    (0x0829, 0x082D),
    (0x0901, 0x0902),
    (0x093C, 0x093C),
    (0x0941, 0x0948),
    (0x094D, 0x094D),
    (0x0951, 0x0957),
    (0x0962, 0x0963),
    (0x0E31, 0x0E31),
    (0x0E34, 0x0E3A),
    (0x0E47, 0x0E4E),
    (0x0EB1, 0x0EB1),
    (0x0EB4, 0x0EB9),
    (0x0EC8, 0x0ECD),
    (0x1AB0, 0x1AFF),
    (0x1DC0, 0x1DFF),
    (0x200B, 0x200F),
    (0x202A, 0x202E),
    (0x2060, 0x2064),
    (0x20D0, 0x20F0),
    (0xFE00, 0xFE0F),
    (0xFE20, 0xFE2F),
    (0xFEFF, 0xFEFF),
    (0xE0100, 0xE01EF),
];

const WIDE: &[Range] = &[
    (0x1100, 0x115F),
    (0x231A, 0x231B),
    (0x2329, 0x232A),
    (0x23E9, 0x23EC),
    (0x25FD, 0x25FE),
    (0x2614, 0x2615),
    (0x2648, 0x2653),
    (0x267F, 0x267F),
    (0x2693, 0x2693),
    (0x26A1, 0x26A1),
    (0x26AA, 0x26AB),
    (0x26BD, 0x26BE),
    (0x26C4, 0x26C5),
    (0x2705, 0x2705),
    (0x270A, 0x270B),
    (0x2728, 0x2728),
    (0x274C, 0x274C),
    (0x274E, 0x274E),
    (0x2753, 0x2755),
    (0x2757, 0x2757),
    (0x2795, 0x2797),
    (0x27B0, 0x27B0),
    (0x27BF, 0x27BF),
    (0x2B1B, 0x2B1C),
    (0x2B50, 0x2B50),
    (0x2B55, 0x2B55),
    (0x2E80, 0x303E),
    (0x3041, 0x33FF),
    (0x3400, 0x4DBF),
    (0x4E00, 0x9FFF),
    (0xA000, 0xA4CF),
    (0xA960, 0xA97F),
    (0xAC00, 0xD7A3),
    (0xF900, 0xFAFF),
    (0xFE10, 0xFE19),
    (0xFE30, 0xFE6F),
    (0xFF00, 0xFF60),
    (0xFFE0, 0xFFE6),
    (0x16FE0, 0x16FE4),
    (0x17000, 0x18AFF),
    (0x1F004, 0x1F004),
    (0x1F0CF, 0x1F0CF),
    (0x1F18E, 0x1F18E),
    (0x1F191, 0x1F19A),
    (0x1F200, 0x1F320),
    (0x1F32D, 0x1F335),
    (0x1F337, 0x1F37C),
    (0x1F37E, 0x1F393),
    (0x1F3A0, 0x1F3CA),
    (0x1F3CF, 0x1F3D3),
    (0x1F3E0, 0x1F3F0),
    (0x1F3F4, 0x1F3F4),
    (0x1F3F8, 0x1F43E),
    (0x1F440, 0x1F440),
    (0x1F442, 0x1F4FC),
    (0x1F4FF, 0x1F53D),
    (0x1F54B, 0x1F54E),
    (0x1F550, 0x1F567),
    (0x1F57A, 0x1F57A),
    (0x1F595, 0x1F596),
    (0x1F5A4, 0x1F5A4),
    (0x1F5FB, 0x1F64F),
    (0x1F680, 0x1F6C5),
    (0x1F6CC, 0x1F6CC),
    (0x1F6D0, 0x1F6D2),
    (0x1F6EB, 0x1F6EC),
    (0x1F6F4, 0x1F6FC),
    (0x1F7E0, 0x1F7EB),
    (0x1F90C, 0x1F9FF),
    (0x1FA70, 0x1FAFF),
    (0x20000, 0x2FFFD),
    (0x30000, 0x3FFFD),
];

fn in_table(table: &[Range], cp: u32) -> bool {
    table
        .binary_search_by(|&(lo, hi)| {
            if cp < lo {
                core::cmp::Ordering::Greater
            } else if cp > hi {
                core::cmp::Ordering::Less
            } else {
                core::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// Columns occupied by `c`: 0 for combining marks and controls, 2 for wide
/// characters, 1 otherwise.
pub fn char_width(c: char) -> usize {
    let cp = c as u32;
    if cp == 0 {
        return 0;
    }
    if cp < 0x20 || (0x7F..0xA0).contains(&cp) {
        return 0;
    }
    if cp < 0x300 {
        return 1;
    }
    if in_table(ZERO_WIDTH, cp) {
        return 0;
    }
    if in_table(WIDE, cp) {
        return 2;
    }
    1
}

/// Total width of a string in cells.
pub fn str_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_is_single_width() {
        assert_eq!(char_width('a'), 1);
        assert_eq!(char_width(' '), 1);
        assert_eq!(char_width('~'), 1);
    }

    #[test]
    fn controls_are_zero_width() {
        assert_eq!(char_width('\n'), 0);
        assert_eq!(char_width('\x1b'), 0);
    }

    #[test]
    fn cjk_is_double_width() {
        assert_eq!(char_width('あ'), 2);
        assert_eq!(char_width('漢'), 2);
        assert_eq!(char_width('한'), 2);
        assert_eq!(char_width('！'), 2); // fullwidth exclamation
    }

    #[test]
    fn combining_marks_are_zero_width() {
        assert_eq!(char_width('\u{0301}'), 0);
        assert_eq!(char_width('\u{fe0f}'), 0);
    }

    #[test]
    fn emoji_is_double_width() {
        assert_eq!(char_width('🚀'), 2);
        assert_eq!(char_width('🦀'), 2);
    }

    #[test]
    fn mixed_string_width() {
        assert_eq!(str_width("ab漢"), 4);
    }
}
