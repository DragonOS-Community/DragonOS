use core::{intrinsics::unlikely, marker::PhantomData};

use crate::traits::BitOps;

#[derive(Debug, Clone)]
pub(crate) struct BitMapCore<T: BitOps> {
    phantom: PhantomData<T>,
}

impl<T: BitOps> BitMapCore<T> {
    pub const fn new() -> Self {
        Self {
            phantom: PhantomData,
        }
    }

    /// 获取位图中的某一位
    pub(crate) fn get(&self, n: usize, data: &[T], index: usize) -> Option<bool> {
        if unlikely(index >= n) {
            return None;
        }

        let element_index = index / T::bit_size();
        let bit_index = index % T::bit_size();

        let element = data.get(element_index)?;
        let bit = <T as BitOps>::get(element, bit_index);

        Some(bit)
    }

    /// 设置位图中的某一位
    pub(crate) fn set(&self, n: usize, data: &mut [T], index: usize, value: bool) -> Option<bool> {
        if unlikely(index >= n) {
            return None;
        }
        let element_index = index / T::bit_size();
        let bit_index = index % T::bit_size();

        let element = data.get_mut(element_index)?;
        let bit = <T as BitOps>::set(element, bit_index, value);

        Some(bit)
    }

    pub(crate) fn set_all(&self, n: usize, data: &mut [T], value: bool) {
        let val = if value { T::max() } else { T::zero() };
        for element in data.iter_mut() {
            *element = val;
        }

        // 特殊处理最后一个元素
        let last_element = data.last_mut().unwrap();
        let mask = T::make_mask(n % T::bit_size());
        if mask != T::zero() {
            *last_element &= mask;
        }
    }

    /// 获取位图中第一个为1的位
    pub(crate) fn first_index(&self, data: &[T]) -> Option<usize> {
        for (i, element) in data.iter().enumerate() {
            let bit = <T as BitOps>::first_index(element);
            if bit.is_some() {
                return Some(i * T::bit_size() + bit.unwrap());
            }
        }

        None
    }

    /// 获取位图中第一个为0的位
    pub(crate) fn first_false_index(&self, n: usize, data: &[T]) -> Option<usize> {
        for (i, element) in data.iter().enumerate() {
            if let Some(bit) = <T as BitOps>::first_false_index(element) {
                return self.make_index(n, i * T::bit_size() + bit);
            }
        }

        None
    }

    /// 获取位图中最后一个为1的位
    pub(crate) fn last_index(&self, n: usize, data: &[T]) -> Option<usize> {
        for (i, element) in data.iter().enumerate().rev() {
            if let Some(bit) = <T as BitOps>::last_index(element) {
                return self.make_index(n, i * T::bit_size() + bit);
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
    pub(crate) fn last_false_index(&self, n: usize, data: &[T]) -> Option<usize> {
        let mut iter = data.iter().rev();
        let mut last_element = *iter.next()?;

        // 对最后一个元素进行特殊处理，因为最后一个元素可能不是满的
        let mut mask = T::make_mask(n % T::bit_size());
        if mask != T::zero() {
            <T as BitOps>::invert(&mut mask);

            last_element |= mask;
        }

        if let Some(bit) = <T as BitOps>::last_false_index(&last_element) {
            return self.make_index(n, (data.len() - 1) * T::bit_size() + bit);
        }

        for element in iter {
            if let Some(bit) = <T as BitOps>::last_false_index(element) {
                return self.make_index(n, (data.len() - 1) * T::bit_size() + bit);
            }
        }

        None
    }

    /// 获取位图中下一个为1的位
    pub(crate) fn next_index(&self, n: usize, data: &[T], index: usize) -> Option<usize> {
        if unlikely(index >= n) {
            return None;
        }

        let element_index = index / T::bit_size();
        let bit_index = index % T::bit_size();

        let element = data.get(element_index)?;
        if let Some(bit) = <T as BitOps>::next_index(element, bit_index) {
            return self.make_index(n, element_index * T::bit_size() + bit);
        }

        for (i, element) in data.iter().enumerate().skip(element_index + 1) {
            if let Some(bit) = <T as BitOps>::first_index(element) {
                return self.make_index(n, i * T::bit_size() + bit);
            }
        }

        None
    }

    /// 获取位图中下一个为0的位
    pub(crate) fn next_false_index(&self, n: usize, data: &[T], index: usize) -> Option<usize> {
        if unlikely(index >= n) {
            return None;
        }

        let element_index = index / T::bit_size();
        let bit_index = index % T::bit_size();

        let element = data.get(element_index)?;
        if let Some(bit) = <T as BitOps>::next_false_index(element, bit_index) {
            return self.make_index(n, element_index * T::bit_size() + bit);
        }

        for (i, element) in data.iter().enumerate().skip(element_index + 1) {
            if let Some(bit) = <T as BitOps>::first_false_index(element) {
                return self.make_index(n, i * T::bit_size() + bit);
            }
        }

        None
    }

    /// 获取位图中上一个为1的位
    pub(crate) fn prev_index(&self, n: usize, data: &[T], index: usize) -> Option<usize> {
        if unlikely(index >= n) {
            return None;
        }
        let element_index = index / T::bit_size();
        let bit_index = index % T::bit_size();

        let element = data.get(element_index)?;
        if let Some(bit) = <T as BitOps>::prev_index(element, bit_index) {
            return self.make_index(n, element_index * T::bit_size() + bit);
        }

        for (i, element) in data.iter().enumerate().take(element_index).rev() {
            if let Some(bit) = <T as BitOps>::last_index(element) {
                return self.make_index(n, i * T::bit_size() + bit);
            }
        }

        None
    }

    pub(crate) fn prev_false_index(&self, n: usize, data: &[T], index: usize) -> Option<usize> {
        let element_index = index / T::bit_size();
        let bit_index = index % T::bit_size();

        let element = data.get(element_index)?;
        if let Some(bit) = <T as BitOps>::prev_false_index(element, bit_index) {
            return self.make_index(n, element_index * T::bit_size() + bit);
        }

        for (i, element) in data.iter().enumerate().take(element_index).rev() {
            if let Some(bit) = <T as BitOps>::last_false_index(element) {
                return self.make_index(n, i * T::bit_size() + bit);
            }
        }

        None
    }

    pub(crate) fn invert(&self, n: usize, data: &mut [T]) {
        for element in data.iter_mut() {
            <T as BitOps>::invert(element);
        }

        // 特殊处理最后一个元素

        let last_element = data.last_mut().unwrap();
        let mask = T::make_mask(n % T::bit_size());
        if mask != T::zero() {
            *last_element &= mask;
        }
    }

    pub(crate) fn is_full(&self, n: usize, data: &[T]) -> bool {
        let mut iter = data.iter().peekable();
        while let Some(element) = iter.next() {
            if iter.peek().is_none() {
                // 这是最后一个元素，进行特殊处理
                let mut element = *element;
                let mut mask = T::make_mask(n % T::bit_size());
                if mask == T::zero() {
                    mask = T::max();
                }

                T::bit_and(&mut element, &mask);
                if element == mask {
                    return true;
                }
            } else {
                if element != &T::make_mask(T::bit_size()) {
                    return false;
                }
            }
        }

        return false;
    }

    pub(crate) fn is_empty(&self, data: &[T]) -> bool {
        for element in data.iter() {
            if element != &T::zero() {
                return false;
            }
        }

        return true;
    }

    fn make_index(&self, n: usize, index: usize) -> Option<usize> {
        if unlikely(index >= n) {
            return None;
        }

        Some(index)
    }
}
