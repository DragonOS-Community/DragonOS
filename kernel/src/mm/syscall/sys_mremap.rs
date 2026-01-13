//! System call handler for the mremap system call.

use crate::arch::{interrupt::TrapFrame, syscall::nr::SYS_MREMAP};
use crate::mm::syscall::page_align_up;
use crate::mm::syscall::sys_munmap::do_munmap;
use crate::mm::syscall::MremapFlags;
use crate::mm::ucontext::AddressSpace;
use crate::mm::MemoryManagementArch;
use crate::mm::{MMArch, VirtAddr, VmFlags};
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use system_error::SystemError;

use alloc::vec::Vec;
/// Handles the mremap system call.
pub struct SysMremapHandle;

impl Syscall for SysMremapHandle {
    /// Returns the number of arguments this syscall takes.
    fn num_args(&self) -> usize {
        5
    }

    /// ## mremap系统调用
    ///
    ///
    /// ## 参数
    ///
    /// - `old_vaddr`：原映射的起始地址
    /// - `old_len`：原映射的长度
    /// - `new_len`：重新映射的长度
    /// - `mremap_flags`：重映射标志
    /// - `new_vaddr`：重新映射的起始地址
    ///
    /// ## 返回值
    ///
    /// 成功时返回重映射的起始地址，失败时返回错误码
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let old_vaddr = VirtAddr::new(Self::old_vaddr(args));
        let old_len = Self::old_len(args);
        let new_len = Self::new_len(args);
        let mremap_flags = MremapFlags::from_bits_truncate(Self::mremap_flags(args) as u8);
        let new_vaddr = VirtAddr::new(Self::new_vaddr(args));

        // 需要重映射到新内存区域的情况下，必须包含MREMAP_MAYMOVE并且指定新地址
        if mremap_flags.contains(MremapFlags::MREMAP_FIXED)
            && (!mremap_flags.contains(MremapFlags::MREMAP_MAYMOVE)
                || new_vaddr == VirtAddr::new(0))
        {
            return Err(SystemError::EINVAL);
        }

        // 不取消旧映射的情况下，必须包含MREMAP_MAYMOVE并且新内存大小等于旧内存大小
        if mremap_flags.contains(MremapFlags::MREMAP_DONTUNMAP)
            && (!mremap_flags.contains(MremapFlags::MREMAP_MAYMOVE) || old_len != new_len)
        {
            return Err(SystemError::EINVAL);
        }

        // 旧内存地址必须对齐
        if !old_vaddr.check_aligned(MMArch::PAGE_SIZE) {
            return Err(SystemError::EINVAL);
        }

        // 将old_len、new_len 对齐页面大小
        let old_len = page_align_up(old_len);
        let new_len = page_align_up(new_len);

        // 不允许重映射内存区域大小为0
        if new_len == 0 {
            return Err(SystemError::EINVAL);
        }

        let current_address_space = AddressSpace::current()?;
        let vma = current_address_space.read().mappings.contains(old_vaddr);
        if vma.is_none() {
            return Err(SystemError::EINVAL);
        }
        let vma = vma.unwrap();
        let (vm_flags, vma_region) = {
            let g = vma.lock();
            (*g.vm_flags(), *g.region())
        };

        // Linux vma_to_resize() semantics:
        // With MREMAP_FIXED, the *source span being remapped* must be within a single VMA.
        // - For shrinking, Linux unmaps the tail first and then checks the shrunken length.
        // - For expansion, the check is against old_len (not new_len), otherwise all fixed
        //   expansions would spuriously fail.
        if mremap_flags.contains(MremapFlags::MREMAP_FIXED) {
            let span_len = if old_len > new_len { new_len } else { old_len };
            let span_end = old_vaddr
                .data()
                .checked_add(span_len)
                .ok_or(SystemError::EINVAL)?;
            if span_end > vma_region.end().data() {
                // Side effects like Linux mremap_to():
                // - unmap destination range first
                // - if shrinking, unmap the tail [old+new_len, old+old_len)
                do_munmap(new_vaddr, new_len)?;
                if old_len > new_len {
                    do_munmap(old_vaddr + new_len, old_len - new_len)?;
                }
                return Err(SystemError::EFAULT);
            }
        }

        // 暂时不支持巨页映射
        if vm_flags.contains(VmFlags::VM_HUGETLB) {
            log::error!("mmap: not support huge page mapping");
            return Err(SystemError::ENOSYS);
        }

        // Linux semantics:
        // - Without MREMAP_FIXED, shrinking is always in-place (just unmap the tail).
        // - With MREMAP_FIXED, shrinking still needs to move the mapping to the destination.
        if old_len > new_len && !mremap_flags.contains(MremapFlags::MREMAP_FIXED) {
            do_munmap(old_vaddr + new_len, old_len - new_len)?;
            return Ok(old_vaddr.data());
        }

        // No-op when size doesn't change and we are not explicitly moving/duplicating.
        if old_len == new_len
            && !mremap_flags.contains(MremapFlags::MREMAP_FIXED)
            && !mremap_flags.contains(MremapFlags::MREMAP_DONTUNMAP)
        {
            return Ok(old_vaddr.data());
        }

        // 重映射到新内存区域
        let r = current_address_space.write().mremap(
            old_vaddr,
            old_len,
            new_len,
            mremap_flags,
            new_vaddr,
            vm_flags,
        )?;

        // Unmap the old mapping only if this was a move (i.e. result differs from old_vaddr).
        // - old_len==0 is a special duplication request and must never unmap the source.
        // - DONTUNMAP keeps the source by definition.
        if !mremap_flags.contains(MremapFlags::MREMAP_DONTUNMAP) && old_len != 0 && r != old_vaddr {
            do_munmap(old_vaddr, old_len)?;
        }

        return Ok(r.data());
    }

    /// Formats the syscall arguments for display/debugging purposes.
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("old_vaddr", format!("{:#x}", Self::old_vaddr(args))),
            FormattedSyscallParam::new("old_len", format!("{:#x}", Self::old_len(args))),
            FormattedSyscallParam::new("new_len", format!("{:#x}", Self::new_len(args))),
            FormattedSyscallParam::new("mremap_flags", format!("{:#x}", Self::mremap_flags(args))),
            FormattedSyscallParam::new("new_vaddr", format!("{:#x}", Self::new_vaddr(args))),
        ]
    }
}

impl SysMremapHandle {
    /// Extracts the old_vaddr argument from syscall parameters.
    fn old_vaddr(args: &[usize]) -> usize {
        args[0]
    }
    /// Extracts the old_len argument from syscall parameters.
    fn old_len(args: &[usize]) -> usize {
        args[1]
    }
    /// Extracts the new_len argument from syscall parameters.
    fn new_len(args: &[usize]) -> usize {
        args[2]
    }
    /// Extracts the mremap_flags argument from syscall parameters.
    fn mremap_flags(args: &[usize]) -> usize {
        args[3]
    }
    /// Extracts the new_vaddr argument from syscall parameters.
    fn new_vaddr(args: &[usize]) -> usize {
        args[4]
    }
}

syscall_table_macros::declare_syscall!(SYS_MREMAP, SysMremapHandle);
