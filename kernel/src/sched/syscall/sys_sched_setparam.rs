use alloc::{string::ToString, sync::Arc, vec::Vec};

use system_error::SystemError;

use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_SCHED_SETPARAM},
    process::{
        cred::{capable, CAPFlags},
        resource::RLimitID,
        ProcessControlBlock, ProcessManager,
    },
    sched::{prio::PrioUtil, LinuxSchedPolicy},
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::UserBufferReader,
    },
};

use super::{
    types::KernelSchedParam,
    util::{find_sched_target, same_sched_owner},
};

struct PreparedUpdate {
    policy: LinuxSchedPolicy,
    expected_rt_priority: Option<i32>,
    rt_priority: Option<i32>,
    unprivileged_allowed: bool,
}

struct SysSchedSetparam;

impl SysSchedSetparam {
    /// Validate and authorize a priority update against one scheduler snapshot.
    /// The manager checks that the authorization-dependent state is still
    /// current at commit.
    fn prepare_update(
        target: &Arc<ProcessControlBlock>,
        user_priority: i32,
    ) -> Result<PreparedUpdate, SystemError> {
        let current = ProcessManager::current_pcb();
        let same_owner = same_sched_owner(&current.cred(), &target.cred());
        let rtprio_limit = target.get_rlimit(RLimitID::Rtprio).rlim_cur;

        let pi_guard = target.sched_info().pi_lock_irqsave();
        let policy = target.sched_info().policy();
        let (expected_rt_priority, rt_priority, within_limit) = match policy {
            LinuxSchedPolicy::Normal => {
                if user_priority != 0 {
                    return Err(SystemError::EINVAL);
                }
                (None, None, true)
            }
            LinuxSchedPolicy::Fifo | LinuxSchedPolicy::Rr => {
                let priority =
                    PrioUtil::user_rt_prio_to_internal(user_priority).ok_or(SystemError::EINVAL)?;
                let old_rt_priority = target.sched_info().normal_prio();
                let old_user_priority =
                    PrioUtil::internal_rt_prio_to_user(old_rt_priority).ok_or(SystemError::EIO)?;
                let within_limit =
                    user_priority <= old_user_priority || user_priority as u64 <= rtprio_limit;
                (Some(old_rt_priority), Some(priority), within_limit)
            }
        };
        drop(pi_guard);

        Ok(PreparedUpdate {
            policy,
            expected_rt_priority,
            rt_priority,
            unprivileged_allowed: same_owner && within_limit,
        })
    }
}

impl Syscall for SysSchedSetparam {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let pid = args[0] as i32;
        let param = args[1] as *const KernelSchedParam;

        if pid < 0 || param.is_null() {
            return Err(SystemError::EINVAL);
        }

        // Linux copies exactly one legacy sched_param before target lookup or
        // priority validation, which determines EFAULT/ESRCH/EINVAL ordering.
        let reader = UserBufferReader::new(
            param,
            core::mem::size_of::<KernelSchedParam>(),
            frame.is_from_user(),
        )?;
        let sched_param: KernelSchedParam = reader.buffer_protected(0)?.read_one(0)?;
        let target = find_sched_target(pid)?;

        loop {
            let prepared = Self::prepare_update(&target, sched_param.sched_priority)?;
            if !prepared.unprivileged_allowed && !capable(CAPFlags::CAP_SYS_NICE) {
                return Err(SystemError::EPERM);
            }
            if ProcessManager::set_scheduler_param(
                &target,
                prepared.policy,
                prepared.expected_rt_priority,
                prepared.rt_priority,
            )? {
                return Ok(0);
            }
        }
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("pid", (args[0] as i32).to_string()),
            FormattedSyscallParam::new("param", format!("{:#x}", args[1])),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_SCHED_SETPARAM, SysSchedSetparam);
