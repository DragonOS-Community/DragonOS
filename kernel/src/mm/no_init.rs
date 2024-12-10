//! 该文件用于系统启动早期，内存管理器初始化之前，提供一些简单的内存映射功能
//!
//! 映射关系为：
//!
//! 虚拟地址 0-100M与虚拟地址 0x8000_0000_0000 - 0x8000_0640_0000 之间具有重映射关系。
//! 也就是说，他们的第二级页表在最顶级页表中，占用了第0和第256个页表项。
//!
//! 对于x86:
//! 这里假设在内核引导文件中，已经填写了前100M的页表，其中，前50M是真实映射到内存的，后面的仅仅创建了页表，表项全部为0。

use bitmap::{traits::BitMapOps, StaticBitmap};

use crate::{
    libs::spinlock::SpinLock,
    mm::{MMArch, MemoryManagementArch, PhysAddr},
};

use core::marker::PhantomData;

use super::{
    allocator::page_frame::{FrameAllocator, PageFrameCount, PageFrameUsage},
    page::EntryFlags,
    PageTableKind, VirtAddr,
};

/// 用于存储重映射页表的位图和页面
pub static EARLY_IOREMAP_PAGES: SpinLock<EarlyIoRemapPages> =
    SpinLock::new(EarlyIoRemapPages::new());

/// 早期重映射使用的页表
#[repr(C)]
#[repr(align(4096))]
#[derive(Clone, Copy)]
struct EarlyRemapPage {
    data: [u64; MMArch::PAGE_SIZE],
}

impl EarlyRemapPage {
    /// 清空数据
    fn zero(&mut self) {
        self.data.fill(0);
    }
}

#[repr(C)]
pub struct EarlyIoRemapPages {
    pages: [EarlyRemapPage; Self::EARLY_REMAP_PAGES_NUM],
    bmp: StaticBitmap<{ Self::EARLY_REMAP_PAGES_NUM }>,
}

impl EarlyIoRemapPages {
    /// 预留的用于在内存管理初始化之前，映射内存所使用的页表数量
    pub const EARLY_REMAP_PAGES_NUM: usize = 256;
    pub const fn new() -> Self {
        Self {
            pages: [EarlyRemapPage {
                data: [0; MMArch::PAGE_SIZE],
            }; Self::EARLY_REMAP_PAGES_NUM],
            bmp: StaticBitmap::new(),
        }
    }

    /// 分配一个页面
    ///
    /// 如果成功，返回虚拟地址
    ///
    /// 如果失败，返回None
    pub fn allocate_page(&mut self) -> Option<VirtAddr> {
        if let Some(index) = self.bmp.first_false_index() {
            self.bmp.set(index, true);
            // 清空数据
            self.pages[index].zero();

            let p = &self.pages[index] as *const EarlyRemapPage as usize;
            let vaddr = VirtAddr::new(p);
            assert!(vaddr.check_aligned(MMArch::PAGE_SIZE));
            return Some(vaddr);
        } else {
            return None;
        }
    }

    pub fn free_page(&mut self, addr: VirtAddr) {
        // 判断地址是否合法
        let start_vaddr = &self.pages[0] as *const EarlyRemapPage as usize;
        let offset = addr.data() - start_vaddr;
        let index = offset / MMArch::PAGE_SIZE;
        if index < Self::EARLY_REMAP_PAGES_NUM {
            assert!(self.bmp.get(index).unwrap());
            self.bmp.set(index, false);
        }
    }
}

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
    unsafe fn allocate(&mut self, count: PageFrameCount) -> Option<(PhysAddr, PageFrameCount)> {
        assert!(count.data() == 1);
        let vaddr = EARLY_IOREMAP_PAGES.lock_irqsave().allocate_page()?;
        let paddr = MMA::virt_2_phys(vaddr)?;
        return Some((paddr, count));
    }

    unsafe fn free(&mut self, address: PhysAddr, count: PageFrameCount) {
        assert_eq!(count.data(), 1);
        assert!(address.check_aligned(MMA::PAGE_SIZE));

        let vaddr = MMA::phys_2_virt(address);
        if let Some(vaddr) = vaddr {
            EARLY_IOREMAP_PAGES.lock_irqsave().free_page(vaddr);
        }
    }
    /// @brief: 获取内存区域页帧的使用情况
    /// @param  self
    /// @return 页帧的使用情况
    unsafe fn usage(&self) -> PageFrameUsage {
        // 暂时不支持
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
#[inline(never)]
pub unsafe fn pseudo_map_phys(vaddr: VirtAddr, paddr: PhysAddr, count: PageFrameCount) {
    let flags: EntryFlags<MMArch> = EntryFlags::new().set_write(true);

    pseudo_map_phys_with_flags(vaddr, paddr, count, flags);
}

/// Use pseudo mapper to map physical memory to virtual memory
/// with READ_ONLY and EXECUTE flags.
#[inline(never)]
pub unsafe fn pseudo_map_phys_ro(vaddr: VirtAddr, paddr: PhysAddr, count: PageFrameCount) {
    let flags: EntryFlags<MMArch> = EntryFlags::new().set_write(false).set_execute(true);

    pseudo_map_phys_with_flags(vaddr, paddr, count, flags);
}

#[inline(never)]
pub unsafe fn pseudo_map_phys_with_flags(
    vaddr: VirtAddr,
    paddr: PhysAddr,
    count: PageFrameCount,
    flags: EntryFlags<MMArch>,
) {
    assert!(vaddr.check_aligned(MMArch::PAGE_SIZE));
    assert!(paddr.check_aligned(MMArch::PAGE_SIZE));

    let mut pseudo_allocator = PseudoAllocator::<MMArch>::new();

    let mut mapper = crate::mm::page::PageMapper::<MMArch, _>::new(
        PageTableKind::Kernel,
        MMArch::table(PageTableKind::Kernel),
        &mut pseudo_allocator,
    );

    for i in 0..count.data() {
        let vaddr = vaddr + i * MMArch::PAGE_SIZE;
        let paddr = paddr + i * MMArch::PAGE_SIZE;
        let flusher: crate::mm::page::PageFlush<MMArch> =
            mapper.map_phys(vaddr, paddr, flags).unwrap();
        flusher.ignore();
    }

    mapper.make_current();
}

/// Unmap physical memory from virtual memory.
///
/// ## 说明
///
/// 该函数在系统启动早期，内存管理尚未初始化的时候使用
#[inline(never)]
pub unsafe fn pseudo_unmap_phys(vaddr: VirtAddr, count: PageFrameCount) {
    assert!(vaddr.check_aligned(MMArch::PAGE_SIZE));

    let mut pseudo_allocator = PseudoAllocator::<MMArch>::new();

    let mut mapper = crate::mm::page::PageMapper::<MMArch, _>::new(
        PageTableKind::Kernel,
        MMArch::table(PageTableKind::Kernel),
        &mut pseudo_allocator,
    );

    for i in 0..count.data() {
        let vaddr = vaddr + i * MMArch::PAGE_SIZE;
        if let Some((_, _, flusher)) = mapper.unmap_phys(vaddr, true) {
            flusher.ignore();
        };
    }

    mapper.make_current();
}
