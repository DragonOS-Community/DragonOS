pub struct Bitmap<'a>(&'a mut [u8]);

impl<'a> Bitmap<'a> {
    pub fn new(bmap: &'a mut [u8], nbits: usize) -> Self {
        Self(&mut bmap[..nbits.div_ceil(8)])
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.0
    }

    pub fn is_bit_clear(&self, bit: usize) -> bool {
        self.0[bit / 8] & (1 << (bit % 8)) == 0
    }

    pub fn set_bit(&mut self, bit: usize) {
        self.0[bit / 8] |= 1 << (bit % 8);
    }

    pub fn clear_bit(&mut self, bit: usize) {
        self.0[bit / 8] &= !(1 << (bit % 8));
    }

    /// Find the first clear bit in the range `[start, end)`
    pub fn first_clear_bit(&self, start: usize, end: usize) -> Option<usize> {
        let end = core::cmp::min(end, self.0.len() * 8);
        (start..end).find(|&i| self.is_bit_clear(i))
    }

    /// Find the first clear bit in the range `[start, end)` and set it if found
    pub fn find_and_set_first_clear_bit(&mut self, start: usize, end: usize) -> Option<usize> {
        self.first_clear_bit(start, end).inspect(|&bit| {
            self.set_bit(bit);
        })
    }
}
