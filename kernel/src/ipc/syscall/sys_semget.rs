use crate::alloc::vec::Vec;
use crate::arch::interrupt::TrapFrame;
use crate::syscall::table::FormattedSyscallParam;
use crate::{
    arch::syscall::nr::SYS_SEMGET,
    ipc::sem::{SemFlags, SemKey},
    process::ProcessManager,
    syscall::table::Syscall,
};
use syscall_table_macros::declare_syscall;
use system_error::SystemError;

pub struct SysSemgetHandle;

/// # SYS_SEMGET 系统调用：创建或获取信号量集合
///
/// ## 参数
///
/// - `key`: 信号量集合键值
/// - `nsems`: 集合内信号量数量
/// - `semflg`: 标志位（IPC_CREAT/IPC_EXCL/权限位）
///
/// ## 返回值
///
/// 成功：信号量集合 id
/// 失败：错误码
impl Syscall for SysSemgetHandle {
    fn num_args(&self) -> usize {
        3
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        if args[0] > i32::MAX as usize {
            return Err(SystemError::EINVAL);
        }
        let key = SemKey::new(args[0] as u32 as usize);
        let nsems = args[1];
        let semflg = SemFlags::from_bits_truncate(args[2] as u32);
        let ipcns = ProcessManager::current_ipcns();
        let mut guard = ipcns.sem.lock();
        guard.semget(key, nsems, semflg)
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
