use core::slice::Iter;

pub struct BitIter<'a> {
    fgcolor: u32,
    bkcolor: u32,
    _color_pattern: EndianPattern,
    _dst_pattern: EndianPattern,
    src: Iter<'a, u8>,
    read_mask: u8,
    byte_per_pixel: u32,
    buffer: u32,
    current: u8,
    left_byte: u32,
    done: bool,
    consumed_bit: u32,
    image_width: u32,
}

impl<'a> BitIter<'a> {
    pub fn new(
        fgcolor: u32,
        bkcolor: u32,
        dst_pattern: EndianPattern,
        color_pattern: EndianPattern,
        byte_per_pixel: u32,
        src: Iter<'a, u8>,
        image_width: u32,
    ) -> Self {
        let mut fgcolor = fgcolor;
        let mut bkcolor = bkcolor;
        if dst_pattern != color_pattern {
            fgcolor = Self::reverse(fgcolor, byte_per_pixel);
            bkcolor = Self::reverse(bkcolor, byte_per_pixel);
        }

        let mut ans = Self {
            fgcolor,
            bkcolor,
            _color_pattern: color_pattern,
            _dst_pattern: dst_pattern,
            src,
            read_mask: 0b10000000,
            byte_per_pixel,
            buffer: 0,
            current: 0,
            left_byte: 0,
            done: false,
            consumed_bit: 0,
            image_width,
        };
        ans.current = *ans.src.next().unwrap();
        return ans;
    }

    fn reverse(num: u32, byte_per_pixel: u32) -> u32 {
        let mask = 0x000000ff;
        let mut ans = 0;
        let mut num = num;
        for _ in 0..3 {
            ans |= mask & num;
            ans <<= 8;
            num >>= 8;
        }
        ans |= mask & num;
        ans >>= (4 - byte_per_pixel) * 8;
        return ans;
    }

    fn move_mask(&mut self) -> bool {
        self.consumed_bit += 1;
        self.read_mask >>= 1;
        if self.read_mask == 0b000000000 {
            self.read_mask = 0b10000000;
            self.current = match self.src.next() {
                Some(x) => *x,
                None => {
                    return false;
                }
            };
            return true;
        } else {
            return true;
        }
    }

    fn full_buffer(&mut self) -> Result<PixelLineStatus, PixelLineStatus> {
        let same_endian = if self._dst_pattern == self._color_pattern {
            1
        } else {
            -1
        };
        let mut color = self.read_bit() << (self.left_byte << 3);
        let mut buffer_pointer = if self._dst_pattern == self._color_pattern {
            0
        } else {
            3
        };
        let mask = 0x000000ff << ((self.byte_per_pixel - 1) << 3);
        let mut temp;
        // while buffer_pointer >= 0 && buffer_pointer <= 3 {
        while (0..=3).contains(&buffer_pointer) {
            if self.consumed_bit >= self.image_width {
                self.consumed_bit = 0;
                return Ok(PixelLineStatus::Full(self.buffer));
            }
            temp = color & mask;
            color <<= 8;
            temp <<= (4 - self.byte_per_pixel) * 8;
            temp >>= buffer_pointer * 8;
            self.buffer |= temp;
            buffer_pointer += same_endian;
            self.left_byte += 1;
            if self.left_byte >= self.byte_per_pixel {
                self.left_byte = 0;
                if !self.move_mask() {
                    return Err(PixelLineStatus::Full(self.buffer));
                }
                color = self.read_bit();
            }
        }
        if self.consumed_bit >= self.image_width {
            self.consumed_bit = 0;
            return Ok(PixelLineStatus::Full(self.buffer));
        }
        return Ok(PixelLineStatus::NotFull(self.buffer));
    }

    fn read_bit(&self) -> u32 {
        match self.read_mask & self.current {
            0 => self.bkcolor,
            _ => self.fgcolor,
        }
    }
}

impl Iterator for BitIter<'_> {
    type Item = (u32, bool);
    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        match self.full_buffer() {
            Ok(x) => {
                self.buffer = 0;
                return Some(x.unwarp());
            }
            Err(x) => {
                self.done = true;
                return Some(x.unwarp());
            }
        }
    }
}
#[derive(PartialEq, PartialOrd)]
pub enum EndianPattern {
    Big,
    Little,
}

pub enum PixelLineStatus {
    Full(u32),
    NotFull(u32),
}

impl PixelLineStatus {
    pub fn unwarp(self) -> (u32, bool) {
        match self {
            PixelLineStatus::Full(x) => (x, true),
            PixelLineStatus::NotFull(x) => (x, false),
        }
    }
}
