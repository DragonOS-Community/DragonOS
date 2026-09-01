#[cfg(target_arch = "x86_64")]
mod sys_pause;

mod sys_sched_get_priority;
mod sys_sched_getparam;
mod sys_sched_getscheduler;
mod sys_sched_rr_get_interval;
mod sys_sched_setparam;
mod sys_sched_setscheduler;
mod sys_sched_yield;
mod types;
pub(crate) mod util;
