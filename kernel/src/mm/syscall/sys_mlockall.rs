//! System call handler for mlockall.

use alloc::vec::Vec;
use system_error::SystemError;

use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_MLOCKALL, MMArch},
    mm::{can_do_mlock, ucontext::AddressSpace, MemoryManagementArch},
    syscall::table::{FormattedSyscallParam, Syscall},
};

use super::sys_mlock::check_mlock_rlimit;

const MCL_CURRENT: usize = 0x01;
const MCL_FUTURE: usize = 0x02;
const MCL_ONFAULT: usize = 0x04;
const MCL_VALID_MASK: usize = MCL_CURRENT | MCL_FUTURE | MCL_ONFAULT;

pub struct SysMlockallHandle;

impl Syscall for SysMlockallHandle {
    fn num_args(&self) -> usize {
        1
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        do_mlockall(Self::flags(args))
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new(
            "flags",
            format!("{:#x}", Self::flags(args)),
        )]
    }
}

impl SysMlockallHandle {
    fn flags(args: &[usize]) -> usize {
        args[0]
    }
}

fn do_mlockall(flags: usize) -> Result<usize, SystemError> {
    if flags == 0 || flags & !MCL_VALID_MASK != 0 {
        return Err(SystemError::EINVAL);
    }
    if flags == MCL_ONFAULT {
        return Err(SystemError::EINVAL);
    }

    if !can_do_mlock() {
        return Err(SystemError::EPERM);
    }

    if flags & MCL_CURRENT != 0 {
        let vm = AddressSpace::current()?;
        let guard = vm.read_interruptible()?;
        let total_pages = guard.vma_usage_bytes() >> MMArch::PAGE_SHIFT;
        let new_pages = total_pages.saturating_sub(guard.locked_vm);
        check_mlock_rlimit(guard.locked_vm, new_pages)?;
    }

    // TODO: implement real mlockall semantics. MCL_CURRENT should mark all current
    // VMAs/pages locked; MCL_FUTURE and MCL_ONFAULT need address-space state so
    // future mappings/faults inherit the requested locking behavior.
    Ok(0)
}

syscall_table_macros::declare_syscall!(SYS_MLOCKALL, SysMlockallHandle);
