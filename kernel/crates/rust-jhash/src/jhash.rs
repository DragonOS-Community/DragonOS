#![allow(clippy::unreadable_literal, clippy::identity_op)]

use core::convert::TryInto as _;

pub fn jhash_mix(a: &mut u32, b: &mut u32, c: &mut u32) {
    *a = a.wrapping_sub(*c);
    *a ^= c.rotate_left(4);
    *c = c.wrapping_add(*b);

    *b = b.wrapping_sub(*a);
    *b ^= a.rotate_left(6);
    *a = a.wrapping_add(*c);

    *c = c.wrapping_sub(*b);
    *c ^= b.rotate_left(8);
    *b = b.wrapping_add(*a);

    *a = a.wrapping_sub(*c);
    *a ^= c.rotate_left(16);
    *c = c.wrapping_add(*b);

    *b = b.wrapping_sub(*a);
    *b ^= a.rotate_left(19);
    *a = a.wrapping_add(*c);

    *c = c.wrapping_sub(*b);
    *c ^= b.rotate_left(4);
    *b = b.wrapping_add(*a);
}

#[must_use]
pub fn jhash_final(mut a: u32, mut b: u32, mut c: u32) -> u32 {
    c ^= b;
    c = c.wrapping_sub(b.rotate_left(14));

    a ^= c;
    a = a.wrapping_sub(c.rotate_left(11));

    b ^= a;
    b = b.wrapping_sub(a.rotate_left(25));

    c ^= b;
    c = c.wrapping_sub(b.rotate_left(16));

    a ^= c;
    a = a.wrapping_sub(c.rotate_left(4));

    b ^= a;
    b = b.wrapping_sub(a.rotate_left(14));

    c ^= b;
    c = c.wrapping_sub(b.rotate_left(24));
    c
}

pub const JHASH_INITVAL: u32 = 0xdeadbeef;

#[must_use]
pub fn jhash(mut key: &[u8], initval: u32) -> u32 {
    let mut a = JHASH_INITVAL
        .wrapping_add(key.len() as u32)
        .wrapping_add(initval);
    let mut b = a;
    let mut c = a;

    while key.len() > 12 {
        a = a.wrapping_add(u32::from_ne_bytes(key[..4].try_into().unwrap()));
        b = b.wrapping_add(u32::from_ne_bytes(key[4..8].try_into().unwrap()));
        c = c.wrapping_add(u32::from_ne_bytes(key[8..12].try_into().unwrap()));
        jhash_mix(&mut a, &mut b, &mut c);
        key = &key[12..];
    }

    if key.is_empty() {
        return c;
    }

    c = c.wrapping_add((*key.get(11).unwrap_or(&0) as u32) << 24);
    c = c.wrapping_add((*key.get(10).unwrap_or(&0) as u32) << 16);
    c = c.wrapping_add((*key.get(9).unwrap_or(&0) as u32) << 8);
    c = c.wrapping_add((*key.get(8).unwrap_or(&0) as u32) << 0);

    b = b.wrapping_add((*key.get(7).unwrap_or(&0) as u32) << 24);
    b = b.wrapping_add((*key.get(6).unwrap_or(&0) as u32) << 16);
    b = b.wrapping_add((*key.get(5).unwrap_or(&0) as u32) << 8);
    b = b.wrapping_add((*key.get(4).unwrap_or(&0) as u32) << 0);

    a = a.wrapping_add((*key.get(3).unwrap_or(&0) as u32) << 24);
    a = a.wrapping_add((*key.get(2).unwrap_or(&0) as u32) << 16);
    a = a.wrapping_add((*key.get(1).unwrap_or(&0) as u32) << 8);
    a = a.wrapping_add((*key.first().unwrap_or(&0) as u32) << 0);

    jhash_final(a, b, c)
}

#[must_use]
pub fn jhash2(mut key: &[u32], initval: u32) -> u32 {
    let mut a = JHASH_INITVAL
        .wrapping_add(key.len() as u32)
        .wrapping_add(initval);
    let mut b = a;
    let mut c = a;

    /* Handle most of the key */
    while key.len() > 3 {
        a = a.wrapping_add(key[0]);
        b = b.wrapping_add(key[1]);
        c = c.wrapping_add(key[2]);
        jhash_mix(&mut a, &mut b, &mut c);
        key = &key[3..];
    }

    match key.len() {
        3 => {
            c = c.wrapping_add(key[2]);
            b = b.wrapping_add(key[1]);
            a = a.wrapping_add(key[0]);
        }
        2 => {
            b = b.wrapping_add(key[1]);
            a = a.wrapping_add(key[0]);
        }
        1 => {
            a = a.wrapping_add(key[0]);
        }
        0 => {
            return c;
        }
        _ => {
            unreachable!("Never happen");
        }
    }
    jhash_final(a, b, c)
}

