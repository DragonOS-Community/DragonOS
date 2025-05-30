#[cfg(any(
    target_arch = "x86_64",
    target_arch = "riscv64",
    target_arch = "loongarch64"
))]
mod sys_getegid;
mod sys_geteuid;
mod sys_getgid;
mod sys_getpgid;
mod sys_getpid;
mod sys_getppid;
mod sys_getsid;
mod sys_gettid;
mod sys_getuid;
mod sys_set_tid_address;
mod sys_setfsgid;
mod sys_setfsuid;
mod sys_setgid;
mod sys_setpgid;
mod sys_setresgid;
mod sys_setresuid;
mod sys_setsid;
mod sys_setuid;
mod sys_get_rusage;
