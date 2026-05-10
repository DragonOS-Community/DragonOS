//! System call handler for mlock2.

use alloc::vec::Vec;
use system_error::SystemError;

use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_MLOCK2},
    mm::{access_ok, can_do_mlock, ucontext::AddressSpace, VirtAddr, VmFlags},
    syscall::table::{FormattedSyscallParam, Syscall},
};

use super::sys_mlock::{check_mlock_rlimit, normalize_mlock_range};

const MLOCK_ONFAULT: usize = 0x01;

pub struct SysMlock2Handle;

impl Syscall for SysMlock2Handle {
    fn num_args(&self) -> usize {
        3
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let start = VirtAddr::new(Self::start(args));
        let len = Self::len(args);
        let flags = Self::flags(args);
        do_mlock2(start, len, flags)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("start", format!("{:#x}", Self::start(args))),
            FormattedSyscallParam::new("len", format!("{:#x}", Self::len(args))),
            FormattedSyscallParam::new("flags", format!("{:#x}", Self::flags(args))),
        ]
    }
}

impl SysMlock2Handle {
    fn start(args: &[usize]) -> usize {
        args[0]
    }

    fn len(args: &[usize]) -> usize {
        args[1]
    }

    fn flags(args: &[usize]) -> usize {
        args[2]
    }
}

fn do_mlock2(start: VirtAddr, len: usize, flags: usize) -> Result<usize, SystemError> {
    if flags & !MLOCK_ONFAULT != 0 {
        return Err(SystemError::EINVAL);
    }

    let (start, len) = normalize_mlock_range(start, len)?;
    if access_ok(start, len).is_err() {
        return Err(SystemError::EINVAL);
    }

    if !can_do_mlock() {
        return Err(SystemError::EPERM);
    }

    let vm = AddressSpace::current()?;
    let mut guard = vm.write_interruptible()?;
    let new_pages = guard.count_unlocked_pages_for_mlock(start, len)?;
    check_mlock_rlimit(guard.locked_vm, new_pages)?;

    let mut new_flags = VmFlags::VM_LOCKED;
    if flags & MLOCK_ONFAULT != 0 {
        new_flags |= VmFlags::VM_LOCKONFAULT;
    }
    guard.apply_vma_lock_flags(start, len, new_flags)?;
    Ok(0)
}

syscall_table_macros::declare_syscall!(SYS_MLOCK2, SysMlock2Handle);
