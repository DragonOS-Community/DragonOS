use core::ops::BitAndAssign;

/// A trait that defines generalised operations on a `Bits::Store` type.
pub trait BitOps: BitAndAssign + Sized + Copy {
    fn get(bits: &Self, index: usize) -> bool;
    fn set(bits: &mut Self, index: usize, value: bool) -> bool;
    fn len(bits: &Self) -> usize;
    fn first_index(bits: &Self) -> Option<usize>;
    fn first_false_index(bits: &Self) -> Option<usize>;
    fn last_index(bits: &Self) -> Option<usize>;
    fn last_false_index(bits: &Self) -> Option<usize>;
    fn next_index(bits: &Self, index: usize) -> Option<usize>;
    fn next_false_index(bits: &Self, index: usize) -> Option<usize>;
    fn prev_index(bits: &Self, index: usize) -> Option<usize>;
    fn prev_false_index(bits: &Self, index: usize) -> Option<usize>;
    fn bit_and(bits: &mut Self, other_bits: &Self);
    fn bit_or(bits: &mut Self, other_bits: &Self);
    fn bit_xor(bits: &mut Self, other_bits: &Self);
    fn invert(bits: &mut Self);
    fn make_mask(shift: usize) -> Self;
    fn bit_size() -> usize;
}

macro_rules! bitops_for {
    ($target:ty) => {
        impl BitOps for $target {
            #[inline]
            fn get(bits: &Self, index: usize) -> bool {
                bits & (1 << index) != 0
            }

            #[inline]
            fn set(bits: &mut Self, index: usize, value: bool) -> bool {
                let mask = 1 << index;
                let prev = *bits & mask;
                if value {
                    *bits |= mask;
                } else {
                    *bits &= !mask;
                }
                prev != 0
            }

            #[inline]
            fn len(bits: &Self) -> usize {
                bits.count_ones() as usize
            }

            #[inline]
            fn first_index(bits: &Self) -> Option<usize> {
                if *bits == 0 {
                    None
                } else {
                    Some(bits.trailing_zeros() as usize)
                }
            }

            #[inline]
            fn first_false_index(bits: &Self) -> Option<usize> {
                if *bits == <$target>::MAX {
                    None
                } else {
                    Some(bits.trailing_ones() as usize)
                }
            }

            #[inline]
            fn last_index(bits: &Self) -> Option<usize> {
                if *bits == 0 {
                    None
                } else {
                    Some(<$target>::BITS as usize - 1 - (bits.leading_zeros() as usize))
                }
            }

            #[inline]
            fn last_false_index(bits: &Self) -> Option<usize> {
                if *bits == <$target>::MAX {
                    None
                } else {
                    Some(<$target>::BITS as usize - 1 - bits.leading_ones() as usize)
                }
            }

            #[inline]
            fn next_index(bits: &Self, index: usize) -> Option<usize> {
                if *bits == 0 || index >= <$target>::BITS as usize - 1 {
                    None
                } else {
                    let intermediate =
                        (*bits & (<$target>::MAX.overflowing_shl(1 + index as u32).0));

                    if intermediate == 0 {
                        None
                    } else {
                        Some(intermediate.trailing_zeros() as usize)
                    }
                }
            }

            #[inline]
            fn next_false_index(bits: &Self, index: usize) -> Option<usize> {
                if *bits == <$target>::MAX || index >= <$target>::BITS as usize - 1 {
                    None
                } else {
                    let intermediate = (*bits | ((1 << (index + 1)) - 1));

                    if intermediate == <$target>::MAX {
                        None
                    } else {
                        Some(intermediate.trailing_ones() as usize)
                    }
                }
            }

            #[inline]
            fn prev_index(bits: &Self, index: usize) -> Option<usize> {
                if *bits == 0 || index == 0 {
                    None
                } else {
                    let intermediate = bits & ((1 << index) - 1);

                    if intermediate == 0 {
                        None
                    } else {
                        Some(<$target>::BITS as usize - 1 - (intermediate.leading_zeros() as usize))
                    }
                }
            }

            #[inline]
            fn prev_false_index(bits: &Self, index: usize) -> Option<usize> {
                if *bits == <$target>::MAX || index == 0 {
                    None
                } else {
                    let intermediate = bits | (<$target>::MAX.overflowing_shl(index as u32 - 1).0);

                    if intermediate == <$target>::MAX {
                        None
                    } else {
                        Some(<$target>::BITS as usize - 1 - (intermediate.leading_zeros() as usize))
                    }
                }
            }

            #[inline]
            fn bit_and(bits: &mut Self, other_bits: &Self) {
                *bits &= *other_bits;
            }

            #[inline]
            fn bit_or(bits: &mut Self, other_bits: &Self) {
                *bits |= *other_bits;
            }

            #[inline]
            fn bit_xor(bits: &mut Self, other_bits: &Self) {
                *bits ^= *other_bits;
            }

            #[inline]
            fn invert(bits: &mut Self) {
                *bits = !*bits;
            }

            #[inline]
            fn make_mask(shift: usize) -> Self {
                (1 << shift) - 1
            }

            #[cfg(feature = "std")]
            fn to_hex(bits: &Self) -> String {
                format!("{:x}", bits)
            }

            #[inline]
            fn bit_size() -> usize {
                <$target>::BITS as usize
            }
        }
    };
}

// 为 `u8` 、 `u16` 、 `u32` 和 `u64` 实现 `BitOps` trait
bitops_for!(u8);
bitops_for!(u16);
bitops_for!(u32);
bitops_for!(u64);
bitops_for!(usize);

/// Bitmap应当实现的trait
pub trait BitMapOps<T: BitOps> {
    fn get(&self, index: usize) -> Option<bool>;
    fn set(&mut self, index: usize, value: bool) -> Option<bool>;
    /// 获取bitmap的长度（以位为单位）
    ///
    /// ## Example
    ///
    /// ```
    /// use bitmap::BitMap;
    ///
    /// let mut bitmap = BitMap::<34>::new(10);
    /// assert_eq!(bitmap.len(), 34);
    /// ```
    ///
    fn len(&self) -> usize;
    /// 获取bitmap的大小（以字节为单位）
    fn size(&self) -> usize;
    fn first_index(&self) -> Option<usize>;
    fn first_false_index(&self) -> Option<usize>;
    fn last_index(&self) -> Option<usize>;
    fn last_false_index(&self) -> Option<usize>;
    fn next_index(&self, index: usize) -> Option<usize>;
    fn next_false_index(&self, index: usize) -> Option<usize>;
    fn prev_index(&self, index: usize) -> Option<usize>;
    fn prev_false_index(&self, index: usize) -> Option<usize>;
    fn invert(&mut self);
    fn is_full(&self) -> bool;
    fn is_empty(&self) -> bool;
    fn as_bytes(&self) -> &[u8];
}
