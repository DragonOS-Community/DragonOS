use system_error::SystemError;

use crate::mm::{
    fault::{FaultFlags, PageFaultHandler, PageFaultMessage},
    ucontext::AddressSpace,
    MemoryManagementArch, VirtAddr, VirtRegion, VmFaultReason, VmFlags,
};

use super::RiscV64MMArch;

impl RiscV64MMArch {
    /// Resolve a userspace write fault.
    /// The caller must enable interrupts before entering this path.
    pub(crate) fn handle_user_store_page_fault(address: VirtAddr) -> Result<(), SystemError> {
        if !address.check_user() {
            return Err(SystemError::EFAULT);
        }

        let mm = AddressSpace::current()?;
        let page = VirtAddr::new(address.data() & !Self::PAGE_OFFSET_MASK);
        let region = VirtRegion::new(page, Self::PAGE_SIZE);
        let mut flags = FaultFlags::FAULT_FLAG_USER
            | FaultFlags::FAULT_FLAG_WRITE
            | FaultFlags::FAULT_FLAG_ALLOW_RETRY
            | FaultFlags::FAULT_FLAG_KILLABLE
            | FaultFlags::FAULT_FLAG_INTERRUPTIBLE;

        loop {
            let mut space_guard = mm.write_guard_no_reservation_conflict(region);
            let vma = space_guard
                .mappings
                .contains(address)
                .ok_or(SystemError::EFAULT)?;

            if !vma.lock().vm_flags().contains(VmFlags::VM_WRITE) {
                return Err(SystemError::EFAULT);
            }

            let outcome = unsafe {
                PageFaultHandler::handle_mm_fault(PageFaultMessage::new(
                    vma,
                    address,
                    flags,
                    &mut space_guard.user_mapper.utable,
                    mm.clone(),
                ))
            };
            if outcome.reason.contains(VmFaultReason::VM_FAULT_OOM) {
                return Err(SystemError::ENOMEM);
            }
            if outcome.reason.intersects(VmFaultReason::VM_FAULT_ERROR) {
                return Err(SystemError::EFAULT);
            }
            if outcome.reason.contains(VmFaultReason::VM_FAULT_RETRY) {
                flags |= FaultFlags::FAULT_FLAG_TRIED;
                drop(space_guard);
                if let Some(wait) = outcome.retry_wait {
                    wait.wait()?;
                }
                continue;
            }
            if outcome.reason.contains(VmFaultReason::VM_FAULT_COMPLETED) {
                return Ok(());
            }
        }
    }
}
