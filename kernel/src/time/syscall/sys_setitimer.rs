use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_SETITIMER},
    process::ProcessManager,
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::{UserBufferReader, UserBufferWriter},
    },
    time::syscall::{ItimerType, Itimerval},
};
use alloc::vec::Vec;
use core::mem::size_of;
use system_error::SystemError;

pub struct SysSetitimerHandle;
impl Syscall for SysSetitimerHandle {
    fn num_args(&self) -> usize {
        3
    }
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let which = ItimerType::try_from(args[0] as i32)?;
        let config = if args[1] == 0 {
            Itimerval::default()
        } else {
            let reader =
                UserBufferReader::new(args[1] as *const Itimerval, size_of::<Itimerval>(), true)?;
            reader.read_one_from_user::<Itimerval>(0)?
        };
        // Full-width validation precedes any state change or output write.
        for value in [config.it_value, config.it_interval] {
            if value.tv_sec < 0 || !(0..1_000_000).contains(&value.tv_usec) {
                return Err(SystemError::EINVAL);
            }
        }
        let old = ProcessManager::current_pcb().set_itimer(which, config);
        // Linux commits before copying old_value: an output EFAULT does not
        // roll back the timer. No faultable copy under the IRQ-safe state lock.
        if args[2] != 0 {
            let mut writer =
                UserBufferWriter::new(args[2] as *mut Itimerval, size_of::<Itimerval>(), true)?;
            writer.copy_one_to_user(&old, 0)?;
        }
        Ok(0)
    }
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("which", format!("{}", args[0])),
            FormattedSyscallParam::new("new_value", format!("{:#x}", args[1])),
            FormattedSyscallParam::new("old_value", format!("{:#x}", args[2])),
        ]
    }
}
syscall_table_macros::declare_syscall!(SYS_SETITIMER, SysSetitimerHandle);
