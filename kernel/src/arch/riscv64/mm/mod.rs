use riscv::register::satp;
use sbi_rt::{HartMask, SbiRet};
use system_error::SystemError;

use crate::{
    arch::MMArch,
    driver::open_firmware::fdt::open_firmware_fdt_driver,
    libs::spinlock::SpinLock,
    mm::{
        allocator::{
            buddy::BuddyAllocator,
            page_frame::{FrameAllocator, PageFrameCount, PageFrameUsage, PhysPageFrame},
        },
        kernel_mapper::KernelMapper,
        page::{EntryFlags, PageEntry, PAGE_1G_SHIFT},
        ucontext::UserMapper,
        MemoryManagementArch, PageTableKind, PhysAddr, VirtAddr, VmFlags,
    },
    smp::cpu::ProcessorId,
};

use self::init::{riscv_mm_init, INITIAL_PGTABLE_VALUE};

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

pub(self) static INNER_ALLOCATOR: SpinLock<Option<BuddyAllocator<MMArch>>> = SpinLock::new(None);

/// RiscV64的内存管理架构结构体(sv39)
#[derive(Debug, Clone, Copy, Hash)]
pub struct RiscV64MMArch;

impl RiscV64MMArch {
    /// 使远程cpu的TLB中，指定地址范围的页失效
    #[allow(dead_code)]
    pub fn remote_invalidate_page(
        cpu: ProcessorId,
        address: VirtAddr,
        size: usize,
    ) -> Result<(), SbiRet> {
        let r = sbi_rt::remote_sfence_vma(Into::into(cpu), address.data(), size);
        if r.is_ok() {
            return Ok(());
        } else {
            return Err(r);
        }
    }

    /// 使指定远程cpu的TLB中，所有范围的页失效
    #[allow(dead_code)]
    pub fn remote_invalidate_all(cpu: ProcessorId) -> Result<(), SbiRet> {
        let r = Self::remote_invalidate_page(
            cpu,
            VirtAddr::new(0),
            1 << RiscV64MMArch::ENTRY_ADDRESS_SHIFT,
        );

        return r;
    }

    pub fn remote_invalidate_all_with_mask(mask: HartMask) -> Result<(), SbiRet> {
        let r = sbi_rt::remote_sfence_vma(mask, 0, 1 << RiscV64MMArch::ENTRY_ADDRESS_SHIFT);
        if r.is_ok() {
            return Ok(());
        } else {
            return Err(r);
        }
    }
}

/// 内核空间起始地址在顶层页表中的索引
const KERNEL_TOP_PAGE_ENTRY_NO: usize = (RiscV64MMArch::PHYS_OFFSET
    & ((1 << RiscV64MMArch::ENTRY_ADDRESS_SHIFT) - 1))
    >> (RiscV64MMArch::ENTRY_ADDRESS_SHIFT - RiscV64MMArch::PAGE_ENTRY_SHIFT);

impl MemoryManagementArch for RiscV64MMArch {
    /// riscv64暂不支持缺页中断
    const PAGE_FAULT_ENABLED: bool = false;

    const PAGE_SHIFT: usize = 12;

    const PAGE_ENTRY_SHIFT: usize = 9;

    /// sv39分页只有三级
    const PAGE_LEVELS: usize = 3;

    const ENTRY_ADDRESS_SHIFT: usize = 39;

    const ENTRY_FLAG_DEFAULT_PAGE: usize = Self::ENTRY_FLAG_PRESENT
        | Self::ENTRY_FLAG_READWRITE
        | Self::ENTRY_FLAG_DIRTY
        | Self::ENTRY_FLAG_ACCESSED
        | Self::ENTRY_FLAG_GLOBAL;

    const ENTRY_FLAG_DEFAULT_TABLE: usize = Self::ENTRY_FLAG_PRESENT;

    const ENTRY_FLAG_PRESENT: usize = 1 << 0;

    const ENTRY_FLAG_READONLY: usize = (1 << 1);

    const ENTRY_FLAG_WRITEABLE: usize = (1 << 2);

    const ENTRY_FLAG_READWRITE: usize = (1 << 2) | (1 << 1);

    const ENTRY_FLAG_USER: usize = (1 << 4);
    const ENTRY_ADDRESS_MASK: usize = Self::ENTRY_ADDRESS_SIZE - (1 << 10);
    const ENTRY_FLAG_WRITE_THROUGH: usize = (2 << 61);

    const ENTRY_FLAG_CACHE_DISABLE: usize = (2 << 61);

    const ENTRY_FLAG_NO_EXEC: usize = 0;

