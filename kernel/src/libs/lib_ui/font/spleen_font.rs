use crate::libs::lib_ui::textui::GlyphMapping;

use super::{BitmapFont, RawBitMap, Size};

struct Mapping;

impl GlyphMapping for Mapping {
    #[inline(always)]
    fn index(&self, c: char) -> usize {
        let c = c as usize;
        match c {
            0..=255 => c,
            _ => '?' as usize - ' ' as usize,
        }
    }
}

const SPLEEN_GLYPH_MAPPING: Mapping = Mapping;

#[allow(non_upper_case_globals)]
pub const SPLEEN_FONT_8x16: BitmapFont<'static> = BitmapFont::new(
    RawBitMap::new(include_bytes!("binaries/spleen-8x16.raw_bytes"), 128),
    &SPLEEN_GLYPH_MAPPING,
    Size::new(8, 16),
);
