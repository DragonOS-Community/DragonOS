//! System call handler for munlockall.

use alloc::vec::Vec;
use system_error::SystemError;

use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_MUNLOCKALL},
    syscall::table::{FormattedSyscallParam, Syscall},
};

pub struct SysMunlockallHandle;

impl Syscall for SysMunlockallHandle {
    fn num_args(&self) -> usize {
        0
    }

    fn handle(&self, _args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        // TODO: implement real munlockall semantics by clearing VM_LOCKED/VM_LOCKONFAULT
        // from all VMAs and clearing PG_UNEVICTABLE from pages that are no longer locked.
        Ok(0)
    }

    fn entry_format(&self, _args: &[usize]) -> Vec<FormattedSyscallParam> {
        Vec::new()
    }
}

syscall_table_macros::declare_syscall!(SYS_MUNLOCKALL, SysMunlockallHandle);
