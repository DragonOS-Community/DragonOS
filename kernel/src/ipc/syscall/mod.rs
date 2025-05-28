pub mod sys_kill;
pub mod sys_sigaction;
pub mod sys_shmget;
pub mod sys_shmat;
 pub mod sys_shmdt;
pub mod sys_pipe2;
#[cfg(target_arch = "x86_64")]
pub mod sys_pipe;

pub mod sys_rt_sigprocmask;
pub mod sys_shmctl;

#[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
pub mod sys_restart;
#[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
pub mod sys_sigpending;