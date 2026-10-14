use alloc::vec::Vec;
use system_error::SystemError;

use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_CLOCK_SETTIME},
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::UserBufferReader,
    },
    time::{
        cpu_clock::{CpuClockOperation, CpuClockTarget},
        PosixTimeSpec,
    },
};

use super::PosixClockID;

pub struct SysClockSettime;

impl Syscall for SysClockSettime {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let clock = PosixClockID::try_from(args[0] as i32)?;
        match clock {
            // Linux static CPU aliases have no clock_set callback.
            PosixClockID::ProcessCPUTimeID | PosixClockID::ThreadCPUTimeID => {
                return Err(SystemError::EINVAL)
            }
            PosixClockID::DynamicCpuClock(_) => {}
            // Setting wall clocks is not implemented by this CPU-clock entry.
            _ => return Err(SystemError::ENOSYS),
        }
        let reader = UserBufferReader::new(
            args[1] as *const PosixTimeSpec,
            core::mem::size_of::<PosixTimeSpec>(),
            true,
        )?;
        let _requested = reader.buffer_protected(0)?.read_one::<PosixTimeSpec>(0)?;
        CpuClockTarget::resolve(clock, CpuClockOperation::Validate)?;
        Err(SystemError::EPERM)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("clock_id", format!("{}", args[0] as i32)),
            FormattedSyscallParam::new("timespec", format!("{:#x}", args[1])),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_CLOCK_SETTIME, SysClockSettime);
