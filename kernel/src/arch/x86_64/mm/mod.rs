pub mod barrier;
pub mod frame;

use crate::include::bindings::bindings::{
    multiboot2_get_memory, multiboot2_iter, multiboot_mmap_entry_t, process_control_block,
};
use crate::{kdebug, kinfo};
use crate::mm::{MemoryManagementArch, PageTableKind, PhysAddr, PhysMemoryArea, VirtAddr};
use crate::syscall::SystemError;

use core::arch::asm;
use core::ffi::c_void;
use core::fmt::Debug;
use core::mem;
use core::ptr::read_volatile;
use core::sync::atomic::{AtomicBool, Ordering};

use self::barrier::mfence;
use self::frame::LockedFrameAllocator;

pub type PageMapper =
    crate::mm::page::PageMapper<crate::arch::x86_64::mm::X86_64MMArch, LockedFrameAllocator>;

/// @brief 用于存储物理内存区域的数组
static mut PHYS_MEMORY_AREAS: [PhysMemoryArea; 512] = [PhysMemoryArea {
    base: PhysAddr::new(0),
    size: 0,
}; 512];

#[derive(Clone, Copy)]
pub struct X86_64MMBootstrapInfo {
    kernel_code_start: usize,
    kernel_code_end: usize,
    kernel_data_end: usize,
    kernel_rodata_end: usize,
    start_brk: usize,
}

impl Debug for X86_64MMBootstrapInfo {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        write!(
            f,
            "kernel_code_start: {:x}, kernel_code_end: {:x}, kernel_data_end: {:x}, kernel_rodata_end: {:x}, start_brk: {:x}",
            self.kernel_code_start, self.kernel_code_end, self.kernel_data_end, self.kernel_rodata_end, self.start_brk)
    }
}

pub static mut BOOTSTRAP_MM_INFO: Option<X86_64MMBootstrapInfo> = None;

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

/// @brief X86_64的内存管理架构结构体
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
    /// x86_64不存在EXEC标志位，只有NO_EXEC（XD）标志位
    const ENTRY_FLAG_EXEC: usize = 0;

    /// 物理地址与虚拟地址的偏移量
    /// 0xffff_8000_0000_0000
    const PHYS_OFFSET: usize = Self::PAGE_NEGATIVE_MASK + (Self::PAGE_ADDRESS_SIZE >> 1);

    /// @brief 获取物理内存区域
    unsafe fn init() -> &'static [crate::mm::PhysMemoryArea] {
        extern "C" {
            fn _text();
            fn _etext();
            fn _edata();
            fn _erodata();
            fn _end();
        }

        let bootstrap_info = X86_64MMBootstrapInfo {
            kernel_code_start: _text as usize,
            kernel_code_end: _etext as usize,
            kernel_data_end: _edata as usize,
            kernel_rodata_end: _erodata as usize,
            start_brk: _end as usize,
        };
        unsafe {
            BOOTSTRAP_MM_INFO = Some(bootstrap_info);
        }

        // 初始化物理内存区域(从multiboot2中获取)
        let areas_count =
            Self::init_memory_area_from_multiboot2().expect("init memory area failed");

        return &PHYS_MEMORY_AREAS[0..areas_count];
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

impl X86_64MMArch {
    unsafe fn init_memory_area_from_multiboot2() -> Result<usize, SystemError> {
        // 这个数组用来存放内存区域的信息（从C获取）
        let mut mb2_mem_info: [multiboot_mmap_entry_t; 512] = mem::zeroed();

        let mut mb2_count: u32 = 0;
        multiboot2_iter(
            Some(multiboot2_get_memory),
            &mut mb2_mem_info as *mut [multiboot_mmap_entry_t; 512] as usize as *mut c_void,
            &mut mb2_count,
        );

        let mb2_count = mb2_count as usize;
        let mut areas_count = 0usize;
        let mut total_mem_size = 0usize;
        for i in 0..mb2_count {
            // Only use the memory area if its type is 1 (RAM)
            if mb2_mem_info[i].type_ == 1 {
                // Skip the memory area if its len is 0
                if mb2_mem_info[i].len == 0 {
                    continue;
                }
                total_mem_size += mb2_mem_info[i].len as usize;
                PHYS_MEMORY_AREAS[areas_count].base = PhysAddr::new(mb2_mem_info[i].addr as usize);
                PHYS_MEMORY_AREAS[areas_count].size = mb2_mem_info[i].len as usize;
                areas_count += 1;
            }
        }
        kinfo!("Total memory size: {} MB, total areas from multiboot2: {mb2_count}, valid areas: {areas_count}", total_mem_size / 1024 / 1024);
        return Ok(areas_count);
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

/// @brief 初始化内存管理模块
pub fn mm_init() {
    static _CALL_ONCE: AtomicBool = AtomicBool::new(false);
    if _CALL_ONCE
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        panic!("mm_init() can only be called once");
    }
    unsafe { X86_64MMArch::init() };
    kdebug!("bootstrap info: {:?}", unsafe { BOOTSTRAP_MM_INFO });
    // todo: 初始化内存管理器
        
}
