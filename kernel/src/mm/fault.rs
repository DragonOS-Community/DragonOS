use core::{
    alloc::Layout,
    cmp::{max, min},
    intrinsics::unlikely,
    panic,
};

use alloc::{sync::Arc, vec::Vec};

use crate::{
    arch::{mm::PageMapper, MMArch},
    libs::align::align_down,
    mm::{
        page::{page_manager_lock_irqsave, EntryFlags},
        ucontext::LockedVMA,
        VirtAddr, VmFaultReason, VmFlags,
    },
    process::{ProcessManager, ProcessState},
};

use crate::mm::MemoryManagementArch;

use super::{
    allocator::page_frame::{FrameAllocator, PhysPageFrame},
    page::{Page, PageFlags},
    phys_2_virt,
};

bitflags! {
    pub struct FaultFlags: u64{
        const FAULT_FLAG_WRITE = 1 << 0;
        const FAULT_FLAG_MKWRITE = 1 << 1;
        const FAULT_FLAG_ALLOW_RETRY = 1 << 2;
        const FAULT_FLAG_RETRY_NOWAIT = 1 << 3;
        const FAULT_FLAG_KILLABLE = 1 << 4;
        const FAULT_FLAG_TRIED = 1 << 5;
        const FAULT_FLAG_USER = 1 << 6;
        const FAULT_FLAG_REMOTE = 1 << 7;
        const FAULT_FLAG_INSTRUCTION = 1 << 8;
        const FAULT_FLAG_INTERRUPTIBLE =1 << 9;
        const FAULT_FLAG_UNSHARE = 1 << 10;
        const FAULT_FLAG_ORIG_PTE_VALID = 1 << 11;
        const FAULT_FLAG_VMA_LOCK = 1 << 12;
    }
}

/// # 缺页异常信息结构体
/// 包含了页面错误处理的相关信息，例如出错的地址、VMA等
#[derive(Debug)]
pub struct PageFaultMessage {
    /// 产生缺页的VMA结构体
    vma: Arc<LockedVMA>,
    /// 缺页地址
    address: VirtAddr,
    /// 异常处理标志
    flags: FaultFlags,
    /// 缺页的文件页在文件中的偏移量
    file_pgoff: Option<usize>,
    /// 缺页对应PageCache中的文件页
    page: Option<Arc<Page>>,
}

impl PageFaultMessage {
    pub fn new(vma: Arc<LockedVMA>, address: VirtAddr, flags: FaultFlags) -> Self {
        let guard = vma.lock();
        let file_pgoff = guard.file_page_offset().map(|file_page_offset| {
            ((address - guard.region().start()) >> MMArch::PAGE_SHIFT) + file_page_offset
        });
        Self {
            vma: vma.clone(),
            address,
            flags,
            file_pgoff,
            page: None,
        }
    }

    #[inline(always)]
    #[allow(dead_code)]
    pub fn vma(&self) -> Arc<LockedVMA> {
        self.vma.clone()
    }

    #[inline(always)]
    #[allow(dead_code)]
    pub fn address(&self) -> &VirtAddr {
        &self.address
    }

    #[inline(always)]
    #[allow(dead_code)]
    pub fn address_aligned_down(&self) -> VirtAddr {
        VirtAddr::new(crate::libs::align::page_align_down(self.address.data()))
    }

    #[inline(always)]
    #[allow(dead_code)]
    pub fn flags(&self) -> &FaultFlags {
        &self.flags
    }
}

impl Clone for PageFaultMessage {
    fn clone(&self) -> Self {
        Self {
            vma: self.vma.clone(),
            address: self.address,
            flags: self.flags,
            file_pgoff: self.file_pgoff,
            page: None,
        }
    }
}

/// 缺页中断处理结构体
pub struct PageFaultHandler;

