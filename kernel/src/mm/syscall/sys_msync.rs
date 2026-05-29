//! System call handler for the msync system call.

use crate::arch::{interrupt::TrapFrame, syscall::nr::SYS_MSYNC, MMArch};

use crate::mm::{
    syscall::{MsFlags, VmFlags},
    ucontext::AddressSpace,
    MemoryManagementArch, VirtAddr,
};

use crate::syscall::table::{FormattedSyscallParam, Syscall};
use system_error::SystemError;

use alloc::vec::Vec;

/// Handles the msync system call.
pub struct SysMsyncHandle;

impl Syscall for SysMsyncHandle {
    fn num_args(&self) -> usize {
        3
    }
    /// ## msync系统调用
    ///
    /// ## 参数
    ///
    /// - `start`：起始地址（必须对齐到页）
    /// - `len`：长度（会被向上对齐到页边界）
    /// - `flags`：标志
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let start = VirtAddr::new(Self::start_vaddr(args));
        let mut len = Self::len(args);
        let flags = MsFlags::from_bits_truncate(Self::flags(args));

        // 检查 start 地址是否页对齐
        if !start.check_aligned(MMArch::PAGE_SIZE) {
            return Err(SystemError::EINVAL);
        }

        // MS_ASYNC 和 MS_SYNC 不能同时设置
        if flags.contains(MsFlags::MS_ASYNC | MsFlags::MS_SYNC) {
            return Err(SystemError::EINVAL);
        }

        // 将 len 向上对齐到页边界（与 Linux 行为一致）
        len = (len + MMArch::PAGE_SIZE - 1) & !(MMArch::PAGE_SIZE - 1);

        let mut start = start.data();
        let end = start + len;

        // 检查溢出
        if end < start {
            return Err(SystemError::ENOMEM);
        }

        // 如果 len=0（对齐后），直接返回成功（与 Linux 行为一致）
        if start == end {
            return Ok(0);
        }

        let current_address_space = AddressSpace::current()?;
        let mut err = Err(SystemError::ENOMEM);
        let mut unmapped_error = Ok(0);
        let mut next_vma = current_address_space
            .read()
            .mappings
            .find_nearest(VirtAddr::new(start));
        loop {
            if let Some(vma) = next_vma.clone() {
                // 读取VMA信息，确保在调用find_nearest前释放锁
                let (vm_start, vm_end, vm_flags, file, backing_pgoff);
                {
                    let guard = vma.lock();
                    vm_start = guard.region().start().data();
                    vm_end = guard.region().end().data();
                    vm_flags = *guard.vm_flags();
                    file = guard.vm_file();
                    backing_pgoff = guard.backing_page_offset();

                    if start < vm_start {
                        if flags == MsFlags::MS_ASYNC {
                            break;
                        }
                        start = vm_start;
                        if start >= vm_end {
                            break;
                        }
                        unmapped_error = Err(SystemError::ENOMEM);
                    }

                    if flags.contains(MsFlags::MS_INVALIDATE)
                        && vm_flags.contains(VmFlags::VM_LOCKED)
                    {
                        err = Err(SystemError::EBUSY);
                        break;
                    }
                }

                let sync_start = start;
                let sync_end = end.min(vm_end);
                start = vm_end;

                if flags.contains(MsFlags::MS_SYNC) && vm_flags.contains(VmFlags::VM_SHARED) {
                    if let Some(file) = file {
                        if sync_start < sync_end {
                            let file_start = backing_pgoff
                                .unwrap_or(0)
                                .checked_shl(MMArch::PAGE_SHIFT as u32)
                                .and_then(|base| base.checked_add(sync_start - vm_start))
                                .ok_or(SystemError::EINVAL)?;
                            let file_end = file_start
                                .checked_add(sync_end - sync_start - 1)
                                .ok_or(SystemError::EINVAL)?;
                            err = file
                                .sync_range_and_check_wb_error(file_start, file_end, true)
                                .map(|_| 0);
                            if err.is_err() {
                                break;
                            }
                        }
                    }
                }

                if start >= end {
                    err = unmapped_error;
                    break;
                }
                next_vma = current_address_space
                    .read()
                    .mappings
                    .find_nearest(VirtAddr::new(start));
            } else {
                return Err(SystemError::ENOMEM);
            }
        }
        return err;
    }

    /// Formats the syscall arguments for display/debugging purposes.
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("start_vaddr", format!("{:#x}", Self::start_vaddr(args))),
            FormattedSyscallParam::new("len", format!("{:#x}", Self::len(args))),
            FormattedSyscallParam::new("flags", format!("{:#x}", Self::flags(args))),
        ]
    }
}

impl SysMsyncHandle {
    /// Extracts the start_vaddr argument from syscall parameters.
    fn start_vaddr(args: &[usize]) -> usize {
        args[0]
    }
    /// Extracts the len argument from syscall parameters.
    fn len(args: &[usize]) -> usize {
        args[1]
    }
    /// Extracts the flags argument from syscall parameters.
    fn flags(args: &[usize]) -> usize {
        args[2]
    }
}

syscall_table_macros::declare_syscall!(SYS_MSYNC, SysMsyncHandle);
