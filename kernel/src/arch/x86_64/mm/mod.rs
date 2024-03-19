pub mod barrier;
pub mod bump;
mod c_adapter;

use alloc::vec::Vec;
use hashbrown::HashSet;
use x86::time::rdtsc;
use x86_64::registers::model_specific::EferFlags;

use crate::driver::serial::serial8250::send_to_default_serial8250_port;
use crate::include::bindings::bindings::{
    multiboot2_get_load_base, multiboot2_get_memory, multiboot2_iter, multiboot_mmap_entry_t,
    multiboot_tag_load_base_addr_t,
};
use crate::libs::align::page_align_up;
use crate::libs::lib_ui::screen_manager::scm_disable_put_to_window;
use crate::libs::spinlock::SpinLock;

use crate::mm::allocator::page_frame::{FrameAllocator, PageFrameCount, PageFrameUsage};
use crate::mm::memblock::mem_block_manager;
use crate::{
    arch::MMArch,
    mm::allocator::{buddy::BuddyAllocator, bump::BumpAllocator},
};

use crate::mm::kernel_mapper::KernelMapper;
use crate::mm::page::{PageEntry, PageFlags};
use crate::mm::{MemoryManagementArch, PageTableKind, PhysAddr, VirtAddr};
use crate::{kdebug, kinfo, kwarn};
use system_error::SystemError;

use core::arch::asm;
use core::ffi::c_void;
use core::fmt::Debug;
use core::mem::{self};

use core::sync::atomic::{compiler_fence, AtomicBool, Ordering};

use super::kvm::vmx::vmcs::VmcsFields;
use super::kvm::vmx::vmx_asm_wrapper::vmx_vmread;

pub type PageMapper =
    crate::mm::page::PageMapper<crate::arch::x86_64::mm::X86_64MMArch, LockedFrameAllocator>;

/// 初始的CR3寄存器的值，用于内存管理初始化时，创建的第一个内核页表的位置
static mut INITIAL_CR3_VALUE: PhysAddr = PhysAddr::new(0);

/// 内核的第一个页表在pml4中的索引
/// 顶级页表的[256, 512)项是内核的页表
static KERNEL_PML4E_NO: usize = (X86_64MMArch::PHYS_OFFSET & ((1 << 48) - 1)) >> 39;

static INNER_ALLOCATOR: SpinLock<Option<BuddyAllocator<MMArch>>> = SpinLock::new(None);

#[derive(Clone, Copy, Debug)]
pub struct X86_64MMBootstrapInfo {
    kernel_load_base_paddr: usize,
    kernel_code_start: usize,
    kernel_code_end: usize,
    kernel_data_end: usize,
    kernel_rodata_end: usize,
    start_brk: usize,
}

pub(super) static mut BOOTSTRAP_MM_INFO: Option<X86_64MMBootstrapInfo> = None;

/// @brief X86_64的内存管理架构结构体
#[derive(Debug, Clone, Copy, Hash)]
pub struct X86_64MMArch;

