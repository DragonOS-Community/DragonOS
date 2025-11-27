use core::ffi::c_int;

use crate::arch::syscall::nr::SYS_DUP3;
use crate::filesystem::vfs::file::FileFlags;
use crate::filesystem::vfs::syscall::dup2::do_dup3;
use crate::{
    arch::interrupt::TrapFrame,
    process::ProcessManager,
    syscall::table::{FormattedSyscallParam, Syscall},
};
use alloc::vec::Vec;
use system_error::SystemError;
pub struct SysDup3Handle;

impl Syscall for SysDup3Handle {
    fn num_args(&self) -> usize {
        3
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let oldfd = Self::oldfd(args);
        let newfd = Self::newfd(args);
        let flags = Self::flags(args);
        let flags = FileFlags::from_bits_truncate(flags);
        if (flags.bits() & !FileFlags::O_CLOEXEC.bits()) != 0 {
            return Err(SystemError::EINVAL);
        }

        if oldfd == newfd {
            return Err(SystemError::EINVAL);
        }

        let binding = ProcessManager::current_pcb().fd_table();
        let mut fd_table_guard = binding.write();
        return do_dup3(oldfd, newfd, flags, &mut fd_table_guard);
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("oldfd", format!("{:#x}", Self::oldfd(args))),
            FormattedSyscallParam::new("newfd", format!("{:#x}", Self::newfd(args))),
            FormattedSyscallParam::new("flags", format!("{:#x}", Self::flags(args))),
        ]
    }
}

impl SysDup3Handle {
    fn oldfd(args: &[usize]) -> c_int {
        args[0] as c_int
    }

    fn newfd(args: &[usize]) -> c_int {
        args[1] as c_int
    }

    fn flags(args: &[usize]) -> u32 {
        args[2] as u32
    }
}

syscall_table_macros::declare_syscall!(SYS_DUP3, SysDup3Handle);
