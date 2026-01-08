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

        let aligned_len = page_align_up(len);
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
        addr_space.write().munlock(addr, aligned_len)?;

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
