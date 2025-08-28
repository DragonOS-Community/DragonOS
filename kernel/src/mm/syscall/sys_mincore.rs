use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_MINCORE;
use crate::arch::MMArch;
use crate::libs::align::page_align_up;
use crate::mm::allocator::page_frame::{PageFrameCount, VirtPageFrame};
use crate::mm::ucontext::AddressSpace;
use crate::mm::{verify_area, MemoryManagementArch};
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::UserBufferWriter;
use system_error::SystemError;

use crate::mm::VirtAddr;
use alloc::vec::Vec;

pub struct SysMincoreHandle;

impl Syscall for SysMincoreHandle {
    fn num_args(&self) -> usize {
        3
    }

    /// ## mincore系统调用
    ///
    /// ## 参数
    ///
    /// - `start_vaddr`：起始地址(已经对齐到页)
    /// - `len`：需要遍历的长度
    /// - `vec`：用户空间的vec指针
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let start_vaddr = VirtAddr::new(Self::start_vaddr(args));
        let len = Self::len(args);
        let vec = Self::vec(args);
        assert!(start_vaddr.check_aligned(MMArch::PAGE_SIZE));

        if verify_area(start_vaddr, len).is_err() {
            return Err(SystemError::ENOMEM);
        }
        if len == 0 {
            return Err(SystemError::EINVAL);
        }
        let len = page_align_up(len);
        let current_address_space = AddressSpace::current()?;
        let start_frame = VirtPageFrame::new(start_vaddr);
        let page_count = len >> MMArch::PAGE_SHIFT;

        let mut writer = UserBufferWriter::new(vec as *mut u8, page_count, true)?;
        let mut buf: &mut [u8] = writer.buffer(0).unwrap();
        let page_count = PageFrameCount::new(page_count);
        current_address_space
            .write()
            .mincore(start_frame, page_count, &mut buf)?;
        return Ok(0);
    }

    /// Formats the syscall arguments for display/debugging purposes.
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("start_vaddr", format!("{:#x}", Self::start_vaddr(args))),
            FormattedSyscallParam::new("len", format!("{:#x}", Self::len(args))),
            FormattedSyscallParam::new("vec", format!("{:#x}", Self::vec(args))),
        ]
    }
}

impl SysMincoreHandle {
    /// Extracts the start_vaddr argument from syscall parameters.
    fn start_vaddr(args: &[usize]) -> usize {
        args[0]
    }
    /// Extracts the len argument from syscall parameters.
    fn len(args: &[usize]) -> usize {
        args[1]
    }
    /// Extracts the
    fn vec(args: &[usize]) -> usize {
        args[2]
    }
}

syscall_table_macros::declare_syscall!(SYS_MINCORE, SysMincoreHandle);
