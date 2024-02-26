use crate::libs::font::FontDesc;

pub const FONT_VGA_8X16: FontDesc = FontDesc {
    index: 1,
    name: "VGA8x16",
    width: 8,
    height: 16,
    char_count: 256,
    data: include_bytes!("../bin/VGA_8X16.bytes"),
};
