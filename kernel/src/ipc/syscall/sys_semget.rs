use crate::alloc::vec::Vec;
use crate::arch::interrupt::TrapFrame;
use crate::syscall::table::FormattedSyscallParam;
use crate::{
    arch::syscall::nr::SYS_SEMGET,
    ipc::sem::{SemFlags, SemKey, SemManager},
    process::ProcessManager,
    syscall::table::Syscall,
};
use syscall_table_macros::declare_syscall;
use system_error::SystemError;

pub struct SysSemgetHandle;

/// # SYS_SEMGET syscall: create or get a semaphore set
///
/// ## Parameters
///
/// - `key`: semaphore-set key
/// - `nsems`: number of semaphores in the set
/// - `semflg`: flags (IPC_CREAT/IPC_EXCL/permission bits)
///
/// ## Return value
///
/// On success: semaphore set ID.
/// On failure: error code
impl Syscall for SysSemgetHandle {
    fn num_args(&self) -> usize {
        3
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let key = SemKey::new(args[0] as u32 as usize);
        let nsems = args[1] as i32;
        if nsems < 0 {
            return Err(SystemError::EINVAL);
        }
        let semflg = SemFlags::from_bits_truncate(args[2] as u32);
        let ipcns = ProcessManager::current_ipcns();
        SemManager::semget(&ipcns, key, nsems as usize, semflg)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("key", format!("{}", args[0])),
            FormattedSyscallParam::new("nsems", format!("{}", args[1])),
            FormattedSyscallParam::new("semflg", format!("{:#x}", args[2])),
        ]
    }
}

declare_syscall!(SYS_SEMGET, SysSemgetHandle);
