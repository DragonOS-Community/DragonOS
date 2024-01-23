use riscv::register::satp;
use system_error::SystemError;

use crate::mm::{
    allocator::page_frame::{FrameAllocator, PageFrameCount, PageFrameUsage, PhysPageFrame},
    page::PageFlags,
    MemoryManagementArch, PageTableKind, PhysAddr, VirtAddr,
};

pub mod bump;
pub(super) mod init;

pub type PageMapper = crate::mm::page::PageMapper<RiscV64MMArch, LockedFrameAllocator>;

/// 内核起始物理地址
pub(self) static mut KERNEL_BEGIN_PA: PhysAddr = PhysAddr::new(0);
/// 内核结束的物理地址
pub(self) static mut KERNEL_END_PA: PhysAddr = PhysAddr::new(0);
/// 内核起始虚拟地址
pub(self) static mut KERNEL_BEGIN_VA: VirtAddr = VirtAddr::new(0);
/// 内核结束虚拟地址
pub(self) static mut KERNEL_END_VA: VirtAddr = VirtAddr::new(0);

/// RiscV64的内存管理架构结构体(sv39)
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

    /// 在距离sv39的顶端还有1G的位置，设置为FIXMAP的起始地址
    const FIXMAP_START_VADDR: VirtAddr = VirtAddr::new(0xffff_ffff_8000_0000);
    /// 设置1MB的fixmap空间
    const FIXMAP_SIZE: usize = 256 * 4096;

    unsafe fn init() {
        todo!()
    }

    unsafe fn invalidate_page(address: VirtAddr) {
        riscv::asm::sfence_vma(0, address.data());
    }

    unsafe fn invalidate_all() {
        riscv::asm::sfence_vma_all();
    }

    unsafe fn table(_table_kind: PageTableKind) -> PhysAddr {
        // phys page number
        let ppn = riscv::register::satp::read().ppn();

        let paddr = PhysPageFrame::from_ppn(ppn).phys_address();

        return paddr;
    }

    unsafe fn set_table(_table_kind: PageTableKind, table: PhysAddr) {
        let ppn = PhysPageFrame::new(table).ppn();
        riscv::asm::sfence_vma_all();
        satp::set(satp::Mode::Sv39, 0, ppn);
    }

    fn virt_is_valid(virt: crate::mm::VirtAddr) -> bool {
        virt.is_canonical()
    }

    fn initial_page_table() -> crate::mm::PhysAddr {
        todo!()
    }

    fn setup_new_usermapper() -> Result<crate::mm::ucontext::UserMapper, SystemError> {
        todo!()
    }

    unsafe fn phys_2_virt(phys: PhysAddr) -> Option<VirtAddr> {
        // riscv的内核文件所占用的空间，由于重定位而导致不满足线性偏移量的关系
        // 因此这里需要特殊处理
        if phys >= KERNEL_BEGIN_PA && phys < KERNEL_END_PA {
            let r = KERNEL_BEGIN_VA + (phys - KERNEL_BEGIN_PA);
            return Some(r);
        }

        if let Some(vaddr) = phys.data().checked_add(Self::PHYS_OFFSET) {
            return Some(VirtAddr::new(vaddr));
        } else {
            return None;
        }
    }

    unsafe fn virt_2_phys(virt: VirtAddr) -> Option<PhysAddr> {
        if virt >= KERNEL_BEGIN_VA && virt < KERNEL_END_VA {
            let r = KERNEL_BEGIN_PA + (virt - KERNEL_BEGIN_VA);
            return Some(r);
        }

        if let Some(paddr) = virt.data().checked_sub(Self::PHYS_OFFSET) {
            let r = PhysAddr::new(paddr);
            return Some(r);
        } else {
            return None;
        }
    }

    fn make_entry(paddr: PhysAddr, page_flags: usize) -> usize {
        let ppn = PhysPageFrame::new(paddr).ppn();
        let r = ((ppn & ((1 << 44) - 1)) << 10) | page_flags;
        return r;
    }
}

impl VirtAddr {
    /// 判断虚拟地址是否合法
    #[inline(always)]
    pub fn is_canonical(self) -> bool {
        let x = self.data() & RiscV64MMArch::PHYS_OFFSET;
        // 如果x为0，说明虚拟地址的高位为0，是合法的用户地址
        // 如果x为PHYS_OFFSET，说明虚拟地址的高位全为1，是合法的内核地址
        return x == 0 || x == RiscV64MMArch::PHYS_OFFSET;
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
