//! System call handler for mlock.

use alloc::vec::Vec;
use system_error::SystemError;

use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_MLOCK, MMArch},
    libs::align::page_align_down,
    mm::{
        access_ok, can_do_mlock, ucontext::AddressSpace, MemoryManagementArch, VirtAddr,
        VirtRegion, VmFlags,
    },
    process::{cred::CAPFlags, resource::RLimitID, ProcessManager},
    syscall::table::{FormattedSyscallParam, Syscall},
};

pub struct SysMlockHandle;

impl Syscall for SysMlockHandle {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let start = VirtAddr::new(Self::start(args));
        let len = Self::len(args);
        do_mlock(start, len, VmFlags::VM_LOCKED)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("start", format!("{:#x}", Self::start(args))),
            FormattedSyscallParam::new("len", format!("{:#x}", Self::len(args))),
        ]
    }
}

impl SysMlockHandle {
    fn start(args: &[usize]) -> usize {
        args[0]
    }

    fn len(args: &[usize]) -> usize {
        args[1]
    }
}

pub(super) fn do_mlock(
    start: VirtAddr,
    len: usize,
    new_flags: VmFlags,
) -> Result<usize, SystemError> {
    if !can_do_mlock() {
        return Err(SystemError::EPERM);
    }

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

        let new_pages = guard.count_unlocked_pages_for_mlock(start, len)?;
        check_mlock_rlimit(guard.locked_vm, new_pages)?;

        match guard.apply_vma_lock_flags_collect(start, len, new_flags) {
            Ok(()) => {
                drop(guard);
                vm.populate_mlock_range_post_commit(start, len)?;
                return Ok(0);
            }
            Err(failure) => {
                drop(guard);
                crate::mm::ucontext::InnerAddressSpace::notify_close_notifications(
                    failure.notifications,
                );
                return Err(failure.err);
            }
        }
    }
}

pub(super) fn check_mlock_rlimit(locked_vm: usize, new_pages: usize) -> Result<(), SystemError> {
    let pcb = ProcessManager::current_pcb();
    if pcb.cred().has_capability(CAPFlags::CAP_IPC_LOCK) {
        return Ok(());
    }

    let rlimit = pcb.get_rlimit(RLimitID::Memlock).rlim_cur;
    let total_pages = locked_vm
        .checked_add(new_pages)
        .ok_or(SystemError::ENOMEM)?;
    let total_bytes = (total_pages as u128) * (MMArch::PAGE_SIZE as u128);
    if total_bytes > rlimit as u128 {
        return Err(SystemError::ENOMEM);
    }

    Ok(())
}

pub(super) fn normalize_mlock_range(
    start: VirtAddr,
    len: usize,
) -> Result<(VirtAddr, usize), SystemError> {
    let offset = start.data() & MMArch::PAGE_OFFSET_MASK;
    let aligned_start = VirtAddr::new(page_align_down(start.data()));
    if len == 0 {
        return Ok((aligned_start, 0));
    }

    let len = len.checked_add(offset).ok_or(SystemError::EINVAL)?;
    let len = len
        .checked_add(MMArch::PAGE_SIZE - 1)
        .ok_or(SystemError::EINVAL)?
        & MMArch::PAGE_MASK;

    aligned_start
        .data()
        .checked_add(len)
        .ok_or(SystemError::EINVAL)?;
    Ok((aligned_start, len))
}

syscall_table_macros::declare_syscall!(SYS_MLOCK, SysMlockHandle);
