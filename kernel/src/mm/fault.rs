use core::{
    alloc::Layout,
    cmp::{max, min},
    intrinsics::unlikely,
    panic,
};

use alloc::sync::Arc;

use crate::{
    arch::{mm::PageMapper, MMArch},
    libs::align::align_down,
    mm::{
        page::{page_manager_lock, EntryFlags},
        ucontext::LockedVMA,
        VirtAddr, VmFaultReason, VmFlags,
    },
    process::{ProcessManager, ProcessState},
};

use crate::mm::MemoryManagementArch;

use super::page::{Page, PageFlags};

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
pub struct PageFaultMessage<'a> {
    /// 产生缺页的VMA结构体
    vma: Arc<LockedVMA>,
    /// 缺页地址
    address: VirtAddr,
    /// 异常处理标志
    flags: FaultFlags,
    /// 页表映射器
    mapper: &'a mut PageMapper,
    /// 缺页的后备对象页在后备对象中的偏移页号（文件/共享匿名）
    backing_pgoff: Option<usize>,
    /// 缺页对应PageCache中的文件页
    page: Option<Arc<Page>>,
    /// 写时拷贝需要的页面
    cow_page: Option<Arc<Page>>,
}

impl<'a> PageFaultMessage<'a> {
    pub fn new(
        vma: Arc<LockedVMA>,
        address: VirtAddr,
        flags: FaultFlags,
        mapper: &'a mut PageMapper,
    ) -> Self {
        let guard = vma.lock();
        let backing_pgoff = guard.backing_page_offset().map(|backing_page_offset| {
            ((address.data() - guard.region().start().data()) >> MMArch::PAGE_SHIFT)
                + backing_page_offset
        });
        Self {
            vma: vma.clone(),
            address: VirtAddr::new(crate::libs::align::page_align_down(address.data())),
            flags,
            backing_pgoff,
            page: None,
            mapper,
            cow_page: None,
        }
    }

    #[inline(always)]
    #[allow(dead_code)]
    pub fn vma(&self) -> Arc<LockedVMA> {
        self.vma.clone()
    }

    #[inline(always)]
    #[allow(dead_code)]
    pub fn address(&self) -> VirtAddr {
        self.address
    }

    #[inline(always)]
    #[allow(dead_code)]
    pub fn address_aligned_down(&self) -> VirtAddr {
        VirtAddr::new(crate::libs::align::page_align_down(self.address.data()))
    }

