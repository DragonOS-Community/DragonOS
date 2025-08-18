use core::mem::size_of;

use crate::{bitmap_core::BitMapCore, traits::BitMapOps};

pub const fn static_bitmap_size<const N: usize>() -> usize {
    N.div_ceil(usize::BITS as usize)
}

/// 静态位图
///
/// 该位图的大小在编译时确定，不可变
#[derive(Debug, Clone)]
pub struct StaticBitmap<const N: usize, const M: usize> {
    pub data: [usize; M],
    core: BitMapCore<usize>,
}

/// 创建静态位图的宏
///
/// 使用方式：static_bitmap!(items_count) 创建一个能容纳 items_count 个位的静态位图
///
/// 示例：
/// ```rust
/// use bitmap::static_bitmap;
/// use bitmap::StaticBitmap;
///
/// let bmp: static_bitmap!(100) = StaticBitmap::new();
/// ```
#[macro_export]
macro_rules! static_bitmap {
    ($count:expr) => {
        $crate::StaticBitmap<{ $count }, { $crate::static_bitmap_size::<{ $count }>() }>
    };
}

impl<const N: usize, const M: usize> Default for StaticBitmap<N, M> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize, const M: usize> StaticBitmap<N, M> {
    /// 创建一个新的静态位图
    pub const fn new() -> Self {
        Self {
            data: [0; M],
            core: BitMapCore::new(),
        }
    }
}

impl<const N: usize, const M: usize> BitMapOps<usize> for StaticBitmap<N, M> {
    #[inline]
    fn get(&self, index: usize) -> Option<bool> {
        return self.core.get(N, &self.data, index);
    }

    #[inline]
    fn set(&mut self, index: usize, value: bool) -> Option<bool> {
        return self.core.set(N, &mut self.data, index, value);
    }

    #[inline]
    fn len(&self) -> usize {
        N
    }

    #[inline]
    fn size(&self) -> usize {
        self.data.len() * size_of::<usize>()
    }

    #[inline]
    fn first_index(&self) -> Option<usize> {
        self.core.first_index(&self.data)
    }

    #[inline]
    fn first_false_index(&self) -> Option<usize> {
        self.core.first_false_index(N, &self.data)
    }

    #[inline]
    fn last_index(&self) -> Option<usize> {
        self.core.last_index(N, &self.data)
    }

    #[inline]
    fn last_false_index(&self) -> Option<usize> {
        self.core.last_false_index(N, &self.data)
    }

    #[inline]
    fn next_index(&self, index: usize) -> Option<usize> {
        self.core.next_index(N, &self.data, index)
    }

    #[inline]
    fn next_false_index(&self, index: usize) -> Option<usize> {
        self.core.next_false_index(N, &self.data, index)
    }

    #[inline]
    fn prev_index(&self, index: usize) -> Option<usize> {
        self.core.prev_index(N, &self.data, index)
    }

    #[inline]
    fn prev_false_index(&self, index: usize) -> Option<usize> {
        self.core.prev_false_index(N, &self.data, index)
    }

    #[inline]
    fn invert(&mut self) {
        self.core.invert(N, &mut self.data);
    }

    #[inline]
    fn is_full(&self) -> bool {
        self.core.is_full(N, &self.data)
    }

    #[inline]
    fn is_empty(&self) -> bool {
        self.core.is_empty(&self.data)
    }

    #[inline]
    unsafe fn as_bytes(&self) -> &[u8] {
        core::slice::from_raw_parts(
            self.data.as_ptr() as *const u8,
            core::mem::size_of::<Self>(),
        )
    }

    fn set_all(&mut self, value: bool) {
        self.core.set_all(N, &mut self.data, value);
    }
}
