use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_GETITIMER},
    process::ProcessManager,
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::UserBufferWriter,
    },
    time::syscall::{ItimerType, Itimerval},
};
use alloc::vec::Vec;
use core::mem::size_of;
use system_error::SystemError;
pub struct SysGetitimerHandle;
impl Syscall for SysGetitimerHandle {
    fn num_args(&self) -> usize {
        2
    }
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let which = ItimerType::try_from(args[0] as i32)?;
        let value = ProcessManager::current_pcb().get_itimer(which);
        let mut writer =
            UserBufferWriter::new(args[1] as *mut Itimerval, size_of::<Itimerval>(), true)?;
        writer.copy_one_to_user(&value, 0)?;
        Ok(0)
    }
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("which", format!("{}", args[0])),
            FormattedSyscallParam::new("value", format!("{:#x}", args[1])),
        ]
    }
}
syscall_table_macros::declare_syscall!(SYS_GETITIMER, SysGetitimerHandle);
