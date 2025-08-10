use core::sync::atomic::{compiler_fence, AtomicBool, Ordering};

use log::{debug, info};
use system_error::SystemError;

use crate::{
    arch::{
        mm::{
            kernel_page_flags, INNER_ALLOCATOR, KERNEL_BEGIN_PA, KERNEL_BEGIN_VA, KERNEL_END_PA,
            KERNEL_END_VA,
        },
        MMArch,
    },
    driver::firmware::efi::efi_manager,
    libs::lib_ui::screen_manager::scm_disable_put_to_window,
    mm::{
        allocator::{buddy::BuddyAllocator, bump::BumpAllocator, page_frame::FrameAllocator},
        kernel_mapper::KernelMapper,
        memblock::mem_block_manager,
        page::PageEntry,
        MemoryManagementArch, PageTableKind, PhysAddr, VirtAddr,
    },
};

use super::RiscV64MMArch;

pub(super) static mut INITIAL_PGTABLE_VALUE: PhysAddr = PhysAddr::new(0);

#[inline(never)]
pub fn mm_early_init() {
    unsafe { init_kernel_addr() };
}

unsafe fn init_kernel_addr() {
    extern "C" {
        /// 内核起始label
        fn boot_text_start_pa();
        /// 内核结束位置的label
        fn _end();

        fn _start();

        /// 内核start标签被加载到的物理地址
        fn __initial_start_load_paddr();
    }
    let initial_start_load_pa = *(__initial_start_load_paddr as usize as *const usize);
    let offset = _start as usize - boot_text_start_pa as usize;
    let start_pa = initial_start_load_pa - offset;

    let offset2 = _end as usize - boot_text_start_pa as usize;
    let end_pa = start_pa + offset2;

    KERNEL_BEGIN_PA = PhysAddr::new(start_pa);
    KERNEL_END_PA = PhysAddr::new(end_pa);

    KERNEL_BEGIN_VA = VirtAddr::new(boot_text_start_pa as usize);
    KERNEL_END_VA = VirtAddr::new(_end as usize);

    debug!(
        "init_kernel_addr: \n\tKERNEL_BEGIN_PA: {KERNEL_BEGIN_PA:?}
        \tKERNEL_END_PA: {KERNEL_END_PA:?}
        \tKERNEL_BEGIN_VA: {KERNEL_BEGIN_VA:?}
        \tKERNEL_END_VA: {KERNEL_END_VA:?}
    "
    );
}

pub(super) unsafe fn riscv_mm_init() -> Result<(), SystemError> {
    mem_block_manager()
        .reserve_block(KERNEL_BEGIN_PA, KERNEL_END_PA - KERNEL_BEGIN_PA)
        .expect("Failed to reserve kernel memory");
    // 开启S-mode User Memory Access，允许内核访问用户空间
    riscv::register::sstatus::set_sum();

    let mut bump_allocator = BumpAllocator::<RiscV64MMArch>::new(0);
    let _old_page_table = MMArch::table(PageTableKind::Kernel);
    let new_page_table: PhysAddr;

    // 使用bump分配器，把所有的内存页都映射到页表
    {
        // debug!("to create new page table");
        // 用bump allocator创建新的页表
        let mut mapper: crate::mm::page::PageMapper<MMArch, &mut BumpAllocator<MMArch>> =
            crate::mm::page::PageMapper::<MMArch, _>::create(
                PageTableKind::Kernel,
                &mut bump_allocator,
            )
            .expect("Failed to create page mapper");
        new_page_table = mapper.table().phys();
        // debug!("PageMapper created");

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
        debug!("Successfully emptied page table");

        let total_num = mem_block_manager().total_initial_memory_regions();
        for i in 0..total_num {
            let area = mem_block_manager().get_initial_memory_region(i).unwrap();
            // debug!("area: base={:?}, size={:#x}, end={:?}", area.base, area.size, area.base + area.size);
            for i in 0..((area.size + MMArch::PAGE_SIZE - 1) / MMArch::PAGE_SIZE) {
                let paddr = area.base.add(i * MMArch::PAGE_SIZE);
                let vaddr = unsafe { MMArch::phys_2_virt(paddr) }.unwrap();
                let flags = kernel_page_flags::<MMArch>(vaddr).set_execute(true);

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
        INITIAL_PGTABLE_VALUE = new_page_table;
    }
    debug!(
        "After mapping all physical memory, DragonOS used: {} KB",
        bump_allocator.usage().used().bytes() / 1024
    );

    // 初始化buddy_allocator
    let buddy_allocator = unsafe { BuddyAllocator::<MMArch>::new(bump_allocator).unwrap() };
    // 设置全局的页帧分配器
    unsafe { set_inner_allocator(buddy_allocator) };
    info!("Successfully initialized buddy allocator");
    // 关闭显示输出
    scm_disable_put_to_window();

    // make the new page table current
    {
        let mut binding = INNER_ALLOCATOR.lock();
        let mut allocator_guard = binding.as_mut().unwrap();
        debug!("To enable new page table.");
        compiler_fence(Ordering::SeqCst);
        let mapper = crate::mm::page::PageMapper::<MMArch, _>::new(
            PageTableKind::Kernel,
            new_page_table,
            &mut allocator_guard,
        );
        compiler_fence(Ordering::SeqCst);
        mapper.make_current();
        compiler_fence(Ordering::SeqCst);
        // debug!("New page table enabled");
    }
    debug!("Successfully enabled new page table");
    info!("riscv mm init done");

    return Ok(());
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
    pub unsafe fn remap_at_low_address(
        mapper: &mut crate::mm::page::PageMapper<MMArch, &mut BumpAllocator<MMArch>>,
    ) {
        let info = efi_manager().kernel_load_info().unwrap();
        let base = PhysAddr::new(info.paddr as usize);
        let size = info.size as usize;

        for i in 0..(size / MMArch::PAGE_SIZE) {
            let paddr = PhysAddr::new(base.data() + i * MMArch::PAGE_SIZE);
            let vaddr = VirtAddr::new(base.data() + i * MMArch::PAGE_SIZE);
            let flags = kernel_page_flags::<MMArch>(vaddr).set_execute(true);

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

        let info = efi_manager().kernel_load_info().unwrap();
        let base = PhysAddr::new(info.paddr as usize);
        let size = info.size as usize;

        for i in 0..(size / MMArch::PAGE_SIZE) {
            let vaddr = VirtAddr::new(base.data() + i * MMArch::PAGE_SIZE);
            let (_, _, flusher) = mapper
                .as_mut()
                .unwrap()
                .unmap_phys(vaddr, true)
                .expect("Failed to unmap frame");
            if flush == false {
                flusher.ignore();
            }
        }
    }
}
