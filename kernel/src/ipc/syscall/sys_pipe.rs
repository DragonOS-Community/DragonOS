use super::sys_pipe2::do_kernel_pipe2;
use crate::arch::syscall::nr::SYS_PIPE;
use crate::{
    filesystem::vfs::file::FileMode,
    syscall::table::{FormattedSyscallParam, Syscall},
};
use alloc::vec::Vec;
use core::ffi::c_int;
use system_error::SystemError;

pub struct SysPipeHandle;

impl SysPipeHandle {
    #[inline(always)]
    fn pipefd(args: &[usize]) -> *mut i32 {
        // 第一个参数是fd指针
        args[0] as *mut c_int
    }
}

impl Syscall for SysPipeHandle {
    fn num_args(&self) -> usize {
        1 // pipefd
    }

    fn handle(&self, args: &[usize], _from_user: bool) -> Result<usize, SystemError> {
        let pipefd = Self::pipefd(args);
        if pipefd.is_null() {
            return Err(SystemError::EFAULT);
        } else {
            do_kernel_pipe2(pipefd, FileMode::empty())
        }
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        let fd_ptr = Self::pipefd(args);
        vec![FormattedSyscallParam::new(
            "fd_ptr",
            format!("{}", fd_ptr as usize),
        )]
    }
}

syscall_table_macros::declare_syscall!(SYS_PIPE, SysPipeHandle);
