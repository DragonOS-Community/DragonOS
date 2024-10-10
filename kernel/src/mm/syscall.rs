use core::{intrinsics::unlikely, slice::from_raw_parts};

use alloc::sync::Arc;
use log::error;
use system_error::SystemError;

use crate::{
    arch::MMArch,
    driver::base::block::SeekFrom,
    ipc::shm::ShmFlags,
    libs::align::{check_aligned, page_align_up},
    mm::MemoryManagementArch,
    syscall::Syscall,
};

use super::{
    allocator::page_frame::{PageFrameCount, VirtPageFrame},
    ucontext::{AddressSpace, DEFAULT_MMAP_MIN_ADDR},
    verify_area, MsFlags, VirtAddr, VmFlags,
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

    /// Memory mremapping flags
    pub struct MremapFlags: u8 {
        const MREMAP_MAYMOVE = 1;
        const MREMAP_FIXED = 2;
        const MREMAP_DONTUNMAP = 4;
    }


    pub struct MadvFlags: u64 {
        /// 默认行为，系统会进行一定的预读和预写，适用于一般读取场景
        const MADV_NORMAL = 0;
        /// 随机访问模式，系统会尽量最小化数据读取量，适用于随机访问的场景
        const MADV_RANDOM = 1;
        /// 顺序访问模式，系统会进行积极的预读，访问后的页面可以尽快释放，适用于顺序读取场景
        const MADV_SEQUENTIAL = 2;
        /// 通知系统预读某些页面，用于应用程序提前准备数据
        const MADV_WILLNEED = 3;
        /// 通知系统应用程序不再需要某些页面，内核可以释放相关资源
        const MADV_DONTNEED = 4;

        /// 将指定范围的页面标记为延迟释放，真正的释放会延迟至内存压力发生时
        const MADV_FREE = 8;
        /// 应用程序请求释放指定范围的页面和相关的后备存储
        const MADV_REMOVE = 9;
        /// 在 fork 时排除指定区域
        const MADV_DONTFORK = 10;
        /// 取消 MADV_DONTFORK 的效果，不再在 fork 时排除指定区域
        const MADV_DOFORK = 11;
        /// 模拟内存硬件错误，触发内存错误处理器处理
        const MADV_HWPOISON = 100;
        /// 尝试软下线指定的内存范围
        const MADV_SOFT_OFFLINE = 101;

        /// 应用程序建议内核尝试合并指定范围内内容相同的页面
        const MADV_MERGEABLE = 12;
        /// 取消 MADV_MERGEABLE 的效果，不再合并页面
        const MADV_UNMERGEABLE = 13;

        /// 应用程序希望将指定范围以透明大页方式支持
        const MADV_HUGEPAGE = 14;
        /// 将指定范围标记为不值得用透明大页支持
        const MADV_NOHUGEPAGE = 15;

        /// 应用程序请求在核心转储时排除指定范围内的页面
        const MADV_DONTDUMP = 16;
        /// 取消 MADV_DONTDUMP 的效果，不再排除核心转储时的页面
        const MADV_DODUMP = 17;

        /// 在 fork 时将子进程的该区域内存填充为零
        const MADV_WIPEONFORK = 18;
        /// 取消 `MADV_WIPEONFORK` 的效果，不再在 fork 时填充子进程的内存
        const MADV_KEEPONFORK = 19;

        /// 应用程序不会立刻使用这些内存，内核将页面设置为非活动状态以便在内存压力发生时轻松回收
        const MADV_COLD = 20;
        /// 应用程序不会立刻使用这些内存，内核立即将这些页面换出
        const MADV_PAGEOUT = 21;

        /// 预先填充页面表，可读，通过触发读取故障
        const MADV_POPULATE_READ = 22;
        /// 预先填充页面表，可写，通过触发写入故障
        const MADV_POPULATE_WRITE = 23;

        /// 与 `MADV_DONTNEED` 类似，会将被锁定的页面释放
        const MADV_DONTNEED_LOCKED = 24;

        /// 同步将页面合并为新的透明大页
        const MADV_COLLAPSE = 25;

    }
}

