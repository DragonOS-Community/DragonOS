use core::mem::size_of;

use crate::{bitmap_core::BitMapCore, traits::BitMapOps};

/// 静态位图
///
/// 该位图的大小在编译时确定，不可变
pub struct StaticBitmap<const N: usize>
where
    [(); (N + usize::BITS as usize - 1) / (usize::BITS as usize)]:,
{
    pub data: [usize; (N + usize::BITS as usize - 1) / (usize::BITS as usize)],
    core: BitMapCore<usize, N>,
}

impl<const N: usize> StaticBitmap<N>
where
    [(); (N + usize::BITS as usize - 1) / (usize::BITS as usize)]:,
{
    /// 创建一个新的静态位图
    pub const fn new() -> Self {
        Self {
            data: [0; (N + usize::BITS as usize - 1) / (usize::BITS as usize)],
            core: BitMapCore::new(),
        }
    }
}

impl<const N: usize> BitMapOps<usize> for StaticBitmap<N>
where
    [(); (N + usize::BITS as usize - 1) / (usize::BITS as usize)]:,
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
        self.data.len() * size_of::<usize>()
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

    #[inline]
    fn invert(&mut self) {
        self.core.invert(&mut self.data);
    }

    #[inline]
    fn is_full(&self) -> bool {
        self.core.is_full(&self.data)
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
        self.core.set_all(&mut self.data, value);
    }
}
