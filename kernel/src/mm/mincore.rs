use crate::arch::MMArch;
use crate::{
    arch::mm::PageMapper,
    mm::{ucontext::LockedVMA, MemoryManagementArch, VirtAddr},
};
use system_error::SystemError;

impl LockedVMA {
    pub fn do_mincore(
        &self,
        mapper: &PageMapper,
        vec: &mut [u8],
        start_addr: VirtAddr,
        end_addr: VirtAddr,
        offset: usize,
    ) -> Result<(), SystemError> {
        let total_pages = (end_addr - start_addr) >> MMArch::PAGE_SHIFT;
        if vec.len() < total_pages + offset {
            return Err(SystemError::EINVAL);
        }

        if !self.can_do_mincore() {
            let pages = (end_addr - start_addr) >> MMArch::PAGE_SHIFT;
            vec[offset..offset + pages].fill(0);
            return Ok(());
        }
        // 支持多级页表遍历；在遇到大页时按4K粒度填充
        self.mincore_walk_page_range(mapper, start_addr, end_addr, 3, vec, offset);
        Ok(())
    }

    fn mincore_walk_page_range(
        &self,
        mapper: &PageMapper,
        start_addr: VirtAddr,
        end_addr: VirtAddr,
        level: usize,
        vec: &mut [u8],
        vec_offset: usize,
    ) -> usize {
        let mut page_count = 0;
        let mut start = start_addr;
        while start < end_addr {
            let entry_size = MMArch::PAGE_SIZE << (level * MMArch::PAGE_ENTRY_SHIFT);
            let next = core::cmp::min(end_addr, start + entry_size);
            if let Some(entry) = mapper.get_entry(start, level) {
                // 大页处理：当上层条目标记为大页时，按子页数量批量填充
                if level > 0 && entry.flags().has_flag(MMArch::ENTRY_FLAG_HUGE_PAGE) {
                    let sub_pages = (next - start) >> MMArch::PAGE_SHIFT;
                    let val = if entry.present() { 1 } else { 0 };
                    vec[vec_offset + page_count..vec_offset + page_count + sub_pages].fill(val);
                    page_count += sub_pages;
                } else if level > 0 {
                    let sub_pages = self.mincore_walk_page_range(
                        mapper,
                        start,
                        next,
                        level - 1,
                        vec,
                        vec_offset + page_count,
                    );
                    page_count += sub_pages;
                } else {
                    vec[vec_offset + page_count] = if entry.present() { 1 } else { 0 };
                    page_count += 1;
                }
            } else {
                let unmapped_pages =
                    self.mincore_unmapped_range(start, next, vec, vec_offset + page_count);
                page_count += unmapped_pages;
            }
            start = next;
        }
        page_count
    }

    fn mincore_unmapped_range(
        &self,
        start_addr: VirtAddr,
        end_addr: VirtAddr,
        vec: &mut [u8],
        vec_offset: usize,
    ) -> usize {
        let nr = (end_addr - start_addr) >> MMArch::PAGE_SHIFT;
        if self.is_anonymous() {
            vec[vec_offset..vec_offset + nr].fill(0);
        } else {
            let guard = self.lock_irqsave();
            let pgoff = ((start_addr - guard.region().start()) >> MMArch::PAGE_SHIFT)
                + guard.file_page_offset().unwrap();
            if guard.vm_file().is_none() {
                vec[vec_offset..vec_offset + nr].fill(0);
                return nr;
            }
            let page_cache = guard.vm_file().unwrap().inode().page_cache();
            match page_cache {
                Some(page_cache) => {
                    let cache_guard = page_cache.lock_irqsave();
                    for i in 0..nr {
                        if cache_guard.get_page(pgoff + i).is_some() {
                            vec[vec_offset + i] = 1;
                        } else {
                            vec[vec_offset + i] = 0;
                        }
                    }
                }
                None => {
                    vec[vec_offset..vec_offset + nr].fill(0);
                }
            }
        }
        nr
    }

    pub fn can_do_mincore(&self) -> bool {
        //todo: 没有实现vm_ops,这里只能找到匿名映射和文件映射。对于设备映射和其他特殊映射（对应linux中vm_ops有值，但不是文件映射的vma），返回false
        if self.is_accessible() {
            return true;
        } else {
            //todo: 若文件不是当前用户所有，需要当前用户对文件有写权限,否则返回false
            return true;
        }
    }
}
