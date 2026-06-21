//! System call handler for munlockall.

use alloc::vec::Vec;
use system_error::SystemError;

use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_MUNLOCKALL},
    mm::ucontext::AddressSpace,
    syscall::table::{FormattedSyscallParam, Syscall},
};

pub struct SysMunlockallHandle;

impl Syscall for SysMunlockallHandle {
    fn num_args(&self) -> usize {
        0
    }

    fn handle(&self, _args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let vm = AddressSpace::current()?;
        loop {
            let mut guard = vm.write_interruptible()?;
            if guard.mappings.first_reservation_region().is_some() {
                drop(guard);
                vm.wait_for_no_reservations_interruptible()?;
                continue;
            }
            guard.clear_all_vma_lock_flags()?;
            return Ok(0);
        }
    }

    fn entry_format(&self, _args: &[usize]) -> Vec<FormattedSyscallParam> {
        Vec::new()
    }
}

syscall_table_macros::declare_syscall!(SYS_MUNLOCKALL, SysMunlockallHandle);
