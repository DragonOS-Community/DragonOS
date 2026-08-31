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

/// Returns the legacy scheduler priority range as `(min, max)`.
///
/// Keep this table aligned with Linux's `sched_get_priority_{min,max}` ABI.
/// Policy flags are intentionally rejected because these syscalls accept a
/// policy value, not the flag-bearing value returned by `sched_getscheduler`.
#[inline]
pub(super) fn legacy_policy_priority_range(policy: i32) -> Option<(i32, i32)> {
    match policy {
        SCHED_FIFO | SCHED_RR => Some((1, crate::sched::prio::MAX_RT_PRIO - 1)),
        SCHED_OTHER | SCHED_BATCH | SCHED_IDLE | SCHED_DEADLINE => Some((0, 0)),
        _ => None,
    }
}

#[inline]
pub(super) fn policy_to_linux(policy: SchedPolicy) -> i32 {
    match policy {
        SchedPolicy::CFS => SCHED_OTHER,
        SchedPolicy::FIFO => SCHED_FIFO,
        SchedPolicy::RT => SCHED_RR,
        SchedPolicy::IDLE => SCHED_IDLE,
    }
}
