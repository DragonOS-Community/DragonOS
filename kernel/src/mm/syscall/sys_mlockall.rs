//! System call handler for mlockall.

use alloc::vec::Vec;
use system_error::SystemError;

use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_MLOCKALL},
    mm::{can_do_mlock, ucontext::AddressSpace, VmFlags},
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

    let mut lock_flags = VmFlags::VM_LOCKED;
    if flags & MCL_ONFAULT != 0 {
        lock_flags |= VmFlags::VM_LOCKONFAULT;
    }

    let vm = AddressSpace::current()?;
    loop {
        let mut guard = vm.write_interruptible()?;
        if guard.mappings.first_reservation_region().is_some() {
            drop(guard);
            vm.wait_for_no_reservations_interruptible()?;
            continue;
        }

        if flags & MCL_CURRENT != 0 {
            let new_pages = guard.count_unlocked_pages_for_mlockall()?;
            check_mlock_rlimit(guard.locked_vm, new_pages)?;
        }
        guard.set_mlock_future(VmFlags::VM_NONE);

        let notifications = if flags & MCL_CURRENT != 0 {
            Some(guard.apply_mlockall_current_collect(lock_flags))
        } else {
            None
        };

        if flags & MCL_FUTURE != 0 {
            guard.set_mlock_future(lock_flags);
        }

        drop(guard);
        if let Some(notifications) = notifications {
            crate::mm::ucontext::InnerAddressSpace::notify_close_notifications(notifications);
        }
        if flags & MCL_CURRENT != 0 {
            vm.populate_mlockall_post_commit();
        }
        return Ok(0);
    }
}

syscall_table_macros::declare_syscall!(SYS_MLOCKALL, SysMlockallHandle);
