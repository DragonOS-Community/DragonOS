use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_VFORK;
use crate::process::fork::CloneFlags;
use crate::process::ProcessManager;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysVfork;

impl Syscall for SysVfork {
    fn num_args(&self) -> usize {
        0
    }

    fn handle(&self, _args: &[usize], frame: &mut TrapFrame) -> Result<usize, SystemError> {
        // 由于Linux vfork需要保证子进程先运行（除非子进程调用execve或者exit），
        // 而我们目前没有实现这个特性，所以暂时使用fork代替vfork（linux文档表示这样也是也可以的）
        ProcessManager::fork(frame, CloneFlags::empty()).map(|pid| pid.into())

        // 下面是以前的实现，除非我们实现了子进程先运行的特性，否则不要使用，不然会导致父进程数据损坏
        // ProcessManager::fork(
        //     frame,
        //     CloneFlags::CLONE_VM | CloneFlags::CLONE_FS | CloneFlags::CLONE_SIGNAL,
        // )
        // .map(|pid| pid.into())
    }

    fn entry_format(&self, _args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![]
    }
}

syscall_table_macros::declare_syscall!(SYS_VFORK, SysVfork);
