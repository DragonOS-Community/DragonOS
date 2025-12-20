use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_CLOCK_GETTIME;
use crate::process::ProcessManager;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::UserBufferWriter;
use crate::time::timekeeping::getnstimeofday;
use crate::time::{syscall::PosixClockID, PosixTimeSpec};
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysClockGettime;

impl SysClockGettime {
    fn clock_id(args: &[usize]) -> i32 {
        args[0] as i32
    }

    fn timespec_ptr(args: &[usize]) -> *mut PosixTimeSpec {
        args[1] as *mut PosixTimeSpec
    }
}

impl Syscall for SysClockGettime {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let clock_id = PosixClockID::try_from(Self::clock_id(args))?;

        let tp = Self::timespec_ptr(args);
        if tp.is_null() {
            return Err(SystemError::EFAULT);
        }

        let mut tp_buf = UserBufferWriter::new::<PosixTimeSpec>(
            tp,
            core::mem::size_of::<PosixTimeSpec>(),
            true,
        )?;

        let timespec = match clock_id {
            PosixClockID::Realtime => getnstimeofday(),
            // 单调/boottime 等目前仍复用 realtime（后续可补齐真正语义）。
            PosixClockID::Monotonic
            | PosixClockID::Boottime
            | PosixClockID::MonotonicRaw
            | PosixClockID::RealtimeCoarse
            | PosixClockID::MonotonicCoarse
            | PosixClockID::RealtimeAlarm
            | PosixClockID::BoottimeAlarm => getnstimeofday(),

            PosixClockID::ProcessCPUTimeID => {
                let pcb = ProcessManager::current_pcb();
                PosixTimeSpec::from_ns(pcb.process_cputime_ns())
            }
            PosixClockID::ThreadCPUTimeID => {
                let pcb = ProcessManager::current_pcb();
                PosixTimeSpec::from_ns(pcb.thread_cputime_ns())
            }
        };

        tp_buf.copy_one_to_user(&timespec, 0)?;

        return Ok(0);
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("clock_id", format!("{}", Self::clock_id(args))),
            FormattedSyscallParam::new(
                "timespec",
                format!("{:#x}", Self::timespec_ptr(args) as usize),
            ),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_CLOCK_GETTIME, SysClockGettime);
