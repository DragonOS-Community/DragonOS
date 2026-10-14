//! Thread-group adapters for the common CPU event domain.
use crate::time::{cpu_clock::CpuClockKind, cpu_events::CpuEventDomain};
use alloc::sync::Arc;

impl super::ProcessSignalState {
    pub(crate) fn cpu_event_domain(&self) -> Arc<CpuEventDomain> {
        self.body.cpu_events.clone()
    }
    pub(crate) fn account_cpu_runtime(&self, delta: u64) {
        self.body.cpu_events.account(CpuClockKind::Sched, delta);
    }
    pub(crate) fn account_cpu_user(&self, delta: u64) {
        self.body.cpu_events.account(CpuClockKind::Virt, delta);
        self.body.cpu_events.account(CpuClockKind::Prof, delta);
    }
    pub(crate) fn account_cpu_system(&self, delta: u64) {
        self.body.cpu_events.account(CpuClockKind::Prof, delta);
    }
    pub(crate) fn cpu_runtime(&self) -> u64 {
        self.body.cpu_events.sample(CpuClockKind::Sched)
    }
}
