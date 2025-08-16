use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_UNSHARE;
use crate::process::fork::CloneFlags;
use crate::process::namespace::unshare::ksys_unshare;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysUnshare;

impl SysUnshare {
    fn flags(args: &[usize]) -> CloneFlags {
        CloneFlags::from_bits_truncate(args[0] as u64)
    }
}

impl Syscall for SysUnshare {
    fn num_args(&self) -> usize {
        1
    }

    /// # 函数的功能
    /// unshare系统调用允许进程将其部分执行上下文与其他进程解耦
    ///
    /// ## 参数
    /// - flags: 指定要解耦的资源类型
    ///
    /// ## 返回值
    /// - 成功时返回0
    /// - 失败时返回错误码
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let flags = Self::flags(args);
        ksys_unshare(flags).map(|_| 0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new(
            "flags",
            format!("{:#x}", Self::flags(args).bits()),
        )]
    }
}

syscall_table_macros::declare_syscall!(SYS_UNSHARE, SysUnshare);
