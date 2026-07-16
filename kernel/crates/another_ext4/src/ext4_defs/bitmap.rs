pub struct Bitmap<'a>(&'a mut [u8]);

impl<'a> Bitmap<'a> {
    pub fn new(bmap: &'a mut [u8], nbits: usize) -> Self {
        Self(&mut bmap[..nbits.div_ceil(8)])
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

    /// Find the first contiguous clear run of exactly `count` bits in
    /// `[start, end)`.  The bitmap is not modified.
    pub fn first_clear_run_in(
        bitmap: &[u8],
        nbits: usize,
        start: usize,
        end: usize,
        count: usize,
    ) -> Option<usize> {
        let end = core::cmp::min(end, core::cmp::min(nbits, bitmap.len() * 8));
        if count == 0 || start > end || count > end.saturating_sub(start) {
            return None;
        }
        let last_start = end - count;
        let mut candidate = start;
        while candidate <= last_start {
            let mut offset = 0;
            while offset < count {
                let bit = candidate + offset;
                if bitmap[bit / 8] & (1 << (bit % 8)) != 0 {
                    break;
                }
                offset += 1;
            }
            if offset == count {
                return Some(candidate);
            }
            candidate = candidate.checked_add(offset + 1)?;
        }
        None
    }

    /// Find the first clear bit in the range `[start, end)` and set it if found
    pub fn find_and_set_first_clear_bit(&mut self, start: usize, end: usize) -> Option<usize> {
        self.first_clear_bit(start, end).inspect(|&bit| {
            self.set_bit(bit);
        })
    }
}

#[cfg(test)]
mod tests {
    use super::Bitmap;

    #[test]
    fn clear_run_respects_fragmentation_and_bounds() {
        let bitmap = [0b0010_1101, 0b1111_0000];
        assert_eq!(Bitmap::first_clear_run_in(&bitmap, 16, 0, 16, 2), Some(6));
        assert_eq!(Bitmap::first_clear_run_in(&bitmap, 16, 8, 16, 4), Some(8));
        assert_eq!(Bitmap::first_clear_run_in(&bitmap, 10, 8, 16, 3), None);
        assert_eq!(Bitmap::first_clear_run_in(&bitmap, 16, 0, 16, 0), None);
    }
}
