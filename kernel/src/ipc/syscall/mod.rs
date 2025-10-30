pub mod sys_kill;
mod sys_pidfd_sendsignal;
#[cfg(target_arch = "x86_64")]
pub mod sys_pipe;
pub mod sys_pipe2;
mod sys_restart;
mod sys_rt_sigprocmask;
mod sys_rt_sigsuspend;
pub mod sys_rt_sigtimedwait;
mod sys_shmat;
mod sys_shmctl;
mod sys_shmdt;
mod sys_shmget;
mod sys_sigaction;
mod sys_sigaltstack;
mod sys_sigpending;
pub mod sys_tgkill;
pub mod sys_tkill;
