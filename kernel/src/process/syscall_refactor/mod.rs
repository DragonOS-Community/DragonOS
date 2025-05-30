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
mod sys_prlimit64;
mod sys_set_tid_address;
mod sys_setfsgid;
mod sys_setfsuid;
mod sys_setgid;
mod sys_setpgid;
mod sys_setresgid;
mod sys_setresuid;
mod sys_setsid;
mod sys_setuid;
mod sys_wait4;

#[cfg(target_arch = "x86_64")]
mod sys_getrlimit;
