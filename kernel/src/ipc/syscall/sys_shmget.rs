use crate::alloc::vec::Vec;
use crate::syscall::table::FormattedSyscallParam;
use crate::{
    arch::syscall::nr::SYS_SHMGET,
    ipc::shm::{shm_manager_lock, ShmFlags, ShmKey, IPC_PRIVATE},
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
    // 暂不支持巨页
    if shmflg.contains(ShmFlags::SHM_HUGETLB) {
        error!("shmget: not support huge page");
        return Err(SystemError::ENOSYS);
    }

    let mut shm_manager_guard = shm_manager_lock();
    match key {
        // 创建共享内存段
        IPC_PRIVATE => shm_manager_guard.add(key, size, shmflg),
        _ => {
            // 查找key对应的共享内存段是否存在
            let id = shm_manager_guard.contains_key(&key);
            if let Some(id) = id {
                // 不能重复创建
                if shmflg.contains(ShmFlags::IPC_CREAT | ShmFlags::IPC_EXCL) {
                    return Err(SystemError::EEXIST);
                }

                // key值存在，说明有对应共享内存，返回该共享内存id
                return Ok(id.data());
            } else {
                // key不存在且shm_flags不包含IPC_CREAT创建IPC对象标志，则返回错误码
                if !shmflg.contains(ShmFlags::IPC_CREAT) {
                    return Err(SystemError::ENOENT);
                }

                // 存在创建IPC对象标志
                return shm_manager_guard.add(key, size, shmflg);
            }
        }
    }
}

impl SysShmgetHandle {
    #[inline(always)]
    fn key(args: &[usize]) -> ShmKey {
        // 第一个参数是共享内存的key
        // In the old code: ShmKey::new(args[0])
        // ShmKey is likely a type alias for i32 or similar, args[0] is usize
        ShmKey::new(args[0]) // Assuming ShmKey::new takes i32
    }

    #[inline(always)]
    fn size(args: &[usize]) -> usize {
        // 第二个参数是共享内存的大小
        args[1]
    }

    #[inline(always)]
    fn shmflg(args: &[usize]) -> ShmFlags {
        // 第三个参数是共享内存的标志
        // In the old code: ShmFlags::from_bits_truncate(args[2] as u32)
        ShmFlags::from_bits_truncate(args[2] as u32)
    }
}

impl Syscall for SysShmgetHandle {
    fn num_args(&self) -> usize {
        3 // key, size, shmflg
    }

    fn handle(&self, args: &[usize], _from_user: bool) -> Result<usize, SystemError> {
        let key = Self::key(args);
        let size = Self::size(args);
        let shmflg = Self::shmflg(args);
        do_kernel_shmget(key, size, shmflg)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("key", format!("{}", Self::key(args).data())),
            // 使用 format! 宏将 usize 类型的 size 转换为 String
            FormattedSyscallParam::new("size", format!("{}", Self::size(args))),
            FormattedSyscallParam::new("shmflg", format!("{:#x}", Self::shmflg(args).bits())),
        ]
    }
}

declare_syscall!(SYS_SHMGET, SysShmgetHandle);
