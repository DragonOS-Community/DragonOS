#[cfg(target_arch = "x86_64")]
mod sys_pause;

mod sys_sched_getparam;
mod sys_sched_getscheduler;
mod sys_sched_yield;
mod util;
