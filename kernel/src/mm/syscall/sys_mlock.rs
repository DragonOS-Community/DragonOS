//! mlock 系统调用实现

use crate::arch::{interrupt::TrapFrame, MMArch,syscall::nr::SYS_MLOCK};
use alloc::vec::Vec;
use crate::mm::{
    mlock::can_do_mlock,
    syscall::page_align_up,
    ucontext::AddressSpace,
    MemoryManagementArch, VirtAddr,
};
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use system_error::SystemError;

pub struct SysMlockHandle;

impl Syscall for SysMlockHandle {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let addr = VirtAddr::new(args[0]);
        let len = args[1];

        // 基本参数校验
        if len == 0 {
            return Ok(0);
        }

        // 长度对齐并检查溢出
        let aligned_len = page_align_up(len);
        if aligned_len == 0 || aligned_len < len {
            return Err(SystemError::ENOMEM);
        }

        // 检查地址范围
        let end = addr
            .data()
            .checked_add(aligned_len)
            .ok_or(SystemError::ENOMEM)?;
        if end > MMArch::USER_END_VADDR.data() {
            return Err(SystemError::ENOMEM);
        }

        // 权限检查
        if !can_do_mlock() {
            return Err(SystemError::EPERM);
        }

        // 获取当前地址空间
        let addr_space = AddressSpace::current()?;

        // 执行 mlock
        addr_space.write().mlock(addr, aligned_len, false)?;

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
