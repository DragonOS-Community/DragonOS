#![allow(dead_code)]
use core::cell::UnsafeCell;
use core::hint::spin_loop;
use core::mem::ManuallyDrop;
use core::ops::{Deref, DerefMut};

use core::sync::atomic::{AtomicBool, Ordering};

use crate::arch::CurrentIrqArch;
use crate::exception::{InterruptArch, IrqFlagsGuard};
use crate::process::ProcessManager;
use system_error::SystemError;

/// 实现了守卫的SpinLock, 能够支持内部可变性
///
#[derive(Debug)]
pub struct SpinLock<T> {
    lock: AtomicBool,
    /// 自旋锁保护的数据
    data: UnsafeCell<T>,
}

/// SpinLock的守卫
/// 该守卫没有构造器，并且其信息均为私有的。我们只能通过SpinLock的lock()方法获得一个守卫。
/// 因此我们可以认为，只要能够获得一个守卫，那么数据就在自旋锁的保护之下。
#[derive(Debug)]
pub struct SpinLockGuard<'a, T: 'a> {
    lock: &'a SpinLock<T>,
    data: *mut T,
    irq_flag: Option<IrqFlagsGuard>,
    flags: SpinLockGuardFlags,
}

impl<'a, T: 'a> SpinLockGuard<'a, T> {
    /// 泄露自旋锁的守卫，返回一个可变的引用
    ///
    ///  ## Safety
    ///
    /// 由于这样做可能导致守卫在另一个线程中被释放，从而导致pcb的preempt count不正确，
    /// 因此必须小心的手动维护好preempt count。
    ///
    /// 并且，leak还可能导致锁的状态不正确。因此请仔细考虑是否真的需要使用这个函数。
    #[inline]
    pub unsafe fn leak(this: Self) -> &'a mut T {
        // Use ManuallyDrop to avoid stacked-borrow invalidation
        let this = ManuallyDrop::new(this);
        // We know statically that only we are referencing data
        unsafe { &mut *this.lock.data.get() }
    }

    fn unlock_no_preempt(&self) {
        unsafe {
            self.lock.force_unlock();
        }
    }
}

/// 向编译器保证，SpinLock在线程之间是安全的.
/// 其中要求类型T实现了Send这个Trait
unsafe impl<T> Sync for SpinLock<T> where T: Send {}

impl<T> SpinLock<T> {
    pub const fn new(value: T) -> Self {
        return Self {
            lock: AtomicBool::new(false),
            data: UnsafeCell::new(value),
        };
    }

    #[inline(always)]
    pub fn lock(&self) -> SpinLockGuard<T> {
        loop {
            let res = self.try_lock();
            if let Ok(res) = res {
                return res;
            }
            spin_loop();
        }
    }

    /// 加锁，但是不更改preempt count
    #[inline(always)]
    pub fn lock_no_preempt(&self) -> SpinLockGuard<T> {
        loop {
            if let Ok(guard) = self.try_lock_no_preempt() {
                return guard;
            }
            spin_loop();
        }
    }

    pub fn lock_irqsave(&self) -> SpinLockGuard<T> {
        loop {
            if let Ok(guard) = self.try_lock_irqsave() {
                return guard;
            }
            spin_loop();
        }
    }

    pub fn try_lock(&self) -> Result<SpinLockGuard<T>, SystemError> {
        // 先增加自旋锁持有计数
        ProcessManager::preempt_disable();

        if self.inner_try_lock() {
            return Ok(SpinLockGuard {
                lock: self,
                data: unsafe { &mut *self.data.get() },
                irq_flag: None,
                flags: SpinLockGuardFlags::empty(),
            });
        }

        // 如果加锁失败恢复自旋锁持有计数
        ProcessManager::preempt_enable();

        return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
    }

    fn inner_try_lock(&self) -> bool {
        let res = self
            .lock
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok();
        return res;
    }

    pub fn try_lock_irqsave(&self) -> Result<SpinLockGuard<T>, SystemError> {
        let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        ProcessManager::preempt_disable();
        if self.inner_try_lock() {
            return Ok(SpinLockGuard {
                lock: self,
                data: unsafe { &mut *self.data.get() },
                irq_flag: Some(irq_guard),
                flags: SpinLockGuardFlags::empty(),
            });
        }
        ProcessManager::preempt_enable();
        drop(irq_guard);
        return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
    }

    pub fn try_lock_no_preempt(&self) -> Result<SpinLockGuard<T>, SystemError> {
        if self.inner_try_lock() {
            return Ok(SpinLockGuard {
                lock: self,
                data: unsafe { &mut *self.data.get() },
                irq_flag: None,
                flags: SpinLockGuardFlags::NO_PREEMPT,
            });
        }
        return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
    }

    /// 强制解锁，并且不更改preempt count
    ///
    /// ## Safety
    ///
    /// 由于这样做可能导致preempt count不正确，因此必须小心的手动维护好preempt count。
    /// 如非必要，请不要使用这个函数。
    pub unsafe fn force_unlock(&self) {
        self.lock.store(false, Ordering::SeqCst);
    }

    fn unlock(&self) {
        self.lock.store(false, Ordering::SeqCst);
        ProcessManager::preempt_enable();
    }

    pub fn is_locked(&self) -> bool {
        self.lock.load(Ordering::SeqCst)
    }
}

/// 实现Deref trait，支持通过获取SpinLockGuard来获取临界区数据的不可变引用
impl<T> Deref for SpinLockGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        return unsafe { &*self.data };
    }
}

/// 实现DerefMut trait，支持通过获取SpinLockGuard来获取临界区数据的可变引用
impl<T> DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        return unsafe { &mut *self.data };
    }
}

/// @brief 为SpinLockGuard实现Drop方法，那么，一旦守卫的生命周期结束，就会自动释放自旋锁，避免了忘记放锁的情况
impl<T> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        if self.flags.contains(SpinLockGuardFlags::NO_PREEMPT) {
            self.unlock_no_preempt();
        } else {
            self.lock.unlock();
        }
        // restore irq
        self.irq_flag.take();
    }
}

bitflags! {
    struct SpinLockGuardFlags: u8 {
        /// 守卫是由“*no_preempt”方法获得的
        const NO_PREEMPT = (1<<0);
    }
}
