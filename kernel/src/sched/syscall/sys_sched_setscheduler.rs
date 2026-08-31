use alloc::{string::ToString, vec::Vec};

use system_error::SystemError;

use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_SCHED_SETSCHEDULER},
    process::{
        cred::{capable, CAPFlags},
        ProcessManager,
    },
    sched::SchedPolicy,
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
    fn validate_policy(policy: i32, priority: i32) -> Result<bool, SystemError> {
        let base_policy = policy & !SCHED_RESET_ON_FORK;
        match base_policy {
            SCHED_OTHER => {
                if priority != 0 {
                    return Err(SystemError::EINVAL);
                }
                Ok(true)
            }
            SCHED_FIFO | SCHED_RR => {
                if !(1..=99).contains(&priority) {
                    return Err(SystemError::EINVAL);
                }
                Ok(false)
            }
            SCHED_BATCH | SCHED_IDLE => {
                if priority != 0 {
                    return Err(SystemError::EINVAL);
                }
                Ok(false)
            }
            // The legacy ABI has no runtime/deadline/period fields, so it
            // cannot express a valid SCHED_DEADLINE request.
            SCHED_DEADLINE => Err(SystemError::EINVAL),
            _ => Err(SystemError::EINVAL),
        }
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
        let supported_fast_path = Self::validate_policy(policy, sched_param.sched_priority)?;
        if !supported_fast_path {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }

        let reset_on_fork = policy & SCHED_RESET_ON_FORK != 0;
        let current = ProcessManager::current_pcb();
        let same_owner = same_sched_owner(&current.cred(), &target.cred());
        let mut privileged = false;

        loop {
            let mut pi_guard = target.sched_info().pi_lock_irqsave();
            if target.sched_info().policy() != SchedPolicy::CFS {
                return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
            }

            let clearing_protected_flag = pi_guard.sched_reset_on_fork() && !reset_on_fork;
            if (same_owner && !clearing_protected_flag) || privileged {
                pi_guard.set_sched_reset_on_fork(reset_on_fork);
                return Ok(0);
            }

            // Capability lookup may take user-namespace locks. Never perform
            // it while holding a scheduler spin lock.
            drop(pi_guard);
            if !capable(CAPFlags::CAP_SYS_NICE) {
                return Err(SystemError::EPERM);
            }
            privileged = true;
        }
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