impl PageFaultHandler {
    /// 处理缺页异常
    /// ## 参数
    ///
    /// - `pfm`: 缺页异常信息
    /// - `mapper`: 页表映射器
    ///
    /// ## 返回值
    /// - VmFaultReason: 页面错误处理信息标志
    pub unsafe fn handle_mm_fault(pfm: PageFaultMessage, mapper: &mut PageMapper) -> VmFaultReason {
        let flags = pfm.flags();
        let vma = pfm.vma();
        let current_pcb = ProcessManager::current_pcb();
        let mut guard = current_pcb.sched_info().inner_lock_write_irqsave();
        guard.set_state(ProcessState::Runnable);

        if !MMArch::vma_access_permitted(
            vma.clone(),
            flags.contains(FaultFlags::FAULT_FLAG_WRITE),
            flags.contains(FaultFlags::FAULT_FLAG_INSTRUCTION),
            flags.contains(FaultFlags::FAULT_FLAG_REMOTE),
        ) {
            return VmFaultReason::VM_FAULT_SIGSEGV;
        }

        let guard = vma.lock();
        let vm_flags = *guard.vm_flags();
        drop(guard);
        if unlikely(vm_flags.contains(VmFlags::VM_HUGETLB)) {
            //TODO: 添加handle_hugetlb_fault处理大页缺页异常
        } else {
            Self::handle_normal_fault(pfm, mapper);
        }

        VmFaultReason::VM_FAULT_COMPLETED
    }

    /// 处理普通页缺页异常
    /// ## 参数
    ///
    /// - `pfm`: 缺页异常信息
    /// - `mapper`: 页表映射器
    ///
    /// ## 返回值
    /// - VmFaultReason: 页面错误处理信息标志
    pub unsafe fn handle_normal_fault(
        pfm: PageFaultMessage,
        mapper: &mut PageMapper,
    ) -> VmFaultReason {
        let address = pfm.address_aligned_down();
        let vma = pfm.vma.clone();
        if mapper.get_entry(address, 3).is_none() {
            mapper
                .allocate_table(address, 2)
                .expect("failed to allocate PUD table");
        }
        let page_flags = vma.lock().flags();

        for level in 2..=3 {
            let level = MMArch::PAGE_LEVELS - level;
            if mapper.get_entry(address, level).is_none() {
                if vma.is_hugepage() {
                    if vma.is_anonymous() {
                        mapper.map_huge_page(address, page_flags);
                    }
                } else if mapper.allocate_table(address, level - 1).is_none() {
                    return VmFaultReason::VM_FAULT_OOM;
                }
            }
        }

        Self::handle_pte_fault(pfm, mapper)
    }

    /// 处理页表项异常
    /// ## 参数
    ///
    /// - `pfm`: 缺页异常信息
    /// - `mapper`: 页表映射器
    ///
    /// ## 返回值
    /// - VmFaultReason: 页面错误处理信息标志
    pub unsafe fn handle_pte_fault(
        pfm: PageFaultMessage,
        mapper: &mut PageMapper,
    ) -> VmFaultReason {
        if pfm.address.data() == 0x10000 {
            log::info!("handle_pte_fault: {:?}", pfm);
        }
        let address = pfm.address_aligned_down();
        let flags = pfm.flags;
        let vma = pfm.vma.clone();
        let mut ret = VmFaultReason::VM_FAULT_COMPLETED;
        if let Some(mut entry) = mapper.get_entry(address, 0) {
            if !entry.present() {
                ret = Self::do_swap_page(pfm.clone(), mapper);
            }
            if entry.protnone() && vma.is_accessible() {
                ret = Self::do_numa_page(pfm.clone(), mapper);
            }
            if flags.intersects(FaultFlags::FAULT_FLAG_WRITE | FaultFlags::FAULT_FLAG_UNSHARE) {
                if !entry.write() {
                    ret = Self::do_wp_page(pfm.clone(), mapper);
                } else {
                    entry.set_flags(EntryFlags::from_data(MMArch::ENTRY_FLAG_DIRTY));
                }
            }
        } else if vma.is_anonymous() {
            ret = Self::do_anonymous_page(pfm.clone(), mapper);
        } else {
            ret = Self::do_fault(pfm.clone(), mapper);
        }

        vma.lock().set_mapped(true);

        return ret;
    }