#[must_use]
fn jhash_nwords(mut a: u32, mut b: u32, mut c: u32, initval: u32) -> u32 {
    a = a.wrapping_add(initval);
    b = b.wrapping_add(initval);
    c = c.wrapping_add(initval);

    jhash_final(a, b, c)
}

#[must_use]
pub fn jhash_3words(a: u32, b: u32, c: u32, initval: u32) -> u32 {
    jhash_nwords(
        a,
        b,
        c,
        initval.wrapping_add(JHASH_INITVAL).wrapping_add(3 << 2),
    )
}

#[must_use]
pub fn jhash_2words(a: u32, b: u32, initval: u32) -> u32 {
    jhash_nwords(
        a,
        b,
        0,
        initval.wrapping_add(JHASH_INITVAL).wrapping_add(2 << 2),
    )
}

#[must_use]
pub fn jhash_1words(a: u32, initval: u32) -> u32 {
    jhash_nwords(
        a,
        0,
        0,
        initval.wrapping_add(JHASH_INITVAL).wrapping_add(1 << 2),
    )
}

enum JHashBuffer {
    None,
    One(u32),
    Two(u32, u32),
}

impl Default for JHashBuffer {
    #[inline(always)]
    fn default() -> JHashBuffer {
        JHashBuffer::None
    }
}

#[derive(Default)]
pub struct JHasher {
    current: u32,
    buffer: JHashBuffer,
}

impl JHasher {
    #[inline(always)]
    #[must_use]
    pub fn new(initval: u32) -> JHasher {
        JHasher {
            current: initval,
            buffer: JHashBuffer::None,
        }
    }

    #[inline(always)]
    fn flush_buffer(&mut self) {
        match self.buffer {
            JHashBuffer::None => {}
            JHashBuffer::One(val1) => {
                self.current = jhash_1words(val1, self.current);
                self.buffer = JHashBuffer::None;
            }
            JHashBuffer::Two(val1, val2) => {
                self.current = jhash_2words(val1, val2, self.current);
                self.buffer = JHashBuffer::None;
            }
        }
    }
}

#[inline(always)]
#[cfg(target_endian = "little")]
fn split_u64(val: u64) -> (u32, u32) {
    (
        ((val >> 32) as u32),
        ((val & 0x00000000FFFFFFFF_u64) as u32),
    )
}

#[cfg(target_endian = "big")]
fn split_u64(val: u64) -> (u32, u32) {
    (
        ((val & 0x00000000FFFFFFFF_u64) as u32),
        ((val >> 32) as u32),
    )
}

impl core::hash::Hasher for JHasher {
    #[inline(always)]
    fn finish(&self) -> u64 {
        match self.buffer {
            JHashBuffer::None => self.current as u64,
            JHashBuffer::One(val1) => jhash_1words(val1, self.current) as u64,
            JHashBuffer::Two(val1, val2) => jhash_2words(val1, val2, self.current) as u64,
        }
    }
    #[inline(always)]
    fn write(&mut self, bytes: &[u8]) {
        self.flush_buffer();
        self.current = jhash(bytes, self.current);
    }
    #[inline(always)]
    fn write_u32(&mut self, val: u32) {
        match self.buffer {
            JHashBuffer::None => {
                self.buffer = JHashBuffer::One(val);
            }
            JHashBuffer::One(val1) => {
                self.buffer = JHashBuffer::Two(val1, val);
            }
            JHashBuffer::Two(val1, val2) => {
                self.current = jhash_3words(val1, val2, val, self.current);
                self.buffer = JHashBuffer::None;
            }
        }
    }
    #[inline(always)]
    fn write_u64(&mut self, val: u64) {
        let (val_a, val_b) = split_u64(val);

        match self.buffer {
            JHashBuffer::None => {
                self.buffer = JHashBuffer::Two(val_a, val_b);
            }
            JHashBuffer::One(val1) => {
                self.current = jhash_3words(val1, val_a, val_b, self.current);
                self.buffer = JHashBuffer::None;
            }
            JHashBuffer::Two(val1, val2) => {
                self.current = jhash_3words(val1, val2, val_a, self.current);
                self.buffer = JHashBuffer::One(val_b);
            }
        }
    }
    #[inline(always)]
    fn write_i32(&mut self, val: i32) {
        self.write_u32(val as u32)
    }
    #[inline(always)]
    fn write_i64(&mut self, val: i64) {
        self.write_u64(val as u64)
    }
}

#[derive(Default, Clone, Debug)]
pub struct JHashBuilder {
    initial_value: u32,
}

impl JHashBuilder {
    #[must_use]
    pub fn new(initial_value: u32) -> JHashBuilder {
        JHashBuilder { initial_value }
    }
}

impl core::hash::BuildHasher for JHashBuilder {
    type Hasher = JHasher;
    fn build_hasher(&self) -> Self::Hasher {
        JHasher::new(self.initial_value)
    }
}
