//! `rt_sigreturn` 系统调用处理：从用户栈恢复信号上下文。
//!
//! 作为普通 syscall 表项走统一分发路径，自动获得 ptrace 的
//! entry/exit-stop、SYSEMU 跳过与参数改写语义（与 Linux 一致，
//! Linux 中 rt_sigreturn 就是 syscall 表普通表项，无任何特判）。

use crate::{
    arch::{interrupt::TrapFrame, ipc::signal::X86_64SignalArch, syscall::nr::SYS_RT_SIGRETURN},
    ipc::signal_types::SignalArch,
    syscall::table::{FormattedSyscallParam, Syscall},
};
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysRtSigreturnHandle;

impl Syscall for SysRtSigreturnHandle {
    fn num_args(&self) -> usize {
        0
    }

    fn handle(&self, _args: &[usize], frame: &mut TrapFrame) -> Result<usize, SystemError> {
        // 返回恢复后的 rax——通用尾部的 set_return_value 写回同一值
        let r = <X86_64SignalArch as SignalArch>::sys_rt_sigreturn(frame) as usize;
        Ok(r)
    }

    fn entry_format(&self, _args: &[usize]) -> Vec<FormattedSyscallParam> {
        Vec::new()
    }
}

syscall_table_macros::declare_syscall!(SYS_RT_SIGRETURN, SysRtSigreturnHandle);