    /// 处理匿名映射页缺页异常
    /// ## 参数
    ///
    /// - `pfm`: 缺页异常信息
    /// - `mapper`: 页表映射器
    ///
    /// ## 返回值
    /// - VmFaultReason: 页面错误处理信息标志
    pub unsafe fn do_anonymous_page(
        pfm: PageFaultMessage,
        mapper: &mut PageMapper,
    ) -> VmFaultReason {
        let address = pfm.address_aligned_down();
        let vma = pfm.vma.clone();
        let guard = vma.lock();
        if let Some(flush) = mapper.map(address, guard.flags()) {
            flush.flush();
            crate::debug::klog::mm::mm_debug_log(
                klog_types::AllocatorLogType::LazyAlloc(klog_types::AllocLogItem::new(
                    Layout::from_size_align(MMArch::PAGE_SIZE, MMArch::PAGE_SIZE).unwrap(),
                    Some(address.data()),
                    Some(mapper.translate(address).unwrap().0.data()),
                )),
                klog_types::LogSource::Buddy,
            );
            let paddr = mapper.translate(address).unwrap().0;
            let mut anon_vma_guard = page_manager_lock_irqsave();
            let page = anon_vma_guard.get_mut(&paddr);
            page.insert_vma(vma.clone());
            VmFaultReason::VM_FAULT_COMPLETED
        } else {
            VmFaultReason::VM_FAULT_OOM
        }
    }

    /// 处理文件映射页的缺页异常
    /// ## 参数
    ///
    /// - `pfm`: 缺页异常信息
    /// - `mapper`: 页表映射器
    ///
    /// ## 返回值
    /// - VmFaultReason: 页面错误处理信息标志
    #[allow(unused_variables)]
    pub unsafe fn do_fault(pfm: PageFaultMessage, mapper: &mut PageMapper) -> VmFaultReason {
        log::info!("do_fault");
        // panic!(
        //     "do_fault has not yet been implemented,
        // fault message: {:?},
        // pid: {}\n",
        //     pfm,
        //     crate::process::ProcessManager::current_pid().data()
        // );
        // TODO https://code.dragonos.org.cn/xref/linux-6.6.21/mm/memory.c#do_fault

        if !pfm.flags().contains(FaultFlags::FAULT_FLAG_WRITE) {
            return Self::do_read_fault(pfm, mapper);
        } else if !pfm.vma().lock().vm_flags().contains(VmFlags::VM_SHARED) {
            return Self::do_cow_fault(pfm, mapper);
        } else {
            return Self::do_shared_fault(pfm, mapper);
        }
    }

    /// 处理私有文件映射的写时复制
    /// ## 参数
    ///
    /// - `pfm`: 缺页异常信息
    /// - `mapper`: 页表映射器
    ///
    /// ## 返回值
    /// - VmFaultReason: 页面错误处理信息标志
    #[allow(dead_code, unused_variables)]
    pub unsafe fn do_cow_fault(pfm: PageFaultMessage, mapper: &mut PageMapper) -> VmFaultReason {
        // panic!(
        //     "do_cow_fault has not yet been implemented,
        // fault message: {:?},
        // pid: {}\n",
        //     pfm,
        //     crate::process::ProcessManager::current_pid().data()
        // );
        // TODO https://code.dragonos.org.cn/xref/linux-6.6.21/mm/memory.c#do_cow_fault

        let mut ret = Self::filemap_fault(pfm.clone(), mapper);

        ret = ret.union(Self::finish_fault(pfm, mapper));

        ret
    }

    /// 处理文件映射页的缺页异常
    /// ## 参数
    ///
    /// - `pfm`: 缺页异常信息
    /// - `mapper`: 页表映射器
    ///
    /// ## 返回值
    /// - VmFaultReason: 页面错误处理信息标志
    #[allow(dead_code, unused_variables)]
    pub unsafe fn do_read_fault(pfm: PageFaultMessage, mapper: &mut PageMapper) -> VmFaultReason {
        // panic!(
        //     "do_read_fault has not yet been implemented,
        // fault message: {:?},
        // pid: {}\n",
        //     pfm,
        //     crate::process::ProcessManager::current_pid().data()
        // );

        // TODO https://code.dragonos.org.cn/xref/linux-6.6.21/mm/memory.c#do_read_fault
        log::info!("do_read_fault");
        let mut ret = Self::do_fault_around(pfm.clone(), mapper);
        if !ret.is_empty() {
            return ret;
        }
        ret = Self::filemap_fault(pfm.clone(), mapper);

        ret = ret.union(Self::finish_fault(pfm, mapper));

        ret
    }

