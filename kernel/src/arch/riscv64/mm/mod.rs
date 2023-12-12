use crate::mm::{
    allocator::page_frame::{FrameAllocator, PageFrameCount, PageFrameUsage},
    page::PageFlags,
    MemoryManagementArch, PhysAddr, VirtAddr,
};

pub mod bump;

pub type PageMapper = crate::mm::page::PageMapper<RiscV64MMArch, LockedFrameAllocator>;

/// RiscV64的内存管理架构结构体
#[derive(Debug, Clone, Copy, Hash)]
pub struct RiscV64MMArch;

impl MemoryManagementArch for RiscV64MMArch {
    const PAGE_SHIFT: usize = 12;

    const PAGE_ENTRY_SHIFT: usize = 9;

    /// sv39分页只有三级
    const PAGE_LEVELS: usize = 3;

    const ENTRY_ADDRESS_SHIFT: usize = 39;

    const ENTRY_FLAG_DEFAULT_PAGE: usize = Self::ENTRY_FLAG_PRESENT;

    const ENTRY_FLAG_DEFAULT_TABLE: usize = Self::ENTRY_FLAG_PRESENT;

    const ENTRY_FLAG_PRESENT: usize = 1 << 0;

    const ENTRY_FLAG_READONLY: usize = 1 << 1;

    const ENTRY_FLAG_READWRITE: usize = (1 << 2) | (1 << 1);

    const ENTRY_FLAG_USER: usize = (1 << 4);

    const ENTRY_FLAG_WRITE_THROUGH: usize = (2 << 61);

    const ENTRY_FLAG_CACHE_DISABLE: usize = (2 << 61);

    const ENTRY_FLAG_NO_EXEC: usize = 0;

    const ENTRY_FLAG_EXEC: usize = (1 << 3);

    const PHYS_OFFSET: usize = 0xffff_ffc0_0000_0000;

    const USER_END_VADDR: crate::mm::VirtAddr = VirtAddr::new(0x0000_003f_ffff_ffff);

    const USER_BRK_START: crate::mm::VirtAddr = VirtAddr::new(0x0000_001f_ffff_ffff);

    const USER_STACK_START: crate::mm::VirtAddr = VirtAddr::new(0x0000_001f_ffa0_0000);

    unsafe fn init() -> &'static [crate::mm::PhysMemoryArea] {
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

    fn setup_new_usermapper() -> Result<crate::mm::ucontext::UserMapper, crate::syscall::SystemError>
    {
        todo!()
    }
}

/// 获取内核地址默认的页面标志
pub unsafe fn kernel_page_flags<A: MemoryManagementArch>(virt: VirtAddr) -> PageFlags<A> {
    unimplemented!("riscv64::kernel_page_flags")
}

/// 全局的页帧分配器
#[derive(Debug, Clone, Copy, Hash)]
pub struct LockedFrameAllocator;

impl FrameAllocator for LockedFrameAllocator {
    unsafe fn allocate(&mut self, count: PageFrameCount) -> Option<(PhysAddr, PageFrameCount)> {
        unimplemented!("RiscV64 LockedFrameAllocator::allocate")
    }

    unsafe fn free(&mut self, address: crate::mm::PhysAddr, count: PageFrameCount) {
        assert!(count.data().is_power_of_two());
        unimplemented!("RiscV64 LockedFrameAllocator::free")
    }

    unsafe fn usage(&self) -> PageFrameUsage {
        unimplemented!("RiscV64 LockedFrameAllocator::usage")
    }
}
