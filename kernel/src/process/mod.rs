pub mod abi;
pub mod cputime;
pub mod cred;
pub mod exec;
pub mod execve;
pub mod exit;
pub mod fork;
pub mod geteuid;
pub mod idle;
pub mod kthread;
pub mod namespace;
pub mod pid;
pub mod pidfd;
pub mod posix_timer;
pub mod preempt;
pub mod process_group;
pub mod ptrace;
pub mod resource;
pub mod rseq;
pub mod seccomp;
pub mod session;
pub mod shebang;
pub mod signal;
pub mod stdio;
pub mod syscall;
pub mod timer;
pub mod trace;
pub mod utils;
pub mod wait;

use core::sync::atomic::AtomicUsize;

mod info;
mod kstack;
mod manager;
mod sched_info;
mod state;
mod task;

int_like!(RawPid, AtomicRawPid, usize, AtomicUsize);

pub use cputime::ProcessCpuTime;
pub(crate) use cred::Cred;
#[allow(unused_imports)]
pub use info::{
    CpuItimer, ProcessBasicInfo, ProcessItimer, ProcessItimers, ProcessSignalInfo, ThreadInfo,
};
#[allow(unused_imports)]
pub use kstack::{KernelStack, KernelStackType};
pub(crate) use manager::{
    account_context_switch, account_successful_fork, all_process, dec_visible_thread_count,
    inc_visible_thread_count, lock_fs_refs_copy, lock_fs_refs_pivot, FsRefsReadGuard,
};
#[allow(unused_imports)]
pub use manager::{
    nr_context_switches, nr_threads, total_forks, ProcessManager, SwitchResult,
    PROCESS_SWITCH_RESULT,
};
use manager::{PTRACE_RELATION_LOCK, __PROCESS_MANAGEMENT_INIT_DONE};
use pid::alloc_pid;
pub(crate) use process_group::Pgid;
#[allow(unused_imports)]
pub(crate) use sched_info::NewTaskPlacement;
#[allow(unused_imports)]
pub use sched_info::{PiProtected, ProcessSchedulerInfo, SchedInfo};
pub use state::{ExitState, ProcessFlags, ProcessState};
pub use task::ProcessControlBlock;

pub fn process_init() {
    ProcessManager::init();
}

/// Context switch hook function. When this function returns, a context switch
/// will occur.
#[cfg(target_arch = "x86_64")]
#[inline(never)]
pub unsafe extern "sysv64" fn switch_finish_hook() {
    ProcessManager::switch_finish_hook();
}
#[cfg(target_arch = "riscv64")]
#[inline(always)]
pub unsafe fn switch_finish_hook() {
    ProcessManager::switch_finish_hook();
}
