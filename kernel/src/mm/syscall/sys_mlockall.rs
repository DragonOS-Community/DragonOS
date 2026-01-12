//! mlockall 系统调用实现
//!
//! # 系统调用原型
//!
//! ```c
//! int mlockall(int flags);
//! ```
//!
//! # 功能
//!
//! 锁定进程地址空间的内存页，防止其被换出到交换空间。
//!
//! # 标志位
//!
//! - `MCL_CURRENT`: 锁定当前已映射的所有页面
//! - `MCL_FUTURE`: 锁定未来映射的所有页面
//! - `MCL_ONFAULT`: 延迟锁定（需与 MCL_CURRENT 或 MCL_FUTURE 组合使用）
//!
//! # 返回值
//!
//! - 0: 成功
//! - -1: 失败，设置 errno
//!
//! # 错误码
//!
//! - `EINVAL`: 标志位无效或未指定 MCL_CURRENT/MCL_FUTURE
//! - `ENOMEM`: 超过 RLIMIT_MEMLOCK 限制
//! - `EPERM`: RLIMIT_MEMLOCK 为 0 且没有 CAP_IPC_LOCK 权限
//!
//! # 注意
//!
//! - 必须至少指定 MCL_CURRENT 或 MCL_FUTURE 之一
//! - MCL_ONFAULT 不能单独使用

use crate::arch::{interrupt::TrapFrame, syscall::nr::SYS_MLOCKALL, MMArch};
use crate::mm::MemoryManagementArch;
use crate::mm::VmFlags;
use crate::mm::{mlock::can_do_mlock, syscall::MlockAllFlags, ucontext::AddressSpace};
use crate::process::{cred::CAPFlags, resource::RLimitID, ProcessManager};
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysMlockallHandle;

impl Syscall for SysMlockallHandle {
    fn num_args(&self) -> usize {
        1
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let flags = MlockAllFlags::from_bits(args[0] as u32).ok_or(SystemError::EINVAL)?;

        // 标志位验证（与 Linux 6.6.21 mm/mlock.c:710-711 一致）
        // 1. 必须至少指定 MCL_CURRENT 或 MCL_FUTURE 之一
        // 2. MCL_ONFAULT 不能单独使用
        if !flags.intersects(MlockAllFlags::MCL_CURRENT | MlockAllFlags::MCL_FUTURE)
            || flags == MlockAllFlags::MCL_ONFAULT
        {
            return Err(SystemError::EINVAL);
        }

        // ========== 权限检查 ==========
        if !can_do_mlock() {
            return Err(SystemError::EPERM);
        }

        let addr_space = AddressSpace::current()?;

        // ========== MCL_CURRENT: RLIMIT_MEMLOCK 检查 + 执行锁定操作 ==========
        // 参考 Linux 内核 mm/mlock.c:720-726：
        // 持有写锁贯穿整个操作以防止 TOCTOU 竞态
        // Linux 6.6.21 语义：
        // - 如果没有 CAP_IPC_LOCK 权限，需要检查 total_vm <= lock_limit
        // - total_vm 是进程地址空间中所有可访问 VMA 的总页面数
        // - 参考：mm/mlock.c:724
        if flags.contains(MlockAllFlags::MCL_CURRENT) {
            let has_cap_ipc_lock = ProcessManager::current_pcb()
                .cred()
                .has_capability(CAPFlags::CAP_IPC_LOCK);

            if !has_cap_ipc_lock {
                let lock_limit = ProcessManager::current_pcb()
                    .get_rlimit(RLimitID::Memlock)
                    .rlim_cur as usize;

                let lock_limit_pages = if lock_limit == usize::MAX {
                    usize::MAX
                } else {
                    lock_limit >> MMArch::PAGE_SHIFT
                };

                // 获取地址空间写锁，保持到 mlockall 完成
                let mut addr_space_write = addr_space.write();

                // 计算 total_vm：所有可访问 VMA 的总页面数
                // 这与 Linux 的 total_vm 语义一致，表示进程地址空间的总大小
                let mut total_vm = 0;
                for vma in addr_space_write.mappings.iter_vmas() {
                    let vma_guard = vma.lock_irqsave();
                    let vm_flags = *vma_guard.vm_flags();
                    let region = *vma_guard.region();

                    // 判断是否可访问
                    let vm_access_flags = VmFlags::VM_READ | VmFlags::VM_WRITE | VmFlags::VM_EXEC;
                    let is_accessible = vm_flags.intersects(vm_access_flags);

                    drop(vma_guard);

                    if is_accessible {
                        let len = region.end().data() - region.start().data();
                        total_vm += len >> MMArch::PAGE_SHIFT;
                    }
                }

                // 检查是否超过限制（在写锁保护下）
                if total_vm > lock_limit_pages {
                    return Err(SystemError::ENOMEM);
                }

                // ========== 执行锁定操作 ==========
                addr_space_write.mlockall(args[0] as u32)?;

                // 释放写锁
                drop(addr_space_write);
            } else {
                // 有 CAP_IPC_LOCK 权限，直接执行锁定操作
                addr_space.write().mlockall(args[0] as u32)?;
            }
        } else {
            // 仅 MCL_FUTURE 标志，直接执行
            addr_space.write().mlockall(args[0] as u32)?;
        }

        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new(
            "flags",
            format!("{:#x}", args[0]),
        )]
    }
}

syscall_table_macros::declare_syscall!(SYS_MLOCKALL, SysMlockallHandle);
