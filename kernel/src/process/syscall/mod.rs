pub mod clone_utils;
mod sys_cap_get_set;
mod sys_clone;
mod sys_clone3;
mod sys_execve;
mod sys_execveat;
mod sys_exit;
mod sys_exit_group;
mod sys_get_rusage;
mod sys_getegid;
mod sys_geteuid;
mod sys_getgid;
mod sys_getpgid;
mod sys_getpid;
mod sys_getppid;
mod sys_getsid;
mod sys_gettid;
mod sys_getuid;
mod sys_groups;
pub mod sys_prlimit64;
mod sys_set_tid_address;
mod sys_setdomainname;
mod sys_setfsgid;
mod sys_setfsuid;
mod sys_setgid;
mod sys_sethostname;
mod sys_setpgid;
mod sys_setresgid;
mod sys_setresuid;
mod sys_setsid;
mod sys_setuid;
mod sys_uname;
mod sys_unshare;
mod sys_wait4;

#[cfg(target_arch = "x86_64")]
mod sys_fork;

#[cfg(target_arch = "x86_64")]
mod sys_getpgrp;
#[cfg(target_arch = "x86_64")]
mod sys_getrlimit;
#[cfg(target_arch = "x86_64")]
mod sys_setrlimit;
#[cfg(target_arch = "x86_64")]
mod sys_vfork;
