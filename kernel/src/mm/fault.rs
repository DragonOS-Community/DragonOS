use core::{alloc::Layout, intrinsics::unlikely, panic};

use alloc::sync::Arc;

use crate::{
    arch::{mm::PageMapper, MMArch},
    mm::{
        page::{page_manager_lock_irqsave, PageFlags},
        ucontext::LockedVMA,
        VirtAddr, VmFaultReason, VmFlags,
    },
    process::{ProcessManager, ProcessState},
};

use crate::mm::MemoryManagementArch;

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
    vma: Arc<LockedVMA>,
    address: VirtAddr,
    flags: FaultFlags,
}

impl PageFaultMessage {
    pub fn new(vma: Arc<LockedVMA>, address: VirtAddr, flags: FaultFlags) -> Self {
        Self {
            vma: vma.clone(),
            address,
            flags,
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

impl Clone for PageFaultMessage {
    fn clone(&self) -> Self {
        Self {
            vma: self.vma.clone(),
            address: self.address,
            flags: self.flags,
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
                    entry.set_flags(PageFlags::from_data(MMArch::ENTRY_FLAG_DIRTY));
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
        panic!(
            "do_fault has not yet been implemented, 
        fault message: {:?}, 
        pid: {}\n",
            pfm,
            crate::process::ProcessManager::current_pid().data()
        );
        // TODO https://code.dragonos.org.cn/xref/linux-6.6.21/mm/memory.c#do_fault
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
        panic!(
            "do_cow_fault has not yet been implemented, 
        fault message: {:?}, 
        pid: {}\n",
            pfm,
            crate::process::ProcessManager::current_pid().data()
        );
        // TODO https://code.dragonos.org.cn/xref/linux-6.6.21/mm/memory.c#do_cow_fault
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
        panic!(
            "do_read_fault has not yet been implemented, 
        fault message: {:?}, 
        pid: {}\n",
            pfm,
            crate::process::ProcessManager::current_pid().data()
        );
        // TODO https://code.dragonos.org.cn/xref/linux-6.6.21/mm/memory.c#do_read_fault
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
}
