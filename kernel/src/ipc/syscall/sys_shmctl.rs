use crate::alloc::vec::Vec;
use crate::arch::interrupt::TrapFrame;
use crate::{
    arch::syscall::nr::SYS_SHMCTL,
    ipc::shm::{
        PosixShmIdDs, PosixShmInfo, PosixShmMetaInfo, ShmCtlCmd, ShmId, ShmLockBegin, ShmManager,
    },
    process::ProcessManager,
    syscall::table::{FormattedSyscallParam, Syscall},
    syscall::user_access::{UserBufferReader, UserBufferWriter},
};
use syscall_table_macros::declare_syscall;
use system_error::SystemError;
pub struct SysShmctlHandle;

/// # SYS_SHMCTL系统调用函数，用于管理共享内存段
///
/// ## 参数
///
/// - `id`: 共享内存id
/// - `cmd`: 操作码
/// - `user_buf`: 用户缓冲区
/// - `from_user`: buf_vaddr是否来自用户地址空间
///
/// ## 返回值
///
/// 成功：0
/// 失败：错误码
pub(super) fn do_kernel_shmctl(
    id: ShmId,
    cmd: ShmCtlCmd,
    user_buf: *const u8,
    from_user: bool,
) -> Result<usize, SystemError> {
    // per-ns 管理器
    let ipcns = ProcessManager::current_ipcns();
    let mut shm_manager_guard = ipcns.shm.lock();

    match cmd {
        // 查看共享内存元信息
        ShmCtlCmd::IpcInfo => {
            let (ret, shm_meta_info) = shm_manager_guard.ipc_info_data();
            drop(shm_manager_guard);
            let mut user_buffer_writer = UserBufferWriter::new(
                user_buf as *mut u8,
                core::mem::size_of::<PosixShmMetaInfo>(),
                from_user,
            )?;
            user_buffer_writer.copy_one_to_user(&shm_meta_info, 0)?;
            Ok(ret)
        }
        // 查看共享内存使用信息
        ShmCtlCmd::ShmInfo => {
            let (ret, shm_info) = shm_manager_guard.shm_info_data()?;
            drop(shm_manager_guard);
            let mut user_buffer_writer = UserBufferWriter::new(
                user_buf as *mut u8,
                core::mem::size_of::<PosixShmInfo>(),
                from_user,
            )?;
            user_buffer_writer.copy_one_to_user(&shm_info, 0)?;
            Ok(ret)
        }
        // 查看id对应的共享内存信息
        ShmCtlCmd::ShmStat | ShmCtlCmd::ShmtStatAny | ShmCtlCmd::IpcStat => {
            let (ret, shm_id_ds) = shm_manager_guard.shm_stat_data(id, cmd)?;
            drop(shm_manager_guard);
            let mut user_buffer_writer = UserBufferWriter::new(
                user_buf as *mut u8,
                core::mem::size_of::<PosixShmIdDs>(),
                from_user,
            )?;
            user_buffer_writer.copy_one_to_user(&shm_id_ds, 0)?;
            Ok(ret)
        }
        // 设置KernIpcPerm
        ShmCtlCmd::IpcSet => {
            drop(shm_manager_guard);
            let user_buffer_reader =
                UserBufferReader::new(user_buf, core::mem::size_of::<PosixShmIdDs>(), from_user)?;
            let mut shm_id_ds = PosixShmIdDs::default();
            user_buffer_reader.copy_one_from_user(&mut shm_id_ds, 0)?;
            let ipcns = ProcessManager::current_ipcns();
            let mut shm_manager_guard = ipcns.shm.lock();
            shm_manager_guard.ipc_set(id, shm_id_ds)
        }
        // 将共享内存段设置为可回收状态
        ShmCtlCmd::IpcRmid => {
            let destroy = shm_manager_guard.ipc_rmid(id)?;
            drop(shm_manager_guard);
            if let Some(destroy) = destroy {
                destroy.finish();
            }
            Ok(0)
        }
        // 锁住共享内存段，不允许内存置换
        ShmCtlCmd::ShmLock => {
            let begin = shm_manager_guard.shm_lock_begin(id)?;
            drop(shm_manager_guard);
            let reclassify = match begin {
                ShmLockBegin::Done(reclassify) => reclassify,
                ShmLockBegin::NeedCharge { size } => {
                    let token = ShmManager::charge_memlock_for_shm(size)?;
                    let ipcns = ProcessManager::current_ipcns();
                    let mut shm_manager_guard = ipcns.shm.lock();
                    shm_manager_guard.shm_lock_commit(id, token)?
                }
            };
            if let Some((page_cache, old_mapping_unevictable)) = reclassify {
                page_cache.reclassify_unevictable_pages(old_mapping_unevictable);
            }
            Ok(0)
        }
        // 解锁共享内存段，允许内存置换
        ShmCtlCmd::ShmUnlock => {
            let reclassify = shm_manager_guard.shm_unlock(id)?;
            drop(shm_manager_guard);
            if let Some((page_cache, old_mapping_unevictable)) = reclassify {
                page_cache.reclassify_unevictable_pages(old_mapping_unevictable);
            }
            Ok(0)
        }
        // 无效操作码
        ShmCtlCmd::Default => Err(SystemError::EINVAL),
    }
}

impl SysShmctlHandle {
    #[inline(always)]
    fn id(args: &[usize]) -> ShmId {
        ShmId::new(args[0]) // Assuming ShmId::new takes usize or can infer from args[0]
    }

    #[inline(always)]
    fn cmd(args: &[usize]) -> ShmCtlCmd {
        ShmCtlCmd::from(args[1])
    }

    #[inline(always)]
    fn user_buf(args: &[usize]) -> *const u8 {
        args[2] as *const u8 // Assuming args[2] is a pointer to user buffer
    }
}

impl Syscall for SysShmctlHandle {
    fn num_args(&self) -> usize {
        3
    }
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("shmid", format!("{}", Self::id(args).data())),
            FormattedSyscallParam::new("cmd", format!("{}", Self::cmd(args))),
            FormattedSyscallParam::new("buf", format!("{:#x}", Self::user_buf(args) as usize)),
        ]
    }

    fn handle(&self, args: &[usize], frame: &mut TrapFrame) -> Result<usize, SystemError> {
        if args[0] > i32::MAX as usize || args[1] > i32::MAX as usize {
            return Err(SystemError::EINVAL);
        }
        let id = Self::id(args);
        let cmd = Self::cmd(args);
        let user_buf = Self::user_buf(args);
        do_kernel_shmctl(id, cmd, user_buf, frame.is_from_user())
    }
}

declare_syscall!(SYS_SHMCTL, SysShmctlHandle);
