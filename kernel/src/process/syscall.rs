use core::ffi::{c_int, c_void};

use alloc::{string::String, vec::Vec};

use super::{Pid, ProcessManager, ProcessState};
use crate::{
    arch::interrupt::TrapFrame,
    filesystem::vfs::MAX_PATHLEN,
    include::bindings::bindings::pid_t,
    syscall::{
        user_access::{check_and_clone_cstr, check_and_clone_cstr_array, UserBufferWriter, UserBufferReader},
        Syscall, SystemError,
    },
};

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
        wstatus: *mut i32,
        options: i32,
        rusage: *mut c_void,
    ) -> Result<usize, SystemError> {
        let mut _rusage_buf =
            UserBufferReader::new::<c_void>(rusage, core::mem::size_of::<c_void>(), true)?;

        let mut wstatus_buf =
            UserBufferWriter::new::<i32>(wstatus, core::mem::size_of::<i32>(), true)?;

        // 暂时不支持options选项
        if options != 0 {
            return Err(SystemError::EINVAL);
        }

        let cur_pcb = ProcessManager::current_pcb();
        let rd_childen = cur_pcb.children.read();
        let child_proc = rd_childen.get(&Pid(pid as usize));
        // 判断是否是子进程
        if child_proc.is_none() {
            return Err(SystemError::ECHILD);
        }
        let child_pcb = child_proc.unwrap();

        if pid > 0 {
            // 等待指定进程
            child_pcb.wait_queue.sleep();
        } else if pid < -1 {
            // TODO 判断是否pgid == -pid（等待指定组任意进程）
            // 暂时不支持
            return Err(SystemError::EINVAL);
        } else if pid == 0 {
            // TODO 判断是否pgid == current_pgid（等待当前组任意进程）
            // 暂时不支持
            return Err(SystemError::EINVAL);
        } else {
            // 等待任意子进程
            rd_childen.iter().for_each(|x| x.1.wait_queue.sleep());
        }
        // 获取退出码
        if let ProcessState::Exited(status) = child_proc.unwrap().sched_info().state() {
            if !wstatus.is_null() {
                wstatus_buf.copy_one_to_user(&status, 0)?;
            }
        }

        drop(child_pcb);
        return Ok(0);
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