/// XD标志位是否被保留
static XD_RESERVED: AtomicBool = AtomicBool::new(false);

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

    const ENTRY_FLAG_ACCESSED: usize = 0;
    const ENTRY_FLAG_DIRTY: usize = 0;

    /// 物理地址与虚拟地址的偏移量
    /// 0xffff_8000_0000_0000
    const PHYS_OFFSET: usize = Self::PAGE_NEGATIVE_MASK + (Self::PAGE_ADDRESS_SIZE >> 1);
    const KERNEL_LINK_OFFSET: usize = 0x100000;

    // 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/arch/x86/include/asm/page_64_types.h#75
    const USER_END_VADDR: VirtAddr =
        VirtAddr::new((Self::PAGE_ADDRESS_SIZE >> 1) - Self::PAGE_SIZE);
    const USER_BRK_START: VirtAddr = VirtAddr::new(0x700000000000);
    const USER_STACK_START: VirtAddr = VirtAddr::new(0x6ffff0a00000);

    const FIXMAP_START_VADDR: VirtAddr = VirtAddr::new(0xffffb00000000000);
    /// 设置FIXMAP区域大小为1M
    const FIXMAP_SIZE: usize = 256 * 4096;

    /// @brief 获取物理内存区域
    unsafe fn init() {
        extern "C" {
            fn _text();
            fn _etext();
            fn _edata();
            fn _erodata();
            fn _end();
        }

        Self::init_xd_rsvd();
        let load_base_paddr = Self::get_load_base_paddr();

        let bootstrap_info = X86_64MMBootstrapInfo {
            kernel_load_base_paddr: load_base_paddr.data(),
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
        Self::init_memory_area_from_multiboot2().expect("init memory area failed");

        kdebug!("bootstrap info: {:?}", unsafe { BOOTSTRAP_MM_INFO });
        kdebug!("phys[0]=virt[0x{:x}]", unsafe {
            MMArch::phys_2_virt(PhysAddr::new(0)).unwrap().data()
        });

        // 初始化内存管理器
        unsafe { allocator_init() };
        send_to_default_serial8250_port("x86 64 init done\n\0".as_bytes());
    }

    /// @brief 刷新TLB中，关于指定虚拟地址的条目
    unsafe fn invalidate_page(address: VirtAddr) {
        compiler_fence(Ordering::SeqCst);
        asm!("invlpg [{0}]", in(reg) address.data(), options(nostack, preserves_flags));
        compiler_fence(Ordering::SeqCst);
    }

    /// @brief 刷新TLB中，所有的条目
    unsafe fn invalidate_all() {
        compiler_fence(Ordering::SeqCst);
        // 通过设置cr3寄存器，来刷新整个TLB
        Self::set_table(PageTableKind::User, Self::table(PageTableKind::User));
        compiler_fence(Ordering::SeqCst);
    }

    /// @brief 获取顶级页表的物理地址
    unsafe fn table(table_kind: PageTableKind) -> PhysAddr {
        match table_kind {
            PageTableKind::Kernel | PageTableKind::User => {
                let paddr: usize;
                compiler_fence(Ordering::SeqCst);
                asm!("mov {}, cr3", out(reg) paddr, options(nomem, nostack, preserves_flags));
                compiler_fence(Ordering::SeqCst);
                return PhysAddr::new(paddr);
            }
            PageTableKind::EPT => {
                let eptp =
                    vmx_vmread(VmcsFields::CTRL_EPTP_PTR as u32).expect("Failed to read eptp");
                return PhysAddr::new(eptp as usize);
            }
        }
    }

    /// @brief 设置顶级页表的物理地址到处理器中
    unsafe fn set_table(_table_kind: PageTableKind, table: PhysAddr) {
        compiler_fence(Ordering::SeqCst);
        asm!("mov cr3, {}", in(reg) table.data(), options(nostack, preserves_flags));
        compiler_fence(Ordering::SeqCst);
    }

    /// @brief 判断虚拟地址是否合法
    fn virt_is_valid(virt: VirtAddr) -> bool {
        return virt.is_canonical();
    }

    /// 获取内存管理初始化时，创建的第一个内核页表的地址
    fn initial_page_table() -> PhysAddr {
        unsafe {
            return INITIAL_CR3_VALUE;
        }
    }

    /// @brief 创建新的顶层页表
    ///
    /// 该函数会创建页表并复制内核的映射到新的页表中
    ///
    /// @return 新的页表
    fn setup_new_usermapper() -> Result<crate::mm::ucontext::UserMapper, SystemError> {
        let new_umapper: crate::mm::page::PageMapper<X86_64MMArch, LockedFrameAllocator> = unsafe {
            PageMapper::create(PageTableKind::User, LockedFrameAllocator)
                .ok_or(SystemError::ENOMEM)?
        };

        let current_ktable: KernelMapper = KernelMapper::lock();
        let copy_mapping = |pml4_entry_no| unsafe {
            let entry: PageEntry<X86_64MMArch> = current_ktable
                .table()
                .entry(pml4_entry_no)
                .unwrap_or_else(|| panic!("entry {} not found", pml4_entry_no));
            new_umapper.table().set_entry(pml4_entry_no, entry)
        };

        // 复制内核的映射
        for pml4_entry_no in KERNEL_PML4E_NO..512 {
            copy_mapping(pml4_entry_no);
        }

        return Ok(crate::mm::ucontext::UserMapper::new(new_umapper));
    }

    const PAGE_SIZE: usize = 1 << Self::PAGE_SHIFT;

    const PAGE_OFFSET_MASK: usize = Self::PAGE_SIZE - 1;

    const PAGE_MASK: usize = !(Self::PAGE_OFFSET_MASK);

    const PAGE_ADDRESS_SHIFT: usize = Self::PAGE_LEVELS * Self::PAGE_ENTRY_SHIFT + Self::PAGE_SHIFT;

    const PAGE_ADDRESS_SIZE: usize = 1 << Self::PAGE_ADDRESS_SHIFT;

    const PAGE_ADDRESS_MASK: usize = Self::PAGE_ADDRESS_SIZE - Self::PAGE_SIZE;

    const PAGE_ENTRY_SIZE: usize = 1 << (Self::PAGE_SHIFT - Self::PAGE_ENTRY_SHIFT);

    const PAGE_ENTRY_NUM: usize = 1 << Self::PAGE_ENTRY_SHIFT;

    const PAGE_ENTRY_MASK: usize = Self::PAGE_ENTRY_NUM - 1;

    const PAGE_NEGATIVE_MASK: usize = !((Self::PAGE_ADDRESS_SIZE) - 1);

    const ENTRY_ADDRESS_SIZE: usize = 1 << Self::ENTRY_ADDRESS_SHIFT;

    const ENTRY_ADDRESS_MASK: usize = Self::ENTRY_ADDRESS_SIZE - Self::PAGE_SIZE;

    const ENTRY_FLAGS_MASK: usize = !Self::ENTRY_ADDRESS_MASK;

    unsafe fn read<T>(address: VirtAddr) -> T {
        return core::ptr::read(address.data() as *const T);
    }

    unsafe fn write<T>(address: VirtAddr, value: T) {
        core::ptr::write(address.data() as *mut T, value);
    }

    unsafe fn write_bytes(address: VirtAddr, value: u8, count: usize) {
        core::ptr::write_bytes(address.data() as *mut u8, value, count);
    }

    unsafe fn phys_2_virt(phys: PhysAddr) -> Option<VirtAddr> {
        if let Some(vaddr) = phys.data().checked_add(Self::PHYS_OFFSET) {
            return Some(VirtAddr::new(vaddr));
        } else {
            return None;
        }
    }

    unsafe fn virt_2_phys(virt: VirtAddr) -> Option<PhysAddr> {
        if let Some(paddr) = virt.data().checked_sub(Self::PHYS_OFFSET) {
            return Some(PhysAddr::new(paddr));
        } else {
            return None;
        }
    }

    #[inline(always)]
    fn make_entry(paddr: PhysAddr, page_flags: usize) -> usize {
        return paddr.data() | page_flags;
    }
}

