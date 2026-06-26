use crate::arch::interrupt::TrapFrame;
use crate::syscall::table::FormattedSyscallParam;
use crate::{
    arch::syscall::nr::SYS_SHMDT,
    mm::{ucontext::AddressSpace, VirtAddr},
    syscall::table::Syscall,
};
use alloc::vec::Vec;
use syscall_table_macros::declare_syscall;
use system_error::SystemError;
pub struct SysShmdtHandle;

impl SysShmdtHandle {
    #[inline(always)]
    fn vaddr(args: &[usize]) -> VirtAddr {
        VirtAddr::new(args[0])
    }
}

impl Syscall for SysShmdtHandle {
    fn num_args(&self) -> usize {
        1
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new(
            "vaddr",
            format!("{}", Self::vaddr(args).data()),
        )]
    }
    /// # SYS_SHMDT系统调用函数，用于取消对共享内存的连接
    ///
    /// ## 参数
    ///
    /// - `vaddr`:  需要取消映射的虚拟内存区域起始地址
    ///
    /// ## 返回值
    ///
    /// 成功：0
    /// 失败：错误码
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let vaddr = Self::vaddr(args);
        let current_address_space = AddressSpace::current()?;
        current_address_space.detach_sysv_shm_wait(vaddr)?;
        Ok(0)
    }
}

declare_syscall!(SYS_SHMDT, SysShmdtHandle);
