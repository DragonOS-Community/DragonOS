//! System call handler for the munmap system call.

use crate::arch::{interrupt::TrapFrame, syscall::nr::SYS_MUNMAP};
use crate::mm::MemoryManagementArch;
use crate::mm::syscall::PageFrameCount;
use crate::mm::syscall::check_aligned;
use crate::mm::syscall::page_align_up;
use crate::mm::ucontext::AddressSpace;
use crate::mm::unlikely;
use crate::mm::{MMArch, VirtAddr, VirtPageFrame, verify_area};
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use alloc::sync::Arc;
use alloc::vec::Vec;
use system_error::SystemError;

/// ## munmap系统调用
///
/// ## 参数
///
/// - `start_vaddr`：取消映射的起始地址（已经对齐到页）
/// - `len`：取消映射的字节数(已经对齐到页)
///
/// ## 返回值
///
/// 成功时返回0，失败时返回错误码
pub struct SysMunmapHandle;

impl Syscall for SysMunmapHandle {
    /// Returns the number of arguments this syscall takes.
    fn num_args(&self) -> usize {
        2
    }

    /// Handles the munmap system call.
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let addr = args[0];
        let len = page_align_up(args[1]);
        if addr & (MMArch::PAGE_SIZE - 1) != 0 {
            // The addr argument is not a multiple of the page size
            return Err(SystemError::EINVAL);
        } else {
            do_munmap(VirtAddr::new(addr), len)
        }
    }

    /// Formats the syscall arguments for display/debugging purposes.
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("start_vaddr", format!("{:#x}", Self::start_vaddr(args))),
            FormattedSyscallParam::new("len", format!("{:#x}", Self::len(args))),
        ]
    }
}

impl SysMunmapHandle {
    /// Extracts the start virtual address argument from syscall parameters.
    fn start_vaddr(args: &[usize]) -> usize {
        args[0]
    }
    /// Extracts the length argument from syscall parameters.
    fn len(args: &[usize]) -> usize {
        args[1]
    }
}

syscall_table_macros::declare_syscall!(SYS_MUNMAP, SysMunmapHandle);

pub(super) fn do_munmap(start_vaddr: VirtAddr, len: usize) -> Result<usize, SystemError> {
    assert!(start_vaddr.check_aligned(MMArch::PAGE_SIZE));
    assert!(check_aligned(len, MMArch::PAGE_SIZE));

    if unlikely(verify_area(start_vaddr, len).is_err()) {
        return Err(SystemError::EINVAL);
    }
    if unlikely(len == 0) {
        return Err(SystemError::EINVAL);
    }

    let current_address_space: Arc<AddressSpace> = AddressSpace::current()?;
    let start_frame = VirtPageFrame::new(start_vaddr);
    let page_count = PageFrameCount::new(len / MMArch::PAGE_SIZE);

    current_address_space
        .write()
        .munmap(start_frame, page_count)
        .map_err(|_| SystemError::EINVAL)?;

    return Ok(0);
}
