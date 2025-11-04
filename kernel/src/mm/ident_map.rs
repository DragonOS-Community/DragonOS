use crate::arch::mm::LockedFrameAllocator;
use crate::arch::MMArch;
use crate::mm::{
    allocator::page_frame::FrameAllocator,
    page::{EntryFlags, PageEntry, PageFlush, PageTable},
    MemoryManagementArch, PhysAddr, VirtAddr,
};
use core::marker::PhantomData;
use core::sync::atomic::compiler_fence;
use core::sync::atomic::Ordering;

/// 恒等页表映射器( paddr == vaddr )
#[derive(Hash)]
pub struct IdentPageMapper<Arch, F> {
    /// 根页表物理地址
    table_paddr: PhysAddr,
    /// 页分配器
    frame_allocator: F,
    phantom: PhantomData<fn() -> Arch>,
}

impl<Arch: MemoryManagementArch, F: FrameAllocator> IdentPageMapper<Arch, F> {
    /// 创建新的页面映射器
    ///
    /// ## 参数
    /// - table_kind 页表类型
    /// - table_paddr 根页表物理地址
    /// - allocator 页分配器
    ///
    /// ## 返回值
    ///
    /// 页面映射器
    pub unsafe fn new(table_paddr: PhysAddr, allocator: F) -> Self {
        return Self {
            table_paddr,
            frame_allocator: allocator,
            phantom: PhantomData,
        };
    }

    pub unsafe fn create(mut allocator: F) -> Self {
        let table_paddr = allocator.allocate_one().unwrap();
        let table_vaddr = Arch::phys_2_virt(table_paddr).unwrap();
        Arch::write_bytes(table_vaddr, 0, Arch::PAGE_SIZE);
        return Self::new(table_paddr, allocator);
    }

    pub fn paddr(&self) -> PhysAddr {
        self.table_paddr
    }

    /// 映射一个物理页到指定的虚拟地址
    pub unsafe fn map_phys(
        table_paddr: PhysAddr,
        virt: VirtAddr,
        phys: PhysAddr,
        mut allocator: F,
    ) -> Option<PageFlush<Arch>> {
        // 验证虚拟地址和物理地址是否对齐
        if !(virt.check_aligned(Arch::PAGE_SIZE) && phys.check_aligned(Arch::PAGE_SIZE)) {
            log::error!(
                "Try to map unaligned page: virt={:?}, phys={:?}",
                virt,
                phys
            );
            return None;
        }

        let virt = VirtAddr::new(virt.data() & (!Arch::PAGE_NEGATIVE_MASK));
        let flags = EntryFlags::from_data(
            Arch::ENTRY_FLAG_PRESENT
                | Arch::ENTRY_FLAG_READWRITE
                | Arch::ENTRY_FLAG_EXEC
                | Arch::ENTRY_FLAG_GLOBAL
                | Arch::ENTRY_FLAG_DIRTY
                | Arch::ENTRY_FLAG_ACCESSED,
        );

        // 创建页表项
        let entry = PageEntry::new(phys, flags);
        let mut table = PageTable::new(VirtAddr::new(0), table_paddr, Arch::PAGE_LEVELS - 1);
        loop {
            let i = table.index_of(virt).unwrap();

            assert!(i < Arch::PAGE_ENTRY_NUM);
            if table.level() == 0 {
                compiler_fence(Ordering::SeqCst);

                table.set_entry(i, entry);
                compiler_fence(Ordering::SeqCst);
                return Some(PageFlush::new(virt));
            } else {
                let next_table = table.next_level_table(i);
                if let Some(next_table) = next_table {
                    table = next_table;
                } else {
                    // 分配下一级页表
                    let frame = allocator.allocate_one().unwrap();

                    // 清空这个页帧
                    MMArch::write_bytes(MMArch::phys_2_virt(frame).unwrap(), 0, MMArch::PAGE_SIZE);
                    // 设置页表项的flags
                    let flags: EntryFlags<Arch> = EntryFlags::new_page_table(false);

                    // 把新分配的页表映射到当前页表
                    table.set_entry(i, PageEntry::new(frame, flags));

                    // 获取新分配的页表
                    table = table.next_level_table(i).unwrap();
                }
            }
        }
    }
}

pub fn ident_pt_alloc() -> usize {
    let new_imapper: IdentPageMapper<MMArch, LockedFrameAllocator> =
        unsafe { IdentPageMapper::create(LockedFrameAllocator) };
    new_imapper.paddr().data()
}

pub fn ident_map_page(table_paddr: usize, virt: usize, phys: usize) {
    unsafe {
        IdentPageMapper::<MMArch, LockedFrameAllocator>::map_phys(
            PhysAddr::new(table_paddr),
            VirtAddr::new(virt),
            PhysAddr::new(phys),
            LockedFrameAllocator,
        )
        .unwrap()
        .flush();
    };
}

/// 需要对齐
pub fn ident_map_pages(table_paddr: usize, virt: usize, phys: usize, nums: usize) {
    for i in 0..nums {
        let virt = virt + i * MMArch::PAGE_SIZE;
        let phys = phys + i * MMArch::PAGE_SIZE;
        unsafe {
            IdentPageMapper::<MMArch, LockedFrameAllocator>::map_phys(
                PhysAddr::new(table_paddr),
                VirtAddr::new(virt),
                PhysAddr::new(phys),
                LockedFrameAllocator,
            )
            .unwrap()
            .flush()
        };
    }
}
