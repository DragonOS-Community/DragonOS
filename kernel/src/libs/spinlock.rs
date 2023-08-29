#![allow(dead_code)]
use core::cell::UnsafeCell;
use core::mem::ManuallyDrop;
use core::ops::{Deref, DerefMut};

use core::sync::atomic::{AtomicBool, Ordering};

use crate::arch::asm::irqflags::{local_irq_restore, local_irq_save};
use crate::arch::interrupt::{cli, sti};
use crate::process::ProcessManager;
use crate::syscall::SystemError;

/// 原始的Spinlock（自旋锁）
/// 请注意，这个自旋锁和C的不兼容。
///
/// @param self.0 这个AtomicBool的值为false时，表示没有被加锁。当它为true时，表示自旋锁已经被上锁。
#[derive(Debug)]
pub struct RawSpinlock(AtomicBool);

impl RawSpinlock {
    /// @brief 初始化自旋锁
    pub const INIT: RawSpinlock = RawSpinlock(AtomicBool::new(false));

    /// @brief 加锁
    pub fn lock(&self) {
        while !self.try_lock() {}
    }

    /// @brief 关中断并加锁
    pub fn lock_irq(&self) {
        cli();
        self.lock();
    }

    /// @brief 尝试加锁
    /// @return 加锁成功->true
    ///         加锁失败->false
    pub fn try_lock(&self) -> bool {
        // 先增加自旋锁持有计数
        ProcessManager::current_pcb().preempt_disable();

        let res = self
            .0
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok();

        // 如果加锁失败恢复自旋锁持有计数
        if res == false {
            ProcessManager::current_pcb().preempt_enable();
        }
        return res;
    }

    /// @brief 解锁
    pub fn unlock(&self) {
        // 减少自旋锁持有计数
        ProcessManager::current_pcb().preempt_enable();
        self.0.store(false, Ordering::Release);
    }

    /// 解锁，但是不更改preempt count
    unsafe fn unlock_no_preempt(&self) {
        self.0.store(false, Ordering::Release);
    }

    /// @brief 放锁并开中断
    pub fn unlock_irq(&self) {
        self.unlock();
        sti();
    }

    /// @brief 判断自旋锁是否被上锁
    ///
    /// @return true 自旋锁被上锁
    /// @return false 自旋锁处于解锁状态
    pub fn is_locked(&self) -> bool {
        return self.0.load(Ordering::Relaxed).into();
    }

    /// @brief 强制设置自旋锁的状态
    /// 请注意，这样操作可能会带来未知的风险。因此它是unsafe的。（尽管从Rust语言本身来说，它是safe的）
    pub unsafe fn set_value(&mut self, value: bool) {
        self.0.store(value, Ordering::SeqCst);
    }

    /// @brief 保存中断状态到flags中，关闭中断，并对自旋锁加锁
    pub fn lock_irqsave(&self, flags: &mut usize) {
        *flags = local_irq_save();
        self.lock();
    }

    /// @brief 恢复rflags以及中断状态并解锁自旋锁
    pub fn unlock_irqrestore(&self, flags: usize) {
        self.unlock();
        local_irq_restore(flags);
    }

    /// @brief 尝试保存中断状态到flags中，关闭中断，并对自旋锁加锁
    /// @return 加锁成功->true
    ///         加锁失败->false
    #[inline(always)]
    pub fn try_lock_irqsave(&self, flags: &mut usize) -> bool {
        *flags = local_irq_save();
        if self.try_lock() {
            return true;
        }
        local_irq_restore(*flags);
        return false;
    }
}
/// 实现了守卫的SpinLock, 能够支持内部可变性
///
#[derive(Debug)]
pub struct SpinLock<T> {
    lock: RawSpinlock,
    /// 自旋锁保护的数据
    data: UnsafeCell<T>,
}

/// SpinLock的守卫
/// 该守卫没有构造器，并且其信息均为私有的。我们只能通过SpinLock的lock()方法获得一个守卫。
/// 因此我们可以认为，只要能够获得一个守卫，那么数据就在自旋锁的保护之下。
#[derive(Debug)]
pub struct SpinLockGuard<'a, T: 'a> {
    lock: &'a SpinLock<T>,
    flag: usize,
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
}

/// 向编译器保证，SpinLock在线程之间是安全的.
/// 其中要求类型T实现了Send这个Trait
unsafe impl<T> Sync for SpinLock<T> where T: Send {}

impl<T> SpinLock<T> {
    pub const fn new(value: T) -> Self {
        return Self {
            lock: RawSpinlock::INIT,
            data: UnsafeCell::new(value),
        };
    }

    #[inline(always)]
    pub fn lock(&self) -> SpinLockGuard<T> {
        self.lock.lock();
        // 加锁成功，返回一个守卫
        return SpinLockGuard {
            lock: self,
            flag: 0,
        };
    }

    pub fn lock_irqsave(&self) -> SpinLockGuard<T> {
        let mut flags: usize = 0;

        self.lock.lock_irqsave(&mut flags);
        // 加锁成功，返回一个守卫
        return SpinLockGuard {
            lock: self,
            flag: flags,
        };
    }

    pub fn try_lock(&self) -> Result<SpinLockGuard<T>, SystemError> {
        if self.lock.try_lock() {
            return Ok(SpinLockGuard {
                lock: self,
                flag: 0,
            });
        }
        return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
    }

    pub fn try_lock_irqsave(&self) -> Result<SpinLockGuard<T>, SystemError> {
        let mut flags: usize = 0;
        if self.lock.try_lock_irqsave(&mut flags) {
            return Ok(SpinLockGuard {
                lock: self,
                flag: flags,
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
        self.lock.unlock_no_preempt();
    }
}

/// 实现Deref trait，支持通过获取SpinLockGuard来获取临界区数据的不可变引用
impl<T> Deref for SpinLockGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        return unsafe { &*self.lock.data.get() };
    }
}

/// 实现DerefMut trait，支持通过获取SpinLockGuard来获取临界区数据的可变引用
impl<T> DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        return unsafe { &mut *self.lock.data.get() };
    }
}

/// @brief 为SpinLockGuard实现Drop方法，那么，一旦守卫的生命周期结束，就会自动释放自旋锁，避免了忘记放锁的情况
impl<T> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        if self.flag != 0 {
            self.lock.lock.unlock_irqrestore(self.flag);
        } else {
            self.lock.lock.unlock();
        }
    }
}
