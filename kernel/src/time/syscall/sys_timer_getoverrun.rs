use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_TIMER_GETOVERRUN},
    process::ProcessManager,
    syscall::table::{FormattedSyscallParam, Syscall},
};
use alloc::vec::Vec;
use syscall_table_macros::declare_syscall;
use system_error::SystemError;

pub struct SysTimerGetoverrunHandle;

impl SysTimerGetoverrunHandle {
    fn timerid(args: &[usize]) -> i32 {
        args[0] as i32
    }
}

impl Syscall for SysTimerGetoverrunHandle {
    fn num_args(&self) -> usize {
        1
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let timerid = Self::timerid(args);
        let pcb = ProcessManager::current_pcb();
        let v = pcb.posix_timers_irqsave().getoverrun(timerid)?;
        Ok(v as isize as usize)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new(
            "timerid",
            format!("{}", args[0]),
        )]
    }
}

declare_syscall!(SYS_TIMER_GETOVERRUN, SysTimerGetoverrunHandle);
