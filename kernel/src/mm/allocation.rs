//! Admission errors stay typed until the MM's resource-lock-free outer path.

use system_error::SystemError;

use crate::cgroup::memory::{
    accounting::{ChargeFailure, ChargeFailureReason},
    runtime::{recover_charge, ChargeRecoveryState, MemoryAllocPolicy, RecoveryDecision},
};
use crate::mm::oom_reaper::JobBirthError;
use crate::{
    arch::CurrentIrqArch,
    exception::{in_interrupt, InterruptArch},
    process::ProcessManager,
};

/// Common eligibility for sleepable allocation work, independent of the VA backend.
pub(crate) fn task_context() -> bool {
    CurrentIrqArch::is_irq_enabled()
        && !in_interrupt()
        && ProcessManager::initialized()
        && ProcessManager::current_pcb().preempt_count() == 0
}

#[derive(Debug)]
pub(crate) enum MemoryAllocationError {
    /// A real frame or fixed-layout heap allocation failed. Never infer this from errno.
    Physical,
    Charge(ChargeFailure),
    System(SystemError),
}

impl From<SystemError> for MemoryAllocationError {
    fn from(error: SystemError) -> Self {
        Self::System(error)
    }
}

impl From<ChargeFailure> for MemoryAllocationError {
    fn from(error: ChargeFailure) -> Self {
        Self::Charge(error)
    }
}

impl From<JobBirthError> for MemoryAllocationError {
    fn from(error: JobBirthError) -> Self {
        match error {
            JobBirthError::Charge(failure) => Self::Charge(failure),
            JobBirthError::Physical(_) => Self::Physical,
        }
    }
}

/// One resource-lock-free outer transaction. Quota and global pressure have
/// distinct accounting and budgets; unrelated syscalls never share this state.
pub(crate) struct AllocationRecoveryState {
    charge: ChargeRecoveryState,
    global: super::fault_recovery::GlobalFaultRecoveryState,
    physical_deadline: Option<u64>,
}

impl AllocationRecoveryState {
    pub(crate) const fn new() -> Self {
        Self {
            charge: ChargeRecoveryState::new(),
            global: super::fault_recovery::GlobalFaultRecoveryState::new(),
            physical_deadline: None,
        }
    }

    fn recover_physical(&mut self) -> Result<(), SystemError> {
        // Check task/IRQ/preemption before reading any task-local NOIO state.
        if !task_context() || crate::cgroup::memory::io::noio_active() {
            return Err(SystemError::ENOMEM);
        }
        let deadline = *self
            .physical_deadline
            .get_or_insert_with(|| crate::time::timer::next_n_us_timer_jiffies(10_000_000));
        if crate::time::timer::clock() >= deadline {
            return Err(SystemError::ENOMEM);
        }
        // Reuse fault reclaim's bounded scan and killable writeback waits, but
        // keep a fixed transaction deadline even when unrelated pages release.
        let retry = self.global.retry()?;
        if retry && crate::time::timer::clock() < deadline {
            Ok(())
        } else {
            Err(SystemError::ENOMEM)
        }
    }
}

impl Default for AllocationRecoveryState {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryAllocationError {
    pub(crate) fn is_recoverable(&self) -> bool {
        matches!(self, Self::Charge(_) | Self::Physical)
    }

    /// Terminal/legacy boundary; an errno never grants recovery authority.
    pub(crate) fn into_errno(self) -> SystemError {
        match self {
            Self::System(error) => error,
            Self::Physical | Self::Charge(_) => SystemError::ENOMEM,
        }
    }

    /// Call only after releasing resource guards and failed MM owners.
    /// Ok means retry the complete unpublished operation with a fresh creator.
    pub(crate) fn recover(self, state: &mut AllocationRecoveryState) -> Result<(), SystemError> {
        match self {
            Self::System(error) => Err(error),
            Self::Physical => state.recover_physical(),
            // The hard counter cap is never reclaimable, including during
            // bootstrap before a current task exists. Do not query NOIO first.
            Self::Charge(failure) if failure.reason == ChargeFailureReason::CounterOverflow => {
                Err(SystemError::ENOMEM)
            }
            Self::Charge(failure) => {
                let policy = if crate::cgroup::memory::io::noio_active() {
                    MemoryAllocPolicy::ReclaimNoIo
                } else {
                    MemoryAllocPolicy::ReclaimThenOom
                };
                match recover_charge(&failure, policy, 0, &mut state.charge)? {
                    RecoveryDecision::RetryOperation => Ok(()),
                    RecoveryDecision::Exhausted => Err(SystemError::ENOMEM),
                }
            }
        }
    }
}

#[cfg(feature = "mm_selftest")]
pub(crate) fn recovery_selftest() -> bool {
    if !task_context() {
        return false;
    }
    let mut state = AllocationRecoveryState::new();
    // An errno alone must never enter reclaim or start its budget.
    for error in [SystemError::ENOMEM, SystemError::EIO, SystemError::EFAULT] {
        if MemoryAllocationError::System(error.clone()).is_recoverable()
            || MemoryAllocationError::System(error.clone()).recover(&mut state) != Err(error)
            || state.physical_deadline.is_some()
        {
            return false;
        }
    }
    {
        let _noio = crate::cgroup::memory::io::MemoryIoScope::enter_noio();
        if MemoryAllocationError::Physical.recover(&mut state) != Err(SystemError::ENOMEM)
            || state.physical_deadline.is_some()
        {
            return false;
        }
    }
    state.physical_deadline = Some(crate::time::timer::clock());
    MemoryAllocationError::Physical.recover(&mut state) == Err(SystemError::ENOMEM)
        && MemoryAllocationError::Physical.is_recoverable()
        && MemoryAllocationError::Physical.into_errno() == SystemError::ENOMEM
        && crate::driver::block::loop_device::LoopDevice::physical_boundary_selftest()
}
