//! System call handler for the mremap system call.

use crate::arch::{interrupt::TrapFrame, syscall::nr::SYS_MREMAP};
use crate::mm::syscall::sys_munmap::do_munmap;
use crate::mm::syscall::MremapFlags;
use crate::mm::ucontext::AddressSpace;
use crate::mm::MemoryManagementArch;
use crate::mm::{MMArch, VirtAddr, VirtRegion, VmFlags};
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
        let mremap_flags_raw = Self::mremap_flags(args);
        let allowed_mremap_flags = (MremapFlags::MREMAP_MAYMOVE
            | MremapFlags::MREMAP_FIXED
            | MremapFlags::MREMAP_DONTUNMAP)
            .bits() as usize;
        if mremap_flags_raw & !allowed_mremap_flags != 0 {
            return Err(SystemError::EINVAL);
        }
        let mremap_flags = MremapFlags::from_bits(mremap_flags_raw as u8).unwrap();
        let new_vaddr = VirtAddr::new(Self::new_vaddr(args));

        // 需要重映射到新内存区域的情况下，必须包含MREMAP_MAYMOVE并且指定新地址
        if mremap_flags.contains(MremapFlags::MREMAP_FIXED)
            && !mremap_flags.contains(MremapFlags::MREMAP_MAYMOVE)
        {
            return Err(SystemError::EINVAL);
        }

        // 不取消旧映射的情况下，必须包含MREMAP_MAYMOVE并且新内存大小等于旧内存大小
        if mremap_flags.contains(MremapFlags::MREMAP_DONTUNMAP)
            && (!mremap_flags.contains(MremapFlags::MREMAP_MAYMOVE) || old_len != new_len)
        {
            return Err(SystemError::EINVAL);
        }
        if mremap_flags.contains(MremapFlags::MREMAP_DONTUNMAP)
            && !new_vaddr.check_aligned(MMArch::PAGE_SIZE)
        {
            return Err(SystemError::EINVAL);
        }

        // 旧内存地址必须对齐
        if !old_vaddr.check_aligned(MMArch::PAGE_SIZE) {
            return Err(SystemError::EINVAL);
        }

        // Linux PAGE_ALIGN 使用 unsigned wrap 语义；极大长度可能 wrap 到 0，
        // 并继续进入 mremap 的 legacy old_len==0 duplicate 分支。这里显式
        // wrapping，避免 Rust debug overflow panic，同时保持 Linux 兼容。
        let old_len = wrapping_page_align_up(old_len);
        let new_len = wrapping_page_align_up(new_len);

        // 不允许重映射内存区域大小为0
        if new_len == 0 {
            return Err(SystemError::EINVAL);
        }

        let current_address_space = AddressSpace::current()?;
        let vma = loop {
            let guard = current_address_space.read();
            if let Some(vma) = guard.mappings.contains(old_vaddr) {
                break vma;
            }
            let probe_region = VirtRegion::new(old_vaddr, MMArch::PAGE_SIZE);
            if guard
                .mappings
                .first_reservation_conflict(probe_region)
                .is_some()
            {
                drop(guard);
                current_address_space.wait_for_no_reservation_conflict(probe_region);
                continue;
            }
            return Err(SystemError::EFAULT);
        };
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
            validate_fixed_target(old_vaddr, old_len, new_vaddr, new_len)?;
            let span_len = if old_len > new_len { new_len } else { old_len };
            let span_end = old_vaddr
                .data()
                .checked_add(span_len)
                .ok_or(SystemError::EINVAL)?;
            if span_end > vma_region.end().data() {
                // Match Linux mremap_to() ordering: MREMAP_FIXED unmaps the
                // destination and, when shrinking, the source tail before the
                // resized source span is rejected by vma_to_resize().
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
        let r = current_address_space.mremap_wait(
            old_vaddr,
            old_len,
            new_len,
            mremap_flags,
            new_vaddr,
            vm_flags,
        )?;

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

fn wrapping_page_align_up(len: usize) -> usize {
    let mask = MMArch::PAGE_SIZE - 1;
    len.wrapping_add(mask) & !mask
}

fn validate_fixed_target(
    old_vaddr: VirtAddr,
    old_len: usize,
    new_vaddr: VirtAddr,
    new_len: usize,
) -> Result<(), SystemError> {
    if !new_vaddr.check_aligned(MMArch::PAGE_SIZE) {
        return Err(SystemError::EINVAL);
    }
    let old_end = old_vaddr.data().wrapping_add(old_len);
    let new_end = new_vaddr
        .data()
        .checked_add(new_len)
        .ok_or(SystemError::EINVAL)?;
    if new_end > MMArch::USER_END_VADDR.data() {
        return Err(SystemError::EINVAL);
    }
    if old_end > new_vaddr.data() && new_end > old_vaddr.data() {
        return Err(SystemError::EINVAL);
    }

    Ok(())
}

syscall_table_macros::declare_syscall!(SYS_MREMAP, SysMremapHandle);
