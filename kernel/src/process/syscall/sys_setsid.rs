use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_SETSID;
use crate::process::session::ksys_setsid;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use alloc::vec::Vec;
use system_error::SystemError;
pub struct SysSetsid;

impl Syscall for SysSetsid {
    fn num_args(&self) -> usize {
        0
    }

    /// # 函数的功能
    /// 创建新的会话
    fn handle(&self, _args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        ksys_setsid().map(|sid| {
            // 返回会话ID
            sid.data()
        })
    }

    fn entry_format(&self, _args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![]
    }
}

syscall_table_macros::declare_syscall!(SYS_SETSID, SysSetsid);
