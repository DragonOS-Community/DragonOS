use alloc::{string::ToString, vec::Vec};

use system_error::SystemError;

use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_SCHED_GETPARAM},
    sched::{prio::PrioUtil, LinuxSchedPolicy},
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::UserBufferWriter,
    },
};

use super::{types::KernelSchedParam, util::find_sched_target};

struct SysSchedGetparam;

impl Syscall for SysSchedGetparam {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let pid = args[0] as i32;
        let param = args[1] as *mut KernelSchedParam;

        if param.is_null() || pid < 0 {
            return Err(SystemError::EINVAL);
        }

        let target = find_sched_target(pid)?;
        let (policy, normal_prio) = {
            let _pi_guard = target.sched_info().pi_lock_irqsave();
            (
                target.sched_info().policy(),
                target.sched_info().normal_prio(),
            )
        };

        let sched_priority = match policy {
            LinuxSchedPolicy::Normal => 0,
            LinuxSchedPolicy::Fifo | LinuxSchedPolicy::Rr => {
                PrioUtil::internal_rt_prio_to_user(normal_prio).ok_or_else(|| {
                    log::error!(
                        "task {} has invalid internal RT priority {}",
                        target.raw_pid().data(),
                        normal_prio
                    );
                    debug_assert!(
                        false,
                        "invalid internal RT priority {normal_prio} for task {}",
                        target.raw_pid().data()
                    );
                    SystemError::EIO
                })?
            }
        };

        let sched_param = KernelSchedParam { sched_priority };
        let mut writer = UserBufferWriter::new(
            param,
            core::mem::size_of::<KernelSchedParam>(),
            frame.is_from_user(),
        )?;
        writer.buffer_protected(0)?.write_one(0, &sched_param)?;

        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("pid", (args[0] as i32).to_string()),
            FormattedSyscallParam::new("param", format!("{:#x}", args[1])),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_SCHED_GETPARAM, SysSchedGetparam);
