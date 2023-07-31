use core::ffi::{c_int, c_void};

use crate::{
    arch::asm::current::current_pcb,
    
    include::bindings::bindings::{pid_t, process_do_exit, ESRCH},
    syscall::{Syscall, SystemError},
};
use super::{Pid, ProcessControlBlock, ProcessManager,ALL_PROCESS};
extern "C" {
    fn c_sys_wait4(pid: pid_t, wstatus: *mut c_int, options: c_int, rusage: *mut c_void) -> c_int;
}

impl Syscall {
    #[allow(dead_code)]
    pub fn fork(&self) -> Result<usize, SystemError> {
        // 由于进程管理未完成重构，fork调用暂时在arch/x86_64/syscall.rs中调用，以后会移动到这里。
        todo!()
    }

    #[allow(dead_code)]
    pub fn vfork(&self) -> Result<usize, SystemError> {
        // 由于进程管理未完成重构，vfork调用暂时在arch/x86_64/syscall.rs中调用，以后会移动到这里。
        todo!()
    }

    #[allow(dead_code)]
    pub fn execve(
        _path: *const c_void,
        _argv: *const *const c_void,
        _envp: *const *const c_void,
    ) -> Result<usize, SystemError> {
        // 由于进程管理未完成重构，execve调用暂时在arch/x86_64/syscall.rs中调用，以后会移动到这里。
        todo!()
    }

    pub fn wait4(
        pid: pid_t,
        wstatus: *mut c_int,
        options: c_int,
        rusage: *mut c_void,
    ) -> Result<usize, SystemError> {
        let ret = unsafe { c_sys_wait4(pid, wstatus, options, rusage) };
        if (ret as isize) < 0 {
            return Err(SystemError::from_posix_errno(-(ret as isize) as i32)
                .expect("wait4: Invalid errno"));
        }
        return Ok(ret as usize);
    }

    /// # 退出进程
    ///
    /// ## 参数
    ///
    /// - status: 退出状态
    pub fn exit(status: usize) -> ! {
        unsafe { process_do_exit(status as u64) };
        loop {}
    }

    // 获取进程ID
    pub fn getpid() -> Result<Pid, SystemError> {
        let current_pcb = ProcessManager::current_pcb();
        return Ok(current_pcb.basic().pid() as Pid);
    }

    //根据
    pub fn getpgid(mut pid:usize) -> Result<Pid,SystemError>{
        if pid== 0{
            let current_pcb = ProcessManager::current_pcb();
            pid = current_pcb.basic().pid();
        }
        let theproc= ProcessManager::find(pid);
        if let Some(theproc) = theproc{
            return Ok(theproc.basic().pgid());
        }else{
            return Err(ESRCH);
        }
    }

    //若为initproc则ppid设置为0   
    pub fn getppid() -> Result<Pid,SystemError>{
        let current_pcb = ProcessManager::current_pcb();
        return Ok(current_pcb.basic().ppid() as Pid);
    }


}
