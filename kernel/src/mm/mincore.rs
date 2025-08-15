use crate::arch::MMArch;
use alloc::vec::Vec;

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
        if !(self.is_anonymous()) {
            //todo: 当进程是否拥有文件写权限或是文件所有者，才允许对映射了文件的vma调用mincore，否则将对应地址范围的位图置为0
        }
        //todo: 处理大页
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
                if level > 0 {
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
            for i in 0..nr {
                vec[vec_offset + i] = 0;
            }
        } else {
            let guard = self.lock_irqsave();
            let pgoff = ((start_addr - guard.region().start()) >> MMArch::PAGE_SHIFT)
                + guard.file_page_offset().unwrap();
            let page_cache = guard.vm_file().unwrap().inode().page_cache();
            match page_cache {
                Some(page_cache) => {
                    for i in 0..nr {
                        if page_cache.lock_irqsave().get_page(pgoff + i).is_some() {
                            vec[vec_offset + i] = 1;
                        } else {
                            vec[vec_offset + i] = 0;
                        }
                    }
                }
                None => {
                    for i in 0..nr {
                        vec[vec_offset + i] = 0;
                    }
                }
            }
        }
        nr
    }
}
