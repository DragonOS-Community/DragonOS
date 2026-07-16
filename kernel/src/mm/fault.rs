use core::{
    alloc::Layout,
    cmp::{max, min},
    intrinsics::unlikely,
    panic,
};

use alloc::sync::Arc;
use system_error::SystemError;

use crate::{
    arch::{mm::PageMapper, MMArch},
    filesystem::page_cache::PageCachePagePin,
    libs::align::align_down,
    mm::{
        page::{page_manager_lock, EntryFlags},
        ucontext::{AddressSpace, InnerAddressSpace, LockedVMA},
        PhysAddr, VirtAddr, VmFaultReason, VmFlags,
    },
    process::{ProcessManager, ProcessState},
};

use crate::mm::MemoryManagementArch;

use super::page::{Page, PageFlags, PageType};

pub trait FaultRetryWait: core::fmt::Debug + Send + Sync {
    fn wait(&self) -> Result<(), SystemError>;
}

#[derive(Debug)]
pub struct VmFaultOutcome {
    pub reason: VmFaultReason,
    pub retry_wait: Option<Arc<dyn FaultRetryWait>>,
}

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
    vm_file: Option<Arc<crate::filesystem::vfs::file::File>>,
    vm_flags: VmFlags,
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
    /// PageCache entry pin held until the fault either installs a PTE/rmap or fails.
    page_pin: Option<PageCachePagePin>,
    /// 写时拷贝需要的页面
    cow_page: Option<Arc<Page>>,
    /// 缺页所属的地址空间。
    ///
    /// do_wp_page 等在修改 PTE 后需要走 mm-aware shootdown 的路径要依赖它（`AddressSpace::flush_tlb_range`）。
    mm: Arc<AddressSpace>,
    retry_wait: Option<Arc<dyn FaultRetryWait>>,
}

impl<'a> PageFaultMessage<'a> {
    pub fn new(
        vma: Arc<LockedVMA>,
        address: VirtAddr,
        flags: FaultFlags,
        mapper: &'a mut PageMapper,
        mm: Arc<AddressSpace>,
    ) -> Self {
        let guard = vma.lock();
        let vm_file = guard.vm_file();
        let vm_flags = *guard.vm_flags();
        let backing_pgoff = guard.backing_page_offset().map(|backing_page_offset| {
            ((address.data() - guard.region().start().data()) >> MMArch::PAGE_SHIFT)
                + backing_page_offset
        });
        Self {
            vma: vma.clone(),
            vm_file,
            vm_flags,
            address: VirtAddr::new(crate::libs::align::page_align_down(address.data())),
            flags,
            backing_pgoff,
            page: None,
            page_pin: None,
            mapper,
            cow_page: None,
            mm,
            retry_wait: None,
        }
    }

    #[inline(always)]
    #[allow(dead_code)]
    pub fn vma(&self) -> Arc<LockedVMA> {
        self.vma.clone()
    }

    pub fn vm_file(&self) -> Option<&Arc<crate::filesystem::vfs::file::File>> {
        self.vm_file.as_ref()
    }

