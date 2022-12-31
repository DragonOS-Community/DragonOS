#![allow(dead_code)]
use core::ptr::read_volatile;

use core::sync::atomic::{AtomicBool, Ordering};

use crate::arch::asm::irqflags::{local_irq_restore, local_irq_save};
use crate::arch::interrupt::{cli, sti};
use crate::include::bindings::bindings::{spin_lock, spin_unlock, spinlock_t};
use crate::process::preempt::{preempt_disable, preempt_enable};

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

#[derive(Debug)]
pub struct RawSpinlock(AtomicBool);

impl RawSpinlock {
    /// @brief 初始化自旋锁
    pub const INIT: RawSpinlock = RawSpinlock(AtomicBool::new(false));

    /// @brief 加锁
    pub fn lock(&mut self) {
        while !self.try_lock() {}
    }

    /// @brief 关中断并加锁
    pub fn lock_irq(&mut self){
        cli();
        self.lock();
    }

    /// @brief 尝试加锁
    /// @return 加锁成功->true
    ///         加锁失败->false
    pub fn try_lock(&mut self) -> bool {
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
    pub fn unlock(&mut self){
        // 减少自旋锁持有计数
        preempt_enable();
        self.0.store(false, Ordering::Release);
    }

    /// @brief 放锁并开中断
    pub fn unlock_irq(&mut self){
        self.unlock();
        sti();
    }

    // todo: spin_lock_irqsave
    // todo: spin_unlock_irqrestore

}
