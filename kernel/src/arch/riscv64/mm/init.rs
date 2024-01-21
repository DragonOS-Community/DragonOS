use system_error::SystemError;

use crate::{
    arch::{
        mm::{KERNEL_BEGIN_PA, KERNEL_BEGIN_VA, KERNEL_END_PA, KERNEL_END_VA},
        MMArch,
    },
    kdebug,
    mm::{
        allocator::page_frame::PageFrameCount,
        no_init::{pseudo_map_phys, EARLY_IOREMAP_PAGES},
        page::{PageEntry, PageMapper, PageTable},
        MemoryManagementArch, PageTableKind, PhysAddr, VirtAddr,
    },
};

#[inline(never)]
pub fn mm_early_init() {
    unsafe { init_kernel_addr() };
    // unsafe { map_initial_page_table_linearly() };
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

    kdebug!(
        "init_kernel_addr: \n\tKERNEL_BEGIN_PA: {KERNEL_BEGIN_PA:?}
        \tKERNEL_END_PA: {KERNEL_END_PA:?}
        \tKERNEL_BEGIN_VA: {KERNEL_BEGIN_VA:?}
        \tKERNEL_END_VA: {KERNEL_END_VA:?}
    "
    );
}