    pub fn vm_flags(&self) -> VmFlags {
        self.vm_flags
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

    #[inline(always)]
    pub fn backing_pgoff(&self) -> Option<usize> {
        self.backing_pgoff
    }

    #[inline(always)]
    pub fn set_page(&mut self, page: Arc<Page>) {
        self.page = Some(page);
    }

    /// 缺页所属的地址空间。
    #[inline(always)]
    pub fn mm(&self) -> &Arc<AddressSpace> {
        &self.mm
    }

    pub fn set_retry_wait(&mut self, wait: Arc<dyn FaultRetryWait>) {
        self.retry_wait = Some(wait);
    }

    /// Return the currently installed raw PFN at the fault address.
    ///
    /// Managed pages deliberately return `None`; filesystem pfn_mkwrite/COW
    /// implementations must not use this interface for page-cache pages.
    pub fn external_pfn(&mut self) -> Option<PhysAddr> {
        if !self.vma.lock().vm_flags().contains(VmFlags::VM_MIXEDMAP) {
            return None;
        }
        let (paddr, _) = self.mapper.translate(self.address_aligned_down())?;
        let mut page_manager = page_manager_lock();
        page_manager.get(&paddr).is_none().then_some(paddr)
    }

    /// Install a validated external PFN in a VM_MIXEDMAP VMA.
    ///
    /// `writable` is accepted only for a writable shared VMA. Private mappings
    /// must use `cow_external_page()` instead, so device memory can never be
    /// exposed writable through MAP_PRIVATE.
    pub unsafe fn map_external_pfn(&mut self, paddr: PhysAddr, writable: bool) -> VmFaultReason {
        if !paddr.check_aligned(MMArch::PAGE_SIZE) {
            return VmFaultReason::VM_FAULT_SIGBUS;
        }
        let (vm_flags, mut pte_flags) = {
            let guard = self.vma.lock();
            (*guard.vm_flags(), guard.flags())
        };
        if !vm_flags.contains(VmFlags::VM_MIXEDMAP)
            || (writable
                && (!vm_flags.contains(VmFlags::VM_SHARED)
                    || !vm_flags.contains(VmFlags::VM_WRITE)))
        {
            return VmFaultReason::VM_FAULT_SIGSEGV;
        }
        pte_flags = pte_flags.set_write(writable);
        let address = self.address_aligned_down();
        let mm = self.mm.clone();
        let _pt_edit = mm.page_table_edit();
        if self.mapper.get_entry(address, 0).is_some() {
            return VmFaultReason::VM_FAULT_NOPAGE;
        }
        let Some(flush) = self.mapper.map_phys(address, paddr, pte_flags) else {
            return VmFaultReason::VM_FAULT_OOM;
        };
        flush.flush();
        self.vma.lock().set_mapped(true);
        VmFaultReason::VM_FAULT_COMPLETED
    }

    /// Upgrade an already installed external PFN after the filesystem has
    /// successfully completed its pfn_mkwrite/layout transaction.
    pub unsafe fn upgrade_external_pfn(&mut self, expected: PhysAddr) -> VmFaultReason {
        let vm_flags = *self.vma.lock().vm_flags();
        if !vm_flags.contains(VmFlags::VM_MIXEDMAP | VmFlags::VM_SHARED | VmFlags::VM_WRITE) {
            return VmFaultReason::VM_FAULT_SIGSEGV;
        }
        let address = self.address_aligned_down();
        let mm = self.mm.clone();
        let _pt_edit = mm.page_table_edit();
        let Some(mut entry) = self.mapper.get_entry(address, 0) else {
            return VmFaultReason::VM_FAULT_NOPAGE;
        };
        if entry.address() != Ok(expected) || entry.write() {
            return VmFaultReason::VM_FAULT_NOPAGE;
        }
        let mut page_manager = page_manager_lock();
        if page_manager.get(&expected).is_some() {
            return VmFaultReason::VM_FAULT_NOPAGE;
        }
        drop(page_manager);
        let table = self.mapper.get_table(address, 0).unwrap();
        let index = table.index_of(address).unwrap();
        entry.set_flags(entry.flags().set_write(true).set_dirty(true));
        table.set_entry(index, entry);
        mm.flush_tlb_range(
            address,
            VirtAddr::new(address.data() + MMArch::PAGE_SIZE),
            MMArch::PAGE_SHIFT as u8,
            false,
        );
        VmFaultReason::VM_FAULT_COMPLETED
    }

    /// Replace an optional external PFN with a private anonymous COW page.
    /// The source bytes must come from a filesystem-validated cache-window
    /// virtual address and cover exactly one base page.
    pub unsafe fn cow_external_page(
        &mut self,
        expected: Option<PhysAddr>,
        source: &[u8],
    ) -> VmFaultReason {
        if source.len() != MMArch::PAGE_SIZE {
            return VmFaultReason::VM_FAULT_SIGBUS;
        }
        let (vm_flags, pte_flags, mlocked) = {
            let guard = self.vma.lock();
            (
                *guard.vm_flags(),
                guard.flags().set_write(true).set_dirty(true),
                guard.vm_flags().contains(VmFlags::VM_LOCKED),
            )
        };
        if !vm_flags.contains(VmFlags::VM_MIXEDMAP)
            || vm_flags.contains(VmFlags::VM_SHARED)
            || !vm_flags.contains(VmFlags::VM_WRITE)
        {
            return VmFaultReason::VM_FAULT_SIGSEGV;
        }

        let page = {
            let mut page_manager = page_manager_lock();
            let mut allocator = crate::arch::mm::LockedFrameAllocator;
            match page_manager.create_one_page(PageType::Normal, PageFlags::empty(), &mut allocator)
            {
                Ok(page) => page,
                Err(_) => return VmFaultReason::VM_FAULT_OOM,
            }
        };
        page.write().copy_from_slice(source);

        let address = self.address_aligned_down();
        let mm = self.mm.clone();
        let _pt_edit = mm.page_table_edit();
        let current = self.mapper.translate(address).map(|(paddr, _)| paddr);
        if current != expected {
            let mut page_manager = page_manager_lock();
            page_manager.remove_page(&page.phys_address());
            return VmFaultReason::VM_FAULT_NOPAGE;
        }
        if let Some(old) = current {
            let mut page_manager = page_manager_lock();
            if page_manager.get(&old).is_some() {
                page_manager.remove_page(&page.phys_address());
                return VmFaultReason::VM_FAULT_NOPAGE;
            }
        }

        PageFaultHandler::attach_fault_mapped_page(&page, &self.vma, mlocked);
        if current.is_some() {
            let table = self.mapper.get_table(address, 0).unwrap();
            let index = table.index_of(address).unwrap();
            table.set_entry(
                index,
                super::page::PageEntry::new(page.phys_address(), pte_flags),
            );
            mm.flush_tlb_range(
                address,
                VirtAddr::new(address.data() + MMArch::PAGE_SIZE),
                MMArch::PAGE_SHIFT as u8,
                false,
            );
            // The replaced external PFN was not RSS-accounted; the new
            // anonymous COW page is managed guest memory and must be.
            mm.account_present_page_add();
        } else if let Some(flush) = self
            .mapper
            .map_phys(address, page.phys_address(), pte_flags)
        {
            flush.flush();
            mm.account_present_page_add();
        } else {
            PageFaultHandler::detach_fault_mapped_page(&page, &self.vma);
            let mut page_manager = page_manager_lock();
            page_manager.remove_page(&page.phys_address());
            return VmFaultReason::VM_FAULT_OOM;
        }
        self.vma.lock().set_mapped(true);
        VmFaultReason::VM_FAULT_COMPLETED | VmFaultReason::VM_FAULT_DONE_COW
    }
}

/// 缺页中断处理结构体
pub struct PageFaultHandler;

enum FilemapMkwriteSize {
    FetchFromInode,
    Stable(usize),
}

impl PageFaultHandler {
    #[inline(always)]
    fn account_new_present_mapping(mm: &Arc<AddressSpace>) {
        mm.account_present_page_add();
    }

