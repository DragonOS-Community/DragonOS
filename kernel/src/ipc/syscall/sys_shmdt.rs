use crate::mm::page::PageFlushAll;
use crate::syscall::table::FormattedSyscallParam;
use crate::{
    arch::syscall::nr::SYS_SHMDT,
    arch::MMArch,
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
    fn handle(&self, args: &[usize], _from_user: bool) -> Result<usize, SystemError> {
        let vaddr = Self::vaddr(args);
        let current_address_space = AddressSpace::current()?;
        let mut address_write_guard = current_address_space.write();

        // 获取vma
        let vma = address_write_guard
            .mappings
            .contains(vaddr)
            .ok_or(SystemError::EINVAL)?;

        // 判断vaddr是否为起始地址
        if vma.lock_irqsave().region().start() != vaddr {
            return Err(SystemError::EINVAL);
        }

        // 取消映射
        let flusher: PageFlushAll<MMArch> = PageFlushAll::new();
        vma.unmap(&mut address_write_guard.user_mapper.utable, flusher);

        return Ok(0);
    }
}

declare_syscall!(SYS_SHMDT, SysShmdtHandle);
