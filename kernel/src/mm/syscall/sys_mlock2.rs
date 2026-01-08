//! mlock2 系统调用实现

use crate::arch::{interrupt::TrapFrame, syscall::nr::SYS_MLOCK2, MMArch};
use crate::mm::{
    mlock::can_do_mlock,
    syscall::{page_align_up, Mlock2Flags},
    ucontext::AddressSpace,
    MemoryManagementArch, VirtAddr,
};
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use alloc::vec::Vec;
use system_error::SystemError;
pub struct SysMlock2Handle;

impl Syscall for SysMlock2Handle {
    fn num_args(&self) -> usize {
        3
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let addr = VirtAddr::new(args[0]);
        let len = args[1];
        let flags = Mlock2Flags::from_bits(args[2] as u32).ok_or(SystemError::EINVAL)?;

        // 参数校验
        if len == 0 {
            return Ok(0);
        }

        // 检查标志位合法性
        if !flags.is_empty() && !flags.contains(Mlock2Flags::MLOCK_ONFAULT) {
            return Err(SystemError::EINVAL);
        }

        let aligned_len = page_align_up(len);
        if aligned_len == 0 || aligned_len < len {
            return Err(SystemError::ENOMEM);
        }

        // 地址范围检查
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

        // 执行 mlock2（支持 MLOCK_ONFAULT）
        let onfault = flags.contains(Mlock2Flags::MLOCK_ONFAULT);
        addr_space.write().mlock(addr, aligned_len, onfault)?;

        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("addr", format!("{:#x}", args[0])),
            FormattedSyscallParam::new("len", format!("{:#x}", args[1])),
            FormattedSyscallParam::new("flags", format!("{:#x}", args[2])),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_MLOCK2, SysMlock2Handle);
