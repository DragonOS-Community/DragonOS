use crate::alloc::vec::Vec;
use crate::arch::interrupt::TrapFrame;
use crate::syscall::table::FormattedSyscallParam;
use crate::{
    arch::syscall::nr::SYS_SHMGET,
    ipc::shm::{ShmFlags, ShmKey, ShmManager, IPC_PRIVATE},
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

    fn existing_segment_result(
        shm_manager: &mut ShmManager,
        id: crate::ipc::shm::ShmId,
        size: usize,
        shmflg: ShmFlags,
    ) -> Result<usize, SystemError> {
        if shmflg.contains(ShmFlags::IPC_CREAT | ShmFlags::IPC_EXCL) {
            return Err(SystemError::EEXIST);
        }

        let kernel_shm = shm_manager.get_by_shmid_checked(id)?;
        if size > kernel_shm.size() {
            return Err(SystemError::EINVAL);
        }

        shm_manager.check_existing_key_permission(id, shmflg)?;
        Ok(id.data())
    }

    let ipcns = ProcessManager::current_ipcns();

    match key {
        IPC_PRIVATE => {
            let numpages = {
                let shm_manager_guard = ipcns.shm.lock();
                shm_manager_guard.validate_new_segment_size(size)?
            };
            let backing = ShmManager::create_default_backing(size)?;
            let mut shm_manager_guard = ipcns.shm.lock();
            shm_manager_guard.add_prepared(key, size, shmflg, backing, numpages)
        }
        _ => {
            let create_numpages = {
                let mut shm_manager_guard = ipcns.shm.lock();
                let id = shm_manager_guard.contains_key(&key).copied();

                if let Some(id) = id {
                    return existing_segment_result(&mut shm_manager_guard, id, size, shmflg);
                }

                if !shmflg.contains(ShmFlags::IPC_CREAT) {
                    // no existing segment and no IPC_CREAT -> ENOENT (Linux semantics)
                    return Err(SystemError::ENOENT);
                }

                shm_manager_guard.validate_new_segment_size(size)?
            };

            let backing = ShmManager::create_default_backing(size)?;
            let mut shm_manager_guard = ipcns.shm.lock();
            if let Some(id) = shm_manager_guard.contains_key(&key).copied() {
                return existing_segment_result(&mut shm_manager_guard, id, size, shmflg);
            }

            shm_manager_guard.add_prepared(key, size, shmflg, backing, create_numpages)
        }
    }
}

impl SysShmgetHandle {
    #[inline(always)]
    fn key(args: &[usize]) -> ShmKey {
        ShmKey::new(args[0] as u32 as usize)
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
