//! Admission errors stay typed until the MM's resource-lock-free outer path.

use system_error::SystemError;

use crate::cgroup::memory::{
    accounting::{ChargeFailure, ChargeFailureReason},
    runtime::{recover_charge, ChargeRecoveryState, MemoryAllocPolicy, RecoveryDecision},
};
use crate::mm::oom_reaper::JobBirthError;

#[derive(Debug)]
pub(crate) enum MemoryAllocationError {
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
            JobBirthError::Physical(_) => Self::System(SystemError::ENOMEM),
        }
    }
}

impl MemoryAllocationError {
    /// Call only after releasing resource guards and failed MM owners.
    /// Ok means retry the complete unpublished operation with a fresh creator.
    pub(crate) fn recover(self, state: &mut ChargeRecoveryState) -> Result<(), SystemError> {
        match self {
            Self::System(error) => Err(error),
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
                match recover_charge(&failure, policy, 0, state)? {
                    RecoveryDecision::RetryOperation => Ok(()),
                    RecoveryDecision::Exhausted => Err(SystemError::ENOMEM),
                }
            }
        }
    }
}