impl X86_64MMArch {
    unsafe fn get_load_base_paddr() -> PhysAddr {
        let mut mb2_lb_info: [multiboot_tag_load_base_addr_t; 512] = mem::zeroed();
        send_to_default_serial8250_port("get_load_base_paddr begin\n\0".as_bytes());

        let mut mb2_count: u32 = 0;
        multiboot2_iter(
            Some(multiboot2_get_load_base),
            &mut mb2_lb_info as *mut [multiboot_tag_load_base_addr_t; 512] as usize as *mut c_void,
            &mut mb2_count,
        );

        if mb2_count == 0 {
            send_to_default_serial8250_port(
                "get_load_base_paddr mb2_count == 0, default to 1MB\n\0".as_bytes(),
            );
            return PhysAddr::new(0x100000);
        }

        let phys = mb2_lb_info[0].load_base_addr as usize;

        return PhysAddr::new(phys);
    }
    unsafe fn init_memory_area_from_multiboot2() -> Result<usize, SystemError> {
        // 这个数组用来存放内存区域的信息（从C获取）
        let mut mb2_mem_info: [multiboot_mmap_entry_t; 512] = mem::zeroed();
        send_to_default_serial8250_port("init_memory_area_from_multiboot2 begin\n\0".as_bytes());

        let mut mb2_count: u32 = 0;
        multiboot2_iter(
            Some(multiboot2_get_memory),
            &mut mb2_mem_info as *mut [multiboot_mmap_entry_t; 512] as usize as *mut c_void,
            &mut mb2_count,
        );
        send_to_default_serial8250_port("init_memory_area_from_multiboot2 2\n\0".as_bytes());

        let mb2_count = mb2_count as usize;
        let mut areas_count = 0usize;
        let mut total_mem_size = 0usize;
        for info_entry in mb2_mem_info.iter().take(mb2_count) {
            // Only use the memory area if its type is 1 (RAM)
            if info_entry.type_ == 1 {
                // Skip the memory area if its len is 0
                if info_entry.len == 0 {
                    continue;
                }

                total_mem_size += info_entry.len as usize;

                mem_block_manager()
                    .add_block(
                        PhysAddr::new(info_entry.addr as usize),
                        info_entry.len as usize,
                    )
                    .unwrap_or_else(|e| {
                        kwarn!(
                            "Failed to add memory block: base={:#x}, size={:#x}, error={:?}",
                            info_entry.addr,
                            info_entry.len,
                            e
                        );
                    });
                areas_count += 1;
            }
        }
        send_to_default_serial8250_port("init_memory_area_from_multiboot2 end\n\0".as_bytes());
        kinfo!("Total memory size: {} MB, total areas from multiboot2: {mb2_count}, valid areas: {areas_count}", total_mem_size / 1024 / 1024);
        return Ok(areas_count);
    }

