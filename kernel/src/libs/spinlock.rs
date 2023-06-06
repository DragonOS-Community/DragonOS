#![allow(dead_code)]
use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::ptr::read_volatile;

use core::sync::atomic::{AtomicBool, Ordering};

use crate::arch::asm::irqflags::{local_irq_restore, local_irq_save};
use crate::arch::interrupt::{cli, sti};
use crate::include::bindings::bindings::{spin_lock, spin_unlock, spinlock_t};
use crate::process::preempt::{preempt_disable, preempt_enable};
use crate::syscall::SystemError;

/// @brief 保存中断状态到flags中，关闭中断，并对自旋锁加锁
#[inline]
pub fn spin_lock_irqsave(lock: *mut spinlock_t, flags: &mut u64) {
    local_irq_save(flags);
    unsafe {
        spin_lock(lock);
    }
}

/// @brief 恢复rflags以及中断状态并解锁自旋锁
#[inline]
pub fn spin_unlock_irqrestore(lock: *mut spinlock_t, flags: &u64) {
    unsafe {
        spin_unlock(lock);
    }
    // kdebug!("123");
    local_irq_restore(flags);
}

/// 判断一个自旋锁是否已经被加锁
#[inline]
pub fn spin_is_locked(lock: &spinlock_t) -> bool {
    let val = unsafe { read_volatile(&lock.lock as *const i8) };

    return if val == 0 { true } else { false };
}

impl Default for spinlock_t {
    fn default() -> Self {
        Self { lock: 1 }
    }
}

/// @brief 关闭中断并加锁
pub fn spin_lock_irq(lock: *mut spinlock_t) {
    cli();
    unsafe {
        spin_lock(lock);
    }
}

/// @brief 解锁并开中断
pub fn spin_unlock_irq(lock: *mut spinlock_t) {
    unsafe {
        spin_unlock(lock);
    }
    sti();
}

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
        preempt_disable();

        let res = self
            .0
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok();

        // 如果加锁失败恢复自旋锁持有计数
        if res == false {
            preempt_enable();
        }
        return res;
    }

    /// @brief 解锁
    pub fn unlock(&self) {
        // 减少自旋锁持有计数
        preempt_enable();
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
    pub fn lock_irqsave(&self, flags: &mut u64) {
        local_irq_save(flags);
        self.lock();
    }

    /// @brief 恢复rflags以及中断状态并解锁自旋锁
    pub fn unlock_irqrestore(&self, flags: &u64) {
        self.unlock();
        local_irq_restore(flags);
    }

    /// @brief 尝试保存中断状态到flags中，关闭中断，并对自旋锁加锁
    /// @return 加锁成功->true
    ///         加锁失败->false
    #[inline(always)]
    pub fn try_lock_irqsave(&self, flags: &mut u64) -> bool {
        local_irq_save(flags);
        if self.try_lock() {
            return true;
        }
        local_irq_restore(flags);
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
    flag: u64,
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
        let mut flags: u64 = 0;
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
        let mut flags: u64 = 0;
        if self.lock.try_lock_irqsave(&mut flags) {
            return Ok(SpinLockGuard {
                lock: self,
                flag: flags,
            });
        }
        return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
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
            self.lock.lock.unlock_irqrestore(&self.flag);
        } else {
            self.lock.lock.unlock();
        }
    }
}
