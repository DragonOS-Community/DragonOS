//! CPU measurement identity and preallocated event registration.
use crate::{
    cgroup::memory::object_for_task,
    process::ProcessControlBlock,
    time::{
        cpu_clock::{CpuClockOperation, CpuClockTarget},
        cpu_events::{CpuEventDomain, CpuEventRegistration},
        syscall::PosixClockID,
    },
};
use alloc::sync::Arc;
use system_error::SystemError;
pub(super) struct CpuBackend {
    pub(super) target: CpuClockTarget,
    pub(super) domain: Arc<CpuEventDomain>,
    registration: Arc<CpuEventRegistration>,
}
impl CpuBackend {
    pub(super) fn create(
        pcb: &Arc<ProcessControlBlock>,
        clock: PosixClockID,
        callback: Arc<super::Callback>,
    ) -> Result<Self, SystemError> {
        let target = CpuClockTarget::resolve(clock, CpuClockOperation::Validate)?;
        let task = target.task().ok_or(SystemError::EINVAL)?;
        let domain = if target.per_thread {
            task.cpu_event_domain()
        } else {
            task.process_signal().cpu_event_domain()
        };
        let mut recovery = crate::cgroup::memory::runtime::ChargeRecoveryState::default();
        let registration = loop {
            match CpuEventDomain::prepare_registration(&object_for_task(pcb), callback.clone()) {
                Ok(registration) => break registration,
                Err(error) => error.recover(&mut recovery)?,
            }
        };
        Ok(Self {
            target,
            domain,
            registration,
        })
    }
    pub(super) fn sample(&self) -> Option<u64> {
        self.target.sample_ns().ok()
    }
    pub(super) fn validate_set(&self) -> Result<(), SystemError> {
        let task = self.target.task().ok_or(SystemError::ESRCH)?;
        if task.process_signal().group_exiting()
            || (self.target.per_thread && (task.is_exited() || task.is_zombie() || task.is_dead()))
        {
            return Err(SystemError::ESRCH);
        }
        Ok(())
    }
    pub(super) fn arm(&self, deadline: u64, generation: u64) -> Result<(), SystemError> {
        self.domain
            .arm(&self.registration, self.target.kind, deadline, generation)
    }
    pub(super) fn cancel(&self) {
        self.domain.cancel(&self.registration);
    }
}