    fn init_xd_rsvd() {
        // 读取ia32-EFER寄存器的值
        let efer: EferFlags = x86_64::registers::model_specific::Efer::read();
        if !efer.contains(EferFlags::NO_EXECUTE_ENABLE) {
            // NO_EXECUTE_ENABLE是false，那么就设置xd_reserved为true
            kdebug!("NO_EXECUTE_ENABLE is false, set XD_RESERVED to true");
            XD_RESERVED.store(true, Ordering::Relaxed);
        }
        compiler_fence(Ordering::SeqCst);
    }

    /// 判断XD标志位是否被保留
    pub fn is_xd_reserved() -> bool {
        // return XD_RESERVED.load(Ordering::Relaxed);

        // 由于暂时不支持execute disable，因此直接返回true
        // 不支持的原因是，目前好像没有能正确的设置page-level的xd位，会触发page fault
        return true;
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

unsafe fn allocator_init() {
    let virt_offset = BOOTSTRAP_MM_INFO.unwrap().start_brk;
    let phy_offset =
        unsafe { MMArch::virt_2_phys(VirtAddr::new(page_align_up(virt_offset))) }.unwrap();

    let mut bump_allocator = BumpAllocator::<X86_64MMArch>::new(phy_offset.data());
    kdebug!(
        "BumpAllocator created, offset={:?}",
        bump_allocator.offset()
    );

    // 暂存初始在head.S中指定的页表的地址，后面再考虑是否需要把它加到buddy的可用空间里面！
    // 现在不加的原因是，我担心会有安全漏洞问题：这些初始的页表，位于内核的数据段。如果归还到buddy，
    // 可能会产生一定的安全风险（有的代码可能根据虚拟地址来进行安全校验）
    let _old_page_table = MMArch::table(PageTableKind::Kernel);

    let new_page_table: PhysAddr;
    // 使用bump分配器，把所有的内存页都映射到页表
    {
        // 用bump allocator创建新的页表
        let mut mapper: crate::mm::page::PageMapper<MMArch, &mut BumpAllocator<MMArch>> =
            crate::mm::page::PageMapper::<MMArch, _>::create(
                PageTableKind::Kernel,
                &mut bump_allocator,
            )
            .expect("Failed to create page mapper");
        new_page_table = mapper.table().phys();
        kdebug!("PageMapper created");

        // 取消最开始时候，在head.S中指定的映射(暂时不刷新TLB)
        {
            let table = mapper.table();
            let empty_entry = PageEntry::<MMArch>::from_usize(0);
            for i in 0..MMArch::PAGE_ENTRY_NUM {
                table
                    .set_entry(i, empty_entry)
                    .expect("Failed to empty page table entry");
            }
        }
        kdebug!("Successfully emptied page table");

        let total_num = mem_block_manager().total_initial_memory_regions();
        for i in 0..total_num {
            let area = mem_block_manager().get_initial_memory_region(i).unwrap();
            // kdebug!("area: base={:?}, size={:#x}, end={:?}", area.base, area.size, area.base + area.size);
            for i in 0..((area.size + MMArch::PAGE_SIZE - 1) / MMArch::PAGE_SIZE) {
                let paddr = area.base.add(i * MMArch::PAGE_SIZE);
                let vaddr = unsafe { MMArch::phys_2_virt(paddr) }.unwrap();
                let flags = kernel_page_flags::<MMArch>(vaddr);

                let flusher = mapper
                    .map_phys(vaddr, paddr, flags)
                    .expect("Failed to map frame");
                // 暂时不刷新TLB
                flusher.ignore();
            }
        }

        // 添加低地址的映射（在smp完成初始化之前，需要使用低地址的映射.初始化之后需要取消这一段映射）
        LowAddressRemapping::remap_at_low_address(&mut mapper);
    }

    unsafe {
        INITIAL_CR3_VALUE = new_page_table;
    }
    kdebug!(
        "After mapping all physical memory, DragonOS used: {} KB",
        bump_allocator.offset() / 1024
    );

    // 初始化buddy_allocator
    let buddy_allocator = unsafe { BuddyAllocator::<X86_64MMArch>::new(bump_allocator).unwrap() };
    // 设置全局的页帧分配器
    unsafe { set_inner_allocator(buddy_allocator) };
    kinfo!("Successfully initialized buddy allocator");
    // 关闭显示输出
    scm_disable_put_to_window();

    // make the new page table current
    {
        let mut binding = INNER_ALLOCATOR.lock();
        let mut allocator_guard = binding.as_mut().unwrap();
        kdebug!("To enable new page table.");
        compiler_fence(Ordering::SeqCst);
        let mapper = crate::mm::page::PageMapper::<MMArch, _>::new(
            PageTableKind::Kernel,
            new_page_table,
            &mut allocator_guard,
        );
        compiler_fence(Ordering::SeqCst);
        mapper.make_current();
        compiler_fence(Ordering::SeqCst);
        kdebug!("New page table enabled");
    }
    kdebug!("Successfully enabled new page table");
}

#[no_mangle]
pub extern "C" fn rs_test_buddy() {
    test_buddy();
}
pub fn test_buddy() {
    // 申请内存然后写入数据然后free掉
    // 总共申请200MB内存
    const TOTAL_SIZE: usize = 200 * 1024 * 1024;

    for i in 0..10 {
        kdebug!("Test buddy, round: {i}");
        // 存放申请的内存块
        let mut v: Vec<(PhysAddr, PageFrameCount)> = Vec::with_capacity(60 * 1024);
        // 存放已经申请的内存块的地址（用于检查重复）
        let mut addr_set: HashSet<PhysAddr> = HashSet::new();

        let mut allocated = 0usize;

        let mut free_count = 0usize;

        while allocated < TOTAL_SIZE {
            let mut random_size = 0u64;
            unsafe { x86::random::rdrand64(&mut random_size) };
            // 一次最多申请4M
            random_size %= 1024 * 4096;
            if random_size == 0 {
                continue;
            }
            let random_size =
                core::cmp::min(page_align_up(random_size as usize), TOTAL_SIZE - allocated);
            let random_size = PageFrameCount::from_bytes(random_size.next_power_of_two()).unwrap();
            // 获取帧
            let (paddr, allocated_frame_count) =
                unsafe { LockedFrameAllocator.allocate(random_size).unwrap() };
            assert!(allocated_frame_count.data().is_power_of_two());
            assert!(paddr.data() % MMArch::PAGE_SIZE == 0);
            unsafe {
                assert!(MMArch::phys_2_virt(paddr)
                    .as_ref()
                    .unwrap()
                    .check_aligned(allocated_frame_count.data() * MMArch::PAGE_SIZE));
            }
            allocated += allocated_frame_count.data() * MMArch::PAGE_SIZE;
            v.push((paddr, allocated_frame_count));
            assert!(addr_set.insert(paddr), "duplicate address: {:?}", paddr);

            // 写入数据
            let vaddr = unsafe { MMArch::phys_2_virt(paddr).unwrap() };
            let slice = unsafe {
                core::slice::from_raw_parts_mut(
                    vaddr.data() as *mut u8,
                    allocated_frame_count.data() * MMArch::PAGE_SIZE,
                )
            };
            for (i, item) in slice.iter_mut().enumerate() {
                *item = ((i + unsafe { rdtsc() } as usize) % 256) as u8;
            }

            // 随机释放一个内存块
            if !v.is_empty() {
                let mut random_index = 0u64;
                unsafe { x86::random::rdrand64(&mut random_index) };
                // 70%概率释放
                if random_index % 10 > 7 {
                    continue;
                }
                random_index %= v.len() as u64;
                let random_index = random_index as usize;
                let (paddr, allocated_frame_count) = v.remove(random_index);
                assert!(addr_set.remove(&paddr));
                unsafe { LockedFrameAllocator.free(paddr, allocated_frame_count) };
                free_count += allocated_frame_count.data() * MMArch::PAGE_SIZE;
            }
        }

        kdebug!(
            "Allocated {} MB memory, release: {} MB, no release: {} bytes",
            allocated / 1024 / 1024,
            free_count / 1024 / 1024,
            (allocated - free_count)
        );

        kdebug!("Now, to release buddy memory");
        // 释放所有的内存
        for (paddr, allocated_frame_count) in v {
            unsafe { LockedFrameAllocator.free(paddr, allocated_frame_count) };
            assert!(addr_set.remove(&paddr));
            free_count += allocated_frame_count.data() * MMArch::PAGE_SIZE;
        }

        kdebug!("release done!, allocated: {allocated}, free_count: {free_count}");
    }
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

/// 获取内核地址默认的页面标志
pub unsafe fn kernel_page_flags<A: MemoryManagementArch>(virt: VirtAddr) -> PageFlags<A> {
    let info: X86_64MMBootstrapInfo = BOOTSTRAP_MM_INFO.unwrap();

    if virt.data() >= info.kernel_code_start && virt.data() < info.kernel_code_end {
        // Remap kernel code  execute
        return PageFlags::new().set_execute(true).set_write(true);
    } else if virt.data() >= info.kernel_data_end && virt.data() < info.kernel_rodata_end {
        // Remap kernel rodata read only
        return PageFlags::new().set_execute(true);
    } else {
        return PageFlags::new().set_write(true).set_execute(true);
    }
}

unsafe fn set_inner_allocator(allocator: BuddyAllocator<MMArch>) {
    static FLAG: AtomicBool = AtomicBool::new(false);
    if FLAG
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        panic!("Cannot set inner allocator twice!");
    }
    *INNER_ALLOCATOR.lock() = Some(allocator);
}

/// 低地址重映射的管理器
///
/// 低地址重映射的管理器，在smp初始化完成之前，需要使用低地址的映射，因此需要在smp初始化完成之后，取消这一段映射
pub struct LowAddressRemapping;

impl LowAddressRemapping {
    // 映射64M
    const REMAP_SIZE: usize = 64 * 1024 * 1024;

    pub unsafe fn remap_at_low_address(
        mapper: &mut crate::mm::page::PageMapper<MMArch, &mut BumpAllocator<MMArch>>,
    ) {
        for i in 0..(Self::REMAP_SIZE / MMArch::PAGE_SIZE) {
            let paddr = PhysAddr::new(i * MMArch::PAGE_SIZE);
            let vaddr = VirtAddr::new(i * MMArch::PAGE_SIZE);
            let flags = kernel_page_flags::<MMArch>(vaddr);

            let flusher = mapper
                .map_phys(vaddr, paddr, flags)
                .expect("Failed to map frame");
            // 暂时不刷新TLB
            flusher.ignore();
        }
    }

    /// 取消低地址的映射
    pub unsafe fn unmap_at_low_address(flush: bool) {
        let mut mapper = KernelMapper::lock();
        assert!(mapper.as_mut().is_some());
        for i in 0..(Self::REMAP_SIZE / MMArch::PAGE_SIZE) {
            let vaddr = VirtAddr::new(i * MMArch::PAGE_SIZE);
            let (_, _, flusher) = mapper
                .as_mut()
                .unwrap()
                .unmap_phys(vaddr, true)
                .expect("Failed to unmap frame");
            if !flush {
                flusher.ignore();
            }
        }
    }
}
