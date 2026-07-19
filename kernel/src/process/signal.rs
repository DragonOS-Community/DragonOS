use crate::ipc::sighand::SigHand;
use crate::process::ProcessControlBlock;
use crate::process::ProcessManager;
use alloc::sync::{Arc, Weak};

impl ProcessManager {
    /// 检查 real_parent 是否与 group_leader 在同一个线程组中
    ///
    /// 参考 Linux: https://elixir.bootlin.com/linux/v6.6.21/source/include/linux/sched.h#L2001
    pub fn same_thread_group(
        group_leader: &Arc<ProcessControlBlock>,
        real_parent: &Weak<ProcessControlBlock>,
    ) -> bool {
        if let Some(parent) = real_parent.upgrade() {
            // 检查 parent 的 group_leader 是否与传入的 group_leader 相同
            if let Some(parent_leader) = parent.threads_read_irqsave().group_leader() {
                return Arc::ptr_eq(&parent_leader, group_leader);
            }
        }
        false
    }
}

impl ProcessControlBlock {
    pub fn with_task_lock_irqsave<R>(&self, f: impl FnOnce() -> R) -> R {
        let _task_guard = self.task_lock.lock_irqsave();
        f()
    }

    pub fn sighand(&self) -> Arc<SigHand> {
        self.sighand.load()
    }

    pub fn replace_sighand(&self, new: Arc<SigHand>) {
        self.with_task_lock_irqsave(|| {
            new.attach_task_ref();
            // SAFETY: task_lock serializes sighand writers. If old and new
            // share an allocation, the replacement slot reference publishes
            // it continuously. Otherwise `old` keeps the removed allocation
            // alive until it is submitted to rcu_defer_drop below.
            let old = unsafe { self.sighand.swap(new.clone()) };
            if Arc::ptr_eq(&old, &new) {
                new.detach_task_ref();
                return;
            }

            old.detach_task_ref();
            crate::rcu::rcu_defer_drop(old);
        });
    }
}
