use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_NANOSLEEP;
use crate::ipc::signal::{RestartBlock, RestartBlockData, RestartFnNanosleep};
use crate::mm::VirtAddr;
use crate::process::ProcessManager;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::{UserBufferReader, UserBufferWriter};
use crate::time::sleep::nanosleep;
use crate::time::syscall::PosixClockID;
use crate::time::timekeeping::monotonic_now;
use crate::time::PosixTimeSpec;
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysNanosleep;

impl SysNanosleep {
    fn sleep_time(args: &[usize]) -> *const PosixTimeSpec {
        args[0] as *const PosixTimeSpec
    }

    fn rm_time(args: &[usize]) -> *mut PosixTimeSpec {
        args[1] as *mut PosixTimeSpec
    }
}

impl Syscall for SysNanosleep {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let sleep_time_reader = UserBufferReader::new(
            Self::sleep_time(args),
            core::mem::size_of::<PosixTimeSpec>(),
            true,
        )?;
        let sleep_time = sleep_time_reader.read_one_from_user::<PosixTimeSpec>(0)?;

        let slt_spec = PosixTimeSpec {
            tv_sec: sleep_time.tv_sec,
            tv_nsec: sleep_time.tv_nsec,
        };
        if !slt_spec.is_valid_timeout() {
            return Err(SystemError::EINVAL);
        }

        let deadline = monotonic_now().saturating_add_ktime(&slt_spec);
        // Linux only writes `rem` when the sleep is interrupted. A successful
        // nanosleep must leave the user buffer untouched.
        match nanosleep(slt_spec) {
            Ok(()) => return Ok(0),
            Err(SystemError::ERESTARTSYS) => {}
            Err(error) => return Err(error),
        }

        let remaining = deadline.saturating_sub_timespec(&monotonic_now());
        if remaining.is_empty() {
            return Ok(0);
        }

        let rmtp = if Self::rm_time(args).is_null() {
            None
        } else {
            Some(VirtAddr::new(Self::rm_time(args) as usize))
        };
        if let Some(rmtp) = rmtp {
            let mut writer = UserBufferWriter::new(
                rmtp.as_ptr::<PosixTimeSpec>(),
                core::mem::size_of::<PosixTimeSpec>(),
                true,
            )?;
            writer.copy_one_to_user(&remaining, 0)?;
        }

        let data = RestartBlockData::new_nanosleep(deadline, PosixClockID::Monotonic, rmtp);
        let restart = RestartBlock::new(&RestartFnNanosleep, data);
        ProcessManager::current_pcb().set_restart_fn(Some(restart))
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new(
                "sleep_time",
                format!("{:#x}", Self::sleep_time(args) as usize),
            ),
            FormattedSyscallParam::new("rm_time", format!("{:#x}", Self::rm_time(args) as usize)),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_NANOSLEEP, SysNanosleep);
