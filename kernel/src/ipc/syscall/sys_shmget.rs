use crate::alloc::vec::Vec;
use crate::arch::interrupt::TrapFrame;
use crate::syscall::table::FormattedSyscallParam;
use crate::{
    arch::syscall::nr::SYS_SHMGET,
    ipc::shm::{ShmFlags, ShmKey, IPC_PRIVATE},
    process::ProcessManager,
    syscall::table::Syscall,
};
use log::error;
use syscall_table_macros::declare_syscall;
use system_error::SystemError;
pub struct SysShmgetHandle;

/// # SYS_SHMGET系统调用函数，用于获取共享内存
///
/// ## 参数
///
/// - `key`: 共享内存键值
/// - `size`: 共享内存大小(bytes)
/// - `shmflg`: 共享内存标志
///
/// ## 返回值
///
/// 成功：共享内存id
/// 失败：错误码
pub(super) fn do_kernel_shmget(
    key: ShmKey,
    size: usize,
    shmflg: ShmFlags,
) -> Result<usize, SystemError> {
    if shmflg.contains(ShmFlags::SHM_HUGETLB) {
        error!("shmget: not support huge page");
        return Err(SystemError::ENOSYS);
    }

    let ipcns = ProcessManager::current_ipcns();
    let mut shm_manager_guard = ipcns.shm.lock();

    match key {
        IPC_PRIVATE => shm_manager_guard.add(key, size, shmflg),
        _ => {
            let id = shm_manager_guard.contains_key(&key);

            if let Some(id) = id {
                let id = *id;
                if shmflg.contains(ShmFlags::IPC_CREAT | ShmFlags::IPC_EXCL) {
                    // IPC_CREAT | IPC_EXCL with existing segment -> EEXIST (Linux semantics)
                    return Err(SystemError::EEXIST);
                }

                let kernel_shm = shm_manager_guard.get_mut(&id).ok_or(SystemError::EINVAL)?;

                if size > kernel_shm.size() {
                    // request_size > existing segment size -> EINVAL (Linux semantics)
                    return Err(SystemError::EINVAL);
                }

                return Ok(id.data());
            } else {
                if !shmflg.contains(ShmFlags::IPC_CREAT) {
                    // no existing segment and no IPC_CREAT -> ENOENT (Linux semantics)
                    return Err(SystemError::ENOENT);
                }

                return shm_manager_guard.add(key, size, shmflg);
            }
        }
    }
}

impl SysShmgetHandle {
    #[inline(always)]
    fn key(args: &[usize]) -> ShmKey {
        ShmKey::new(args[0])
    }

    #[inline(always)]
    fn size(args: &[usize]) -> usize {
        args[1]
    }

    #[inline(always)]
    fn shmflg(args: &[usize]) -> ShmFlags {
        ShmFlags::from_bits_truncate(args[2] as u32)
    }
}

impl Syscall for SysShmgetHandle {
    fn num_args(&self) -> usize {
        3
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let key = Self::key(args);
        let size = Self::size(args);
        let shmflg = Self::shmflg(args);
        do_kernel_shmget(key, size, shmflg)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("key", format!("{}", Self::key(args).data())),
            FormattedSyscallParam::new("size", format!("{}", Self::size(args))),
            FormattedSyscallParam::new("shmflg", format!("{:#x}", Self::shmflg(args).bits())),
        ]
    }
}

declare_syscall!(SYS_SHMGET, SysShmgetHandle);