    /// 处理对共享文件映射区写入引起的缺页
    /// ## 参数
    ///
    /// - `pfm`: 缺页异常信息
    /// - `mapper`: 页表映射器
    ///
    /// ## 返回值
    /// - VmFaultReason: 页面错误处理信息标志
    #[allow(dead_code, unused_variables)]
    pub unsafe fn do_shared_fault(pfm: PageFaultMessage, mapper: &mut PageMapper) -> VmFaultReason {
        panic!(
            "do_shared_fault has not yet been implemented, 
        fault message: {:?}, 
        pid: {}\n",
            pfm,
            crate::process::ProcessManager::current_pid().data()
        );
        // TODO https://code.dragonos.org.cn/xref/linux-6.6.21/mm/memory.c#do_shared_fault
    }

    /// 处理被置换页面的缺页异常
    /// ## 参数
    ///
    /// - `pfm`: 缺页异常信息
    /// - `mapper`: 页表映射器
    ///
    /// ## 返回值
    /// - VmFaultReason: 页面错误处理信息标志
    #[allow(unused_variables)]
    pub unsafe fn do_swap_page(pfm: PageFaultMessage, mapper: &mut PageMapper) -> VmFaultReason {
        panic!(
            "do_swap_page has not yet been implemented, 
        fault message: {:?}, 
        pid: {}\n",
            pfm,
            crate::process::ProcessManager::current_pid().data()
        );
        // TODO https://code.dragonos.org.cn/xref/linux-6.6.21/mm/memory.c#do_swap_page
    }

    /// 处理NUMA的缺页异常
    /// ## 参数
    ///
    /// - `pfm`: 缺页异常信息
    /// - `mapper`: 页表映射器
    ///
    /// ## 返回值
    /// - VmFaultReason: 页面错误处理信息标志
    #[allow(unused_variables)]
    pub unsafe fn do_numa_page(pfm: PageFaultMessage, mapper: &mut PageMapper) -> VmFaultReason {
        panic!(
            "do_numa_page has not yet been implemented, 
        fault message: {:?}, 
        pid: {}\n",
            pfm,
            crate::process::ProcessManager::current_pid().data()
        );
        // TODO https://code.dragonos.org.cn/xref/linux-6.6.21/mm/memory.c#do_numa_page
    }

    /// 处理写保护页面的写保护异常
    /// ## 参数
    ///
    /// - `pfm`: 缺页异常信息
    /// - `mapper`: 页表映射器
    ///
    /// ## 返回值
    /// - VmFaultReason: 页面错误处理信息标志
    pub unsafe fn do_wp_page(pfm: PageFaultMessage, mapper: &mut PageMapper) -> VmFaultReason {
        let address = pfm.address_aligned_down();
        let vma = pfm.vma.clone();
        let old_paddr = mapper.translate(address).unwrap().0;
        let mut page_manager = page_manager_lock_irqsave();
        let map_count = page_manager.get_mut(&old_paddr).map_count();
        drop(page_manager);

        let mut entry = mapper.get_entry(address, 0).unwrap();
        let new_flags = entry.flags().set_write(true);

        if map_count == 1 {
            let table = mapper.get_table(address, 0).unwrap();
            let i = table.index_of(address).unwrap();
            entry.set_flags(new_flags);
            table.set_entry(i, entry);
            VmFaultReason::VM_FAULT_COMPLETED
        } else if let Some(flush) = mapper.map(address, new_flags) {
            let mut page_manager = page_manager_lock_irqsave();
            let old_page = page_manager.get_mut(&old_paddr);
            old_page.remove_vma(&vma);
            drop(page_manager);

            flush.flush();
            let paddr = mapper.translate(address).unwrap().0;
            let mut anon_vma_guard = page_manager_lock_irqsave();
            let page = anon_vma_guard.get_mut(&paddr);
            page.insert_vma(vma.clone());

            (MMArch::phys_2_virt(paddr).unwrap().data() as *mut u8).copy_from_nonoverlapping(
                MMArch::phys_2_virt(old_paddr).unwrap().data() as *mut u8,
                MMArch::PAGE_SIZE,
            );

            VmFaultReason::VM_FAULT_COMPLETED
        } else {
            VmFaultReason::VM_FAULT_OOM
        }
    }

