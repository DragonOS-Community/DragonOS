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
use crate::process::{resource::RLimitID, ProcessManager};
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

        // ========== 标志位验证 ==========
        // 必须至少指定 MCL_CURRENT 或 MCL_FUTURE 之一
        if !flags.intersects(MlockAllFlags::MCL_CURRENT | MlockAllFlags::MCL_FUTURE) {
            return Err(SystemError::EINVAL);
        }
        // MCL_ONFAULT 不能单独使用
        if flags == MlockAllFlags::MCL_ONFAULT {
            return Err(SystemError::EINVAL);
        }

        // ========== 权限检查 ==========
        if !can_do_mlock() {
            return Err(SystemError::EPERM);
        }

        let addr_space = AddressSpace::current()?;

        // ========== MCL_CURRENT: RLIMIT_MEMLOCK 检查 ==========
        if flags.contains(MlockAllFlags::MCL_CURRENT) {
            let lock_limit = ProcessManager::current_pcb()
                .get_rlimit(RLimitID::Memlock)
                .rlim_cur as usize;

            let lock_limit_pages = if lock_limit == usize::MAX {
                usize::MAX
            } else {
                lock_limit >> MMArch::PAGE_SHIFT
            };

            let addr_space_read = addr_space.read();
            let current_locked = addr_space_read.locked_vm();

            // 计算需要锁定的页面数（未锁定的可访问 VMA）
            let mut pages_to_lock = 0;
            for vma in addr_space_read.mappings.iter_vmas() {
                let vma_guard = vma.lock_irqsave();
                let vm_flags = *vma_guard.vm_flags();
                let region = *vma_guard.region();

                if !vma.is_accessible() {
                    continue;
                }

                let already_locked = vm_flags.contains(VmFlags::VM_LOCKED)
                    || vm_flags.contains(VmFlags::VM_LOCKONFAULT);

                if !already_locked {
                    let len = region.end().data() - region.start().data();
                    pages_to_lock += len >> MMArch::PAGE_SHIFT;
                }
            }

            if current_locked + pages_to_lock > lock_limit_pages {
                return Err(SystemError::ENOMEM);
            }
        }

        // ========== 执行锁定操作 ==========
        addr_space.write().mlockall(args[0] as u32)?;

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
