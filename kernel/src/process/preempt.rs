use crate::arch::x86_64::asm::current::current_pcb;

/// @brief 增加进程的锁持有计数
#[inline]
pub fn preempt_disable() {
    current_pcb().preempt_count += 1;
}

/// @brief 减少进程的锁持有计数
#[inline]
pub fn preempt_enable() {
    current_pcb().preempt_count -= 1;
}
