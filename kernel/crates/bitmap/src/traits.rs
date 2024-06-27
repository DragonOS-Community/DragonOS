use core::ops::{BitAnd, BitAndAssign, BitOrAssign, Not};

/// A trait that defines generalised operations on a `Bits::Store` type.
pub trait BitOps:
    BitAndAssign + Sized + Copy + PartialEq + Not + BitOrAssign + BitOrAssign + BitAnd
{
    fn get(bits: &Self, index: usize) -> bool;
    fn set(bits: &mut Self, index: usize, value: bool) -> bool;
    fn set_value(bits: &mut Self, value: Self);
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
    fn zero() -> Self;
    fn max() -> Self;
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
            fn set_value(bits: &mut Self, value: Self) {
                *bits = value;
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
                    let intermediate = bits | (<$target>::MAX.overflowing_shl(index as u32).0);

                    if intermediate == <$target>::MAX {
                        None
                    } else {
                        Some(<$target>::BITS as usize - 1 - (intermediate.leading_ones() as usize))
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
                if shift == <$target>::BITS as usize {
                    <$target>::MAX
                } else {
                    (1 << shift) - 1
                }
            }

            #[inline]
            fn bit_size() -> usize {
                <$target>::BITS as usize
            }

            #[inline]
            fn zero() -> Self {
                0
            }

            #[inline]
            fn max() -> Self {
                <$target>::MAX
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
    /// 获取指定index的位
    ///
    /// ## 返回
    ///
    /// - `Some(true)` - 该位为1
    /// - `Some(false)` - 该位为0
    /// - `None` - index超出范围
    fn get(&self, index: usize) -> Option<bool>;

    /// 设置指定index的位，并返回该位之前的值
    ///
    /// ## 参数
    ///
    /// - `index` - 位的index
    /// - `value` - 位的新值
    ///
    /// ## 返回
    ///
    /// - `Some(true)` - 该位之前为1
    /// - `Some(false)` - 该位之前为0
    /// - `None` - index超出范围
    fn set(&mut self, index: usize, value: bool) -> Option<bool>;

    /// 将所有位设置为指定值
    fn set_all(&mut self, value: bool);

    /// 获取bitmap的长度（以位为单位）
    ///
    /// ## Example
    ///
    /// ```
    /// use bitmap::StaticBitmap;
    /// use bitmap::traits::BitMapOps;
    ///
    /// let mut bitmap = StaticBitmap::<34>::new();
    /// assert_eq!(bitmap.len(), 34);
    /// ```
    ///
    fn len(&self) -> usize;
    /// 获取bitmap的大小（以字节为单位）
    fn size(&self) -> usize;

    /// 获取第一个为1的位的index
    ///
    /// ## 返回
    ///
    /// - `Some(index)` - 第一个为1的位的index
    /// - `None` - 不存在为1的位
    fn first_index(&self) -> Option<usize>;

    /// 获取第一个为0的位的index
    ///
    /// ## 返回
    ///
    /// - `Some(index)` - 第一个为0的位的index
    /// - `None` - 不存在为0的位
    fn first_false_index(&self) -> Option<usize>;

    /// 获取最后一个为1的位的index
    ///
    /// ## 返回
    ///
    /// - `Some(index)` - 最后一个为1的位的index
    /// - `None` - 不存在为1的位
    fn last_index(&self) -> Option<usize>;

    /// 获取最后一个为0的位的index
    ///
    /// ## 返回
    ///
    /// - `Some(index)` - 最后一个为0的位的index
    /// - `None` - 不存在为0的位
    fn last_false_index(&self) -> Option<usize>;

    /// 获取指定index之后第一个为1的位的index
    fn next_index(&self, index: usize) -> Option<usize>;

    /// 获取指定index之后第一个为0的位的index
    fn next_false_index(&self, index: usize) -> Option<usize>;

    /// 获取指定index之前第一个为1的位的index
    fn prev_index(&self, index: usize) -> Option<usize>;

    /// 获取指定index之前第一个为0的位的index
    fn prev_false_index(&self, index: usize) -> Option<usize>;

    /// 反转bitmap
    fn invert(&mut self);

    /// 判断bitmap是否满了
    fn is_full(&self) -> bool;

    /// 判断bitmap是否为空
    fn is_empty(&self) -> bool;

    /// # Safety
    /// *不应直接修改字节数组*
    ///
    /// 将bitmap转换为字节数组
    unsafe fn as_bytes(&self) -> &[u8];
}
