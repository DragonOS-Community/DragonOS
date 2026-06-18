//! System call handler for munlock.

use alloc::vec::Vec;
use system_error::SystemError;

use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_MUNLOCK},
    mm::{access_ok, ucontext::AddressSpace, VirtAddr, VirtRegion, VmFlags},
    syscall::table::{FormattedSyscallParam, Syscall},
};

use super::sys_mlock::normalize_mlock_range;

pub struct SysMunlockHandle;

impl Syscall for SysMunlockHandle {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let start = VirtAddr::new(Self::start(args));
        let len = Self::len(args);
        let (start, len) = normalize_mlock_range(start, len)?;
        if len == 0 {
            return Ok(0);
        }
        if access_ok(start, len).is_err() {
            return Err(SystemError::EINVAL);
        }

        let vm = AddressSpace::current()?;
        let region = VirtRegion::new(start, len);
        loop {
            let mut guard = vm.write_interruptible()?;
            if guard.mappings.first_reservation_conflict(region).is_some() {
                drop(guard);
                vm.wait_for_no_reservation_conflict_interruptible(region)?;
                continue;
            }
            guard.apply_vma_lock_flags(start, len, VmFlags::VM_NONE, false)?;
            return Ok(0);
        }
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("start", format!("{:#x}", Self::start(args))),
            FormattedSyscallParam::new("len", format!("{:#x}", Self::len(args))),
        ]
    }
}

impl SysMunlockHandle {
    fn start(args: &[usize]) -> usize {
        args[0]
    }

    fn len(args: &[usize]) -> usize {
        args[1]
    }
}

syscall_table_macros::declare_syscall!(SYS_MUNLOCK, SysMunlockHandle);
