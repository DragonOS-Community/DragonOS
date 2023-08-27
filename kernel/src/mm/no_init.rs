//! 该文件用于系统启动早期，内存管理器初始化之前，提供一些简单的内存映射功能
//!
//! 这里假设在内核引导文件中，已经填写了前100M的页表，其中，前50M是真实映射到内存的，后面的仅仅创建了页表，表项全部为0。
//! 因此这里映射内存不需要任何动态分配。
//!
//! 映射关系为：
//!
//! 虚拟地址 0-100M与虚拟地址 0x8000_0000_0000 - 0x8000_0640_0000 之间具有重映射关系。
//! 也就是说，他们的第二级页表在最顶级页表中，占用了第0和第256个页表项。
//!

use crate::mm::{MMArch, MemoryManagementArch, PhysAddr};
use core::marker::PhantomData;

use super::{
    allocator::page_frame::{FrameAllocator, PageFrameCount, PageFrameUsage},
    page::PageFlags,
    PageTableKind, VirtAddr,
};

/// 伪分配器
struct PseudoAllocator<MMA> {
    phantom: PhantomData<MMA>,
}

impl<MMA: MemoryManagementArch> PseudoAllocator<MMA> {
    pub const fn new() -> Self {
        Self {
            phantom: PhantomData,
        }
    }
}

/// 为NoInitAllocator实现FrameAllocator
impl<MMA: MemoryManagementArch> FrameAllocator for PseudoAllocator<MMA> {
    unsafe fn allocate(&mut self, _count: PageFrameCount) -> Option<(PhysAddr, PageFrameCount)> {
        panic!("NoInitAllocator can't allocate page frame");
    }

    unsafe fn free(&mut self, _address: PhysAddr, _count: PageFrameCount) {
        panic!("NoInitAllocator can't free page frame");
    }
    /// @brief: 获取内存区域页帧的使用情况
    /// @param  self
    /// @return 页帧的使用情况
    unsafe fn usage(&self) -> PageFrameUsage {
        panic!("NoInitAllocator can't get page frame usage");
    }
}

/// Use pseudo mapper to map physical memory to virtual memory.
///
/// ## Safety
///
/// 调用该函数时，必须保证内存管理器尚未初始化。否则将导致未定义的行为
///
/// 并且，内核引导文件必须以4K页为粒度，填写了前100M的内存映射关系。（具体以本文件开头的注释为准）
pub unsafe fn pseudo_map_phys(vaddr: VirtAddr, paddr: PhysAddr, count: PageFrameCount) {
    assert!(vaddr.check_aligned(MMArch::PAGE_SIZE));
    assert!(paddr.check_aligned(MMArch::PAGE_SIZE));

    let mut pseudo_allocator = PseudoAllocator::<MMArch>::new();

    let mut mapper = crate::mm::page::PageMapper::<MMArch, _>::new(
        PageTableKind::Kernel,
        MMArch::table(PageTableKind::Kernel),
        &mut pseudo_allocator,
    );

    let flags: PageFlags<MMArch> = PageFlags::new().set_write(true).set_execute(true);

    for i in 0..count.data() {
        let vaddr = vaddr + i * MMArch::PAGE_SIZE;
        let paddr = paddr + i * MMArch::PAGE_SIZE;
        let flusher = mapper.map_phys(vaddr, paddr, flags).unwrap();
        flusher.ignore();
    }

    mapper.make_current();
}
