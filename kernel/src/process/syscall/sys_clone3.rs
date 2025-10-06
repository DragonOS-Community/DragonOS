use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_CLONE3;
use crate::process::fork::KernelCloneArgs;
use crate::process::syscall::clone_utils::do_clone;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysClone3;

impl Syscall for SysClone3 {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let uarg_ptr = Self::uargs(args);
        let size = Self::size(args);

        let mut kargs = KernelCloneArgs::new();
        kargs.copy_clone_args_from_user(uarg_ptr, size)?;

        do_clone(kargs, frame)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("uargs", format!("{:#x}", Self::uargs(args))),
            FormattedSyscallParam::new("size", format!("{:#x}", Self::size(args))),
        ]
    }
}

impl SysClone3 {
    fn uargs(args: &[usize]) -> usize {
        args[0]
    }

    fn size(args: &[usize]) -> usize {
        args[1]
    }
}

syscall_table_macros::declare_syscall!(SYS_CLONE3, SysClone3);
