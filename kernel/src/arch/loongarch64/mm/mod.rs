pub mod bump;

use crate::mm::{
    allocator::page_frame::{FrameAllocator, PageFrameCount, PageFrameUsage},
    page::EntryFlags,
    MemoryManagementArch, PhysAddr, VirtAddr, VmFlags,
};

use crate::arch::MMArch;

pub type PageMapper = crate::mm::page::PageMapper<LoongArch64MMArch, LockedFrameAllocator>;

/// LoongArch64的内存管理架构结构体
#[derive(Debug, Clone, Copy, Hash)]
pub struct LoongArch64MMArch;

impl MemoryManagementArch for LoongArch64MMArch {
    const PAGE_FAULT_ENABLED: bool = false;

    const PAGE_SHIFT: usize = 0;

    const PAGE_ENTRY_SHIFT: usize = 0;

    const PAGE_LEVELS: usize = 0;

    const ENTRY_ADDRESS_SHIFT: usize = 0;

    const ENTRY_FLAG_DEFAULT_PAGE: usize = 0;

    const ENTRY_FLAG_DEFAULT_TABLE: usize = 0;

    const ENTRY_FLAG_PRESENT: usize = 0;

    const ENTRY_FLAG_READONLY: usize = 0;

    const ENTRY_FLAG_WRITEABLE: usize = 0;

    const ENTRY_FLAG_READWRITE: usize = 0;

    const ENTRY_FLAG_USER: usize = 0;

    const ENTRY_FLAG_WRITE_THROUGH: usize = 0;

    const ENTRY_FLAG_CACHE_DISABLE: usize = 0;

    const ENTRY_FLAG_NO_EXEC: usize = 0;

    const ENTRY_FLAG_EXEC: usize = 0;

    const ENTRY_FLAG_DIRTY: usize = 0;

    const ENTRY_FLAG_ACCESSED: usize = 0;

    const ENTRY_FLAG_HUGE_PAGE: usize = 0;

    const ENTRY_FLAG_GLOBAL: usize = 0;

    const PHYS_OFFSET: usize = 0x9000_0000_0000_0000;

    const KERNEL_LINK_OFFSET: usize = 0;

    const USER_END_VADDR: crate::mm::VirtAddr = VirtAddr::new(0);

    const USER_BRK_START: crate::mm::VirtAddr = VirtAddr::new(0);

    const USER_STACK_START: crate::mm::VirtAddr = VirtAddr::new(0);

    const FIXMAP_START_VADDR: crate::mm::VirtAddr = VirtAddr::new(0);

    const FIXMAP_SIZE: usize = 0;

    const MMIO_BASE: crate::mm::VirtAddr = VirtAddr::new(0);

    const MMIO_SIZE: usize = 0;

    const PAGE_NONE: usize = 0;

    const PAGE_SHARED: usize = 0;

    const PAGE_SHARED_EXEC: usize = 0;

    const PAGE_COPY_NOEXEC: usize = 0;

    const PAGE_COPY_EXEC: usize = 0;

    const PAGE_COPY: usize = 0;

    const PAGE_READONLY: usize = 0;

    const PAGE_READONLY_EXEC: usize = 0;

    const PAGE_READ: usize = 0;

    const PAGE_READ_EXEC: usize = 0;

    const PAGE_WRITE: usize = 0;

    const PAGE_WRITE_EXEC: usize = 0;

    const PAGE_EXEC: usize = 0;

    const PROTECTION_MAP: [crate::mm::page::EntryFlags<Self>; 16] = protection_map();

    unsafe fn init() {
        todo!()
    }

    unsafe fn invalidate_page(address: crate::mm::VirtAddr) {
        todo!()
    }

    unsafe fn invalidate_all() {
        todo!()
    }

    unsafe fn table(table_kind: crate::mm::PageTableKind) -> crate::mm::PhysAddr {
        todo!()
    }

    unsafe fn set_table(table_kind: crate::mm::PageTableKind, table: crate::mm::PhysAddr) {
        todo!()
    }

    fn virt_is_valid(virt: crate::mm::VirtAddr) -> bool {
        todo!()
    }

    fn initial_page_table() -> crate::mm::PhysAddr {
        todo!()
    }

    fn setup_new_usermapper() -> Result<crate::mm::ucontext::UserMapper, system_error::SystemError>
    {
        todo!()
    }