    const ENTRY_FLAG_EXEC: usize = (1 << 3);
    const ENTRY_FLAG_ACCESSED: usize = (1 << 6);
    const ENTRY_FLAG_DIRTY: usize = (1 << 7);
    const ENTRY_FLAG_GLOBAL: usize = (1 << 5);

    const PHYS_OFFSET: usize = 0xffff_ffc0_0000_0000;
    const KERNEL_LINK_OFFSET: usize = 0x1000000;

    const USER_END_VADDR: crate::mm::VirtAddr = VirtAddr::new(0x0000_003f_ffff_ffff);

    const USER_BRK_START: crate::mm::VirtAddr = VirtAddr::new(0x0000_001f_ffff_ffff);

    const USER_STACK_START: crate::mm::VirtAddr = VirtAddr::new(0x0000_001f_ffa0_0000);

    /// 在距离sv39的顶端还有64M的位置，设置为FIXMAP的起始地址
    const FIXMAP_START_VADDR: VirtAddr = VirtAddr::new(0xffff_ffff_fc00_0000);
    /// 设置1MB的fixmap空间
    const FIXMAP_SIZE: usize = 256 * 4096;

    /// 在距离sv39的顶端还有2G的位置，设置为MMIO空间的起始地址
    const MMIO_BASE: VirtAddr = VirtAddr::new(0xffff_ffff_8000_0000);
    /// 设置1g的MMIO空间
    const MMIO_SIZE: usize = 1 << PAGE_1G_SHIFT;

    const ENTRY_FLAG_HUGE_PAGE: usize = Self::ENTRY_FLAG_PRESENT | Self::ENTRY_FLAG_READWRITE;

    #[inline(never)]
    unsafe fn init() {
        riscv_mm_init().expect("init kernel memory management architecture failed");
    }

