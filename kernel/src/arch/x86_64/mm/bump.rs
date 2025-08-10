use crate::{
    libs::align::{page_align_down, page_align_up},
    mm::{
        allocator::bump::BumpAllocator,
        memblock::{mem_block_manager, MemoryAreaAttr},
        MemoryManagementArch, PhysAddr, PhysMemoryArea, VirtAddr,
    },
};

use super::{X86_64MMBootstrapInfo, BOOTSTRAP_MM_INFO};

impl<MMA: MemoryManagementArch> BumpAllocator<MMA> {
    pub unsafe fn arch_remain_areas(
        ret_areas: &mut [PhysMemoryArea],
        mut res_count: usize,
    ) -> usize {
        let info: X86_64MMBootstrapInfo = BOOTSTRAP_MM_INFO.unwrap();
        let load_base = info.kernel_load_base_paddr;
        let kernel_code_start = MMA::virt_2_phys(VirtAddr::new(info.kernel_code_start))
            .unwrap()
            .data();

        let offset_start = page_align_up(core::cmp::max(load_base + 16384, 0x200000));
        let offset_end = page_align_down(kernel_code_start - 16384);

        // 把内核代码前的空间加入到可用内存区域中
        for area in mem_block_manager().to_iter() {
            let area_base = area.area_base_aligned().data();
            let area_end = area.area_end_aligned().data();
            if area_base >= offset_end {
                break;
            }

            if area_end <= offset_start {
                continue;
            }

            let new_start = core::cmp::max(offset_start, area_base);
            let new_end = core::cmp::min(offset_end, area_end);

            if new_start >= new_end {
                continue;
            }

            ret_areas[res_count] = PhysMemoryArea::new(
                PhysAddr::new(new_start),
                new_end - new_start,
                MemoryAreaAttr::empty(),
            );

            res_count += 1;
        }

        return res_count;
    }
}
