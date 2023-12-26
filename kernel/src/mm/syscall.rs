use core::intrinsics::unlikely;

use alloc::sync::Arc;
use system_error::SystemError;

use crate::{
    arch::MMArch,
    kerror,
    libs::align::{check_aligned, page_align_up},
    mm::MemoryManagementArch,
    syscall::Syscall,
};

use super::{
    allocator::page_frame::{PageFrameCount, VirtPageFrame},
    ucontext::{AddressSpace, DEFAULT_MMAP_MIN_ADDR},
    verify_area, VirtAddr,
};

bitflags! {
    /// Memory protection flags
    pub struct ProtFlags: u64 {
        const PROT_NONE = 0x0;
        const PROT_READ = 0x1;
        const PROT_WRITE = 0x2;
        const PROT_EXEC = 0x4;
    }

    /// Memory mapping flags
    pub struct MapFlags: u64 {
        const MAP_NONE = 0x0;
        /// share changes
        const MAP_SHARED = 0x1;
        /// changes are private
        const MAP_PRIVATE = 0x2;
        /// Interpret addr exactly
        const MAP_FIXED = 0x10;
        /// don't use a file
        const MAP_ANONYMOUS = 0x20;
        // linux-6.1-rc5/include/uapi/asm-generic/mman.h#7
        /// stack-like segment
        const MAP_GROWSDOWN = 0x100;
        /// ETXTBSY
        const MAP_DENYWRITE = 0x800;
        /// Mark it as an executable
        const MAP_EXECUTABLE = 0x1000;
        /// Pages are locked
        const MAP_LOCKED = 0x2000;
        /// don't check for reservations
        const MAP_NORESERVE = 0x4000;
        /// populate (prefault) pagetables
        const MAP_POPULATE = 0x8000;
        /// do not block on IO
        const MAP_NONBLOCK = 0x10000;
        /// give out an address that is best suited for process/thread stacks
        const MAP_STACK = 0x20000;
        /// create a huge page mapping
        const MAP_HUGETLB = 0x40000;
        /// perform synchronous page faults for the mapping
        const MAP_SYNC = 0x80000;
        /// MAP_FIXED which doesn't unmap underlying mapping
        const MAP_FIXED_NOREPLACE = 0x100000;

        /// For anonymous mmap, memory could be uninitialized
        const MAP_UNINITIALIZED = 0x4000000;

    }
}

impl Syscall {
    pub fn brk(new_addr: VirtAddr) -> Result<VirtAddr, SystemError> {
        // kdebug!("brk: new_addr={:?}", new_addr);
        let address_space = AddressSpace::current()?;
        let mut address_space = address_space.write();

        if new_addr < address_space.brk_start || new_addr >= MMArch::USER_END_VADDR {
            return Ok(address_space.brk);
        }
        if new_addr == address_space.brk {
            return Ok(address_space.brk);
        }

        unsafe {
            address_space
                .set_brk(VirtAddr::new(page_align_up(new_addr.data())))
                .ok();

            return Ok(address_space.sbrk(0).unwrap());
        }
    }

    pub fn sbrk(incr: isize) -> Result<VirtAddr, SystemError> {
        let address_space = AddressSpace::current()?;
        assert!(address_space.read().user_mapper.utable.is_current());
        let mut address_space = address_space.write();
        let r = unsafe { address_space.sbrk(incr) };

        return r;
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
    pub fn mmap(
        start_vaddr: VirtAddr,
        len: usize,
        prot_flags: usize,
        map_flags: usize,
        _fd: i32,
        _offset: usize,
    ) -> Result<usize, SystemError> {
        let map_flags = MapFlags::from_bits_truncate(map_flags as u64);
        let prot_flags = ProtFlags::from_bits_truncate(prot_flags as u64);

        if start_vaddr < VirtAddr::new(DEFAULT_MMAP_MIN_ADDR)
            && map_flags.contains(MapFlags::MAP_FIXED)
        {
            kerror!(
                "mmap: MAP_FIXED is not supported for address below {}",
                DEFAULT_MMAP_MIN_ADDR
            );
            return Err(SystemError::EINVAL);
        }
        // 暂时不支持除匿名页以外的映射
        if !map_flags.contains(MapFlags::MAP_ANONYMOUS) {
            kerror!("mmap: not support file mapping");
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }

        // 暂时不支持巨页映射
        if map_flags.contains(MapFlags::MAP_HUGETLB) {
            kerror!("mmap: not support huge page mapping");
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        let current_address_space = AddressSpace::current()?;
        let start_page = current_address_space.write().map_anonymous(
            start_vaddr,
            len,
            prot_flags,
            map_flags,
            true,
        )?;
        return Ok(start_page.virt_address().data());
    }

    /// ## munmap系统调用
    ///
    /// ## 参数
    ///
    /// - `start_vaddr`：取消映射的起始地址（已经对齐到页）
    /// - `len`：取消映射的字节数(已经对齐到页)
    ///
    /// ## 返回值
    ///
    /// 成功时返回0，失败时返回错误码
    pub fn munmap(start_vaddr: VirtAddr, len: usize) -> Result<usize, SystemError> {
        assert!(start_vaddr.check_aligned(MMArch::PAGE_SIZE));
        assert!(check_aligned(len, MMArch::PAGE_SIZE));

        if unlikely(verify_area(start_vaddr, len).is_err()) {
            return Err(SystemError::EINVAL);
        }
        if unlikely(len == 0) {
            return Err(SystemError::EINVAL);
        }

        let current_address_space: Arc<AddressSpace> = AddressSpace::current()?;
        let start_frame = VirtPageFrame::new(start_vaddr);
        let page_count = PageFrameCount::new(len / MMArch::PAGE_SIZE);

        current_address_space
            .write()
            .munmap(start_frame, page_count)
            .map_err(|_| SystemError::EINVAL)?;
        return Ok(0);
    }

    /// ## mprotect系统调用
    ///
    /// ## 参数
    ///
    /// - `start_vaddr`：起始地址(已经对齐到页)
    /// - `len`：长度(已经对齐到页)
    /// - `prot_flags`：保护标志
    pub fn mprotect(
        start_vaddr: VirtAddr,
        len: usize,
        prot_flags: usize,
    ) -> Result<usize, SystemError> {
        assert!(start_vaddr.check_aligned(MMArch::PAGE_SIZE));
        assert!(check_aligned(len, MMArch::PAGE_SIZE));

        if unlikely(verify_area(start_vaddr, len).is_err()) {
            return Err(SystemError::EINVAL);
        }
        if unlikely(len == 0) {
            return Err(SystemError::EINVAL);
        }

        let prot_flags = ProtFlags::from_bits(prot_flags as u64).ok_or(SystemError::EINVAL)?;

        let current_address_space: Arc<AddressSpace> = AddressSpace::current()?;
        let start_frame = VirtPageFrame::new(start_vaddr);
        let page_count = PageFrameCount::new(len / MMArch::PAGE_SIZE);

        current_address_space
            .write()
            .mprotect(start_frame, page_count, prot_flags)
            .map_err(|_| SystemError::EINVAL)?;
        return Ok(0);
    }
}
