//! munlock 系统调用实现

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

        if len == 0 {
            return Ok(0);
        }

        // 页面对齐：起始地址向下对齐，长度调整
        // 参考 Linux mm/mlock.c: 用户传入的地址需要向下对齐到页边界
        // 长度需要加上页内偏移后再向上对齐
        let page_offset = addr.data() & (MMArch::PAGE_SIZE - 1);
        let aligned_addr = VirtAddr::new(addr.data() - page_offset);
        let adjusted_len = len.saturating_add(page_offset);
        let aligned_len = page_align_up(adjusted_len);
        if aligned_len == 0 || aligned_len < len {
            return Err(SystemError::ENOMEM);
        }

        let end = addr
            .data()
            .checked_add(aligned_len)
            .ok_or(SystemError::ENOMEM)?;
        if end > MMArch::USER_END_VADDR.data() {
            return Err(SystemError::ENOMEM);
        }

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