    /// 缺页附近页预读
    /// ## 参数
    ///
    /// - `pfm`: 缺页异常信息
    /// - `mapper`: 页表映射器
    ///
    /// ## 返回值
    /// - VmFaultReason: 页面错误处理信息标志
    pub unsafe fn do_fault_around(pfm: PageFaultMessage, mapper: &mut PageMapper) -> VmFaultReason {
        log::info!("do_fault_around");
        if mapper.get_table(*pfm.address(), 0).is_none() {
            mapper
                .allocate_table(*pfm.address(), 0)
                .expect("failed to allocate pte table");
        }
        let vma = pfm.vma();
        let vma_guard = vma.lock();
        let vma_region = *vma_guard.region();
        drop(vma_guard);

        // 缺页在VMA中的偏移量
        let vm_pgoff = (*pfm.address() - vma_region.start()) >> MMArch::PAGE_SHIFT;

        // 缺页在PTE中的偏移量
        let pte_pgoff =
            (pfm.address().data() >> MMArch::PAGE_SHIFT) & (1 << MMArch::PAGE_ENTRY_SHIFT);

        // 缺页在文件中的偏移量
        let file_pgoff = pfm.file_pgoff.expect("no file_pgoff");

        let vma_pages_count = (vma_region.end() - vma_region.start()) >> MMArch::PAGE_SHIFT;

        let fault_around_page_number = 16;

        // 开始位置不能超出当前pte和vma头部
        let from_pte = max(
            align_down(pte_pgoff, fault_around_page_number),
            pte_pgoff - min(vm_pgoff, pte_pgoff),
        );

        // pte结束位置不能超过：
        // 1.最大预读上限（默认16）
        // 2.最大pte(512)
        // 3.vma结束位置(pte_pgoff + (vma_pages_count - vm_pgoff)计算出vma结束页号对当前pte开头的偏移)
        let to_pte = min(
            from_pte + fault_around_page_number,
            min(
                1 << MMArch::PAGE_SHIFT,
                pte_pgoff + (vma_pages_count - vm_pgoff),
            ),
        );

        // 预先分配pte页表（如果不存在）
        if mapper.get_table(*pfm.address(), 0).is_none()
            && mapper.allocate_table(*pfm.address(), 0).is_none()
        {
            return VmFaultReason::VM_FAULT_OOM;
        }

        // from_pte - pte_pgoff得出预读起始pte相对缺失页的偏移，加上pfm.file_pgoff（缺失页在文件中的偏移）得出起始页在文件中的偏移，结束pte同理
        Self::filemap_map_pages(
            pfm.clone(),
            mapper,
            file_pgoff + (from_pte - pte_pgoff),
            file_pgoff + (to_pte - pte_pgoff),
        );

        VmFaultReason::empty()
    }

    /// 通用的VMA文件映射页面映射函数
    /// ## 参数
    ///
    /// - `pfm`: 缺页异常信息
    /// - `mapper`: 页表映射器
    ///
    /// ## 返回值
    /// - VmFaultReason: 页面错误处理信息标志
    pub unsafe fn filemap_map_pages(
        pfm: PageFaultMessage,
        mapper: &mut PageMapper,
        start_pgoff: usize,
        end_pgoff: usize,
    ) -> VmFaultReason {
        let vma = pfm.vma();
        let vma_guard = vma.lock();
        let file = vma_guard.vm_file().expect("no vm_file in vma");
        let page_cache = file.inode().page_cache().unwrap();

        // 起始页地址
        let addr = vma_guard.region().start
            + ((start_pgoff
                - vma_guard
                    .file_page_offset()
                    .expect("file_page_offset is none"))
                << MMArch::PAGE_SHIFT);
        // let pages = page_cache.get_pages(start_pgoff, end_pgoff);
        // let uptodate_pages = pages
        //     .iter()
        //     .filter(|page| page.flags().contains(PageFlags::PG_UPTODATE));
        for pgoff in start_pgoff..=end_pgoff {
            if let Some(page) = page_cache.get_page(pgoff) {
                if page.flags().contains(PageFlags::PG_UPTODATE) {
                    let phys = page.phys_frame().phys_address();
                    let virt = phys_2_virt(phys.data());

                    let address =
                        VirtAddr::new(addr.data() + ((pgoff - start_pgoff) << MMArch::PAGE_SHIFT));
                    mapper.map(address, vma_guard.flags()).unwrap().flush();
                    let frame = virt as *mut u8;
                    let new_frame =
                        phys_2_virt(mapper.translate(address).unwrap().0.data()) as *mut u8;
                    new_frame.copy_from_nonoverlapping(frame, MMArch::PAGE_SIZE);
                }
            }
        }
        VmFaultReason::empty()
    }

