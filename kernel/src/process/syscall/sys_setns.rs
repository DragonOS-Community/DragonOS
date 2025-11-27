use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_SETNS;
use crate::process::namespace::setns::ksys_setns;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysSetns;

impl SysSetns {
    #[inline(always)]
    fn fd(args: &[usize]) -> i32 {
        args[0] as i32
    }

    #[inline(always)]
    fn nstype(args: &[usize]) -> i32 {
        args[1] as i32
    }
}

impl Syscall for SysSetns {
    fn num_args(&self) -> usize {
        2
    }

    /// DragonOS 的 setns 系统调用实现（当前仅支持 pidfd + namespace flag）
    ///
    /// - `fd`：指向 pidfd 的文件描述符
    /// - `nstype`：CloneFlags 风格的命名空间标志组合
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fd = Self::fd(args);
        let nstype = Self::nstype(args);

        ksys_setns(fd, nstype).map(|_| 0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("fd", format!("{}", Self::fd(args))),
            FormattedSyscallParam::new("nstype", format!("{:#x}", Self::nstype(args))),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_SETNS, SysSetns);
