#![no_std]
#![feature(core_intrinsics)]
#![feature(generic_const_exprs)]
use core::{intrinsics::unlikely, marker::PhantomData};

use traits::{BitMapOps, BitOps};

mod traits;
mod utils;

/// 静态位图
///
/// 该位图的大小在编译时确定，不可变
pub struct StaticBitMap<const N: usize>
where
    [(); N / (usize::BITS as usize)]:,
{
    data: [usize; N / (usize::BITS as usize)],
    core: BitMapCore<usize, N>,
}

impl<const N: usize> StaticBitMap<N>
where
    [(); N / (usize::BITS as usize)]:,
{
    /// 创建一个新的静态位图
    pub const fn new() -> Self {
        Self {
            data: [0; N / (usize::BITS as usize)],
            core: BitMapCore {
                phantom: PhantomData,
            },
        }
    }
}

impl<const N: usize> BitMapOps<usize> for StaticBitMap<N>
where
    [(); N / (usize::BITS as usize)]:,
{
    #[inline]
    fn get(&self, index: usize) -> Option<bool> {
        return self.core.get(&self.data, index);
    }

    #[inline]
    fn set(&mut self, index: usize, value: bool) -> Option<bool> {
        return self.core.set(&mut self.data, index, value);
    }

    #[inline]
    fn len(&self) -> usize {
        N
    }

    #[inline]
    fn size(&self) -> usize {
        core::mem::size_of::<Self>()
    }

    #[inline]
    fn first_index(&self) -> Option<usize> {
        self.core.first_index(&self.data)
    }

    #[inline]
    fn first_false_index(&self) -> Option<usize> {
        self.core.first_false_index(&self.data)
    }

    #[inline]
    fn last_index(&self) -> Option<usize> {
        self.core.last_index(&self.data)
    }

    #[inline]
    fn last_false_index(&self) -> Option<usize> {
        self.core.last_false_index(&self.data)
    }

    #[inline]
    fn next_index(&self, index: usize) -> Option<usize> {
        self.core.next_index(&self.data, index)
    }

    #[inline]
    fn next_false_index(&self, index: usize) -> Option<usize> {
        self.core.next_false_index(&self.data, index)
    }

    #[inline]
    fn prev_index(&self, index: usize) -> Option<usize> {
        self.core.prev_index(&self.data, index)
    }

    #[inline]
    fn prev_false_index(&self, index: usize) -> Option<usize> {
        self.core.prev_false_index(&self.data, index)
    }

    fn invert(&mut self) {
        todo!()
    }

    fn is_full(&self) -> bool {
        todo!()
    }

    fn is_empty(&self) -> bool {
        todo!()
    }

    fn as_bytes(&self) -> &[u8] {
        todo!()
    }
}

struct BitMapCore<T: traits::BitOps, const N: usize> {
    phantom: PhantomData<T>,
}

impl<T: traits::BitOps, const N: usize> BitMapCore<T, N> {
    /// 获取位图中的某一位
    fn get(&self, data: &[T], index: usize) -> Option<bool> {
        if unlikely(index >= N) {
            return None;
        }
        let element_index = index / T::bit_size();
        let bit_index = index % T::bit_size();

        let element = data.get(element_index)?;
        let bit = <T as traits::BitOps>::get(element, bit_index);

        Some(bit)
    }

    /// 设置位图中的某一位
    fn set(&self, data: &mut [T], index: usize, value: bool) -> Option<bool> {
        if unlikely(index >= N) {
            return None;
        }
        let element_index = index / T::bit_size();
        let bit_index = index % T::bit_size();

        let element = data.get_mut(element_index)?;
        let bit = <T as traits::BitOps>::set(element, bit_index, value);

        Some(bit)
    }

    /// 获取位图中第一个为1的位
    fn first_index(&self, data: &[T]) -> Option<usize> {
        for (i, element) in data.iter().enumerate() {
            let bit = <T as traits::BitOps>::first_index(element);
            if bit.is_some() {
                return Some(i * T::bit_size() + bit.unwrap());
            }
        }

        None
    }

    /// 获取位图中第一个为0的位
    fn first_false_index(&self, data: &[T]) -> Option<usize> {
        for (i, element) in data.iter().enumerate() {
            let bit = <T as traits::BitOps>::first_false_index(element);
            if bit.is_some() {
                return Some(i * T::bit_size() + bit.unwrap());
            }
        }

        None
    }