    fn mkwrite_finished(ret: VmFaultReason) -> bool {
        ret.intersects(
            VmFaultReason::VM_FAULT_ERROR
                | VmFaultReason::VM_FAULT_NOPAGE
                | VmFaultReason::VM_FAULT_RETRY
                | VmFaultReason::VM_FAULT_COMPLETED,
        )
    }

    fn attach_fault_mapped_page(page: &Arc<Page>, vma: &Arc<LockedVMA>, mlocked: bool) {
        let mut page_guard = page.write();
        page_guard.insert_vma(vma.clone(), mlocked);
    }

    fn detach_fault_mapped_page(page: &Arc<Page>, vma: &Arc<LockedVMA>) {
        let mut page_guard = page.write();
        page_guard.remove_vma(vma.as_ref());
        drop(page_guard);
        InnerAddressSpace::remove_page_unevictable_if_unneeded(page);
    }

    fn file_page_cache(
        pfm: &PageFaultMessage<'_>,
    ) -> Option<Arc<crate::filesystem::page_cache::PageCache>> {
        let vma = pfm.vma();
        let vma_guard = vma.lock();
        let file = vma_guard.vm_file()?;
        file.inode().page_cache()
    }

    /// 处理缺页异常
    /// ## 参数
    ///
    /// - `pfm`: 缺页异常信息
    /// - `mapper`: 页表映射器
    ///
    /// ## 返回值
    /// - VmFaultReason: 页面错误处理信息标志
    #[inline(never)]
    pub unsafe fn handle_mm_fault(mut pfm: PageFaultMessage) -> VmFaultOutcome {
        let flags = pfm.flags();
        let vma = pfm.vma();
        let current_pcb = ProcessManager::current_pcb();
        {
            current_pcb.sched_info().set_state(ProcessState::Runnable);
        }

        if !MMArch::vma_access_permitted(
            vma.clone(),
            flags.contains(FaultFlags::FAULT_FLAG_WRITE),
            flags.contains(FaultFlags::FAULT_FLAG_INSTRUCTION),
            flags.contains(FaultFlags::FAULT_FLAG_REMOTE),
        ) {
            return VmFaultOutcome {
                reason: VmFaultReason::VM_FAULT_SIGSEGV,
                retry_wait: None,
            };
        }

        let guard = vma.lock();
        let vm_flags = *guard.vm_flags();
        drop(guard);
        if unlikely(vm_flags.contains(VmFlags::VM_HUGETLB)) {
            //TODO: 添加handle_hugetlb_fault处理大页缺页异常
        } else {
            let reason = Self::handle_normal_fault(&mut pfm);
            return VmFaultOutcome {
                reason,
                retry_wait: pfm.retry_wait.take(),
            };
        }

        VmFaultOutcome {
            reason: VmFaultReason::VM_FAULT_COMPLETED,
            retry_wait: pfm.retry_wait.take(),
        }
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
        let vma = pfm.vma();
        let mm = pfm.mm().clone();
        {
            let _pt_edit = mm.page_table_edit();
            let mapper = &mut pfm.mapper;
            if mapper.get_entry(address, 3).is_none() {
                mapper
                    .allocate_table(address, 2)
                    .expect("failed to allocate PUD table");
            }
        }
        let page_flags = vma.lock().flags();

        for level in 2..=3 {
            let level = MMArch::PAGE_LEVELS - level;
            {
                let _pt_edit = mm.page_table_edit();
                let mapper = &mut pfm.mapper;
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
        // pte存在
        if let Some(mut entry) = pfm.mapper.get_entry(address, 0) {
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

        if ret.intersects(
            VmFaultReason::VM_FAULT_ERROR
                | VmFaultReason::VM_FAULT_NOPAGE
                | VmFaultReason::VM_FAULT_RETRY,
        ) {
            return ret;
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
        if crate::mm::oom::should_inject_fault_oom() {
            return VmFaultReason::VM_FAULT_OOM | VmFaultReason::VM_FAULT_OOM_INJECTED;
        }
        let address = pfm.address_aligned_down();
        let vma = pfm.vma.clone();
        let mm = pfm.mm().clone();
        let _pt_edit = mm.page_table_edit();
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
                    let mlocked = vma.lock().vm_flags().contains(VmFlags::VM_LOCKED);
                    if let Some(flush) = mapper.map_phys(address, page.phys_address(), flags) {
                        flush.flush();
                        Self::account_new_present_mapping(pfm.mm());
                        Self::attach_fault_mapped_page(&page, &vma, mlocked);
                        return VmFaultReason::VM_FAULT_COMPLETED;
                    } else {
                        return VmFaultReason::VM_FAULT_OOM;
                    }
                }
            }
        }

        // Fallback: private anonymous page (MAP_PRIVATE or non-shared anon)
        let guard = vma.lock();
        let flags = guard.flags();
        let mlocked = guard.vm_flags().contains(VmFlags::VM_LOCKED);
        drop(guard);
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
            drop(page_manager_guard);
            Self::account_new_present_mapping(pfm.mm());
            Self::attach_fault_mapped_page(&page, &vma, mlocked);
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
        let page_cache = Self::file_page_cache(pfm);
        let _invalidate = page_cache
            .as_ref()
            .map(|page_cache| page_cache.invalidate_read());
        let file = pfm.vm_file().unwrap().clone();
        let mut ret = file.with_io_fs(|fs| fs.fault(pfm));

        if unlikely(ret.intersects(
            VmFaultReason::VM_FAULT_ERROR
                | VmFaultReason::VM_FAULT_NOPAGE
                | VmFaultReason::VM_FAULT_RETRY
                | VmFaultReason::VM_FAULT_DONE_COW,
        )) {
            return ret;
        }
        if ret.contains(VmFaultReason::VM_FAULT_COMPLETED) {
            return ret;
        }

        let cache_page = pfm.page.clone().unwrap();
        let mapper = &mut pfm.mapper;

        let mut page_manager_guard = page_manager_lock();
        if let Ok(page) = page_manager_guard
            .copy_page_as_normal(&cache_page.phys_address(), mapper.allocator_mut())
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
        let file = pfm.vm_file().unwrap().clone();
        let fault_first = file.with_io_fs(|fs| fs.fault_before_map_pages());
        let mut ret = VmFaultReason::empty();
        if fault_first {
            ret = file.with_io_fs(|fs| fs.fault(pfm));
            if ret.contains(VmFaultReason::VM_FAULT_COMPLETED)
                || ret.intersects(
                    VmFaultReason::VM_FAULT_ERROR
                        | VmFaultReason::VM_FAULT_NOPAGE
                        | VmFaultReason::VM_FAULT_RETRY,
                )
            {
                return ret;
            }
        }

        // VM_MIXEDMAP faults are owned by the filesystem: generic fault-around
        // would populate page-cache pages and bypass DAX PFN validation.
        let around = if pfm.vm_flags().contains(VmFlags::VM_MIXEDMAP) {
            VmFaultReason::empty()
        } else {
            Self::do_fault_around(pfm)
        };
        ret |= around;
        if !around.is_empty() {
            return ret;
        }
        if pfm.mapper.translate(pfm.address_aligned_down()).is_some() {
            return ret | VmFaultReason::VM_FAULT_COMPLETED;
        }

        if !fault_first {
            ret |= file.with_io_fs(|fs| fs.fault(pfm));
            if ret.contains(VmFaultReason::VM_FAULT_COMPLETED)
                || ret.intersects(
                    VmFaultReason::VM_FAULT_ERROR
                        | VmFaultReason::VM_FAULT_NOPAGE
                        | VmFaultReason::VM_FAULT_RETRY,
                )
            {
                return ret;
            }
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
        let page_cache = Self::file_page_cache(pfm);
        let _invalidate = page_cache
            .as_ref()
            .map(|page_cache| page_cache.invalidate_read());
        let file = pfm.vm_file().unwrap().clone();
        let mut ret = file.with_io_fs(|fs| fs.fault(pfm));

        if ret.intersects(VmFaultReason::VM_FAULT_ERROR) {
            return ret;
        }
        if ret.contains(VmFaultReason::VM_FAULT_COMPLETED) {
            return ret;
        }

        ret = ret.union(file.with_io_fs(|fs| fs.page_mkwrite(pfm)));
        if Self::mkwrite_finished(ret) {
            return ret;
        }

        let cache_page = pfm.page.clone().expect("no cache_page in PageFaultMessage");

        // 将pagecache页设为脏页，以便回收时能够回写
        let page_type = { cache_page.read().page_type().clone() };
        if let PageType::File(info) = page_type {
            if let Some(page_cache) = info.page_cache.upgrade() {
                let mut dirty_reservation = match page_cache.prepare_page_dirty() {
                    Ok(reservation) => reservation,
                    Err(_) => return VmFaultReason::VM_FAULT_SIGBUS,
                };
                cache_page.write().add_flags(PageFlags::PG_DIRTY);
                if page_cache
                    .mark_page_dirty_prepared(info.index, &mut dirty_reservation)
                    .is_err()
                {
                    cache_page.write().remove_flags(PageFlags::PG_DIRTY);
                    return VmFaultReason::VM_FAULT_SIGBUS;
                }
            }
        }
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
        let mm = pfm.mm().clone();

        let Some((old_paddr, _old_flags)) = pfm.mapper.translate(address) else {
            return VmFaultReason::VM_FAULT_NOPAGE;
        };
        let mut page_manager = page_manager_lock();
        let old_page = page_manager.get(&old_paddr);
        if old_page.is_none() {
            drop(page_manager);
            if !vma.lock().vm_flags().contains(VmFlags::VM_MIXEDMAP) {
                panic!("unmanaged PFN {old_paddr:?} installed in a non-mixed VMA");
            }
            let file = vma.lock().vm_file();
            let Some(file) = file else {
                return VmFaultReason::VM_FAULT_SIGBUS;
            };
            // The filesystem owns validation of the DAX mapping identity and
            // source window. Shared mappings complete pfn_mkwrite; private
            // mappings complete COW through PageFaultMessage's guarded helper.
            return if vma.lock().vm_flags().contains(VmFlags::VM_SHARED) {
                file.with_io_fs(|fs| fs.page_mkwrite(pfm))
            } else {
                file.with_io_fs(|fs| fs.fault(pfm))
            };
        }
        let old_page = old_page.unwrap();
        let map_count = old_page.read().map_count();
        drop(page_manager);

        if vma.lock().vm_flags().contains(VmFlags::VM_SHARED) {
            let file = {
                let guard = vma.lock();
                guard.vm_file()
            };
            if let Some(file) = file {
                pfm.set_page(old_page.clone());
                let ret = file.with_io_fs(|fs| fs.page_mkwrite(pfm));
                if Self::mkwrite_finished(ret) {
                    return ret;
                }
            }
        }

        let _pt_edit = mm.page_table_edit();
        let mapper = &mut pfm.mapper;
        let Some(mut entry) = mapper.get_entry(address, 0) else {
            return VmFaultReason::VM_FAULT_NOPAGE;
        };
        if entry.address() != Ok(old_paddr) {
            return VmFaultReason::VM_FAULT_NOPAGE;
        }
        let new_flags = entry.flags().set_write(true).set_dirty(true);

        // 统一为 do_wp_page 所有分支做 mm-aware shootdown：
        // 这里的 `mm` 可能被多个线程/CPU 共享（CLONE_VM / CLONE_THREAD），
        // 任何修改 PTE（RO -> RW 或替换物理页）的分支都必须让远端 CPU 同步失效旧 TLB。
        // 旧的本地 `invlpg` / 无 flush 写法会让其他 CPU 继续持有旧翻译，
        // 导致错误的重复写保护 fault 或访问到已被替换的旧物理页（风险 4 残留）。
        let end = VirtAddr::new(address.data() + MMArch::PAGE_SIZE);

        if vma.lock().vm_flags().contains(VmFlags::VM_SHARED) {
            // 共享映射：原地升级 PTE 为可写，并标记脏。
            let table = mapper.get_table(address, 0).unwrap();
            let i = table.index_of(address).unwrap();
            let page_type = { old_page.read().page_type().clone() };
            if let PageType::File(info) = page_type {
                if let Some(page_cache) = info.page_cache.upgrade() {
                    let mut dirty_reservation = match page_cache.prepare_page_dirty() {
                        Ok(reservation) => reservation,
                        Err(_) => return VmFaultReason::VM_FAULT_SIGBUS,
                    };
                    old_page.write().add_flags(PageFlags::PG_DIRTY);
                    if page_cache
                        .mark_page_dirty_prepared(info.index, &mut dirty_reservation)
                        .is_err()
                    {
                        old_page.write().remove_flags(PageFlags::PG_DIRTY);
                        return VmFaultReason::VM_FAULT_SIGBUS;
                    }
                }
            }
            entry.set_flags(new_flags);
            table.set_entry(i, entry);

            // PTE 从 RO 升级为 RW：其他 CPU 持有的 RO 缓存会导致它们访问时触发虚假写保护 fault，
            // 必须 mm-aware shootdown 让其它 CPU 重新 walk 最新 PTE。
            mm.flush_tlb_range(address, end, MMArch::PAGE_SHIFT as u8, false);
            VmFaultReason::VM_FAULT_COMPLETED
        } else if vma.is_anonymous() {
            // 私有匿名映射，根据引用计数判断是否拷贝页面
            if map_count == 1 {
                // 只剩当前 mm 引用该页：直接原地升级为可写。
                // 注意：这里 `map_count == 1` 仅代表“只有一个 VMA 在 rmap 链上映射到该物理页”，
                // 不代表“只有一个 CPU 在运行这个 mm”，所以仍然需要 mm-aware shootdown。
                let table = mapper.get_table(address, 0).unwrap();
                let i = table.index_of(address).unwrap();
                entry.set_flags(new_flags);
                table.set_entry(i, entry);

                mm.flush_tlb_range(address, end, MMArch::PAGE_SHIFT as u8, false);
                VmFaultReason::VM_FAULT_COMPLETED
            } else {
                let new_page = {
                    let mut page_manager_guard = page_manager_lock();
                    match page_manager_guard.copy_page(&old_paddr, mapper.allocator_mut()) {
                        Ok(page) => page,
                        Err(_) => return VmFaultReason::VM_FAULT_OOM,
                    }
                };
                let new_paddr = new_page.phys_address();

                let table = mapper.get_table(address, 0).unwrap();
                let i = table.index_of(address).unwrap();

                // Copy before publishing the writable PTE. DragonOS does not yet
                // hold a Linux-style per-PTE lock while every fault reader checks
                // the PTE, so do not create a transient empty PTE here.
                table.set_entry(i, super::page::PageEntry::new(new_paddr, new_flags));
                mm.flush_tlb_range(address, end, MMArch::PAGE_SHIFT as u8, false);

                Self::attach_fault_mapped_page(
                    &new_page,
                    &vma,
                    vma.lock().vm_flags().contains(VmFlags::VM_LOCKED),
                );
                Self::detach_fault_mapped_page(&old_page, &vma);
                VmFaultReason::VM_FAULT_COMPLETED
            }
        } else {
            // 私有文件映射，必须拷贝页面
            let new_page = {
                let mut page_manager_guard = page_manager_lock();
                match page_manager_guard.copy_page_as_normal(&old_paddr, mapper.allocator_mut()) {
                    Ok(page) => page,
                    Err(_) => return VmFaultReason::VM_FAULT_OOM,
                }
            };
            let new_paddr = new_page.phys_address();

            let table = mapper.get_table(address, 0).unwrap();
            let i = table.index_of(address).unwrap();
            table.set_entry(i, super::page::PageEntry::new(new_paddr, new_flags));
            mm.flush_tlb_range(address, end, MMArch::PAGE_SHIFT as u8, false);

            Self::attach_fault_mapped_page(
                &new_page,
                &vma,
                vma.lock().vm_flags().contains(VmFlags::VM_LOCKED),
            );
            Self::detach_fault_mapped_page(&old_page, &vma);
            VmFaultReason::VM_FAULT_COMPLETED
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
        let mm = pfm.mm().clone();
        {
            let _pt_edit = mm.page_table_edit();
            let mapper = &mut pfm.mapper;
            if mapper.get_table(address, 0).is_none() {
                mapper
                    .allocate_table(address, 0)
                    .expect("failed to allocate pte table");
            }
        }
        let vma_guard = vma.lock();
        let vma_region = *vma_guard.region();
        drop(vma_guard);

        // 缺页在VMA中的偏移量
        let vm_pgoff = (address.data() - vma_region.start().data()) >> MMArch::PAGE_SHIFT;

        // 缺页在PTE中的偏移量
        let pte_pgoff = (address.data() >> MMArch::PAGE_SHIFT) & MMArch::PAGE_ENTRY_MASK;

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
        {
            let _pt_edit = mm.page_table_edit();
            let mapper = &mut pfm.mapper;
            if mapper.get_table(address, 0).is_none() && mapper.allocate_table(address, 0).is_none()
            {
                return VmFaultReason::VM_FAULT_OOM;
            }
        }

        let file = pfm.vm_file().unwrap().clone();
        let start_pgoff = backing_pgoff - (pte_pgoff - from_pte);
        let end_pgoff = backing_pgoff + (to_pte - pte_pgoff);
        file.with_io_fs(|fs| fs.map_pages(pfm, start_pgoff, end_pgoff));

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
        let mm = pfm.mm().clone();
        let _pt_edit = mm.page_table_edit();
        let mapper = &mut pfm.mapper;
        let mlocked = vma_guard.vm_flags().contains(VmFlags::VM_LOCKED);
        let vma_flags = *vma_guard.vm_flags();
        let entry_flags = vma_guard.flags();
        let region_start = vma_guard.region().start;
        let backing_page_offset = vma_guard
            .backing_page_offset()
            .expect("backing_page_offset is none");
        let is_private_file_vma =
            vma_guard.vm_file().is_some() && !vma_flags.contains(VmFlags::VM_SHARED);

        // 起始页地址
        let addr = region_start + ((start_pgoff - backing_page_offset) << MMArch::PAGE_SHIFT);
        drop(vma_guard);

        for pgoff in start_pgoff..end_pgoff {
            if let Some(page_pin) = page_cache.manager().peek_page_pinned(pgoff) {
                let page = page_pin.page();
                let page_guard = page.upread();
                if !page_guard.flags().contains(PageFlags::PG_UPTODATE) {
                    continue;
                }
                let phys = page.phys_address();
                drop(page_guard);

                let address =
                    VirtAddr::new(addr.data() + ((pgoff - start_pgoff) << MMArch::PAGE_SHIFT));
                if mapper.get_entry(address, 0).is_none() {
                    let mut flags = entry_flags;
                    if is_private_file_vma
                        || vma_flags.contains(VmFlags::VM_SHARED | VmFlags::VM_WRITE)
                    {
                        flags = flags.set_write(false);
                    }
                    Self::attach_fault_mapped_page(&page, &vma, mlocked);
                    if let Some(flush) = mapper.map_phys(address, phys, flags) {
                        flush.flush();
                        Self::account_new_present_mapping(&mm);
                    } else {
                        Self::detach_fault_mapped_page(&page, &vma);
                    }
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

        if let Ok(md) = file.inode().metadata() {
            let size = md.size.max(0) as usize;
            if size == 0 || backing_pgoff.saturating_mul(MMArch::PAGE_SIZE) >= size {
                return VmFaultReason::VM_FAULT_SIGBUS;
            }
        }

        if !page_cache.is_page_ready(backing_pgoff) {
            ret = VmFaultReason::VM_FAULT_MAJOR;
        }

        match page_cache.manager().commit_page_pinned(backing_pgoff) {
            Ok(page_pin) => {
                let page = page_pin.page();
                pfm.page = Some(page);
                pfm.page_pin = Some(page_pin);
            }
            Err(SystemError::ENOMEM) => {
                return VmFaultReason::VM_FAULT_OOM;
            }
            Err(_) => {
                return VmFaultReason::VM_FAULT_SIGBUS;
            }
        }
        ret
    }

    pub unsafe fn filemap_page_mkwrite(pfm: &mut PageFaultMessage) -> VmFaultReason {
        Self::filemap_page_mkwrite_inner(pfm, FilemapMkwriteSize::FetchFromInode)
    }

    /// Prepare a shared writable file-backed page using an inode size which
    /// the filesystem has already stabilized against truncate and dirty-page
    /// admission.  Filesystems whose metadata operation can enter userspace
    /// must use this entry point from a fault critical section.
    pub unsafe fn filemap_page_mkwrite_with_stable_size(
        pfm: &mut PageFaultMessage,
        stable_size: usize,
    ) -> VmFaultReason {
        Self::filemap_page_mkwrite_inner(pfm, FilemapMkwriteSize::Stable(stable_size))
    }

    unsafe fn filemap_page_mkwrite_inner(
        pfm: &mut PageFaultMessage,
        size_source: FilemapMkwriteSize,
    ) -> VmFaultReason {
        let vma = pfm.vma();
        let vma_guard = vma.lock();
        let file = vma_guard.vm_file().expect("no vm_file in vma");
        drop(vma_guard);

        let page = match pfm.page.clone() {
            Some(page) => page,
            None => {
                log::warn!("filemap_page_mkwrite: SIGBUS — pfm.page is None");
                return VmFaultReason::VM_FAULT_SIGBUS;
            }
        };
        let (page_cache, page_index) = match page.read().page_type().clone() {
            PageType::File(info) => match info.page_cache.upgrade() {
                Some(page_cache) => (page_cache, info.index),
                None => {
                    log::warn!(
                        "filemap_page_mkwrite: SIGBUS — PageType::File but page_cache weak ref invalid"
                    );
                    return VmFaultReason::VM_FAULT_SIGBUS;
                }
            },
            other => {
                log::warn!(
                    "filemap_page_mkwrite: SIGBUS — page type is {:?}, not File",
                    other
                );
                return VmFaultReason::VM_FAULT_SIGBUS;
            }
        };

        let backing_pgoff = match pfm.backing_pgoff {
            Some(backing_pgoff) => backing_pgoff,
            None => {
                log::warn!("filemap_page_mkwrite: SIGBUS — backing_pgoff is None");
                return VmFaultReason::VM_FAULT_SIGBUS;
            }
        };
        if page_index != backing_pgoff {
            log::warn!(
                "filemap_page_mkwrite: RETRY — page_index {} != backing_pgoff {}",
                page_index,
                backing_pgoff
            );
            return VmFaultReason::VM_FAULT_RETRY;
        }

        let size = match size_source {
            FilemapMkwriteSize::FetchFromInode => file
                .inode()
                .metadata()
                .ok()
                .map(|metadata| metadata.size.max(0) as usize),
            FilemapMkwriteSize::Stable(size) => Some(size),
        };
        if let Some(size) = size {
            if size == 0 || backing_pgoff.saturating_mul(MMArch::PAGE_SIZE) >= size {
                log::warn!(
                    "filemap_page_mkwrite: SIGBUS — file size {} too small for pgoff {}",
                    size,
                    backing_pgoff
                );
                return VmFaultReason::VM_FAULT_SIGBUS;
            }
        }

        match page_cache.manager().prepare_page_mkwrite(page_index, &page) {
            Ok(()) => {}
            Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                if let Some(wait) = page_cache
                    .manager()
                    .page_mkwrite_retry_wait(page_index, &page)
                {
                    pfm.set_retry_wait(wait);
                }
                return VmFaultReason::VM_FAULT_RETRY;
            }
            Err(e) => {
                log::warn!(
                    "filemap_page_mkwrite: SIGBUS — prepare_page_mkwrite failed: {:?}",
                    e
                );
                return VmFaultReason::VM_FAULT_SIGBUS;
            }
        }
        VmFaultReason::empty()
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

        match page_cache.manager().commit_overwrite_pinned(backing_pgoff) {
            Ok(page_pin) => {
                let page = page_pin.page();
                pfm.page = Some(page);
                pfm.page_pin = Some(page_pin);
                VmFaultReason::empty()
            }
            Err(SystemError::ENOMEM) => VmFaultReason::VM_FAULT_OOM,
            Err(_) => VmFaultReason::VM_FAULT_SIGBUS,
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
        if crate::mm::oom::should_inject_fault_oom() {
            return VmFaultReason::VM_FAULT_OOM | VmFaultReason::VM_FAULT_OOM_INJECTED;
        }
        let vma = pfm.vma();
        let vma_guard = vma.lock();
        let flags = pfm.flags();
        let cache_page = pfm.page.clone();
        let cow_page = pfm.cow_page.clone();
        let address = pfm.address();
        let mm = pfm.mm().clone();
        let _pt_edit = mm.page_table_edit();
        let mapper = &mut pfm.mapper;

        let is_private_file_vma =
            vma_guard.vm_file().is_some() && !vma_guard.vm_flags().contains(VmFlags::VM_SHARED);
        let page_to_map = if flags.contains(FaultFlags::FAULT_FLAG_WRITE) && is_private_file_vma {
            // 私有文件映射的写时复制
            cow_page.expect("no cow_page in PageFaultMessage")
        } else {
            // 直接映射到PageCache
            cache_page.expect("no cache_page in PageFaultMessage")
        };

        let page_phys = page_to_map.phys_address();
        let mlocked = vma_guard.vm_flags().contains(VmFlags::VM_LOCKED);

        let mut map_flags = vma_guard.flags();
        if (is_private_file_vma
            || vma_guard
                .vm_flags()
                .contains(VmFlags::VM_SHARED | VmFlags::VM_WRITE))
            && !flags.contains(FaultFlags::FAULT_FLAG_WRITE)
        {
            map_flags = map_flags.set_write(false);
        }
        drop(vma_guard);

        Self::attach_fault_mapped_page(&page_to_map, &vma, mlocked);
        let result = if let Some(flush) = mapper.map_phys(address, page_phys, map_flags) {
            flush.flush();
            Self::account_new_present_mapping(pfm.mm());
            VmFaultReason::VM_FAULT_COMPLETED
        } else {
            Self::detach_fault_mapped_page(&page_to_map, &vma);
            VmFaultReason::VM_FAULT_OOM
        };
        pfm.page_pin.take();
        result
    }

    /// Map a zeroed anonymous page for /dev/zero style mappings.
    pub unsafe fn zero_fault(pfm: &mut PageFaultMessage) -> VmFaultReason {
        if crate::mm::oom::should_inject_fault_oom() {
            return VmFaultReason::VM_FAULT_OOM | VmFaultReason::VM_FAULT_OOM_INJECTED;
        }
        let address = pfm.address_aligned_down();
        let vma = pfm.vma();
        let guard = vma.lock();
        let flags = guard.flags();
        let mlocked = guard.vm_flags().contains(VmFlags::VM_LOCKED);
        drop(guard);
        let mm = pfm.mm().clone();
        let _pt_edit = mm.page_table_edit();
        let mapper = &mut pfm.mapper;

        if let Some(flush) = mapper.map(address, flags) {
            flush.flush();
            let mut pm = page_manager_lock();
            let paddr = mapper.translate(address).unwrap().0;
            let page = pm.get_unwrap(&paddr);
            drop(pm);
            Self::account_new_present_mapping(pfm.mm());
            Self::attach_fault_mapped_page(&page, &vma, mlocked);
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
        if crate::mm::oom::should_inject_fault_oom() {
            return VmFaultReason::VM_FAULT_OOM | VmFaultReason::VM_FAULT_OOM_INJECTED;
        }
        let vma = pfm.vma();
        let vma_guard = vma.lock();
        let backing_pgoff = match vma_guard.backing_page_offset() {
            Some(off) => off,
            None => return VmFaultReason::VM_FAULT_SIGBUS,
        };
        let base = vma_guard.region().start();
        let flags = vma_guard.flags();
        let mlocked = vma_guard.vm_flags().contains(VmFlags::VM_LOCKED);
        drop(vma_guard);

        let mm = pfm.mm().clone();
        let _pt_edit = mm.page_table_edit();
        let mapper = &mut pfm.mapper;
        for pgoff in start_pgoff..end_pgoff {
            let addr = VirtAddr::new(base.data() + ((pgoff - backing_pgoff) << MMArch::PAGE_SHIFT));
            if let Some(flush) = mapper.map(addr, flags) {
                flush.flush();
                let mut pm = page_manager_lock();
                let paddr = mapper.translate(addr).unwrap().0;
                let page = pm.get_unwrap(&paddr);
                drop(pm);
                Self::account_new_present_mapping(&mm);
                Self::attach_fault_mapped_page(&page, &vma, mlocked);
            } else {
                return VmFaultReason::VM_FAULT_OOM;
            }
        }

        VmFaultReason::VM_FAULT_COMPLETED
    }
}
