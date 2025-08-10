//! System call handler for the brk system call.

use crate::arch::{interrupt::TrapFrame, syscall::nr::SYS_BRK};
use crate::mm::ucontext::AddressSpace;
use crate::mm::MemoryManagementArch;
use crate::mm::VirtAddr;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use system_error::SystemError;

use alloc::vec::Vec;

/// Handler for the brk system call, which sets the end of the data segment (heap).
pub struct SysBrkHandle;

impl Syscall for SysBrkHandle {
    /// Returns the number of arguments this syscall takes.
    fn num_args(&self) -> usize {
        1
    }

    /// Handles the brk system call.
    ///
    /// # Arguments
    /// * `args` - The syscall arguments, where args[0] is the new end address of the heap.
    ///
    /// # Returns
    /// * On success, returns the new program break (heap end) as usize.
    /// * On failure, returns a SystemError.
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let addr = Self::addr(args);
        let new_addr = VirtAddr::new(addr);
        let address_space = AddressSpace::current()?;
        let mut address_space = address_space.write();

        if new_addr < address_space.brk_start || new_addr >= crate::arch::MMArch::USER_END_VADDR {
            return Ok(address_space.brk.data());
        }
        if new_addr == address_space.brk {
            return Ok(address_space.brk.data());
        }

        unsafe {
            address_space
                .set_brk(VirtAddr::new(crate::libs::align::page_align_up(
                    new_addr.data(),
                )))
                .ok();
            return Ok(address_space.sbrk(0).unwrap().data());
        }
    }

    /// Formats the syscall arguments for display/debugging purposes.
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new(
            "addr",
            format!("{:#x}", Self::addr(args)),
        )]
    }
}

impl SysBrkHandle {
    /// Extracts the address argument from syscall parameters.
    fn addr(args: &[usize]) -> usize {
        args[0]
    }
}

syscall_table_macros::declare_syscall!(SYS_BRK, SysBrkHandle);
