//! mlock 系统调用实现
//!
//! # 系统调用原型
//!
//! ```c
//! int mlock(const void *addr, size_t len);
//! ```
//!
//! # 功能
//!
//! 锁定指定地址范围的内存页，防止其被换出到交换空间。
//! 被锁定的页面驻留在物理内存中，不会被换出。
//!
//! # 参数
//!
//! - `addr`: 起始地址（会自动向下对齐到页边界）
//! - `len`: 长度（会自动向上对齐到页边界）
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
//! - `EINVAL`: 地址或长度无效
//!
//! # 注意
//!
//! - 多次锁定同一页面会增加引用计数，需要对应次数的 munlock
//! - fork 后子进程不继承锁定的页面（locked_vm 重置为 0）

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

        // ========== 地址有效性检查 ==========
        // 拒绝 NULL 指针（除非 len = 0，这是合法的空操作）
        if args[0] == 0 && len > 0 {
            return Err(SystemError::EINVAL);
        }

        // ========== 参数基本校验 ==========
        if len == 0 {
            return Ok(0);
        }

        // ========== 地址对齐 ==========
        let page_offset = addr.data() & (MMArch::PAGE_SIZE - 1);
        let aligned_addr = VirtAddr::new(addr.data() - page_offset);
        let adjusted_len = len.saturating_add(page_offset);
        let aligned_len = page_align_up(adjusted_len);

        if aligned_len == 0 || aligned_len < len {
            return Err(SystemError::ENOMEM);
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

        // ========== RLIMIT_MEMLOCK 检查 + 执行锁定操作 ==========
        // 参考 Linux 内核 mm/mlock.c:593-612：
        // - 先获取 mmap_write_lock（写锁）
        // - 在锁保护下检查 RLIMIT_MEMLOCK
        // - 在锁保护下执行 mlock 操作
        // - 最后释放锁
        // 这样可以防止 TOCTOU 竞态：在检查和操作之间，并发的 munlock/munmap
        // 不会修改 locked_vm，避免计账不一致或下溢

        let lock_limit = ProcessManager::current_pcb()
            .get_rlimit(RLimitID::Memlock)
            .rlim_cur as usize;

        let lock_limit_pages = if lock_limit == usize::MAX {
            usize::MAX
        } else {
            lock_limit >> MMArch::PAGE_SHIFT
        };

        // 获取地址空间写锁，保持到 mlock 完成
        let mut addr_space_write = addr_space.write();

        // 资源限制检查（在写锁保护下）
        let requested_pages = aligned_len >> MMArch::PAGE_SHIFT;
        let current_locked = addr_space_write.locked_vm();

        let mut locked = current_locked + requested_pages;
        if locked > lock_limit_pages {
            let already_locked_in_range =
                addr_space_write.count_mm_mlocked_page_nr(aligned_addr, aligned_len);
            locked = current_locked + requested_pages - already_locked_in_range;
        }

        if locked > lock_limit_pages {
            return Err(SystemError::ENOMEM);
        }

        // ========== 执行锁定操作 ==========
        // 无论是否包含不可访问的 VMA，都设置 VM_LOCKED 标志
        // 这是破坏性操作，即使返回错误也不回滚（遵循 Linux 语义）
        let has_inaccessible_vma = addr_space_write.mlock(aligned_addr, aligned_len, false)?;

        // 释放写锁（通过 drop 显式释放，或让 Rust 在作用域结束时自动释放）
        drop(addr_space_write);

        // 如果包含不可访问的 VMA，返回 ENOMEM
        // 这模拟了 Linux 中 __mm_populate() 在 PROT_NONE 映射上失败的行为
        if has_inaccessible_vma {
            return Err(SystemError::ENOMEM);
        }

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
