//! Task-bound reserve authority. Only explicit progress scopes may grant it.
use crate::process::{ProcessControlBlock, ProcessFlags, ProcessManager};
use alloc::sync::Arc;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum ReserveLevel {
    #[default]
    Kernel,
    User,
    OomExit,
    Reclaim,
}

pub(crate) fn kernel_level() -> ReserveLevel {
    if !ProcessManager::initialized() || crate::exception::in_interrupt() {
        return ReserveLevel::Kernel;
    }
    level_for_flags(ProcessManager::current_pcb().flags().load(), false)
}

fn level_for_flags(flags: ProcessFlags, interrupt: bool) -> ReserveLevel {
    if interrupt {
        return ReserveLevel::Kernel;
    }
    if flags.contains(ProcessFlags::RECLAIM_RESERVE) {
        ReserveLevel::Reclaim
    } else if flags.contains(ProcessFlags::OOM_EXIT_RESERVE) {
        ReserveLevel::OomExit
    } else {
        ReserveLevel::Kernel
    }
}

struct ReserveAccessGuard {
    task: Arc<ProcessControlBlock>,
    flag: ProcessFlags,
    already_set: bool,
    _task_bound: core::marker::PhantomData<*mut ()>,
}

impl Drop for ReserveAccessGuard {
    fn drop(&mut self) {
        if !self.already_set {
            self.task.flags().remove(self.flag);
        }
    }
}

fn with_reserve<R>(flag: ProcessFlags, operation: impl FnOnce() -> R) -> R {
    assert!(!crate::exception::in_interrupt());
    let task = ProcessManager::current_pcb();
    let already_set = task.flags().contains(flag);
    task.flags().insert(flag);
    let _guard = ReserveAccessGuard {
        task,
        flag,
        already_set,
        _task_bound: core::marker::PhantomData,
    };
    operation()
}

pub(crate) fn with_reclaim_reserve<R>(operation: impl FnOnce() -> R) -> R {
    with_reserve(ProcessFlags::RECLAIM_RESERVE, operation)
}

/// Caller must establish the existing committed OOM-victim witness first.
pub(crate) fn with_oom_exit_reserve<R>(operation: impl FnOnce() -> R) -> R {
    with_reserve(ProcessFlags::OOM_EXIT_RESERVE, operation)
}

#[cfg(feature = "mm_selftest")]
pub(super) fn selftest() -> bool {
    let before = ProcessManager::current_pcb().flags().load();
    let nested = with_reclaim_reserve(|| {
        let user = super::page_frame::FrameAllocationRequest::user(
            super::page_frame::PageFrameCount::ONE,
            crate::mm::PhysAddr::new(usize::MAX),
        );
        kernel_level() == ReserveLevel::Reclaim
            && user.reserve_level() == ReserveLevel::User
            && with_oom_exit_reserve(|| kernel_level() == ReserveLevel::Reclaim)
            && kernel_level() == ReserveLevel::Reclaim
    });
    let both = ProcessFlags::RECLAIM_RESERVE | ProcessFlags::OOM_EXIT_RESERVE;
    nested
        && ProcessManager::current_pcb().flags().load() == before
        && level_for_flags(both, true) == ReserveLevel::Kernel
        && !both.fork_inherited().intersects(both)
        && crate::mm::page::reclaim_io_admission_selftest()
}