    /// 通用的VMA文件映射错误处理函数
    /// ## 参数
    ///
    /// - `pfm`: 缺页异常信息
    /// - `mapper`: 页表映射器
    ///
    /// ## 返回值
    /// - VmFaultReason: 页面错误处理信息标志
    pub unsafe fn filemap_fault(
        mut pfm: PageFaultMessage,
        mapper: &mut PageMapper,
    ) -> VmFaultReason {
        log::info!("filemap_fault");
        let vma = pfm.vma();
        let vma_guard = vma.lock();
        let file = vma_guard.vm_file().expect("no vm_file in vma");
        let page_cache = file.inode().page_cache().unwrap();
        let file_pgoff = pfm.file_pgoff.expect("no file_pgoff");
        let mut ret = VmFaultReason::empty();

        if let Some(page) = page_cache.get_page(file_pgoff) {
            // TODO 异步从磁盘中预读页面进PageCache
            let address = vma_guard.region().start
                + ((file_pgoff
                    - vma_guard
                        .file_page_offset()
                        .expect("file_page_offset is none"))
                    << MMArch::PAGE_SHIFT);
            mapper.map(address, vma_guard.flags()).unwrap().flush();
            let frame = phys_2_virt(page.phys_frame().phys_address().data()) as *mut u8;
            let new_frame = phys_2_virt(mapper.translate(address).unwrap().0.data()) as *mut u8;
            new_frame.copy_from_nonoverlapping(frame, MMArch::PAGE_SIZE);
        } else {
            // TODO 同步预读
            //涉及磁盘IO，返回标志为VM_FAULT_MAJOR
            ret = VmFaultReason::VM_FAULT_MAJOR;
            let mut buf: Vec<u8> = vec![0; MMArch::PAGE_SIZE];
            file.pread(
                file_pgoff * MMArch::PAGE_SIZE,
                MMArch::PAGE_SIZE,
                &mut buf[..],
            )
            .unwrap();
            let allocator = mapper.allocator_mut();

            // 分配一个物理页面作为加入PageCache的新页
            let new_cache_page = allocator.allocate_one().unwrap();
            (phys_2_virt(new_cache_page.data()) as *mut u8)
                .copy_from_nonoverlapping(buf.as_mut_ptr(), MMArch::PAGE_SIZE);

            let page = Arc::new(Page::new(false, PhysPageFrame::new(new_cache_page)));

            page_cache.add_page(file_pgoff, page.clone());

            pfm.page = Some(page);

            //     // 分配空白页并映射到缺页地址
            //     mapper.map(pfm.address, vma_guard.flags()).unwrap().flush();
            //     let new_frame = phys_2_virt(mapper.translate(pfm.address).unwrap().0.data());
            //     (new_frame as *mut u8).copy_from_nonoverlapping(buf.as_mut_ptr(), MMArch::PAGE_SIZE);
        }
        ret
    }

    pub unsafe fn finish_fault(pfm: PageFaultMessage, mapper: &mut PageMapper) -> VmFaultReason {
        let vma = pfm.vma();
        let vma_guard = vma.lock();
        let page = pfm.page.clone().expect("no page in pfm");
        let cache_frame = page.phys_frame().phys_address();

        if pfm.flags().contains(FaultFlags::FAULT_FLAG_WRITE)
            && !pfm.vma().lock().vm_flags().contains(VmFlags::VM_SHARED)
        {
            // 私有文件映射的写时复制场景
            // 分配空白页并映射到缺页地址
            mapper
                .map(
                    pfm.address,
                    vma_guard.flags().set_write(true).set_dirty(true),
                )
                .unwrap()
                .flush();

            //复制PageCache内容到新的页内
            let new_frame = phys_2_virt(mapper.translate(pfm.address).unwrap().0.data());
            (new_frame as *mut u8).copy_from_nonoverlapping(
                phys_2_virt(cache_frame.data()) as *mut u8,
                MMArch::PAGE_SIZE,
            );
        } else {
            // 直接映射到PageCache
            mapper.map_phys(*pfm.address(), cache_frame, vma_guard.flags());
        }
        VmFaultReason::VM_FAULT_COMPLETED
    }
}
