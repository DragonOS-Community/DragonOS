use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_ALARM},
    process::ProcessManager,
    syscall::table::{FormattedSyscallParam, Syscall},
    time::syscall::{ItimerType, Itimerval, PosixTimeval},
};
use alloc::vec::Vec;
use system_error::SystemError;
pub struct SysAlarm;
impl Syscall for SysAlarm {
    fn num_args(&self) -> usize {
        1
    }
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let old = ProcessManager::current_pcb()
            .set_itimer(
                ItimerType::Real,
                Itimerval {
                    it_value: PosixTimeval {
                        tv_sec: args[0] as u32 as i64,
                        tv_usec: 0,
                    },
                    it_interval: PosixTimeval::default(),
                },
            )
            .it_value;
        let round_up = (old.tv_sec == 0 && old.tv_usec != 0) || old.tv_usec >= 500_000;
        Ok(old.tv_sec as usize + usize::from(round_up))
    }
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new(
            "seconds",
            format!("{}", args[0] as u32),
        )]
    }
}
#[cfg(target_arch = "x86_64")]
syscall_table_macros::declare_syscall!(SYS_ALARM, SysAlarm);
