use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_PERSONALITY},
    process::ProcessManager,
    syscall::table::{FormattedSyscallParam, Syscall},
};
use alloc::vec::Vec;
use system_error::SystemError;

/// `SYS_personality` — read or set the process execution domain.
/// When the argument is `0xffffffff`, only the current value is read;
/// otherwise set the new value and return the old one.
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
