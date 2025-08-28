use crate::process::ProcessControlBlock;
use crate::process::ProcessManager;
use alloc::sync::{Arc, Weak};

impl ProcessManager {
    pub fn same_thread_group(
        group_leader: &Arc<ProcessControlBlock>,
        real_parent: &Weak<ProcessControlBlock>,
    ) -> bool {
        group_leader
            .threads_read_irqsave()
            .group_tasks
            .iter()
            .any(|x| x.ptr_eq(real_parent))
    }
}
