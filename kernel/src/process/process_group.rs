use super::{ProcessControlBlock, ProcessManager, RawPid};
use crate::process::pid::{Pid, PidType};
use alloc::sync::Arc;

/// 进程组ID
pub type Pgid = RawPid;

impl ProcessManager {
    // 参考 https://code.dragonos.org.cn/xref/linux-6.6.21/kernel/exit.c#345
    pub fn is_current_pgrp_orphaned() -> bool {
        let current_pcb = ProcessManager::current_pcb();
        let pgrp = current_pcb.task_pgrp().unwrap();
        Self::will_become_orphaned_pgrp(&pgrp, None)
    }

    /// 检查一个进程组是否为孤儿进程组
    ///
    /// https://code.dragonos.org.cn/xref/linux-6.6.21/kernel/exit.c#326
    #[inline(never)]
    fn will_become_orphaned_pgrp(
        pgrp: &Arc<Pid>,
        ignored_pcb: Option<&Arc<ProcessControlBlock>>,
    ) -> bool {
        for pcb in pgrp.tasks_iter(PidType::PGID) {
            let real_parent = pcb.real_parent_pcb().unwrap();
            if ignored_pcb.is_some() && Arc::ptr_eq(&pcb, ignored_pcb.unwrap()) {
                continue;
            }
            if (pcb.is_exited() && pcb.threads_read_irqsave().thread_group_empty())
                || real_parent.is_global_init()
            {
                continue;
            }

            if real_parent.task_pgrp() != Some(pgrp.clone())
                && real_parent.task_session() == pcb.task_session()
            {
                return false;
            }
        }

        return true;
    }
}

impl ProcessControlBlock {
    pub fn task_pgrp(&self) -> Option<Arc<Pid>> {
        self.sighand().pid(PidType::PGID)
    }

    pub fn task_session(&self) -> Option<Arc<Pid>> {
        self.sighand().pid(PidType::SID)
    }

    /// 参考 https://code.dragonos.org.cn/xref/linux-6.6.21/kernel/signal.c?fi=task_join_group_stop#393
    pub(super) fn task_join_group_stop(&self) {
        // todo: 实现  https://code.dragonos.org.cn/xref/linux-6.6.21/kernel/signal.c?fi=task_join_group_stop#393
    }
}
