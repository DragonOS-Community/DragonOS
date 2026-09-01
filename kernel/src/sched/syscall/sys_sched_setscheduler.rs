use alloc::{string::ToString, vec::Vec};

use system_error::SystemError;

use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_SCHED_SETSCHEDULER},
    process::{
        cred::{capable, CAPFlags},
        resource::RLimitID,
        ProcessManager, SchedChangeRequest,
    },
    sched::{prio::PrioUtil, LinuxSchedPolicy},
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::UserBufferReader,
    },
};

use super::{
    types::{
        KernelSchedParam, SCHED_BATCH, SCHED_DEADLINE, SCHED_FIFO, SCHED_IDLE, SCHED_OTHER,
        SCHED_RESET_ON_FORK, SCHED_RR,
    },
    util::{find_sched_target, same_sched_owner},
};

struct SysSchedSetscheduler;

impl SysSchedSetscheduler {
    fn build_request(policy: i32, priority: i32) -> Result<SchedChangeRequest, SystemError> {
        let base_policy = policy & !SCHED_RESET_ON_FORK;
        let reset_on_fork = policy & SCHED_RESET_ON_FORK != 0;
        match base_policy {
            SCHED_OTHER => {
                if priority != 0 {
                    return Err(SystemError::EINVAL);
                }
                Ok(SchedChangeRequest::Normal { reset_on_fork })
            }
            SCHED_FIFO => {
                let priority =
                    PrioUtil::user_rt_prio_to_internal(priority).ok_or(SystemError::EINVAL)?;
                Ok(SchedChangeRequest::Fifo {
                    priority,
                    reset_on_fork,
                })
            }
            SCHED_RR => {
                let priority =
                    PrioUtil::user_rt_prio_to_internal(priority).ok_or(SystemError::EINVAL)?;
                Ok(SchedChangeRequest::Rr {
                    priority,
                    reset_on_fork,
                })
            }
            SCHED_BATCH | SCHED_IDLE => {
                if priority != 0 {
                    return Err(SystemError::EINVAL);
                }
                Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
            }
            // The legacy ABI has no runtime/deadline/period fields, so it
            // cannot express a valid SCHED_DEADLINE request.
            SCHED_DEADLINE => Err(SystemError::EINVAL),
            _ => Err(SystemError::EINVAL),
        }
    }

    /// Check the Linux unprivileged sched_setscheduler rules without invoking
    /// capability lookup while a scheduler spin lock is held.
    fn unprivileged_allowed(
        target: &alloc::sync::Arc<crate::process::ProcessControlBlock>,
        request: SchedChangeRequest,
    ) -> Result<bool, SystemError> {
        let current = ProcessManager::current_pcb();
        if !same_sched_owner(&current.cred(), &target.cred()) {
            return Ok(false);
        }

        let rtprio_limit = match request {
            SchedChangeRequest::Fifo { .. } | SchedChangeRequest::Rr { .. } => {
                Some(target.get_rlimit(RLimitID::Rtprio).rlim_cur)
            }
            SchedChangeRequest::Normal { .. } => None,
        };
        let pi_guard = target.sched_info().pi_lock_irqsave();
        let reset_on_fork = match request {
            SchedChangeRequest::Normal { reset_on_fork }
            | SchedChangeRequest::Fifo { reset_on_fork, .. }
            | SchedChangeRequest::Rr { reset_on_fork, .. } => reset_on_fork,
        };
        if pi_guard.sched_reset_on_fork() && !reset_on_fork {
            return Ok(false);
        }

        let (new_policy, priority) = match request {
            SchedChangeRequest::Normal { .. } => return Ok(true),
            SchedChangeRequest::Fifo { priority, .. } => (LinuxSchedPolicy::Fifo, priority),
            SchedChangeRequest::Rr { priority, .. } => (LinuxSchedPolicy::Rr, priority),
        };
        let new_user_priority =
            PrioUtil::internal_rt_prio_to_user(priority).ok_or(SystemError::EINVAL)?;
        let old_policy = target.sched_info().policy();
        let old_user_priority = match old_policy {
            LinuxSchedPolicy::Normal => 0,
            LinuxSchedPolicy::Fifo | LinuxSchedPolicy::Rr => {
                PrioUtil::internal_rt_prio_to_user(target.sched_info().normal_prio())
                    .ok_or(SystemError::EIO)?
            }
        };
        let rtprio_limit = rtprio_limit.expect("RT authorization must read RLIMIT_RTPRIO");

        if old_policy != new_policy && rtprio_limit == 0 {
            return Ok(false);
        }
        if new_user_priority > old_user_priority && new_user_priority as u64 > rtprio_limit {
            return Ok(false);
        }
        Ok(true)
    }
}

impl Syscall for SysSchedSetscheduler {
    fn num_args(&self) -> usize {
        3
    }

    fn handle(&self, args: &[usize], frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let pid = args[0] as i32;
        let policy = args[1] as i32;
        let param = args[2] as *const KernelSchedParam;

        // Linux rejects a negative policy before inspecting pid or userspace.
        if policy < 0 {
            return Err(SystemError::EINVAL);
        }
        if pid < 0 || param.is_null() {
            return Err(SystemError::EINVAL);
        }

        // The legacy kernel ABI copies exactly one i32, before target lookup
        // and policy/priority validation.
        let reader = UserBufferReader::new(
            param,
            core::mem::size_of::<KernelSchedParam>(),
            frame.is_from_user(),
        )?;
        let sched_param: KernelSchedParam = reader.buffer_protected(0)?.read_one(0)?;

        let target = find_sched_target(pid)?;
        let request = Self::build_request(policy, sched_param.sched_priority)?;
        if !Self::unprivileged_allowed(&target, request)? && !capable(CAPFlags::CAP_SYS_NICE) {
            return Err(SystemError::EPERM);
        }

        ProcessManager::set_scheduler(&target, request)?;
        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("pid", (args[0] as i32).to_string()),
            FormattedSyscallParam::new("policy", format!("{:#x}", args[1] as i32)),
            FormattedSyscallParam::new("param", format!("{:#x}", args[2])),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_SCHED_SETSCHEDULER, SysSchedSetscheduler);
