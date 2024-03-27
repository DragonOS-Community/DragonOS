use super::textui::GlyphMapping;

pub mod spleen_font;

pub use spleen_font::SPLEEN_FONT_8x16 as FONT_8x16;

/// Stores the font bitmap and some additional info for each font.
#[derive(Clone, Copy)]
pub struct BitmapFont<'a> {
    /// The raw bitmap data for the font.
    bitmap: RawBitMap<'a>,

    /// The char to glyph mapping.
    glyph_mapping: &'a dyn GlyphMapping,

    /// The size of each character in the raw bitmap data.
    size: Size,

    bytes_per_char: usize,
}

#[allow(dead_code)]
impl<'a> BitmapFont<'a> {
    pub const fn new(
        bitmap: RawBitMap<'a>,
        glyph_mapping: &'a dyn GlyphMapping,
        size: Size,
    ) -> Self {
        Self {
            bitmap,
            glyph_mapping,
            size,
            bytes_per_char: (size.width + 7) / 8 * size.height,
        }
    }
    /// Return the width of each character.
    pub const fn width(&self) -> u32 {
        self.size.width as u32
    }

    /// Return the height of each character.
    pub const fn height(&self) -> u32 {
        self.size.height as u32
    }

    #[inline(always)]
    pub fn char_map(&self, character: char) -> &'a [u8] {
        //获得ASCII的index
        let index = self.glyph_mapping.index(character);
        let pos = index * self.bytes_per_char;

        return &self.bitmap.data[pos..pos + self.bytes_per_char];
    }
}

#[derive(Clone, Copy)]
pub struct Size {
    pub width: usize,
    pub height: usize,
}

impl Size {
    pub const fn new(width: usize, height: usize) -> Self {
        Self { width, height }
    }
}

#[derive(Clone, Copy)]
pub struct RawBitMap<'a> {
    pub data: &'a [u8],
    pub size: Size,
}

#[allow(dead_code)]
impl RawBitMap<'_> {
    pub const fn new(data: &'static [u8], width: usize) -> Self {
        let size = Size {
            width: 128,
            height: data.len() / width / 8,
        };
        Self { data, size }
    }

    pub const fn size(&self) -> Size {
        self.size
    }

    pub const fn len(&self) -> usize {
        self.data.len()
    }
}
