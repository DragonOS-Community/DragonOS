//! mlock 系统调用实现

use crate::arch::{interrupt::TrapFrame, syscall::nr::SYS_MLOCK, MMArch};
use crate::mm::{
    mlock::can_do_mlock, syscall::page_align_up, ucontext::AddressSpace, MemoryManagementArch,
    VirtAddr, 
};
use crate::process::{resource::RLimitID, ProcessManager};
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysMlockHandle;

impl Syscall for SysMlockHandle {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let addr = VirtAddr::new(args[0]);
        let len = args[1];

        // 基本参数校验
        if len == 0 {
            return Ok(0);
        }

        // 长度对齐并检查溢出
        let aligned_len = page_align_up(len);
        if aligned_len == 0 || aligned_len < len {
            return Err(SystemError::ENOMEM);
        }

        // 检查地址范围
        let end = addr
            .data()
            .checked_add(aligned_len)
            .ok_or(SystemError::ENOMEM)?;
        if end > MMArch::USER_END_VADDR.data() {
            return Err(SystemError::ENOMEM);
        }

        // 权限检查
        if !can_do_mlock() {
            return Err(SystemError::EPERM);
        }

        // 获取当前地址空间
        let addr_space = AddressSpace::current()?;

        // RLIMIT_MEMLOCK 检查
        // 参考 Linux: mm/mlock.c:do_mlock()
        let lock_limit = ProcessManager::current_pcb()
            .get_rlimit(RLimitID::Memlock)
            .rlim_cur as usize;

        // 将限制转换为页面数
        let lock_limit_pages = if lock_limit == usize::MAX {
            usize::MAX
        } else {
            lock_limit >> MMArch::PAGE_SHIFT
        };

        let requested_pages = aligned_len >> MMArch::PAGE_SHIFT;

        // 计算当前已锁定的页面数
        let current_locked = addr_space.read().locked_vm();

        // 检查是否超过限制
        // 参考 Linux: mm/mlock.c:do_mlock() 和 user_lock_limit()
        // 如果没有 CAP_IPC_LOCK 权限，需要检查 RLIMIT_MEMLOCK 限制
        if current_locked + requested_pages > lock_limit_pages {
            return Err(SystemError::ENOMEM);
        }

        // 执行 mlock
        addr_space.write().mlock(addr, aligned_len, false)?;

        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("addr", format!("{:#x}", args[0])),
            FormattedSyscallParam::new("len", format!("{:#x}", args[1])),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_MLOCK, SysMlockHandle);
