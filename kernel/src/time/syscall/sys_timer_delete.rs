use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_TIMER_DELETE},
    process::ProcessManager,
    syscall::table::{FormattedSyscallParam, Syscall},
};
use alloc::vec::Vec;
use syscall_table_macros::declare_syscall;
use system_error::SystemError;

pub struct SysTimerDeleteHandle;

impl SysTimerDeleteHandle {
    fn timerid(args: &[usize]) -> i32 {
        args[0] as i32
    }
}

impl Syscall for SysTimerDeleteHandle {
    fn num_args(&self) -> usize {
        1
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let timerid = Self::timerid(args);
        let pcb = ProcessManager::current_pcb();
        pcb.posix_timers_irqsave().delete(&pcb, timerid)?;
        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new(
            "timerid",
            format!("{}", args[0]),
        )]
    }
}

declare_syscall!(SYS_TIMER_DELETE, SysTimerDeleteHandle);
