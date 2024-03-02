use self::font_type::vga8x16::FONT_VGA_8X16;

pub mod font_type;

pub struct FontDesc {
    pub index: usize,
    pub name: &'static str,
    pub width: u32,
    pub height: u32,
    pub char_count: u32,
    pub data: &'static [u8],
}

impl FontDesc {
    pub fn get_default_font(_xres: u32, _yres: u32, _font_w: u32, _font_h: u32) -> &'static Self {
        // todo: 目前先直接返回一个字体
        &FONT_VGA_8X16
    }

    pub const DOUBLE_WIDTH_RANGE: &'static [(u32, u32)] = &[
        (0x1100, 0x115F),
        (0x2329, 0x232A),
        (0x2E80, 0x303E),
        (0x3040, 0xA4CF),
        (0xAC00, 0xD7A3),
        (0xF900, 0xFAFF),
        (0xFE10, 0xFE19),
        (0xFE30, 0xFE6F),
        (0xFF00, 0xFF60),
        (0xFFE0, 0xFFE6),
        (0x20000, 0x2FFFD),
        (0x30000, 0x3FFFD),
    ];
    pub fn is_double_width(ch: u32) -> bool {
        if ch < Self::DOUBLE_WIDTH_RANGE.first().unwrap().0
            || ch > Self::DOUBLE_WIDTH_RANGE.last().unwrap().1
        {
            return false;
        }

        for (first, last) in Self::DOUBLE_WIDTH_RANGE {
            if ch > *first && ch < *last {
                return true;
            }
        }

        false
    }
}
