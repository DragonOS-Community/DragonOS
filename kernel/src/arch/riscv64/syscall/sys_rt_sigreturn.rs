//! rt_sigreturn 的系统调用表入口，恢复用户信号帧并保留原 a0 返回值。

use crate::{
    arch::{interrupt::TrapFrame, ipc::signal::RiscV64SignalArch, syscall::nr::SYS_RT_SIGRETURN},
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
        // 通用系统调用出口会写回返回值，因此这里透传恢复后的 a0。
        let r = <RiscV64SignalArch as SignalArch>::sys_rt_sigreturn(frame) as usize;
        Ok(r)
    }

    fn entry_format(&self, _args: &[usize]) -> Vec<FormattedSyscallParam> {
        Vec::new()
    }
}

syscall_table_macros::declare_syscall!(SYS_RT_SIGRETURN, SysRtSigreturnHandle);
