use core::intrinsics::likely;

use crate::process::{ProcessManager, __PROCESS_MANAGEMENT_INIT_DONE};

pub struct PreemptGuard;

impl PreemptGuard {
    pub fn new() -> Self {
        ProcessManager::preempt_disable();
        Self
    }
}

impl Drop for PreemptGuard {
    fn drop(&mut self) {
        ProcessManager::preempt_enable();
    }
}

impl ProcessManager {
    /// 增加当前进程的锁持有计数
    #[inline(always)]
    pub fn preempt_disable() {
        if likely(unsafe { __PROCESS_MANAGEMENT_INIT_DONE }) {
            ProcessManager::current_pcb().preempt_disable();
        }
    }

    /// 减少当前进程的锁持有计数
    #[inline(always)]
    pub fn preempt_enable() {
        if likely(unsafe { __PROCESS_MANAGEMENT_INIT_DONE }) {
            ProcessManager::current_pcb().preempt_enable();
        }
    }
}
