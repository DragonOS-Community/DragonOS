#![allow(dead_code)]
use core::ptr::read_volatile;

use crate::arch::x86_64::asm::irqflags::{local_irq_restore, local_irq_save};
use crate::include::bindings::bindings::{spin_lock, spin_unlock, spinlock_t};

/// @brief 保存中断状态到flags中，关闭中断，并对自旋锁加锁
pub fn spin_lock_irqsave(lock: *mut spinlock_t, flags: &mut u64) {
    local_irq_save(flags);
    unsafe {
        spin_lock(lock);
    }
}

/// @brief 恢复rflags以及中断状态并解锁自旋锁
#[no_mangle]
pub fn spin_unlock_irqrestore(lock: *mut spinlock_t, flags: &u64) {
    unsafe {
        spin_unlock(lock);
    }
    // kdebug!("123");
    local_irq_restore(flags);
}

/// 判断一个自旋锁是否已经被加锁
pub fn spin_is_locked(lock: &spinlock_t) -> bool {
    let val = unsafe { read_volatile(&lock.lock as *const i8) };

    return if val == 0 { true } else { false };
}
