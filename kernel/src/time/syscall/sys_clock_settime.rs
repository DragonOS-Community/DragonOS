use alloc::vec::Vec;
use system_error::SystemError;

use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_CLOCK_SETTIME},
    process::cred::{capable, CAPFlags},
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::UserBufferReader,
    },
    time::{
        cpu_clock::{CpuClockOperation, CpuClockTarget},
        timekeeping::{do_settimeofday64, validate_settimeofday},
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
            PosixClockID::Realtime | PosixClockID::DynamicCpuClock(_) => {}
            // Linux clocks without a setter reject the ID before user access.
            _ => return Err(SystemError::EINVAL),
        }
        let reader = UserBufferReader::new(
            args[1] as *const PosixTimeSpec,
            core::mem::size_of::<PosixTimeSpec>(),
            true,
        )?;
        let requested = reader.buffer_protected(0)?.read_one::<PosixTimeSpec>(0)?;
        if clock == PosixClockID::Realtime {
            // Match do_sys_settimeofday64(): formatted-value validation precedes
            // privilege checks, while the REALTIME >= MONOTONIC check belongs
            // to the authorized low-level update under the timekeeper lock.
            validate_settimeofday(requested)?;
            // The wall clock is global. A capability in a child user namespace
            // must not authorize changing it, even for that namespace's root.
            if !capable(CAPFlags::CAP_SYS_TIME) {
                return Err(SystemError::EPERM);
            }
            do_settimeofday64(requested)?;
            return Ok(0);
        }
        // Dynamic CPU clocks validate the target but never permit resetting it;
        // their setter deliberately does not validate the timespec's value.
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
