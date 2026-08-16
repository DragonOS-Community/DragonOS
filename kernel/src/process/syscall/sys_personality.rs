use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_PERSONALITY},
    process::ProcessManager,
    syscall::table::{FormattedSyscallParam, Syscall},
};
use alloc::vec::Vec;
use system_error::SystemError;

/// `SYS_personality` —— 读取或设置进程执行域。
/// 当参数为 `0xffffffff` 时只读当前值；否则设新值并返回旧值。
pub struct SysPersonality;

impl Syscall for SysPersonality {
    fn num_args(&self) -> usize {
        1
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let new = args[0] as u32;
        let current_pcb = ProcessManager::current_pcb();
        let mut basic = current_pcb.basic_mut();
        let old = basic.personality();
        if new != 0xffffffff {
            basic.set_personality(new);
        }
        Ok(old as usize)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new(
            "personality",
            format!("{:#x}", args[0] as u32),
        )]
    }
}

syscall_table_macros::declare_syscall!(SYS_PERSONALITY, SysPersonality);