    #[inline(always)]
    #[allow(dead_code)]
    pub fn flags(&self) -> FaultFlags {
        self.flags
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
    #[inline(never)]
    pub unsafe fn handle_mm_fault(mut pfm: PageFaultMessage) -> VmFaultReason {
        let flags = pfm.flags();
        let vma = pfm.vma();
        let current_pcb = ProcessManager::current_pcb();
        let mut guard = current_pcb.sched_info().inner_lock_write_irqsave();
        guard.set_state(ProcessState::Runnable);
        drop(guard); // 必须在进入后续处理前释放锁，避免在持锁时进行I/O导致死锁

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
            return Self::handle_normal_fault(&mut pfm);
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
    pub unsafe fn handle_normal_fault(pfm: &mut PageFaultMessage) -> VmFaultReason {
        let address = pfm.address_aligned_down();
        let vma = pfm.vma.clone();
        let mapper = &mut pfm.mapper;
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

        Self::handle_pte_fault(pfm)
    }

    /// 处理页表项异常
    /// ## 参数
    ///
    /// - `pfm`: 缺页异常信息
    /// - `mapper`: 页表映射器
    ///
    /// ## 返回值
    /// - VmFaultReason: 页面错误处理信息标志
    pub unsafe fn handle_pte_fault(pfm: &mut PageFaultMessage) -> VmFaultReason {
        let address = pfm.address_aligned_down();
        let flags = pfm.flags;
        let vma = pfm.vma.clone();
        let mut ret = VmFaultReason::VM_FAULT_COMPLETED;
        let mapper = &pfm.mapper;

        // pte存在
        if let Some(mut entry) = mapper.get_entry(address, 0) {
            if !entry.present() {
                ret = Self::do_swap_page(pfm);
            }

            if entry.protnone() && vma.is_accessible() {
                ret = Self::do_numa_page(pfm);
            }

            if flags.intersects(FaultFlags::FAULT_FLAG_WRITE | FaultFlags::FAULT_FLAG_UNSHARE) {
                if !entry.write() {
                    ret = Self::do_wp_page(pfm);
                } else {
                    entry.set_flags(EntryFlags::from_data(MMArch::ENTRY_FLAG_DIRTY));
                }
            }
        } else if vma.is_anonymous() {
            ret = Self::do_anonymous_page(pfm);
        } else {
            ret = Self::do_fault(pfm);
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
    pub unsafe fn do_anonymous_page(pfm: &mut PageFaultMessage) -> VmFaultReason {
        let address = pfm.address_aligned_down();
        let vma = pfm.vma.clone();
        let mapper = &mut pfm.mapper;

        // If this is an anonymous shared mapping, use a shared backing so pages are visible across fork
        {
            let guard = vma.lock();
            if guard.vm_flags().contains(VmFlags::VM_SHARED) {
                let shared = guard.shared_anon.clone();
                if let Some(shared) = shared {
                    // Compute page index within the shared-anon backing object.
                    // Base offset is stored in VMA.backing_pgoff.
                    let base = guard.backing_page_offset().unwrap_or(0);
                    let pgoff = base
                        + ((address.data() - guard.region().start().data()) >> MMArch::PAGE_SHIFT);

                    // Linux semantics: access beyond the backing size should SIGBUS.
                    if pgoff >= shared.size_pages() {
                        drop(guard);
                        return VmFaultReason::VM_FAULT_SIGBUS;
                    }
                    drop(guard);

                    // Atomically get or create the shared page to avoid races
                    let page = match shared.get_or_create_page(pgoff) {
                        Ok(p) => p,
                        Err(_) => return VmFaultReason::VM_FAULT_OOM,
                    };

                    // Map the shared page into current process
                    let flags = vma.lock().flags();
                    if let Some(flush) = mapper.map_phys(address, page.phys_address(), flags) {
                        flush.flush();
                        page.write().insert_vma(vma.clone());
                        return VmFaultReason::VM_FAULT_COMPLETED;
                    } else {
                        return VmFaultReason::VM_FAULT_OOM;
                    }
                }
            }
        }

        // Fallback: private anonymous page (MAP_PRIVATE or non-shared anon)
        let flags = vma.lock().flags();
        if let Some(flush) = mapper.map(address, flags) {
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
            let mut page_manager_guard = page_manager_lock();
            let page = page_manager_guard.get_unwrap(&paddr);
            page.write().insert_vma(vma.clone());
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
    #[inline(never)]
    pub unsafe fn do_fault(pfm: &mut PageFaultMessage) -> VmFaultReason {
        if !pfm.flags().contains(FaultFlags::FAULT_FLAG_WRITE) {
            Self::do_read_fault(pfm)
        } else if !pfm.vma().lock().vm_flags().contains(VmFlags::VM_SHARED) {
            Self::do_cow_fault(pfm)
        } else {
            Self::do_shared_fault(pfm)
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
    #[inline(never)]
    pub unsafe fn do_cow_fault(pfm: &mut PageFaultMessage) -> VmFaultReason {
        let mut ret = Self::filemap_fault(pfm);

        if unlikely(ret.intersects(
            VmFaultReason::VM_FAULT_ERROR
                | VmFaultReason::VM_FAULT_NOPAGE
                | VmFaultReason::VM_FAULT_RETRY
                | VmFaultReason::VM_FAULT_DONE_COW,
        )) {
            return ret;
        }

        let cache_page = pfm.page.clone().unwrap();
        let mapper = &mut pfm.mapper;

        let mut page_manager_guard = page_manager_lock();
        if let Ok(page) =
            page_manager_guard.copy_page(&cache_page.phys_address(), mapper.allocator_mut())
        {
            pfm.cow_page = Some(page.clone());
        } else {
            return VmFaultReason::VM_FAULT_OOM;
        }
        ret = ret.union(Self::finish_fault(pfm));

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
    pub unsafe fn do_read_fault(pfm: &mut PageFaultMessage) -> VmFaultReason {
        let fs = pfm.vma().lock().vm_file().unwrap().inode().fs();

        let mut ret = Self::do_fault_around(pfm);
        if !ret.is_empty() {
            return ret;
        }

        ret = fs.fault(pfm);

        if ret.contains(VmFaultReason::VM_FAULT_COMPLETED) {
            return ret;
        }

        // 出现错误类返回时，不再继续 finish_fault 以避免 unwrap/panic
        if ret.intersects(VmFaultReason::VM_FAULT_ERROR) {
            return ret;
        }

        ret = ret.union(Self::finish_fault(pfm));

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
    #[inline(never)]
    pub unsafe fn do_shared_fault(pfm: &mut PageFaultMessage) -> VmFaultReason {
        let mut ret = Self::filemap_fault(pfm);

        if ret.intersects(VmFaultReason::VM_FAULT_ERROR) {
            return ret;
        }

        let cache_page = pfm.page.clone().expect("no cache_page in PageFaultMessage");

        // 将pagecache页设为脏页，以便回收时能够回写
        cache_page.write().add_flags(PageFlags::PG_DIRTY);
        ret = ret.union(Self::finish_fault(pfm));

        ret
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
    pub unsafe fn do_swap_page(pfm: &mut PageFaultMessage) -> VmFaultReason {
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
    pub unsafe fn do_numa_page(pfm: &mut PageFaultMessage) -> VmFaultReason {
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
    #[inline(never)]
    pub unsafe fn do_wp_page(pfm: &mut PageFaultMessage) -> VmFaultReason {
        let address = pfm.address_aligned_down();
        let vma = pfm.vma.clone();
        let mapper = &mut pfm.mapper;

        let old_paddr = mapper.translate(address).unwrap().0;
        let mut page_manager = page_manager_lock();
        let old_page = page_manager.get_unwrap(&old_paddr);
        let map_count = old_page.read().map_count();
        drop(page_manager);

        let mut entry = mapper.get_entry(address, 0).unwrap();
        let new_flags = entry.flags().set_write(true).set_dirty(true);

        if vma.lock().vm_flags().contains(VmFlags::VM_SHARED) {
            // 共享映射，直接修改页表项保护位，标记为脏页
            let table = mapper.get_table(address, 0).unwrap();
            let i = table.index_of(address).unwrap();
            entry.set_flags(new_flags);
            table.set_entry(i, entry);

            old_page.write().add_flags(PageFlags::PG_DIRTY);

            VmFaultReason::VM_FAULT_COMPLETED
        } else if vma.is_anonymous() {
            // 私有匿名映射，根据引用计数判断是否拷贝页面
            if map_count == 1 {
                let table = mapper.get_table(address, 0).unwrap();
                let i = table.index_of(address).unwrap();
                entry.set_flags(new_flags);
                table.set_entry(i, entry);
                VmFaultReason::VM_FAULT_COMPLETED
            } else if let Some(flush) = mapper.map(address, new_flags) {
                let mut page_manager_guard = page_manager_lock();
                let old_page = page_manager_guard.get_unwrap(&old_paddr);
                old_page.write().remove_vma(&vma);
                // drop(page_manager_guard);

                flush.flush();
                let paddr = mapper.translate(address).unwrap().0;
                // let mut page_manager_guard = page_manager_lock();
                let page = page_manager_guard.get_unwrap(&paddr);
                page.write().insert_vma(vma.clone());

                (MMArch::phys_2_virt(paddr).unwrap().data() as *mut u8).copy_from_nonoverlapping(
                    MMArch::phys_2_virt(old_paddr).unwrap().data() as *mut u8,
                    MMArch::PAGE_SIZE,
                );

                VmFaultReason::VM_FAULT_COMPLETED
            } else {
                VmFaultReason::VM_FAULT_OOM
            }
        } else {
            // 私有文件映射，必须拷贝页面
            if let Some(flush) = mapper.map(address, new_flags) {
                let mut page_manager_guard = page_manager_lock();
                let old_page = page_manager_guard.get_unwrap(&old_paddr);
                old_page.write().remove_vma(&vma);
                // drop(page_manager_guard);

                flush.flush();
                let paddr = mapper.translate(address).unwrap().0;
                // let mut page_manager_guard = page_manager_lock();
                let page = page_manager_guard.get_unwrap(&paddr);
                page.write().insert_vma(vma.clone());

                (MMArch::phys_2_virt(paddr).unwrap().data() as *mut u8).copy_from_nonoverlapping(
                    MMArch::phys_2_virt(old_paddr).unwrap().data() as *mut u8,
                    MMArch::PAGE_SIZE,
                );

                VmFaultReason::VM_FAULT_COMPLETED
            } else {
                VmFaultReason::VM_FAULT_OOM
            }
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
    pub unsafe fn do_fault_around(pfm: &mut PageFaultMessage) -> VmFaultReason {
        let vma = pfm.vma();
        let address = pfm.address();
        let mapper = &mut pfm.mapper;

        if mapper.get_table(address, 0).is_none() {
            mapper
                .allocate_table(address, 0)
                .expect("failed to allocate pte table");
        }
        let vma_guard = vma.lock();
        let vma_region = *vma_guard.region();
        drop(vma_guard);

        // 缺页在VMA中的偏移量
        let vm_pgoff = (address.data() - vma_region.start().data()) >> MMArch::PAGE_SHIFT;

        // 缺页在PTE中的偏移量
        let pte_pgoff = (address.data() >> MMArch::PAGE_SHIFT) & (1 << MMArch::PAGE_ENTRY_SHIFT);

        // 缺页在文件中的偏移量
        let backing_pgoff = pfm.backing_pgoff.expect("no backing_pgoff");

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
                MMArch::PAGE_ENTRY_NUM,
                pte_pgoff + (vma_pages_count - vm_pgoff),
            ),
        );

        // 预先分配pte页表（如果不存在）
        if mapper.get_table(address, 0).is_none() && mapper.allocate_table(address, 0).is_none() {
            return VmFaultReason::VM_FAULT_OOM;
        }

        let fs = pfm.vma().lock().vm_file().unwrap().inode().fs();
        // from_pte - pte_pgoff得出预读起始pte相对缺失页的偏移，加上pfm.backing_pgoff（缺失页在文件中的偏移）得出起始页在文件中的偏移，结束pte同理
        fs.map_pages(
            pfm,
            backing_pgoff + (from_pte - pte_pgoff),
            backing_pgoff + (to_pte - pte_pgoff),
        );

        VmFaultReason::empty()
    }

    /// 通用的VMA文件映射页面映射函数，将PageCache中的页面映射到进程空间
    /// ## 参数
    ///
    /// - `pfm`: 缺页异常信息
    /// - `mapper`: 页表映射器
    ///
    /// ## 返回值
    /// - VmFaultReason: 页面错误处理信息标志
    pub unsafe fn filemap_map_pages(
        pfm: &mut PageFaultMessage,

        start_pgoff: usize,
        end_pgoff: usize,
    ) -> VmFaultReason {
        let vma = pfm.vma();
        let vma_guard = vma.lock();
        let file = vma_guard.vm_file().expect("no vm_file in vma");
        let page_cache = match file.inode().page_cache() {
            Some(cache) => cache,
            None => {
                // 文件没有页面缓存，可能是不支持内存映射的设备文件
                log::warn!(
                    "filemap_map_pages: file has no page cache, inode type: {}",
                    crate::libs::name::get_type_name(&file.inode())
                );
                return VmFaultReason::VM_FAULT_SIGBUS;
            }
        };
        let mapper = &mut pfm.mapper;

        // 起始页地址
        let addr = vma_guard.region().start
            + ((start_pgoff
                - vma_guard
                    .backing_page_offset()
                    .expect("backing_page_offset is none"))
                << MMArch::PAGE_SHIFT);

        for pgoff in start_pgoff..end_pgoff {
            if let Some(page) = page_cache.lock().get_page(pgoff) {
                let page_guard = page.read();
                if page_guard.flags().contains(PageFlags::PG_UPTODATE) {
                    let phys = page.phys_address();

                    let address =
                        VirtAddr::new(addr.data() + ((pgoff - start_pgoff) << MMArch::PAGE_SHIFT));
                    mapper
                        .map_phys(address, phys, vma_guard.flags())
                        .unwrap()
                        .flush();
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
    pub unsafe fn filemap_fault(pfm: &mut PageFaultMessage) -> VmFaultReason {
        let vma = pfm.vma();
        let vma_guard = vma.lock();
        let file = vma_guard.vm_file().expect("no vm_file in vma");
        let page_cache = match file.inode().page_cache() {
            Some(cache) => cache,
            None => {
                // 文件没有页面缓存，可能是不支持内存映射的设备文件
                log::warn!(
                    "filemap_fault: file has no page cache, inode type: {}",
                    crate::libs::name::get_type_name(&file.inode())
                );
                return VmFaultReason::VM_FAULT_SIGBUS;
            }
        };
        let backing_pgoff = pfm.backing_pgoff.expect("no backing_pgoff");
        let mut ret = VmFaultReason::empty();

        let page = page_cache.lock().get_page(backing_pgoff);
        if let Some(page) = page {
            // TODO 异步从磁盘中预读页面进PageCache

            // 直接将PageCache中的页面作为要映射的页面
            pfm.page = Some(page.clone());
        } else {
            // TODO 同步预读
            // 涉及磁盘IO，返回标志为VM_FAULT_MAJOR
            ret = VmFaultReason::VM_FAULT_MAJOR;
            let mut buffer = vec![0u8; MMArch::PAGE_SIZE];
            match file.pread(
                backing_pgoff * MMArch::PAGE_SIZE,
                MMArch::PAGE_SIZE,
                buffer.as_mut_slice(),
            ) {
                Ok(read_len) => {
                    // 超出文件末尾，返回SIGBUS而不是panic
                    if read_len == 0 {
                        return VmFaultReason::VM_FAULT_SIGBUS;
                    }
                }
                Err(e) => {
                    log::warn!(
                        "filemap_fault: pread failed at pgoff {}, err {:?}",
                        backing_pgoff,
                        e
                    );
                    return VmFaultReason::VM_FAULT_SIGBUS;
                }
            }
            drop(buffer);

            let page = page_cache.lock().get_page(backing_pgoff);
            if let Some(page) = page {
                pfm.page = Some(page);
            } else {
                // 读取后仍未获取到页面，视为错误
                return VmFaultReason::VM_FAULT_SIGBUS;
            }
        }
        ret
    }

    /// 纯 page-cache 后端的缺页处理（不走 pread/磁盘 IO）。
    ///
    /// 适用场景：tmpfs/ramfs 等内存文件系统，它们以 page_cache 作为唯一数据后端。
    ///
    /// 行为：
    /// - 若 inode 没有 page_cache，则返回 SIGBUS
    /// - 若缺页落在文件末尾之后（以页粒度判断），返回 SIGBUS
    /// - 若页已存在，直接使用该页
    /// - 若页不存在，则创建一个零页并插入 page_cache
    pub unsafe fn pagecache_fault_zero(pfm: &mut PageFaultMessage) -> VmFaultReason {
        let vma = pfm.vma();
        let vma_guard = vma.lock();
        let file = vma_guard.vm_file().expect("no vm_file in vma");
        drop(vma_guard);

        let inode = file.inode();
        let page_cache = match inode.page_cache() {
            Some(cache) => cache,
            None => return VmFaultReason::VM_FAULT_SIGBUS,
        };

        let backing_pgoff = pfm.backing_pgoff.expect("no backing_pgoff");

        // 以"页起始是否越过 EOF"判断 SIGBUS。
        // 特别地：size==0 时，任何访问都应 SIGBUS（符合 mmap 语义，且满足 gvisor death tests）。
        if let Ok(md) = inode.metadata() {
            let size = md.size.max(0) as usize;
            if backing_pgoff.saturating_mul(MMArch::PAGE_SIZE) >= size {
                return VmFaultReason::VM_FAULT_SIGBUS;
            }
        }

        // 先尝试直接获取
        if let Some(page) = page_cache.lock().get_page(backing_pgoff) {
            // 标记为 UPTODATE，便于 map_pages / readahead（即便对 tmpfs 通常不会走）。
            page.write().add_flags(PageFlags::PG_UPTODATE);
            pfm.page = Some(page);
            return VmFaultReason::empty();
        }

        {
            let mut pc = page_cache.lock();
            // 可能并发创建：create 后再 get（忽略已存在/并发导致的轻微差异）
            pc.create_zero_pages(backing_pgoff, 1).map_err(|_| ()).ok();
        }

        let page = page_cache.lock().get_page(backing_pgoff);
        if let Some(page) = page {
            page.write().add_flags(PageFlags::PG_UPTODATE);
            pfm.page = Some(page);
            VmFaultReason::empty()
        } else {
            VmFaultReason::VM_FAULT_SIGBUS
        }
    }

    /// 将文件页映射到缺页地址
    /// ## 参数
    ///
    /// - `pfm`: 缺页异常信息
    /// - `mapper`: 页表映射器
    ///
    /// ## 返回值
    /// - VmFaultReason: 页面错误处理信息标志
    pub unsafe fn finish_fault(pfm: &mut PageFaultMessage) -> VmFaultReason {
        let vma = pfm.vma();
        let vma_guard = vma.lock();
        let flags = pfm.flags();
        let cache_page = pfm.page.clone();
        let cow_page = pfm.cow_page.clone();
        let address = pfm.address();
        let mapper = &mut pfm.mapper;

        let page_to_map = if flags.contains(FaultFlags::FAULT_FLAG_WRITE)
            && !vma_guard.vm_flags().contains(VmFlags::VM_SHARED)
        {
            // 私有文件映射的写时复制
            cow_page.expect("no cow_page in PageFaultMessage")
        } else {
            // 直接映射到PageCache
            cache_page.expect("no cache_page in PageFaultMessage")
        };

        let page_phys = page_to_map.phys_address();

        mapper.map_phys(address, page_phys, vma_guard.flags());
        page_to_map.write().insert_vma(pfm.vma());
        VmFaultReason::VM_FAULT_COMPLETED
    }

    /// Map a zeroed anonymous page for /dev/zero style mappings.
    pub unsafe fn zero_fault(pfm: &mut PageFaultMessage) -> VmFaultReason {
        let address = pfm.address_aligned_down();
        let vma = pfm.vma();
        let flags = vma.lock().flags();
        let mapper = &mut pfm.mapper;

        if let Some(flush) = mapper.map(address, flags) {
            flush.flush();
            let mut pm = page_manager_lock();
            let paddr = mapper.translate(address).unwrap().0;
            let page = pm.get_unwrap(&paddr);
            page.write().insert_vma(vma);
            VmFaultReason::VM_FAULT_COMPLETED
        } else {
            VmFaultReason::VM_FAULT_OOM
        }
    }

    /// Prefault zeroed anonymous pages for /dev/zero style mappings.
    pub unsafe fn zero_map_pages(
        pfm: &mut PageFaultMessage,
        start_pgoff: usize,
        end_pgoff: usize,
    ) -> VmFaultReason {
        let vma = pfm.vma();
        let vma_guard = vma.lock();
        let backing_pgoff = match vma_guard.backing_page_offset() {
            Some(off) => off,
            None => return VmFaultReason::VM_FAULT_SIGBUS,
        };
        let base = vma_guard.region().start();
        let flags = vma_guard.flags();
        drop(vma_guard);

        let mapper = &mut pfm.mapper;
        for pgoff in start_pgoff..end_pgoff {
            let addr = VirtAddr::new(base.data() + ((pgoff - backing_pgoff) << MMArch::PAGE_SHIFT));
            if let Some(flush) = mapper.map(addr, flags) {
                flush.flush();
                let mut pm = page_manager_lock();
                let paddr = mapper.translate(addr).unwrap().0;
                let page = pm.get_unwrap(&paddr);
                page.write().insert_vma(vma.clone());
            } else {
                return VmFaultReason::VM_FAULT_OOM;
            }
        }

        VmFaultReason::VM_FAULT_COMPLETED
    }
}
