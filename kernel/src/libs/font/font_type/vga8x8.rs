use crate::libs::font::FontDesc;

#[allow(dead_code)]
pub const FONT_VGA_8X8: FontDesc = FontDesc {
    index: 0,
    name: "VGA8x8",
    width: 8,
    height: 8,
    char_count: 256,
    data: include_bytes!("../bin/VGA_8X8.bytes"),
};
