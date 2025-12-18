use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_TIMER_GETTIME},
    process::posix_timer::PosixItimerspec,
    process::ProcessManager,
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::UserBufferWriter,
    },
};
use alloc::vec::Vec;
use core::mem::size_of;
use syscall_table_macros::declare_syscall;
use system_error::SystemError;

pub struct SysTimerGettimeHandle;

impl SysTimerGettimeHandle {
    fn timerid(args: &[usize]) -> i32 {
        args[0] as i32
    }
    fn curr_value(args: &[usize]) -> *mut PosixItimerspec {
        args[1] as *mut PosixItimerspec
    }
}

impl Syscall for SysTimerGettimeHandle {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let timerid = Self::timerid(args);
        let curr_value_ptr = Self::curr_value(args);
        if curr_value_ptr.is_null() {
            return Err(SystemError::EINVAL);
        }
        let pcb = ProcessManager::current_pcb();
        let val = pcb.posix_timers_irqsave().gettime(timerid)?;
        let mut writer = UserBufferWriter::new(curr_value_ptr, size_of::<PosixItimerspec>(), true)?;
        // 用异常表保护版本写回，避免用户地址缺页/无效导致内核崩溃
        writer
            .buffer_protected(0)?
            .write_one::<PosixItimerspec>(0, &val)?;
        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("timerid", format!("{}", args[0])),
            FormattedSyscallParam::new("curr_value", format!("{:#x}", args[1])),
        ]
    }
}

declare_syscall!(SYS_TIMER_GETTIME, SysTimerGettimeHandle);
