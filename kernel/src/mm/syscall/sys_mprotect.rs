//! System call handler for the mprotect system call.

use crate::arch::{interrupt::TrapFrame, syscall::nr::SYS_MPROTECT, MMArch};
use crate::mm::{
    syscall::{check_aligned, PageFrameCount, ProtFlags},
    ucontext::AddressSpace,
    MemoryManagementArch, VirtPageFrame, {verify_area, VirtAddr},
};

use crate::syscall::table::{FormattedSyscallParam, Syscall};
use system_error::SystemError;

use alloc::sync::Arc;
use alloc::vec::Vec;

/// Handles the mprotect system call.
pub struct SysMprotectHandle;

impl Syscall for SysMprotectHandle {
    fn num_args(&self) -> usize {
        3
    }

    /// ## mprotect系统调用
    ///
    /// ## 参数
    ///
    /// - `start_vaddr`：起始地址(已经对齐到页)
    /// - `len`：长度(已经对齐到页)
    /// - `prot_flags`：保护标志
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let start_vaddr = VirtAddr::new(Self::start_vaddr(args));
        let len = Self::len(args);
        let prot_flags =
            ProtFlags::from_bits(Self::prot_flags(args) as u64).ok_or(SystemError::EINVAL)?;

        if !start_vaddr.check_aligned(MMArch::PAGE_SIZE) || !check_aligned(len, MMArch::PAGE_SIZE) {
            return Err(SystemError::EINVAL);
        }

        if verify_area(start_vaddr, len).is_err() {
            return Err(SystemError::EINVAL);
        }
        if len == 0 {
            return Err(SystemError::EINVAL);
        }

        let current_address_space: Arc<AddressSpace> = AddressSpace::current()?;
        let start_frame = VirtPageFrame::new(start_vaddr);
        let page_count = PageFrameCount::new(len / MMArch::PAGE_SIZE);

        current_address_space
            .write()
            .mprotect(start_frame, page_count, prot_flags)
            .map_err(|_| SystemError::EINVAL)?;
        return Ok(0);
    }

    /// Formats the syscall arguments for display/debugging purposes.
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("start_vaddr", format!("{:#x}", Self::start_vaddr(args))),
            FormattedSyscallParam::new("len", format!("{:#x}", Self::len(args))),
            FormattedSyscallParam::new("prot_flags", format!("{:#x}", Self::prot_flags(args))),
        ]
    }
}

impl SysMprotectHandle {
    /// Extracts the start_vaddr argument from syscall parameters.
    fn start_vaddr(args: &[usize]) -> usize {
        args[0]
    }
    /// Extracts the len argument from syscall parameters.
    fn len(args: &[usize]) -> usize {
        args[1]
    }
    /// Extracts the prot_flags argument from syscall parameters.
    fn prot_flags(args: &[usize]) -> usize {
        args[2]
    }
}

syscall_table_macros::declare_syscall!(SYS_MPROTECT, SysMprotectHandle);
