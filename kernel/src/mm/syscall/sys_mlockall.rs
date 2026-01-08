//! mlockall 系统调用实现

use crate::arch::{interrupt::TrapFrame, syscall::nr::SYS_MLOCKALL, MMArch};
use crate::mm::{mlock::can_do_mlock, syscall::MlockAllFlags, ucontext::AddressSpace};
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysMlockallHandle;

impl Syscall for SysMlockallHandle {
    fn num_args(&self) -> usize {
        1
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let flags = MlockAllFlags::from_bits(args[0] as u32).ok_or(SystemError::EINVAL)?;

        // 检查标志位组合合法性
        // MCL_ONFAULT 必须与 MCL_CURRENT 或 MCL_FUTURE 一起使用
        if flags.contains(MlockAllFlags::MCL_ONFAULT)
            && !flags.intersects(MlockAllFlags::MCL_CURRENT | MlockAllFlags::MCL_FUTURE)
        {
            return Err(SystemError::EINVAL);
        }

        // 权限检查
        if !can_do_mlock() {
            return Err(SystemError::EPERM);
        }

        let addr_space = AddressSpace::current()?;
        addr_space.write().mlockall(args[0] as u32)?;

        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new(
            "flags",
            format!("{:#x}", args[0]),
        )]
    }
}

syscall_table_macros::declare_syscall!(SYS_MLOCKALL, SysMlockallHandle);
