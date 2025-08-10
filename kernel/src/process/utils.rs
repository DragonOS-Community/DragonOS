use crate::process::ProcessManager;

use super::{__PROCESS_MANAGEMENT_INIT_DONE, ProcessFlags};

pub fn current_pcb_flags() -> ProcessFlags {
    if unsafe { !__PROCESS_MANAGEMENT_INIT_DONE } {
        return ProcessFlags::empty();
    }
    return *ProcessManager::current_pcb().flags();
}

pub fn current_pcb_preempt_count() -> usize {
    if unsafe { !__PROCESS_MANAGEMENT_INIT_DONE } {
        return 0;
    }
    return ProcessManager::current_pcb().preempt_count();
}
