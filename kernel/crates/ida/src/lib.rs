#![no_std]
#![feature(core_intrinsics)]
#![allow(internal_features)]
#![allow(clippy::needless_return)]

#[cfg(test)]
#[macro_use]
extern crate std;

use core::cmp::min;
use core::intrinsics::unlikely;
use core::marker::PhantomData;
use core::ops::Deref;

struct EmptyIdaItemRef<'a> {
    _marker: PhantomData<&'a EmptyIdaItem>,
}

impl Deref for EmptyIdaItemRef<'_> {
    type Target = EmptyIdaItem;

    fn deref(&self) -> &Self::Target {
        &EmptyIdaItem
    }
}

struct EmptyIdaItem;

unsafe impl kdepends::xarray::ItemEntry for EmptyIdaItem {
    type Ref<'a>
        = EmptyIdaItemRef<'a>
    where
        Self: 'a;

    fn into_raw(self) -> *const () {
        core::ptr::null()
    }

    unsafe fn from_raw(_raw: *const ()) -> Self {
        EmptyIdaItem
    }

    unsafe fn raw_as_ref<'a>(_raw: *const ()) -> Self::Ref<'a> {
        EmptyIdaItemRef {
            _marker: PhantomData,
        }
    }
}
/// id分配器
pub struct IdAllocator {
    current_id: usize,
    min_id: usize,
    max_id: usize,
    used: usize,
    xarray: kdepends::xarray::XArray<EmptyIdaItem>,
}

impl IdAllocator {
    /// 创建一个新的id分配器
    pub const fn new(initial_id: usize, max_id: usize) -> Option<Self> {
        if initial_id >= max_id {
            return None;
        }
        Some(Self {
            current_id: initial_id,
            min_id: initial_id,
            max_id,
            used: 0,
            xarray: kdepends::xarray::XArray::new(),
        })
    }

    /// 可用的id数量
    #[inline]
    pub fn available(&self) -> usize {
        self.max_id - self.min_id - self.used
    }

    /// 分配一个新的id
    ///
    /// ## 返回
    ///
    /// 如果分配成功，返回Some(id)，否则返回None
    pub fn alloc(&mut self) -> Option<usize> {
        if unlikely(self.available() == 0) {
            return None;
        }

        if let Some(try1) = self.do_find_first_free_index(self.current_id, self.max_id) {
            self.current_id = try1;
            self.xarray.store(try1 as u64, EmptyIdaItem);
            self.used += 1;
            return Some(try1);
        }

        // 从头开始找
        if let Some(try2) =
            self.do_find_first_free_index(self.min_id, min(self.current_id, self.max_id))
        {
            self.current_id = try2;
            self.xarray.store(try2 as u64, EmptyIdaItem);
            self.used += 1;
            return Some(try2);
        }
        return None;
    }

    /// 检查id是否存在
    ///
    /// ## 参数
    ///
    /// - `id`：要检查的id
    ///
    /// ## 返回
    ///
    /// 如果id存在，返回true，否则返回false
    pub fn exists(&self, id: usize) -> bool {
        if id < self.min_id || id >= self.max_id {
            return false;
        }
        self.xarray.load(id as u64).is_some()
    }

    fn do_find_first_free_index(&self, start_id: usize, end: usize) -> Option<usize> {
        (start_id..end).find(|&i| !self.exists(i))
    }

    /// 释放一个id
    ///
    /// ## 参数
    ///
    /// - `id`：要释放的id
    pub fn free(&mut self, id: usize) {
        if id < self.min_id || id >= self.max_id {
            return;
        }
        if self.xarray.remove(id as u64).is_some() {
            self.used -= 1;
        }
    }

    /// 返回已经使用的id数量
    pub fn used(&self) -> usize {
        self.used
    }

    /// 返回最大id数
    pub fn get_max_id(&self) -> usize {
        self.max_id
    }
}

impl core::fmt::Debug for IdAllocator {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("IdAllocator")
            .field("current_id", &self.current_id)
            .field("min_id", &self.min_id)
            .field("max_id", &self.max_id)
            .field("used", &self.used)
            .field("xarray", &"xarray<()>")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_new_fail() {
        assert_eq!(IdAllocator::new(10, 10).is_none(), true);
        assert_eq!(IdAllocator::new(11, 10).is_none(), true);
    }
    #[test]
    fn test_new_success() {
        assert_eq!(IdAllocator::new(9, 10).is_some(), true);
        assert_eq!(IdAllocator::new(0, 10).is_some(), true);
    }

    #[test]
    fn test_id_allocator() {
        let mut ida = IdAllocator::new(0, 10).unwrap();
        assert_eq!(ida.alloc(), Some(0));
        assert_eq!(ida.alloc(), Some(1));
        assert_eq!(ida.alloc(), Some(2));
        assert_eq!(ida.alloc(), Some(3));
        assert_eq!(ida.alloc(), Some(4));
        assert_eq!(ida.alloc(), Some(5));
        assert_eq!(ida.alloc(), Some(6));
        assert_eq!(ida.alloc(), Some(7));
        assert_eq!(ida.alloc(), Some(8));
        assert_eq!(ida.alloc(), Some(9));
        assert_eq!(ida.alloc(), None);

        for i in 0..10 {
            assert_eq!(ida.exists(i), true);
        }

        ida.free(5);

        for i in 0..10 {
            if i == 5 {
                assert_eq!(ida.exists(i), false);
            } else {
                assert_eq!(ida.exists(i), true);
            }
        }
        assert_eq!(ida.used(), 9);
        assert_eq!(ida.alloc(), Some(5));
        assert_eq!(ida.alloc(), None);

        assert_eq!(ida.used(), 10);
        for i in 0..10 {
            ida.free(i);
        }

        assert_eq!(ida.used(), 0);
    }
}
