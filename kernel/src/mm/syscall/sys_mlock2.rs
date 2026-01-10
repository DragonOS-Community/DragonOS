//! mlock2 系统调用实现
//!
//! # 系统调用原型
//!
//! ```c
//! int mlock2(const void *addr, size_t len, int flags);
//! ```
//!
//! # 功能
//!
//! 锁定指定地址范围的内存页，防止其被换出到交换空间。
//!
//! # 与 mlock 的区别
//!
//! mlock2 支持额外的 flags 参数，可用于指定锁定行为：
//! - `MLOCK_ONFAULT`: 延迟锁定，仅在页面首次被访问时才锁定
//!
//! # 参数
//!
//! - `addr`: 起始地址（会自动向下对齐到页边界）
//! - `len`: 长度（会自动向上对齐到页边界）
//! - `flags`: 标志位（0 或 MLOCK_ONFAULT）
//!
//! # 返回值
//!
//! - 0: 成功
//! - -1: 失败，设置 errno
//!
//! # 错误码
//!
//! - `ENOMEM`: 超过 RLIMIT_MEMLOCK 限制
//! - `EPERM`: RLIMIT_MEMLOCK 为 0 且没有 CAP_IPC_LOCK 权限
//! - `EINVAL`: flags 包含无效位
//! - `ENOTSUP`: 暂不支持该标志

use crate::arch::{interrupt::TrapFrame, syscall::nr::SYS_MLOCK2, MMArch};
use crate::mm::{
    mlock::can_do_mlock,
    syscall::{page_align_up, Mlock2Flags},
    ucontext::AddressSpace,
    MemoryManagementArch, VirtAddr,
};
use crate::process::{resource::RLimitID, ProcessManager};
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysMlock2Handle;

impl Syscall for SysMlock2Handle {
    fn num_args(&self) -> usize {
        3
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let addr = VirtAddr::new(args[0]);
        let len = args[1];
        let flags = Mlock2Flags::from_bits(args[2] as u32).ok_or(SystemError::EINVAL)?;

        // ========== 参数基本校验 ==========
        if len == 0 {
            return Ok(0);
        }

        // ========== 地址对齐 ==========
        // 起始地址向下对齐到页边界，长度向上对齐
        let page_offset = addr.data() & (MMArch::PAGE_SIZE - 1);
        let aligned_addr = VirtAddr::new(addr.data() - page_offset);
        let adjusted_len = len.saturating_add(page_offset);
        let aligned_len = page_align_up(adjusted_len);

        // 检查对齐后长度是否溢出
        if aligned_len == 0 || aligned_len < len {
            return Err(SystemError::ENOMEM);
        }

        // ========== 标志位验证 ==========
        // 只允许 MLOCK_ONFAULT 标志
        if !flags.is_empty() && !flags.contains(Mlock2Flags::MLOCK_ONFAULT) {
            return Err(SystemError::EINVAL);
        }

        // ========== 地址范围检查 ==========
        let end = aligned_addr
            .data()
            .checked_add(aligned_len)
            .ok_or(SystemError::ENOMEM)?;
        if end > MMArch::USER_END_VADDR.data() {
            return Err(SystemError::ENOMEM);
        }

        // ========== 权限检查 ==========
        if !can_do_mlock() {
            return Err(SystemError::EPERM);
        }

        let addr_space = AddressSpace::current()?;

        // ========== RLIMIT_MEMLOCK 检查 ==========
        let lock_limit = ProcessManager::current_pcb()
            .get_rlimit(RLimitID::Memlock)
            .rlim_cur as usize;

        let lock_limit_pages = if lock_limit == usize::MAX {
            usize::MAX
        } else {
            lock_limit >> MMArch::PAGE_SHIFT
        };

        let requested_pages = aligned_len >> MMArch::PAGE_SHIFT;
        let addr_space_read = addr_space.read();
        let current_locked = addr_space_read.locked_vm();

        // 检查是否超过资源限制
        let mut locked = current_locked + requested_pages;
        if locked > lock_limit_pages {
            // 计算范围内已锁定的页面（避免重复计数）
            let already_locked_in_range =
                addr_space_read.count_mm_mlocked_page_nr(aligned_addr, aligned_len);
            drop(addr_space_read);
            locked = current_locked + requested_pages - already_locked_in_range;
        }

        if locked > lock_limit_pages {
            return Err(SystemError::ENOMEM);
        }

        // ========== 执行锁定操作 ==========
        let onfault = flags.contains(Mlock2Flags::MLOCK_ONFAULT);
        addr_space.write().mlock(aligned_addr, aligned_len, onfault)?;

        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("addr", format!("{:#x}", args[0])),
            FormattedSyscallParam::new("len", format!("{:#x}", args[1])),
            FormattedSyscallParam::new("flags", format!("{:#x}", args[2])),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_MLOCK2, SysMlock2Handle);
