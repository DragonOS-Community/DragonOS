pub mod barrier;
use crate::include::bindings::bindings::process_control_block;
use crate::mm::{MemoryManagementArch, PageTableKind, PhysAddr, VirtAddr};

use core::arch::asm;
use core::ptr::read_volatile;

use self::barrier::mfence;

/// @brief 切换进程的页表
///
/// @param 下一个进程的pcb。将会把它的页表切换进来。
///
/// @return 下一个进程的pcb(把它return的目的主要是为了归还所有权)
#[inline(always)]
#[allow(dead_code)]
pub fn switch_mm(
    next_pcb: &'static mut process_control_block,
) -> &'static mut process_control_block {
    mfence();
    // kdebug!("to get pml4t");
    let pml4t = unsafe { read_volatile(&next_pcb.mm.as_ref().unwrap().pgd) };

    unsafe {
        asm!("mov cr3, {}", in(reg) pml4t);
    }
    mfence();
    return next_pcb;
}

#[derive(Debug, Clone, Copy)]
pub struct X86_64MMArch;

impl MemoryManagementArch for X86_64MMArch {
    /// 4K页
    const PAGE_SHIFT: usize = 12;

    /// 每个页表项占8字节，总共有512个页表项
    const PAGE_ENTRY_SHIFT: usize = 9;

    /// 四级页表（PML4T、PDPT、PDT、PT）
    const PAGE_LEVELS: usize = 4;

    /// 页表项的有效位的index。在x86_64中，页表项的第[0, 47]位表示地址和flag，
    /// 第[48, 51]位表示保留。因此，有效位的index为52。
    /// 请注意，第63位是XD位，表示是否允许执行。
    const ENTRY_ADDRESS_SHIFT: usize = 52;

    const ENTRY_FLAG_DEFAULT_PAGE: usize = Self::ENTRY_FLAG_PRESENT;

    const ENTRY_FLAG_DEFAULT_TABLE: usize = Self::ENTRY_FLAG_PRESENT;

    const ENTRY_FLAG_PRESENT: usize = 1 << 0;

    const ENTRY_FLAG_READONLY: usize = 0;

    const ENTRY_FLAG_READWRITE: usize = 1 << 1;

    const ENTRY_FLAG_USER: usize = 1 << 2;

    const ENTRY_FLAG_WRITE_THROUGH: usize = 1 << 3;

    const ENTRY_FLAG_CACHE_DISABLE: usize = 1 << 4;

    const ENTRY_FLAG_NO_EXEC: usize = 1 << 63;

    /// 物理地址与虚拟地址的偏移量
    /// 0xffff_8000_0000_0000
    const PHYS_OFFSET: usize = Self::PAGE_NEGATIVE_MASK + (Self::PAGE_ADDRESS_SIZE >> 1);

    /// @brief 获取物理内存区域
    unsafe fn init() -> &'static [crate::mm::PhysMemoryArea] {
        todo!()
    }

    /// @brief 刷新TLB中，关于指定虚拟地址的条目
    unsafe fn invalidate_page(address: VirtAddr) {
        asm!("invlpg [{0}]", in(reg) address.data());
    }

    /// @brief 刷新TLB中，所有的条目
    unsafe fn invalidate_all() {
        // 通过设置cr3寄存器，来刷新整个TLB
        Self::set_table(PageTableKind::User, Self::table(PageTableKind::User));
    }

    /// @brief 获取顶级页表的物理地址
    unsafe fn table(_table_kind: PageTableKind) -> PhysAddr {
        let paddr: usize;
        asm!("mov {0}, cr3", out(reg) paddr);
        return PhysAddr::new(paddr);
    }

    /// @brief 设置顶级页表的物理地址到处理器中
    unsafe fn set_table(_table_kind: PageTableKind, table: PhysAddr) {
        asm!("mov cr3, {0}", in(reg) table.data());
    }

    /// @brief 判断虚拟地址是否合法
    fn virt_is_valid(virt: VirtAddr) -> bool {
        return virt.is_canonical();
    }
}

impl VirtAddr {
    /// @brief 判断虚拟地址是否合法
    #[inline(always)]
    pub fn is_canonical(self) -> bool {
        let x = self.data() & X86_64MMArch::PHYS_OFFSET;
        // 如果x为0，说明虚拟地址的高位为0，是合法的用户地址
        // 如果x为PHYS_OFFSET，说明虚拟地址的高位全为1，是合法的内核地址
        return x == 0 || x == X86_64MMArch::PHYS_OFFSET;
    }
}