impl From<MapFlags> for VmFlags {
    fn from(map_flags: MapFlags) -> Self {
        let mut vm_flags = VmFlags::VM_NONE;

        if map_flags.contains(MapFlags::MAP_GROWSDOWN) {
            vm_flags |= VmFlags::VM_GROWSDOWN;
        }

        if map_flags.contains(MapFlags::MAP_LOCKED) {
            vm_flags |= VmFlags::VM_LOCKED;
        }

        if map_flags.contains(MapFlags::MAP_SYNC) {
            vm_flags |= VmFlags::VM_SYNC;
        }

        if map_flags.contains(MapFlags::MAP_SHARED) {
            vm_flags |= VmFlags::VM_SHARED;
        }

        vm_flags
    }
}

impl From<ProtFlags> for VmFlags {
    fn from(prot_flags: ProtFlags) -> Self {
        let mut vm_flags = VmFlags::VM_NONE;

        if prot_flags.contains(ProtFlags::PROT_READ) {
            vm_flags |= VmFlags::VM_READ;
        }

        if prot_flags.contains(ProtFlags::PROT_WRITE) {
            vm_flags |= VmFlags::VM_WRITE;
        }

        if prot_flags.contains(ProtFlags::PROT_EXEC) {
            vm_flags |= VmFlags::VM_EXEC;
        }

        vm_flags
    }
}

impl From<ShmFlags> for VmFlags {
    fn from(shm_flags: ShmFlags) -> Self {
        let mut vm_flags = VmFlags::VM_NONE;

        if shm_flags.contains(ShmFlags::SHM_RDONLY) {
            vm_flags |= VmFlags::VM_READ;
        } else {
            vm_flags |= VmFlags::VM_READ | VmFlags::VM_WRITE;
        }

        if shm_flags.contains(ShmFlags::SHM_EXEC) {
            vm_flags |= VmFlags::VM_EXEC;
        }

        if shm_flags.contains(ShmFlags::SHM_HUGETLB) {
            vm_flags |= VmFlags::VM_HUGETLB;
        }

        vm_flags
    }
}

impl From<VmFlags> for MapFlags {
    fn from(value: VmFlags) -> Self {
        let mut map_flags = MapFlags::MAP_NONE;

        if value.contains(VmFlags::VM_GROWSDOWN) {
            map_flags |= MapFlags::MAP_GROWSDOWN;
        }

        if value.contains(VmFlags::VM_LOCKED) {
            map_flags |= MapFlags::MAP_LOCKED;
        }

        if value.contains(VmFlags::VM_SYNC) {
            map_flags |= MapFlags::MAP_SYNC;
        }

        if value.contains(VmFlags::VM_MAYSHARE) {
            map_flags |= MapFlags::MAP_SHARED;
        }

        map_flags
    }
}

impl From<VmFlags> for ProtFlags {
    fn from(value: VmFlags) -> Self {
        let mut prot_flags = ProtFlags::PROT_NONE;

        if value.contains(VmFlags::VM_READ) {
            prot_flags |= ProtFlags::PROT_READ;
        }

        if value.contains(VmFlags::VM_WRITE) {
            prot_flags |= ProtFlags::PROT_WRITE;
        }

        if value.contains(VmFlags::VM_EXEC) {
            prot_flags |= ProtFlags::PROT_EXEC;
        }

        prot_flags
    }
}

