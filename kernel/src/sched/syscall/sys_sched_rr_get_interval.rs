use alloc::{string::ToString, vec::Vec};

use system_error::SystemError;

use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_SCHED_RR_GET_INTERVAL},
    sched::{realtime::RR_TIMESLICE_TICKS, LinuxSchedPolicy},
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::UserBufferWriter,
    },
    time::{jiffies::TICK_NESC, PosixTimeSpec},
};

use super::util::find_sched_target;

struct SysSchedRrGetInterval;

impl Syscall for SysSchedRrGetInterval {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let pid = args[0] as i32;
        let interval = args[1] as *mut PosixTimeSpec;

        // Linux resolves the target before touching the output buffer, so an
        // absent task takes precedence over a bad userspace pointer.
        let target = find_sched_target(pid)?;
        let policy = {
            let _pi_guard = target.sched_info().pi_lock_irqsave();
            target.sched_info().policy()
        };

        // FIFO has no quantum. DragonOS Fair entities currently use a slice
        // shorter than one scheduler tick, which Linux's jiffy ABI quantizes
        // to zero as well. RR is the only supported policy with a nonzero
        // interval in this ABI.
        let interval_ticks = match policy {
            LinuxSchedPolicy::Rr => RR_TIMESLICE_TICKS,
            LinuxSchedPolicy::Normal | LinuxSchedPolicy::Fifo => 0,
        };
        let value =
            PosixTimeSpec::from_ns(u64::from(interval_ticks).saturating_mul(u64::from(TICK_NESC)));

        let mut writer = UserBufferWriter::new(
            interval,
            core::mem::size_of::<PosixTimeSpec>(),
            frame.is_from_user(),
        )?;
        writer.buffer_protected(0)?.write_one(0, &value)?;

        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("pid", (args[0] as i32).to_string()),
            FormattedSyscallParam::new("interval", format!("{:#x}", args[1])),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_SCHED_RR_GET_INTERVAL, SysSchedRrGetInterval);