    fn make_entry(paddr: crate::mm::PhysAddr, page_flags: usize) -> usize {
        todo!()
    }
}

/// 获取内核地址默认的页面标志
pub unsafe fn kernel_page_flags<A: MemoryManagementArch>(_virt: VirtAddr) -> EntryFlags<A> {
    EntryFlags::from_data(LoongArch64MMArch::ENTRY_FLAG_DEFAULT_PAGE)
        .set_user(false)
        .set_execute(true)
}

/// 全局的页帧分配器
#[derive(Debug, Clone, Copy, Hash)]
pub struct LockedFrameAllocator;

impl FrameAllocator for LockedFrameAllocator {
    unsafe fn allocate(&mut self, count: PageFrameCount) -> Option<(PhysAddr, PageFrameCount)> {
        todo!("LockedFrameAllocator::allocate")
    }

    unsafe fn free(&mut self, address: PhysAddr, count: PageFrameCount) {
        todo!("LockedFrameAllocator::free")
    }

    unsafe fn usage(&self) -> PageFrameUsage {
        todo!("LockedFrameAllocator::usage")
    }
}

/// 获取保护标志的映射表
///
///
/// ## 返回值
/// - `[usize; 16]`: 长度为16的映射表
const fn protection_map() -> [EntryFlags<MMArch>; 16] {
    let mut map = [unsafe { EntryFlags::from_data(0) }; 16];
    unsafe {
        map[VmFlags::VM_NONE.bits()] = EntryFlags::from_data(MMArch::PAGE_NONE);
        map[VmFlags::VM_READ.bits()] = EntryFlags::from_data(MMArch::PAGE_READONLY);
        map[VmFlags::VM_WRITE.bits()] = EntryFlags::from_data(MMArch::PAGE_COPY);
        map[VmFlags::VM_WRITE.bits() | VmFlags::VM_READ.bits()] =
            EntryFlags::from_data(MMArch::PAGE_COPY);
        map[VmFlags::VM_EXEC.bits()] = EntryFlags::from_data(MMArch::PAGE_READONLY_EXEC);
        map[VmFlags::VM_EXEC.bits() | VmFlags::VM_READ.bits()] =
            EntryFlags::from_data(MMArch::PAGE_READONLY_EXEC);
        map[VmFlags::VM_EXEC.bits() | VmFlags::VM_WRITE.bits()] =
            EntryFlags::from_data(MMArch::PAGE_COPY_EXEC);
        map[VmFlags::VM_EXEC.bits() | VmFlags::VM_WRITE.bits() | VmFlags::VM_READ.bits()] =
            EntryFlags::from_data(MMArch::PAGE_COPY_EXEC);
        map[VmFlags::VM_SHARED.bits()] = EntryFlags::from_data(MMArch::PAGE_NONE);
        map[VmFlags::VM_SHARED.bits() | VmFlags::VM_READ.bits()] =
            EntryFlags::from_data(MMArch::PAGE_READONLY);
        map[VmFlags::VM_SHARED.bits() | VmFlags::VM_WRITE.bits()] =
            EntryFlags::from_data(MMArch::PAGE_SHARED);
        map[VmFlags::VM_SHARED.bits() | VmFlags::VM_WRITE.bits() | VmFlags::VM_READ.bits()] =
            EntryFlags::from_data(MMArch::PAGE_SHARED);
        map[VmFlags::VM_SHARED.bits() | VmFlags::VM_EXEC.bits()] =
            EntryFlags::from_data(MMArch::PAGE_READONLY_EXEC);
        map[VmFlags::VM_SHARED.bits() | VmFlags::VM_EXEC.bits() | VmFlags::VM_READ.bits()] =
            EntryFlags::from_data(MMArch::PAGE_READONLY_EXEC);
        map[VmFlags::VM_SHARED.bits() | VmFlags::VM_EXEC.bits() | VmFlags::VM_WRITE.bits()] =
            EntryFlags::from_data(MMArch::PAGE_SHARED_EXEC);
        map[VmFlags::VM_SHARED.bits()
            | VmFlags::VM_EXEC.bits()
            | VmFlags::VM_WRITE.bits()
            | VmFlags::VM_READ.bits()] = EntryFlags::from_data(MMArch::PAGE_SHARED_EXEC);
    }

    map
}
