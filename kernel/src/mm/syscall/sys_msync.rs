//! System call handler for the msync system call.

use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_MSYNC, MMArch},
    driver::base::block::SeekFrom,
};

use crate::mm::{
    syscall::{check_aligned, MsFlags, VmFlags},
    ucontext::AddressSpace,
    unlikely, verify_area, MemoryManagementArch, VirtAddr,
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
    /// - `start`：起始地址(已经对齐到页)
    /// - `len`：长度(已经对齐到页)
    /// - `flags`：标志
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let start = VirtAddr::new(Self::start_vaddr(args));
        let len = Self::len(args);
        let flags = MsFlags::from_bits_truncate(Self::flags(args));

        if !start.check_aligned(MMArch::PAGE_SIZE) || !check_aligned(len, MMArch::PAGE_SIZE) {
            return Err(SystemError::EINVAL);
        }
        if unlikely(verify_area(start, len).is_err()) {
            return Err(SystemError::EINVAL);
        }
        if unlikely(len == 0) {
            return Err(SystemError::EINVAL);
        }

        let mut start = start.data();
        let end = start + len;
        let mut unmapped_error = Ok(0);

        if !flags.intersects(MsFlags::MS_ASYNC | MsFlags::MS_INVALIDATE | MsFlags::MS_SYNC) {
            return Err(SystemError::EINVAL);
        }
        if flags.contains(MsFlags::MS_ASYNC | MsFlags::MS_SYNC) {
            return Err(SystemError::EINVAL);
        }
        if end < start {
            return Err(SystemError::ENOMEM);
        }
        if start == end {
            return Ok(0);
        }

        let current_address_space = AddressSpace::current()?;
        let mut err = Err(SystemError::ENOMEM);
        let mut next_vma = current_address_space
            .read()
            .mappings
            .find_nearest(VirtAddr::new(start));
        loop {
            if let Some(vma) = next_vma.clone() {
                // 读取VMA信息，确保在调用find_nearest前释放锁
                let (vm_start, vm_end, vm_flags, file, file_pgoff);
                {
                    let guard = vma.lock_irqsave();
                    vm_start = guard.region().start().data();
                    vm_end = guard.region().end().data();
                    vm_flags = *guard.vm_flags();
                    file = guard.vm_file();
                    file_pgoff = guard.file_page_offset();

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

                let fstart = (start - vm_start) + (file_pgoff.unwrap_or(0) << MMArch::PAGE_SHIFT);
                let fend = fstart + (core::cmp::min(end, vm_end) - start) - 1;
                let old_start = start;
                start = vm_end;

                if flags.contains(MsFlags::MS_SYNC) && vm_flags.contains(VmFlags::VM_SHARED) {
                    if let Some(file) = file {
                        let old_pos = file.lseek(SeekFrom::SeekCurrent(0)).unwrap();
                        file.lseek(SeekFrom::SeekSet(fstart as i64)).unwrap();
                        err = file.write(len, unsafe {
                            core::slice::from_raw_parts(old_start as *mut u8, fend - fstart + 1)
                        });
                        file.lseek(SeekFrom::SeekSet(old_pos as i64)).unwrap();
                        if err.is_err() {
                            break;
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
