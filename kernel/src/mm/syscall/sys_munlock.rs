//! munlock 系统调用实现
//!
//! # 系统调用原型
//!
//! ```c
//! int munlock(const void *addr, size_t len);
//! ```
//!
//! # 功能
//!
//! 解锁指定地址范围的内存页，允许其被换出到交换空间。
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
//! - `EINVAL`: 地址或长度无效
//! - `ENOMEM`: 地址范围超出用户空间
//!
//! # 注意
//!
//! - 如果页面被多次锁定（引用计数 > 1），需要对应次数的 munlock
//! - 部分解锁：可以只解锁范围内的部分页面

use crate::arch::{interrupt::TrapFrame, syscall::nr::SYS_MUNLOCK, MMArch};
use crate::mm::{syscall::page_align_up, ucontext::AddressSpace, MemoryManagementArch, VirtAddr};
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysMunlockHandle;

impl Syscall for SysMunlockHandle {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let addr = VirtAddr::new(args[0]);
        let len = args[1];

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

        // ========== 执行解锁操作 ==========
        let addr_space = AddressSpace::current()?;
        addr_space.write().munlock(aligned_addr, aligned_len)?;

        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("addr", format!("{:#x}", args[0])),
            FormattedSyscallParam::new("len", format!("{:#x}", args[1])),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_MUNLOCK, SysMunlockHandle);
