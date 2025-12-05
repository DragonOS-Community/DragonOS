use crate::{
    ipc::shm::ShmFlags,
    libs::align::{check_aligned, page_align_up},
};

use super::{allocator::page_frame::PageFrameCount, MsFlags, VmFlags};

mod mempolice_utils;
mod sys_brk;
mod sys_fadvise64;
mod sys_get_mempolicy;
mod sys_madvise;
mod sys_mincore;
mod sys_mmap;
mod sys_mprotect;
mod sys_mremap;
mod sys_msync;
mod sys_munmap;
pub mod sys_sbrk;

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