impl Syscall {
    pub fn brk(new_addr: VirtAddr) -> Result<VirtAddr, SystemError> {
        // debug!("brk: new_addr={:?}", new_addr);
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
        fd: i32,
        offset: usize,
    ) -> Result<usize, SystemError> {
        let map_flags = MapFlags::from_bits_truncate(map_flags as u64);
        let prot_flags = ProtFlags::from_bits_truncate(prot_flags as u64);

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
    pub fn mremap(
        old_vaddr: VirtAddr,
        old_len: usize,
        new_len: usize,
        mremap_flags: MremapFlags,
        new_vaddr: VirtAddr,
    ) -> Result<usize, SystemError> {
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
        let vm_flags = *vma.lock_irqsave().vm_flags();

        // 暂时不支持巨页映射
        if vm_flags.contains(VmFlags::VM_HUGETLB) {
            error!("mmap: not support huge page mapping");
            return Err(SystemError::ENOSYS);
        }

        // 缩小旧内存映射区域
        if old_len > new_len {
            Self::munmap(old_vaddr + new_len, old_len - new_len)?;
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

        if !mremap_flags.contains(MremapFlags::MREMAP_DONTUNMAP) {
            Self::munmap(old_vaddr, old_len)?;
        }

        return Ok(r.data());
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

    /// ## madvise系统调用
    ///
    /// ## 参数
    ///
    /// - `start_vaddr`：起始地址(已经对齐到页)
    /// - `len`：长度(已经对齐到页)
    /// - `madv_flags`：建议标志
    pub fn madvise(
        start_vaddr: VirtAddr,
        len: usize,
        madv_flags: usize,
    ) -> Result<usize, SystemError> {
        if !start_vaddr.check_aligned(MMArch::PAGE_SIZE) || !check_aligned(len, MMArch::PAGE_SIZE) {
            return Err(SystemError::EINVAL);
        }

        if unlikely(verify_area(start_vaddr, len).is_err()) {
            return Err(SystemError::EINVAL);
        }
        if unlikely(len == 0) {
            return Err(SystemError::EINVAL);
        }

        let madv_flags = MadvFlags::from_bits(madv_flags as u64).ok_or(SystemError::EINVAL)?;

        let current_address_space: Arc<AddressSpace> = AddressSpace::current()?;
        let start_frame = VirtPageFrame::new(start_vaddr);
        let page_count = PageFrameCount::new(len / MMArch::PAGE_SIZE);

        current_address_space
            .write()
            .madvise(start_frame, page_count, madv_flags)
            .map_err(|_| SystemError::EINVAL)?;
        return Ok(0);
    }

    /// ## msync系统调用
    ///
    /// ## 参数
    ///
    /// - `start`：起始地址(已经对齐到页)
    /// - `len`：长度(已经对齐到页)
    /// - `flags`：标志
    pub fn msync(start: VirtAddr, len: usize, flags: usize) -> Result<usize, SystemError> {
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
        let flags = MsFlags::from_bits_truncate(flags);
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
                let guard = vma.lock_irqsave();
                let vm_start = guard.region().start().data();
                let vm_end = guard.region().end().data();
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
                let vm_flags = *guard.vm_flags();
                if flags.contains(MsFlags::MS_INVALIDATE) && vm_flags.contains(VmFlags::VM_LOCKED) {
                    err = Err(SystemError::EBUSY);
                    break;
                }
                let file = guard.vm_file();
                let fstart = (start - vm_start)
                    + (guard.file_page_offset().unwrap_or(0) << MMArch::PAGE_SHIFT);
                let fend = fstart + (core::cmp::min(end, vm_end) - start) - 1;
                let old_start = start;
                start = vm_end;
                // log::info!("flags: {:?}", flags);
                // log::info!("vm_flags: {:?}", vm_flags);
                // log::info!("file: {:?}", file);
                if flags.contains(MsFlags::MS_SYNC) && vm_flags.contains(VmFlags::VM_SHARED) {
                    if let Some(file) = file {
                        let old_pos = file.lseek(SeekFrom::SeekCurrent(0)).unwrap();
                        file.lseek(SeekFrom::SeekSet(fstart as i64)).unwrap();
                        err = file.write(len, unsafe {
                            from_raw_parts(old_start as *mut u8, fend - fstart + 1)
                        });
                        file.lseek(SeekFrom::SeekSet(old_pos as i64)).unwrap();
                        if err.is_err() {
                            break;
                        } else if start >= end {
                            err = unmapped_error;
                            break;
                        }
                        next_vma = current_address_space
                            .read()
                            .mappings
                            .find_nearest(VirtAddr::new(start));
                    }
                } else {
                    if start >= end {
                        err = unmapped_error;
                        break;
                    }
                    next_vma = current_address_space
                        .read()
                        .mappings
                        .find_nearest(VirtAddr::new(vm_end));
                }
            } else {
                return Err(SystemError::ENOMEM);
            }
        }
        return err;
    }
}
