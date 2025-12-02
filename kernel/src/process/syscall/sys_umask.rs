use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_UMASK;
use crate::filesystem::vfs::InodeMode;
use crate::process::ProcessManager;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysUmaskHandle;

impl SysUmaskHandle {
    fn mask(args: &[usize]) -> InodeMode {
        InodeMode::from_bits_truncate(args[0] as u32)
    }
}

impl Syscall for SysUmaskHandle {
    fn num_args(&self) -> usize {
        1
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let new_mask = Self::mask(args) & InodeMode::S_IRWXUGO;
        let old_mask = ProcessManager::current_pcb()
            .fs_struct()
            .set_umask(new_mask);
        Ok(old_mask.bits() as usize)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new(
            "mask",
            format!("{:#o}", Self::mask(args)),
        )]
    }
}

syscall_table_macros::declare_syscall!(SYS_UMASK, SysUmaskHandle);
