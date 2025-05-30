use crate::alloc::vec::Vec;
use crate::{
    arch::syscall::nr::SYS_SHMCTL,
    ipc::shm::{shm_manager_lock, ShmCtlCmd, ShmId},
    syscall::table::{FormattedSyscallParam, Syscall},
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
    let mut shm_manager_guard = shm_manager_lock();

    match cmd {
        // 查看共享内存元信息
        ShmCtlCmd::IpcInfo => shm_manager_guard.ipc_info(user_buf, from_user),
        // 查看共享内存使用信息
        ShmCtlCmd::ShmInfo => shm_manager_guard.shm_info(user_buf, from_user),
        // 查看id对应的共享内存信息
        ShmCtlCmd::ShmStat | ShmCtlCmd::ShmtStatAny | ShmCtlCmd::IpcStat => {
            shm_manager_guard.shm_stat(id, cmd, user_buf, from_user)
        }
        // 设置KernIpcPerm
        ShmCtlCmd::IpcSet => shm_manager_guard.ipc_set(id, user_buf, from_user),
        // 将共享内存段设置为可回收状态
        ShmCtlCmd::IpcRmid => shm_manager_guard.ipc_rmid(id),
        // 锁住共享内存段，不允许内存置换
        ShmCtlCmd::ShmLock => shm_manager_guard.shm_lock(id),
        // 解锁共享内存段，允许内存置换
        ShmCtlCmd::ShmUnlock => shm_manager_guard.shm_unlock(id),
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

    fn handle(&self, args: &[usize], from_user: bool) -> Result<usize, SystemError> {
        let id = Self::id(args);
        let cmd = Self::cmd(args);
        let user_buf = Self::user_buf(args);
        do_kernel_shmctl(id, cmd, user_buf, from_user)
    }
}

declare_syscall!(SYS_SHMCTL, SysShmctlHandle);