    unsafe fn arch_post_init() {
        // 映射fdt
        open_firmware_fdt_driver()
            .map_fdt()
            .expect("openfirmware map fdt failed");
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

    fn virt_is_valid(virt: VirtAddr) -> bool {
        virt.is_canonical()
    }

    fn initial_page_table() -> PhysAddr {
        unsafe { INITIAL_PGTABLE_VALUE }
    }

    fn setup_new_usermapper() -> Result<UserMapper, SystemError> {
        let new_umapper: crate::mm::page::PageMapper<MMArch, LockedFrameAllocator> = unsafe {
            PageMapper::create(PageTableKind::User, LockedFrameAllocator)
                .ok_or(SystemError::ENOMEM)?
        };

        let current_ktable: KernelMapper = KernelMapper::lock();
        let copy_mapping = |pml4_entry_no| unsafe {
            let entry: PageEntry<RiscV64MMArch> = current_ktable
                .table()
                .entry(pml4_entry_no)
                .unwrap_or_else(|| panic!("entry {} not found", pml4_entry_no));
            new_umapper.table().set_entry(pml4_entry_no, entry)
        };

        // 复制内核的映射
        for pml4_entry_no in KERNEL_TOP_PAGE_ENTRY_NO..512 {
            copy_mapping(pml4_entry_no);
        }

        return Ok(crate::mm::ucontext::UserMapper::new(new_umapper));
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
        let r = ((ppn & ((1 << 54) - 1)) << 10) | page_flags;
        return r;
    }

    fn vma_access_permitted(
        _vma: alloc::sync::Arc<crate::mm::ucontext::LockedVMA>,
        _write: bool,
        _execute: bool,
        _foreign: bool,
    ) -> bool {
        true
    }

    const PAGE_NONE: usize = Self::ENTRY_FLAG_GLOBAL | Self::ENTRY_FLAG_READONLY;

    const PAGE_READ: usize = PAGE_ENTRY_BASE | Self::ENTRY_FLAG_READONLY;

    const PAGE_WRITE: usize =
        PAGE_ENTRY_BASE | Self::ENTRY_FLAG_READONLY | Self::ENTRY_FLAG_WRITEABLE;

    const PAGE_EXEC: usize = PAGE_ENTRY_BASE | Self::ENTRY_FLAG_EXEC;

    const PAGE_READ_EXEC: usize =
        PAGE_ENTRY_BASE | Self::ENTRY_FLAG_READONLY | Self::ENTRY_FLAG_EXEC;

    const PAGE_WRITE_EXEC: usize = PAGE_ENTRY_BASE
        | Self::ENTRY_FLAG_READONLY
        | Self::ENTRY_FLAG_EXEC
        | Self::ENTRY_FLAG_WRITEABLE;

    const PAGE_COPY: usize = Self::PAGE_READ;
    const PAGE_COPY_EXEC: usize = Self::PAGE_READ_EXEC;
    const PAGE_SHARED: usize = Self::PAGE_WRITE;
    const PAGE_SHARED_EXEC: usize = Self::PAGE_WRITE_EXEC;

    const PAGE_COPY_NOEXEC: usize = 0;
    const PAGE_READONLY: usize = 0;
    const PAGE_READONLY_EXEC: usize = 0;

    const PROTECTION_MAP: [EntryFlags<MMArch>; 16] = protection_map();
}

const fn protection_map() -> [EntryFlags<MMArch>; 16] {
    let mut map = [0; 16];
    map[VmFlags::VM_NONE.bits()] = MMArch::PAGE_NONE;
    map[VmFlags::VM_READ.bits()] = MMArch::PAGE_READONLY;
    map[VmFlags::VM_WRITE.bits()] = MMArch::PAGE_COPY;
    map[VmFlags::VM_WRITE.bits() | VmFlags::VM_READ.bits()] = MMArch::PAGE_COPY;
    map[VmFlags::VM_EXEC.bits()] = MMArch::PAGE_READONLY_EXEC;
    map[VmFlags::VM_EXEC.bits() | VmFlags::VM_READ.bits()] = MMArch::PAGE_READONLY_EXEC;
    map[VmFlags::VM_EXEC.bits() | VmFlags::VM_WRITE.bits()] = MMArch::PAGE_COPY_EXEC;
    map[VmFlags::VM_EXEC.bits() | VmFlags::VM_WRITE.bits() | VmFlags::VM_READ.bits()] =
        MMArch::PAGE_COPY_EXEC;
    map[VmFlags::VM_SHARED.bits()] = MMArch::PAGE_NONE;
    map[VmFlags::VM_SHARED.bits() | VmFlags::VM_READ.bits()] = MMArch::PAGE_READONLY;
    map[VmFlags::VM_SHARED.bits() | VmFlags::VM_WRITE.bits()] = MMArch::PAGE_SHARED;
    map[VmFlags::VM_SHARED.bits() | VmFlags::VM_WRITE.bits() | VmFlags::VM_READ.bits()] =
        MMArch::PAGE_SHARED;
    map[VmFlags::VM_SHARED.bits() | VmFlags::VM_EXEC.bits()] = MMArch::PAGE_READONLY_EXEC;
    map[VmFlags::VM_SHARED.bits() | VmFlags::VM_EXEC.bits() | VmFlags::VM_READ.bits()] =
        MMArch::PAGE_READONLY_EXEC;
    map[VmFlags::VM_SHARED.bits() | VmFlags::VM_EXEC.bits() | VmFlags::VM_WRITE.bits()] =
        MMArch::PAGE_SHARED_EXEC;
    map[VmFlags::VM_SHARED.bits()
        | VmFlags::VM_EXEC.bits()
        | VmFlags::VM_WRITE.bits()
        | VmFlags::VM_READ.bits()] = MMArch::PAGE_SHARED_EXEC;
    let mut ret = [unsafe { EntryFlags::from_data(0) }; 16];
    let mut index = 0;
    while index < 16 {
        ret[index] = unsafe { EntryFlags::from_data(map[index]) };
        index += 1;
    }
    ret
}

const PAGE_ENTRY_BASE: usize = RiscV64MMArch::ENTRY_FLAG_PRESENT
    | RiscV64MMArch::ENTRY_FLAG_ACCESSED
    | RiscV64MMArch::ENTRY_FLAG_USER;

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
pub unsafe fn kernel_page_flags<A: MemoryManagementArch>(_virt: VirtAddr) -> EntryFlags<A> {
    EntryFlags::from_data(RiscV64MMArch::ENTRY_FLAG_DEFAULT_PAGE)
        .set_user(false)
        .set_execute(true)
}

/// 全局的页帧分配器
#[derive(Debug, Clone, Copy, Hash)]
pub struct LockedFrameAllocator;

impl FrameAllocator for LockedFrameAllocator {
    unsafe fn allocate(&mut self, count: PageFrameCount) -> Option<(PhysAddr, PageFrameCount)> {
        if let Some(ref mut allocator) = *INNER_ALLOCATOR.lock_irqsave() {
            return allocator.allocate(count);
        } else {
            return None;
        }
    }

    unsafe fn free(&mut self, address: crate::mm::PhysAddr, count: PageFrameCount) {
        assert!(count.data().is_power_of_two());
        if let Some(ref mut allocator) = *INNER_ALLOCATOR.lock_irqsave() {
            return allocator.free(address, count);
        }
    }

    unsafe fn usage(&self) -> PageFrameUsage {
        if let Some(ref mut allocator) = *INNER_ALLOCATOR.lock_irqsave() {
            return allocator.usage();
        } else {
            panic!("usage error");
        }
    }
}
