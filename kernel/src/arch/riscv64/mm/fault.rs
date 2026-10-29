use crate::{
    arch::ipc::signal::Signal,
    cgroup::memory::runtime::ChargeRecoveryState,
    ipc::signal_types::{BUS_ADRERR, SEGV_ACCERR, SEGV_MAPERR},
    mm::{
        allocation::MemoryAllocationError,
        fault::{
            account_fault_result, recover_fault_charge, FaultFlags, PageFaultHandler,
            PageFaultMessage,
        },
        fault_recovery::GlobalFaultRecoveryState,
        oom::{OomContext, OomOutcome},
        ucontext::AddressSpace,
        MemoryManagementArch, VirtAddr, VirtRegion, VmFaultReason, VmFlags,
    },
    process::ProcessManager,
};
use system_error::SystemError;

use super::RiscV64MMArch;

/// Keep the fault classification sampled under the MM lock. Re-looking up the
/// VMA after dropping that lock can confuse MAPERR and ACCERR after munmap.
pub(crate) struct UserFaultSignal {
    pub signal: Signal,
    pub code: i32,
}

impl UserFaultSignal {
    fn segv(code: i32) -> Self {
        Self {
            signal: Signal::SIGSEGV,
            code,
        }
    }

    fn bus() -> Self {
        Self {
            signal: Signal::SIGBUS,
            code: BUS_ADRERR,
        }
    }
}

impl RiscV64MMArch {
    /// Resolve a user instruction, read or write fault with interrupts enabled.
    /// Ok also covers deferred recovery: return to signal delivery before retry.
    pub(crate) fn handle_user_page_fault(
        address: VirtAddr,
        cause: usize,
        ip: usize,
    ) -> Result<(), UserFaultSignal> {
        if !address.check_user() {
            return Err(UserFaultSignal::segv(SEGV_MAPERR));
        }
        let mm = AddressSpace::current().map_err(|_| UserFaultSignal::segv(SEGV_MAPERR))?;
        let page = VirtAddr::new(address.data() & !Self::PAGE_OFFSET_MASK);
        let region = VirtRegion::new(page, Self::PAGE_SIZE);
        let mut flags = FaultFlags::FAULT_FLAG_USER
            | FaultFlags::FAULT_FLAG_ALLOW_RETRY
            | FaultFlags::FAULT_FLAG_KILLABLE
            | FaultFlags::FAULT_FLAG_INTERRUPTIBLE;
        let permission = match cause {
            12 => {
                flags |= FaultFlags::FAULT_FLAG_INSTRUCTION;
                VmFlags::VM_EXEC
            }
            13 => VmFlags::VM_READ | VmFlags::VM_WRITE,
            15 => {
                flags |= FaultFlags::FAULT_FLAG_WRITE;
                VmFlags::VM_WRITE
            }
            _ => unreachable!("not a user page fault"),
        };
        let mut charge_recovery = ChargeRecoveryState::new();
        let mut global_recovery = GlobalFaultRecoveryState::new();
        loop {
            let creator = crate::cgroup::memory::object_for_task(&ProcessManager::current_pcb());
            let data_account = mm.memory_account();
            let mut space_guard = mm.write_guard_no_reservation_conflict(region);
            let vma = space_guard
                .mappings
                .contains(address)
                .ok_or_else(|| UserFaultSignal::segv(SEGV_MAPERR))?;
            if !vma.lock().vm_flags().intersects(permission) {
                return Err(UserFaultSignal::segv(SEGV_ACCERR));
            }
            let outcome = unsafe {
                PageFaultHandler::handle_mm_fault(PageFaultMessage::new(
                    vma,
                    address,
                    flags,
                    &mut space_guard.user_mapper.utable,
                    mm.clone(),
                    creator,
                    data_account.clone(),
                ))
            };
            // No MM/VMA guard may survive into waits, reclaim or OOM policy.
            drop(space_guard);
            if let Some(failure) = outcome.charge_failure {
                if recover_charge(failure, &mut charge_recovery, &data_account, flags) {
                    continue;
                }
                return Ok(());
            }
            let mut reason = outcome.reason;
            if reason.contains(VmFaultReason::VM_FAULT_COMPLETED) {
                return Ok(());
            }
            if reason.contains(VmFaultReason::VM_FAULT_RETRY) {
                flags |= FaultFlags::FAULT_FLAG_TRIED;
                if let Some(wait) = outcome.retry_wait {
                    match wait.wait() {
                        Ok(()) => continue,
                        Err(MemoryAllocationError::Charge(failure)) => {
                            if recover_charge(failure, &mut charge_recovery, &data_account, flags) {
                                continue;
                            }
                            return Ok(());
                        }
                        Err(MemoryAllocationError::System(
                            SystemError::EINTR | SystemError::ERESTARTSYS,
                        )) => return Ok(()),
                        Err(MemoryAllocationError::System(SystemError::ENOMEM)) => {
                            reason = VmFaultReason::VM_FAULT_OOM
                        }
                        Err(MemoryAllocationError::System(_)) => {
                            reason = VmFaultReason::VM_FAULT_SIGBUS
                        }
                    }
                    account_fault_result(&data_account, flags, reason);
                } else {
                    continue;
                }
            }
            if reason.contains(VmFaultReason::VM_FAULT_OOM) {
                if !reason.contains(VmFaultReason::VM_FAULT_OOM_INJECTED) {
                    match global_recovery.retry() {
                        Ok(true) => continue,
                        Err(SystemError::EINTR | SystemError::ERESTARTSYS) => return Ok(()),
                        _ => {}
                    }
                }
                let ctx = OomContext {
                    trigger_pid: ProcessManager::current_pid(),
                    trigger_tgid: ProcessManager::current_pcb().raw_tgid(),
                    fault_address: address,
                    fault_ip: ip,
                    order: 0,
                };
                match crate::mm::oom::pagefault_out_of_memory(ctx) {
                    OomOutcome::Retry => continue,
                    OomOutcome::CurrentTaskKilled => return Ok(()),
                    OomOutcome::NoVictim => {
                        panic!("fatal user page-fault OOM: addr={address:?}, pc={ip:#x}")
                    }
                }
            }
            if reason.intersects(
                VmFaultReason::VM_FAULT_SIGBUS
                    | VmFaultReason::VM_FAULT_HWPOISON
                    | VmFaultReason::VM_FAULT_HWPOISON_LARGE,
            ) {
                return Err(UserFaultSignal::bus());
            }
            if reason.contains(VmFaultReason::VM_FAULT_SIGSEGV) {
                return Err(UserFaultSignal::segv(SEGV_MAPERR));
            }
            if reason.intersects(VmFaultReason::VM_FAULT_ERROR) {
                panic!("unexpected user page-fault result: {reason:?}");
            }
            return Ok(());
        }
    }
}

/// A memcg limit must never fall through to the global OOM killer. Exhausted
/// recovery yields to pending signals and a later hardware fault, as on x86.
fn recover_charge(
    failure: crate::cgroup::memory::accounting::ChargeFailure,
    state: &mut ChargeRecoveryState,
    account: &crate::cgroup::memory::MemoryAccount,
    flags: FaultFlags,
) -> bool {
    match recover_fault_charge(failure, state, account, flags) {
        Ok(()) => true,
        Err(SystemError::EINTR | SystemError::ERESTARTSYS) => false,
        Err(_) => {
            crate::sched::sched_yield();
            false
        }
    }
}
