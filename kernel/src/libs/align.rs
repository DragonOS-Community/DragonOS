#![allow(dead_code)]
//! 这是一个关于对齐的库，提供了一些对齐的宏和函数、结构体

use core::{alloc::GlobalAlloc, fmt::Debug, ptr::Unique};

use system_error::SystemError;

use crate::{arch::MMArch, mm::MemoryManagementArch, KERNEL_ALLOCATOR};

/// # AlignedBox
///
/// 一个用于分配对齐内存的结构体。分配的内存地址符合`ALIGN`的要求。
/// 如果类型T的对齐要求大于`ALIGN`，则采用T的对齐要求。
///
/// ## 说明
///
/// `ALIGN`: 对齐要求，必须是2的幂次方,且不为0，否则编译时报错
pub struct AlignedBox<T, const ALIGN: usize> {
    inner: Unique<T>,
}

impl<T, const ALIGN: usize> AlignedBox<T, ALIGN> {
    const LAYOUT: core::alloc::Layout = {
        const fn max(a: usize, b: usize) -> usize {
            if a > b {
                a
            } else {
                b
            }
        }
        let layout = core::alloc::Layout::from_size_align(
            core::mem::size_of::<T>(),
            max(ALIGN, core::mem::align_of::<T>()),
        );

        if let Ok(layout) = layout {
            layout
        } else {
            panic!("Check alignment failed at compile time.")
        }
    };

    /// 分配一个新的内存空间，并将其初始化为零。然后返回AlignedBox
    ///
    /// # Errors
    ///
    /// 如果分配失败，则返回`Err(SystemError::ENOMEM)`
    #[inline(always)]
    pub fn new_zeroed() -> Result<Self, SystemError>
    where
        T: SafeForZero,
    {
        let ptr = unsafe { KERNEL_ALLOCATOR.alloc_zeroed(Self::LAYOUT) };
        if ptr.is_null() {
            return Err(SystemError::ENOMEM);
        } else {
            return Ok(AlignedBox {
                inner: unsafe { Unique::new_unchecked(ptr.cast()) },
            });
        }
    }

    pub unsafe fn new_unchecked(ptr: *mut T) -> Self {
        return AlignedBox {
            inner: Unique::new_unchecked(ptr),
        };
    }
}

impl<T, const ALIGN: usize> Debug for AlignedBox<T, ALIGN> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        return write!(
            f,
            "AlignedBox<{:?}, {:?}>, ptr: {:p}, size: {:}",
            core::any::type_name::<T>(),
            core::mem::align_of::<T>(),
            self.inner.as_ptr(),
            core::mem::size_of::<T>()
        );
    }
}

impl<T, const ALIGN: usize> Drop for AlignedBox<T, ALIGN> {
    fn drop(&mut self) {
        unsafe {
            // 释放 Unique 智能指针所拥有的内存，并调用类型的析构函数以清理资源
            core::ptr::drop_in_place(self.inner.as_ptr());
            // dealloc memory space
            KERNEL_ALLOCATOR.dealloc(self.inner.as_ptr().cast(), Self::LAYOUT);
        }
    }
}

impl<T, const ALIGN: usize> core::ops::Deref for AlignedBox<T, ALIGN> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.inner.as_ptr() }
    }
}

impl<T, const ALIGN: usize> core::ops::DerefMut for AlignedBox<T, ALIGN> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.inner.as_ptr() }
    }
}

impl<T: Clone + SafeForZero, const ALIGN: usize> Clone for AlignedBox<T, ALIGN> {
    fn clone(&self) -> Self {
        let mut new: AlignedBox<T, ALIGN> =
            Self::new_zeroed().unwrap_or_else(|_| alloc::alloc::handle_alloc_error(Self::LAYOUT));
        new.clone_from(self);
        return new;
    }
}

/// 一个用于表明某个类型是安全的用于零初始化的 trait
///
/// 该 trait 用于表明某个类型是安全的用于零初始化的，即该类型的所有位都可以被初始化为 0 而不会出现未定义行为。
pub unsafe trait SafeForZero {}

unsafe impl<const NUM: usize> SafeForZero for [u8; NUM] {}

/// 将给定的地址按照页面大小，向上对齐。
///
/// 参数 `addr`：要对齐的地址。
///
/// 返回值：对齐后的地址。
pub const fn page_align_up(addr: usize) -> usize {
    let page_size = MMArch::PAGE_SIZE;
    return (addr + page_size - 1) & (!(page_size - 1));
}

pub const fn page_align_down(addr: usize) -> usize {
    let page_size = MMArch::PAGE_SIZE;
    return addr & (!(page_size - 1));
}

pub const fn align_up(addr: usize, align: usize) -> usize {
    assert!(align != 0 && align.is_power_of_two());
    return (addr + align - 1) & (!(align - 1));
}

pub const fn align_down(addr: usize, align: usize) -> usize {
    assert!(align != 0 && align.is_power_of_two());
    return addr & (!(align - 1));
}

/// ## 检查是否对齐
///
/// 检查给定的值是否对齐到给定的对齐要求。
///
/// ## 参数
/// - `value`：要检查的值
/// - `align`：对齐要求，必须是2的幂次方,且不为0，否则运行时panic
///
/// ## 返回值
///
/// 如果对齐则返回`true`，否则返回`false`
pub fn check_aligned(value: usize, align: usize) -> bool {
    assert!(align != 0 && align.is_power_of_two());
    return value & (align - 1) == 0;
}
