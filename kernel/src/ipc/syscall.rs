use core::ffi::c_int;

use crate::{
    filesystem::vfs::file::{File, FileMode},
    process::{Pid, ProcessManager},
    syscall::{Syscall, SystemError},
};

use super::pipe::LockedPipeInode;

impl Syscall {
    /// # 创建匿名管道
    ///
    /// ## 参数
    ///
    /// - `fd`: 用于返回文件描述符的数组
    pub fn pipe(fd: &mut [i32]) -> Result<usize, SystemError> {
        let pipe_ptr = LockedPipeInode::new();
        let read_file = File::new(pipe_ptr.clone(), FileMode::O_RDONLY)?;
        let write_file = File::new(pipe_ptr.clone(), FileMode::O_WRONLY)?;
        let binding = ProcessManager::current_pcb().fd_table();
        let mut fd_table_guard = binding.write();

        let read_fd = fd_table_guard.alloc_fd(read_file, None)?;
        let write_fd = fd_table_guard.alloc_fd(write_file, None)?;

        drop(fd_table_guard);

        fd[0] = read_fd;
        fd[1] = write_fd;

        return Ok(0);
    }

    pub fn kill(_pid: Pid, _sig: c_int) -> Result<usize, SystemError> {
        // todo: 由于进程管理重构，目前删除了signal功能，将来重新实现它。
        return Err(SystemError::ENOSYS);
    }

    /// @brief 用户程序用于设置信号处理动作的函数（遵循posix2008）
    ///
    /// @param regs->r8 signumber 信号的编号
    /// @param regs->r9 act 新的，将要被设置的sigaction
    /// @param regs->r10 oact 返回给用户的原本的sigaction（内核将原本的sigaction的值拷贝给这个地址）
    ///
    /// @return int 错误码
    #[no_mangle]
    pub fn sigaction(
        _sig: c_int,
        _act: usize,
        _old_act: usize,
        _from_user: bool,
    ) -> Result<usize, SystemError> {
        // todo: 由于进程管理重构，目前删除了signal功能，将来重新实现它。
        return Err(SystemError::ENOSYS);
    }
}
