use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_VFORK;
use crate::process::fork::{CloneFlags, KernelCloneArgs};
use crate::process::syscall::clone_utils;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysVfork;

impl Syscall for SysVfork {
    fn num_args(&self) -> usize {
        0
    }

    fn handle(&self, _args: &[usize], frame: &mut TrapFrame) -> Result<usize, SystemError> {
        // vfork 的正确实现：使用 CLONE_VFORK | CLONE_VM
        // CLONE_VFORK: 父进程会被阻塞，直到子进程调用 execve 或 exit
        // CLONE_VM: 子进程和父进程共享内存空间
        let mut clone_args = KernelCloneArgs::new();
        clone_args.flags = CloneFlags::CLONE_VFORK | CloneFlags::CLONE_VM;

        clone_utils::do_clone(clone_args, frame)
    }

    fn entry_format(&self, _args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![]
    }
}

syscall_table_macros::declare_syscall!(SYS_VFORK, SysVfork);
