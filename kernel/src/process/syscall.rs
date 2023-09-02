use core::ffi::{c_int, c_void};

use alloc::{string::String, vec::Vec};

use super::{Pid, ProcessManager};
use crate::{
    arch::interrupt::TrapFrame,
    filesystem::vfs::MAX_PATHLEN,
    include::bindings::bindings::pid_t,
    syscall::{
        user_access::{check_and_clone_cstr, check_and_clone_cstr_array},
        Syscall, SystemError,
    },
};
extern "C" {
    fn c_sys_wait4(pid: pid_t, wstatus: *mut c_int, options: c_int, rusage: *mut c_void) -> c_int;
}

impl Syscall {
    pub fn execve(
        path: *const u8,
        argv: *const *const u8,
        envp: *const *const u8,
        frame: &mut TrapFrame,
    ) -> Result<(), SystemError> {
        if path.is_null() {
            return Err(SystemError::EINVAL);
        }

        let x = || {
            let path: String = check_and_clone_cstr(path, Some(MAX_PATHLEN))?;
            let argv: Vec<String> = check_and_clone_cstr_array(argv)?;
            let envp: Vec<String> = check_and_clone_cstr_array(envp)?;
            Ok((path, argv, envp))
        };
        let r: Result<(String, Vec<String>, Vec<String>), SystemError> = x();
        if let Err(e) = r {
            panic!("Failed to execve: {:?}", e);
        }
        let (path, argv, envp) = r.unwrap();

        return Self::do_execve(path, argv, envp, frame);
    }

    pub fn wait4(
        pid: pid_t,
        wstatus: *mut c_int,
        options: c_int,
        rusage: *mut c_void,
    ) -> Result<usize, SystemError> {
        // TODO 将c_sys_wait4使用rust实现
        let ret = unsafe { c_sys_wait4(pid, wstatus, options, rusage) };
        if (ret as isize) < 0 {
            return Err(
                SystemError::from_posix_errno((ret as isize) as i32).expect("wait4: Invalid errno")
            );
        }
        return Ok(ret as usize);
    }

    /// # 退出进程
    ///
    /// ## 参数
    ///
    /// - status: 退出状态
    pub fn exit(status: usize) -> ! {
        ProcessManager::exit(status);
    }

    /// @brief 获取当前进程的pid
    pub fn getpid() -> Result<Pid, SystemError> {
        let current_pcb = ProcessManager::current_pcb();
        return Ok(current_pcb.basic().pid());
    }

    /// @brief 获取指定进程的pgid
    ///
    /// @param pid 指定一个进程号
    ///
    /// @return 成功，指定进程的进程组id
    /// @return 错误，不存在该进程
    pub fn getpgid(mut pid: Pid) -> Result<Pid, SystemError> {
        if pid == Pid(0) {
            let current_pcb = ProcessManager::current_pcb();
            pid = current_pcb.basic().pid();
        }
        let target_proc = ProcessManager::find(pid).ok_or(SystemError::ESRCH)?;
        return Ok(target_proc.basic().pgid());
    }
    /// @brief 获取当前进程的父进程id

    /// 若为initproc则ppid设置为0   
    pub fn getppid() -> Result<Pid, SystemError> {
        let current_pcb = ProcessManager::current_pcb();
        return Ok(current_pcb.basic().ppid());
    }
}
