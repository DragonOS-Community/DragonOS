//! System call handler for the mmap system call.

use super::ProtFlags;
use crate::arch::{interrupt::TrapFrame, syscall::nr::SYS_MMAP, MMArch};
use crate::mm::syscall::page_align_up;
use crate::mm::syscall::MapFlags;
use crate::mm::ucontext::DEFAULT_MMAP_MIN_ADDR;
use crate::mm::AddressSpace;
use crate::mm::VirtAddr;
use crate::mm::{verify_area, MemoryManagementArch};
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use log::error;
use system_error::SystemError;

use crate::process::{resource::RLimitID, ProcessManager};
use alloc::vec::Vec;

/// Handler for the mmap system call, which maps files or devices into memory.
pub struct SysMmapHandle;

impl Syscall for SysMmapHandle {
    /// Returns the number of arguments this syscall takes.
    fn num_args(&self) -> usize {
        6
    }

    /// ## mmap系统调用
    ///
    /// 该函数的实现参考了Linux内核的实现，但是并不完全相同。因为有些功能咱们还没实现
    ///
    /// ## 参数
    ///
    /// - `start_vaddr`：映射的起始地址
    /// - `len`：映射的长度
    /// - `prot`：保护标志
    /// - `flags`：映射标志
    /// - `fd`：文件描述符（暂时不支持）
    /// - `offset`：文件偏移量 （暂时不支持）
    ///
    /// ## 返回值
    ///
    /// 成功时返回映射的起始地址，失败时返回错误码
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let start_vaddr = VirtAddr::new(Self::start_vaddr(args));
        let len_raw = Self::len(args);
        if len_raw == 0 {
            return Err(SystemError::EINVAL);
        }
        let len = page_align_up(len_raw);
        if len == 0 || len < len_raw {
            return Err(SystemError::ENOMEM);
        }
        let prot_flags = Self::prot(args);
        let map_flags = Self::flags(args);
        let fd = Self::fd(args);
        let offset = Self::offset(args);
        // 基础参数校验
        if verify_area(start_vaddr, len).is_err() {
            return Err(SystemError::EFAULT);
        }

        let map_flags = MapFlags::from_bits_truncate(map_flags as u64);
        let prot_flags = ProtFlags::from_bits_truncate(prot_flags as u64);

        // 仅允许 MAP_PRIVATE 或 MAP_SHARED 之一
        let has_private = map_flags.contains(MapFlags::MAP_PRIVATE);
        let has_shared = map_flags.contains(MapFlags::MAP_SHARED);
        if has_private == has_shared {
            return Err(SystemError::EINVAL);
        }

        // RLIMIT_AS 检查（粗略：累计 VMA 大小）
        let rlim_as = ProcessManager::current_pcb()
            .get_rlimit(RLimitID::As)
            .rlim_cur as usize;
        if rlim_as != usize::MAX {
            let vm = AddressSpace::current()?;
            let usage = vm.read().vma_usage_bytes();
            if usage.checked_add(len).is_none_or(|v| v > rlim_as) {
                return Err(SystemError::ENOMEM);
            }
        }

        // MAP_FIXED 需页对齐
        if map_flags.contains(MapFlags::MAP_FIXED)
            && !start_vaddr.check_aligned(<MMArch as MemoryManagementArch>::PAGE_SIZE)
        {
            return Err(SystemError::EINVAL);
        }

        if start_vaddr < VirtAddr::new(DEFAULT_MMAP_MIN_ADDR)
            && map_flags.contains(MapFlags::MAP_FIXED)
        {
            error!(
                "mmap: MAP_FIXED is not supported for address below {}",
                DEFAULT_MMAP_MIN_ADDR
            );
            return Err(SystemError::EINVAL);
        }

        // 暂时不支持巨页映射
        if map_flags.contains(MapFlags::MAP_HUGETLB) {
            error!("mmap: not support huge page mapping");
            return Err(SystemError::ENOSYS);
        }
        let current_address_space = AddressSpace::current()?;
        let start_page = if map_flags.contains(MapFlags::MAP_ANONYMOUS) {
            // 匿名映射
            current_address_space.write().map_anonymous(
                start_vaddr,
                len,
                prot_flags,
                map_flags,
                true,
                false,
            )?
        } else {
            // 文件映射
            current_address_space.write().file_mapping(
                start_vaddr,
                len,
                prot_flags,
                map_flags,
                fd,
                offset,
                true,
                false,
            )?
        };
        return Ok(start_page.virt_address().data());
    }

    /// Formats the syscall arguments for display/debugging purposes.
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("start_vaddr", format!("{:#x}", Self::start_vaddr(args))),
            FormattedSyscallParam::new("len", format!("{:#x}", Self::len(args))),
            FormattedSyscallParam::new("prot", format!("{:#x}", Self::prot(args))),
            FormattedSyscallParam::new("flags", format!("{:#x}", Self::flags(args))),
            FormattedSyscallParam::new("fd", format!("{}", Self::fd(args))),
            FormattedSyscallParam::new("offset", format!("{:#x}", Self::offset(args))),
        ]
    }
}

impl SysMmapHandle {
    /// Extracts the start virtual address argument from syscall parameters.
    fn start_vaddr(args: &[usize]) -> usize {
        args[0]
    }
    /// Extracts the length argument from syscall parameters.
    fn len(args: &[usize]) -> usize {
        args[1]
    }
    /// Extracts the protection flags argument from syscall parameters.
    fn prot(args: &[usize]) -> usize {
        args[2]
    }
    /// Extracts the mapping flags argument from syscall parameters.
    fn flags(args: &[usize]) -> usize {
        args[3]
    }
    /// Extracts the file descriptor argument from syscall parameters.
    fn fd(args: &[usize]) -> i32 {
        args[4] as i32
    }
    /// Extracts the file offset argument from syscall parameters.
    fn offset(args: &[usize]) -> usize {
        args[5]
    }
}
syscall_table_macros::declare_syscall!(SYS_MMAP, SysMmapHandle);
