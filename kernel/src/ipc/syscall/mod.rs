pub mod sys_kill;
pub mod sys_pipe2;
mod sys_restart;
mod sys_rt_sigprocmask;
mod sys_shmat;
mod sys_shmctl;
mod sys_shmdt;
mod sys_shmget;
mod sys_sigaction;
mod sys_sigpending;

#[cfg(target_arch = "x86_64")]
pub mod sys_pipe;
