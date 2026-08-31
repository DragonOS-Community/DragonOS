use crate::sched::SchedPolicy;

/// Legacy Linux scheduler parameter ABI.
///
/// The kernel ABI contains exactly one `int`, even when libc exposes a larger
/// source-level structure with reserved fields.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct KernelSchedParam {
    pub sched_priority: i32,
}

const _: () = assert!(core::mem::size_of::<KernelSchedParam>() == 4);

pub(super) const SCHED_OTHER: i32 = 0;
pub(super) const SCHED_FIFO: i32 = 1;
pub(super) const SCHED_RR: i32 = 2;
pub(super) const SCHED_BATCH: i32 = 3;
pub(super) const SCHED_IDLE: i32 = 5;
pub(super) const SCHED_DEADLINE: i32 = 6;
pub(super) const SCHED_RESET_ON_FORK: i32 = 0x4000_0000;

#[inline]
pub(super) fn policy_to_linux(policy: SchedPolicy) -> i32 {
    match policy {
        SchedPolicy::CFS => SCHED_OTHER,
        SchedPolicy::FIFO => SCHED_FIFO,
        SchedPolicy::RT => SCHED_RR,
        SchedPolicy::IDLE => SCHED_IDLE,
    }
}
