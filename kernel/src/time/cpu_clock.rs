//! Linux CPU clocks: namespace-relative lookup followed by stable PID identity.

use alloc::sync::Arc;
use system_error::SystemError;

use crate::process::{
    pid::{Pid, PidType},
    ProcessControlBlock, ProcessManager, RawPid,
};

use super::{
    cpu_events::CpuEventDomain,
    jiffies::NSEC_PER_JIFFY,
    syscall::{PosixClockID, CPUCLOCK_CLOCK_MASK, CPUCLOCK_PERTHREAD_MASK},
    PosixTimeSpec,
};

/// A single CPU sleep pins its clock identity, not the task. A restarted
/// syscall resolves the raw clock ID again, matching Linux's restart block.
pub(crate) struct CpuClockSleep {
    target: CpuClockTarget,
    domain: Arc<CpuEventDomain>,
}

impl CpuClockSleep {
    pub(crate) fn resolve(clock: PosixClockID) -> Result<Option<Self>, SystemError> {
        if clock == PosixClockID::ThreadCPUTimeID {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        if !matches!(
            clock,
            PosixClockID::ProcessCPUTimeID | PosixClockID::DynamicCpuClock(_)
        ) {
            return Ok(None);
        }
        let target = CpuClockTarget::resolve(clock, CpuClockOperation::Validate)?;
        let task = target.task().ok_or(SystemError::EINVAL)?;
        if target.per_thread && Arc::ptr_eq(&task, &ProcessManager::current_pcb()) {
            return Err(SystemError::EINVAL);
        }
        let domain = if target.per_thread {
            task.cpu_event_domain().clone()
        } else {
            task.process_signal().cpu_event_domain().clone()
        };
        Ok(Some(Self { target, domain }))
    }

    pub(crate) fn now(&self) -> PosixTimeSpec {
        PosixTimeSpec::from_ns(
            self.target
                .sample_ns()
                .unwrap_or_else(|_| self.domain.sample(self.target.kind)),
        )
    }

    pub(crate) fn wait(&self, deadline: u64) -> Result<(), SystemError> {
        self.domain.wait(self.target.kind, deadline, || {
            self.target.sample_ns().is_ok_and(|now| now >= deadline)
        })?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CpuClockKind {
    Prof,
    Virt,
    Sched,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CpuClockOperation {
    Gettime,
    Validate,
}

/// Does not pin the task or retarget a reused numeric PID.
#[derive(Clone, Debug)]
pub(crate) struct CpuClockTarget {
    pub(crate) kind: CpuClockKind,
    pub(crate) per_thread: bool,
    pub(crate) pid: Arc<Pid>,
}

impl CpuClockTarget {
    pub(crate) fn resolve(
        clock: PosixClockID,
        operation: CpuClockOperation,
    ) -> Result<Self, SystemError> {
        let (kind, per_thread, number) = match clock {
            PosixClockID::ProcessCPUTimeID => (CpuClockKind::Sched, false, 0),
            PosixClockID::ThreadCPUTimeID => (CpuClockKind::Sched, true, 0),
            PosixClockID::DynamicCpuClock(raw) => {
                let kind = match raw & CPUCLOCK_CLOCK_MASK {
                    0 => CpuClockKind::Prof,
                    1 => CpuClockKind::Virt,
                    2 => CpuClockKind::Sched,
                    _ => return Err(SystemError::EINVAL),
                };
                (
                    kind,
                    raw & CPUCLOCK_PERTHREAD_MASK != 0,
                    (!(raw as i32 >> 3)) as usize,
                )
            }
            _ => return Err(SystemError::EINVAL),
        };
        let current = ProcessManager::current_pcb();
        let mut pid = if number == 0 {
            current.task_pid_ptr(if per_thread {
                PidType::PID
            } else {
                PidType::TGID
            })
        } else {
            ProcessManager::find_vpid(RawPid::new(number))
        }
        .ok_or(SystemError::EINVAL)?;

        if per_thread {
            let task = pid.pid_task(PidType::PID).ok_or(SystemError::EINVAL)?;
            if !Arc::ptr_eq(&task.process_signal(), &current.process_signal()) {
                return Err(SystemError::EINVAL);
            }
        } else {
            // Only gettime accepts the caller's non-leader TID as its group.
            if operation == CpuClockOperation::Gettime
                && current
                    .task_pid_ptr(PidType::PID)
                    .is_some_and(|own| Arc::ptr_eq(&own, &pid))
            {
                pid = current
                    .task_pid_ptr(PidType::TGID)
                    .ok_or(SystemError::EINVAL)?;
            }
            if !pid.has_task(PidType::TGID) {
                return Err(SystemError::EINVAL);
            }
        }
        Ok(Self {
            kind,
            per_thread,
            pid,
        })
    }

    pub(crate) fn task(&self) -> Option<Arc<ProcessControlBlock>> {
        self.pid.pid_task(if self.per_thread {
            PidType::PID
        } else {
            PidType::TGID
        })
    }

    pub(crate) fn sample_ns(&self) -> Result<u64, SystemError> {
        let task = self.task().ok_or(SystemError::EINVAL)?;
        if self.kind == CpuClockKind::Sched {
            return Ok(if self.per_thread {
                task.thread_cputime_ns()
            } else {
                task.process_cputime_ns()
            });
        }
        let (user, system) = if self.per_thread {
            task.thread_tick_cputime()
        } else {
            task.process_tick_cputime()
        };
        Ok(match self.kind {
            CpuClockKind::Prof => user.saturating_add(system),
            CpuClockKind::Virt => user,
            CpuClockKind::Sched => unreachable!(),
        })
    }

    pub(crate) fn resolution(&self) -> PosixTimeSpec {
        PosixTimeSpec::new(
            0,
            if self.kind == CpuClockKind::Sched {
                1
            } else {
                NSEC_PER_JIFFY as i64
            },
        )
    }
}