    /// 获取位图中最后一个为1的位
    fn last_index(&self, data: &[T]) -> Option<usize> {
        for (i, element) in data.iter().enumerate().rev() {
            let bit = <T as traits::BitOps>::last_index(element);
            if bit.is_some() {
                return Some(i * T::bit_size() + bit.unwrap());
            }
        }

        None
    }

    /// 获取位图中最后一个为0的位
    ///
    /// ## 参数
    ///
    /// - `data`：位图数据
    /// - `n`：位图有效位数
    fn last_false_index(&self, data: &[T]) -> Option<usize> {
        let mut iter = data.iter().rev();
        let mut last_element = *iter.next()?;

        // 对最后一个元素进行特殊处理，因为最后一个元素可能不是满的
        last_element &= T::make_mask(N % T::bit_size());

        if let Some(bit) = <T as traits::BitOps>::last_false_index(&last_element) {
            return Some((data.len() - 1) * T::bit_size() + bit);
        }

        for element in iter {
            if let Some(bit) = <T as traits::BitOps>::last_false_index(element) {
                return self.make_index((data.len() - 1) * T::bit_size() + bit);
            }
        }

        None
    }

    /// 获取位图中下一个为1的位
    fn next_index(&self, data: &[T], index: usize) -> Option<usize> {
        if unlikely(index >= N) {
            return None;
        }

        let element_index = index / T::bit_size();
        let bit_index = index % T::bit_size();

        let element = data.get(element_index)?;
        if let Some(bit) = <T as traits::BitOps>::next_index(element, bit_index) {
            return self.make_index(element_index * T::bit_size() + bit);
        }

        for (i, element) in data.iter().enumerate().skip(element_index + 1) {
            if let Some(bit) = <T as traits::BitOps>::first_index(element) {
                return self.make_index(i * T::bit_size() + bit);
            }
        }

        None
    }

    /// 获取位图中下一个为0的位
    fn next_false_index(&self, data: &[T], index: usize) -> Option<usize> {
        if unlikely(index >= N) {
            return None;
        }

        let element_index = index / T::bit_size();
        let bit_index = index % T::bit_size();

        let element = data.get(element_index)?;
        if let Some(bit) = <T as traits::BitOps>::next_false_index(element, bit_index) {
            return self.make_index(element_index * T::bit_size() + bit);
        }

        for (i, element) in data.iter().enumerate().skip(element_index + 1) {
            if let Some(bit) = <T as traits::BitOps>::first_false_index(element) {
                return self.make_index(i * T::bit_size() + bit);
            }
        }

        None
    }

    /// 获取位图中上一个为1的位
    fn prev_index(&self, data: &[T], index: usize) -> Option<usize> {
        if unlikely(index >= N) {
            return None;
        }
        let element_index = index / T::bit_size();
        let bit_index = index % T::bit_size();

        let element = data.get(element_index)?;
        if let Some(bit) = <T as traits::BitOps>::prev_index(element, bit_index) {
            return self.make_index(element_index * T::bit_size() + bit);
        }

        for (i, element) in data.iter().enumerate().take(element_index).rev() {
            if let Some(bit) = <T as traits::BitOps>::last_index(element) {
                return self.make_index(i * T::bit_size() + bit);
            }
        }

        None
    }

    fn prev_false_index(&self, data: &[T], index: usize) -> Option<usize> {
        let element_index = index / T::bit_size();
        let bit_index = index % T::bit_size();

        let element = data.get(element_index)?;
        if let Some(bit) = <T as traits::BitOps>::prev_false_index(element, bit_index) {
            return self.make_index(element_index * T::bit_size() + bit);
        }

        for (i, element) in data.iter().enumerate().take(element_index).rev() {
            if let Some(bit) = <T as traits::BitOps>::last_false_index(element) {
                return self.make_index(i * T::bit_size() + bit);
            }
        }

        None
    }

    fn make_index(&self, index: usize) -> Option<usize> {
        if unlikely(index >= N) {
            return None;
        }

        Some(index)
    }
}

pub fn add(left: usize, right: usize) -> usize {
    left + right
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_works() {
        let result = add(2, 2);
        assert_eq!(result, 4);
    }
}
