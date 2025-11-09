//! System call handler for the madvise system call.

use crate::arch::{interrupt::TrapFrame, syscall::nr::SYS_MADVISE, MMArch};
use crate::libs::align::page_align_up;
use crate::mm::{
    syscall::{MadvFlags, PageFrameCount},
    ucontext::AddressSpace,
    MemoryManagementArch, VirtPageFrame, {verify_area, VirtAddr},
};
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use system_error::SystemError;

use alloc::sync::Arc;
use alloc::vec::Vec;

/// Handles the madvise system call, which gives advice about the use of memory.
pub struct SysMadviseHandle;

impl Syscall for SysMadviseHandle {
    fn num_args(&self) -> usize {
        3
    }

    /// ## madvise系统调用
    ///
    /// ## 参数
    /// - `start_vaddr`：起始地址(必须页对齐)
    /// - `len`：长度(不需要页对齐，会自动向上舍入到页边界)
    /// - `madv_flags`：建议标志
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let start_vaddr = VirtAddr::new(Self::start_vaddr(args));
        let len = Self::len(args);
        let madv_flags =
            MadvFlags::from_bits(Self::madv_flags(args) as u64).ok_or(SystemError::EINVAL)?;

        // 长度为 0 时直接返回成功
        if len == 0 {
            return Ok(0);
        }

        // 起始地址必须页对齐
        if !start_vaddr.check_aligned(MMArch::PAGE_SIZE) {
            return Err(SystemError::EINVAL);
        }

        // 检查向上舍入到页边界后是否会溢出
        // page_align_up 的计算方式是 (addr + PAGE_SIZE - 1) & ~(PAGE_SIZE - 1)
        // 我们需要确保 len + PAGE_SIZE - 1 不会溢出
        let page_size = MMArch::PAGE_SIZE;
        if len > usize::MAX - (page_size - 1) {
            // 向上舍入会导致溢出
            return Err(SystemError::EINVAL);
        }

        // 将长度向上舍入到页边界
        let aligned_len = page_align_up(len);

        // 检查地址范围是否会溢出
        // 检查 start + aligned_len 是否溢出
        if start_vaddr.data().checked_add(aligned_len).is_none() {
            return Err(SystemError::EINVAL);
        }

        // 验证地址范围的有效性
        if verify_area(start_vaddr, aligned_len).is_err() {
            return Err(SystemError::EINVAL);
        }

        let current_address_space: Arc<AddressSpace> = AddressSpace::current()?;
        let start_frame = VirtPageFrame::new(start_vaddr);
        let page_count = PageFrameCount::new(aligned_len / MMArch::PAGE_SIZE);

        current_address_space
            .write()
            .madvise(start_frame, page_count, madv_flags)
            .map_err(|_| SystemError::EINVAL)?;
        return Ok(0);
    }

    /// Formats the syscall arguments for display/debugging purposes.
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("start_vaddr", format!("{:#x}", Self::start_vaddr(args))),
            FormattedSyscallParam::new("len", format!("{:#x}", Self::len(args))),
            FormattedSyscallParam::new("madv_flags", format!("{:#x}", Self::madv_flags(args))),
        ]
    }
}

impl SysMadviseHandle {
    /// Extracts the start_vaddr argument from syscall parameters.
    fn start_vaddr(args: &[usize]) -> usize {
        args[0]
    }
    /// Extracts the len argument from syscall parameters.
    fn len(args: &[usize]) -> usize {
        args[1]
    }
    /// Extracts the madv_flags argument from syscall parameters.
    fn madv_flags(args: &[usize]) -> usize {
        args[2]
    }
}

syscall_table_macros::declare_syscall!(SYS_MADVISE, SysMadviseHandle);
