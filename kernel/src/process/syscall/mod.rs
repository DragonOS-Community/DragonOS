mod sys_clone;
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
mod sys_uname;
mod sys_wait4;

#[cfg(target_arch = "x86_64")]
mod sys_fork;

#[cfg(target_arch = "x86_64")]
mod sys_getpgrp;
#[cfg(target_arch = "x86_64")]
mod sys_getrlimit;
#[cfg(target_arch = "x86_64")]
mod sys_vfork;

//参考资料：https://code.dragonos.org.cn/xref/linux-6.1.9/include/uapi/linux/utsname.h#17
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PosixOldUtsName {
    pub sysname: [u8; 65],
    pub nodename: [u8; 65],
    pub release: [u8; 65],
    pub version: [u8; 65],
    pub machine: [u8; 65],
}

impl PosixOldUtsName {
    pub fn new() -> Self {
        const SYS_NAME: &[u8] = b"Linux";
        const NODENAME: &[u8] = b"DragonOS";
        const RELEASE: &[u8] = b"5.19.0";
        const VERSION: &[u8] = b"5.19.0";

        #[cfg(target_arch = "x86_64")]
        const MACHINE: &[u8] = b"x86_64";

        #[cfg(target_arch = "aarch64")]
        const MACHINE: &[u8] = b"aarch64";

        #[cfg(target_arch = "riscv64")]
        const MACHINE: &[u8] = b"riscv64";

        #[cfg(target_arch = "loongarch64")]
        const MACHINE: &[u8] = b"longarch64";

        let mut r = Self {
            sysname: [0; 65],
            nodename: [0; 65],
            release: [0; 65],
            version: [0; 65],
            machine: [0; 65],
        };

        r.sysname[0..SYS_NAME.len()].copy_from_slice(SYS_NAME);
        r.nodename[0..NODENAME.len()].copy_from_slice(NODENAME);
        r.release[0..RELEASE.len()].copy_from_slice(RELEASE);
        r.version[0..VERSION.len()].copy_from_slice(VERSION);
        r.machine[0..MACHINE.len()].copy_from_slice(MACHINE);

        return r;
    }
}
