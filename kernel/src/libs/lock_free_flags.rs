use core::{cell::UnsafeCell, fmt::Debug};

/// 一个无锁的标志位
///
/// 可与bitflags配合使用，以实现无锁的标志位
///
/// ## Safety
///
/// 由于标识位的修改是无锁，且不保证原子性，因此需要使用者自行在别的机制中，确保
/// 哪怕标识位的值是老的，执行动作也不会有问题（或者有状态恢复机制）。
pub struct LockFreeFlags<T> {
    inner: UnsafeCell<T>,
}

impl<T> LockFreeFlags<T> {
    pub unsafe fn new(inner: T) -> Self {
        Self {
            inner: UnsafeCell::new(inner),
        }
    }

    #[allow(clippy::mut_from_ref)]
    pub fn get_mut(&self) -> &mut T {
        unsafe {
            (self.inner.get().as_ref().unwrap() as *const T as *mut T)
                .as_mut()
                .unwrap()
        }
    }

    pub fn get(&self) -> &T {
        unsafe { self.inner.get().as_ref().unwrap() }
    }
}

unsafe impl<T: Sync> Sync for LockFreeFlags<T> {}
unsafe impl<T: Send> Send for LockFreeFlags<T> {}

impl<T: Clone> Clone for LockFreeFlags<T> {
    fn clone(&self) -> Self {
        unsafe { Self::new(self.get().clone()) }
    }
}

impl<T: Debug> Debug for LockFreeFlags<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LockFreeFlags")
            .field("inner", self.get())
            .finish()
    }
}
