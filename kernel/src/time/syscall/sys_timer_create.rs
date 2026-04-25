use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_TIMER_CREATE},
    process::posix_timer::PosixSigevent,
    process::ProcessManager,
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::{UserBufferReader, UserBufferWriter},
    },
    time::syscall::PosixClockID,
};
use alloc::vec::Vec;
use core::mem::size_of;
use syscall_table_macros::declare_syscall;
use system_error::SystemError;

pub struct SysTimerCreateHandle;

impl SysTimerCreateHandle {
    fn clockid(args: &[usize]) -> Result<PosixClockID, SystemError> {
        PosixClockID::try_from(args[0] as i32)
    }
    fn sevp(args: &[usize]) -> *const PosixSigevent {
        args[1] as *const PosixSigevent
    }
    fn timeridp(args: &[usize]) -> *mut i32 {
        args[2] as *mut i32
    }
}

impl Syscall for SysTimerCreateHandle {
    fn num_args(&self) -> usize {
        3
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let clockid = Self::clockid(args)?;
        let sevp = Self::sevp(args);
        let timeridp = Self::timeridp(args);

        if timeridp.is_null() {
            return Err(SystemError::EINVAL);
        }

        let pcb = ProcessManager::current_pcb();

        let sev = if sevp.is_null() {
            None
        } else {
            let reader = UserBufferReader::new(sevp, size_of::<PosixSigevent>(), true)?;
            // 用异常表保护版本读取
            Some(reader.buffer_protected(0)?.read_one::<PosixSigevent>(0)?)
        };

        let id = pcb.posix_timers_irqsave().create(&pcb, clockid, sev)?;

        let mut writer = UserBufferWriter::new(timeridp, size_of::<i32>(), true)?;
        // 用异常表保护版本写回
        writer.buffer_protected(0)?.write_one::<i32>(0, &id)?;
        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("clockid", format!("{}", args[0])),
            FormattedSyscallParam::new("sevp", format!("{:#x}", args[1])),
            FormattedSyscallParam::new("timerid", format!("{:#x}", args[2])),
        ]
    }
}

declare_syscall!(SYS_TIMER_CREATE, SysTimerCreateHandle);
