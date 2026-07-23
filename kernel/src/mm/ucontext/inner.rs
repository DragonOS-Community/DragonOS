use super::*;

#[derive(Debug)]
pub struct InnerAddressSpace {
    pub(super) mm_id: u64,
    pub user_mapper: UserMapper,
    pub mappings: UserMappings,
    /// Number of locked user pages, in page units.
    pub locked_vm: usize,
    /// Flags inherited by future mappings after mlockall(MCL_FUTURE).
    pub mlock_future: VmFlags,
    pub mmap_min: VirtAddr,
    /// User stack information struct
    pub user_stack: Option<UserStack>,

    pub elf_brk_start: VirtAddr,
    pub elf_brk: VirtAddr,

    /// Start address of the current process's heap
    pub brk_start: VirtAddr,
    /// End address of the current process's heap (exclusive)
    pub brk: VirtAddr,

    pub start_code: VirtAddr,
    pub end_code: VirtAddr,
    pub start_data: VirtAddr,
    pub end_data: VirtAddr,

    /// Weak reference back to the outer `AddressSpace`.
    ///
    /// Back-filled by `AddressSpace::new` after constructing the Arc; used by internal
    /// `munmap`/`mprotect`/... to obtain `Arc<AddressSpace>` for constructing `MmuGather`
    /// and initiating TLB shootdown.
    pub(super) outer: Weak<AddressSpace>,
}

impl InnerAddressSpace {
    /// Current virtual memory usage of this address space in bytes (simple sum of all VMA sizes)
    pub fn vma_usage_bytes(&self) -> usize {
        let vma_bytes = self
            .mappings
            .iter_vmas()
            .map(|v| {
                let g = v.lock();
                g.region().size()
            })
            .sum::<usize>();
        vma_bytes.saturating_add(self.mappings.reservation_usage_bytes())
    }

    pub fn new(_create_stack: bool) -> Result<Self, SystemError> {
        let result = Self {
            mm_id: 0,
            user_mapper: MMArch::setup_new_usermapper()?,
            mappings: UserMappings::new(),
            locked_vm: 0,
            mlock_future: VmFlags::VM_NONE,
            mmap_min: VirtAddr(DEFAULT_MMAP_MIN_ADDR),
            elf_brk_start: VirtAddr::new(0),
            elf_brk: VirtAddr::new(0),
            brk_start: MMArch::USER_BRK_START,
            brk: MMArch::USER_BRK_START,
            user_stack: None,
            start_code: VirtAddr(0),
            end_code: VirtAddr(0),
            start_data: VirtAddr(0),
            end_data: VirtAddr(0),
            outer: Weak::new(),
        };

        return Ok(result);
    }

    /// Attempt to clone the current address space, including all its mappings
    ///
    /// # Returns
    ///
    /// Returns an Arc pointer to the new cloned address space
    #[inline(never)]
    pub fn try_clone(&mut self) -> Result<Arc<AddressSpace>, SystemError> {
        if self.mappings.first_reservation_region().is_some() {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }

        let new_addr_space = AddressSpace::new(false)?;
        let mut new_guard = new_addr_space.write();

        // The parent mm may be shared by multiple threads (CLONE_VM / CLONE_THREAD), meaning threads running on other CPUs
        // may still have writable TLB entries cached in the parent page tables. When COW write-protects the parent PTEs below,
        // an mm-aware TLB shootdown is required: a local invlpg alone would still allow remote CPUs to write through stale writable TLB entries,
        // breaking COW semantics (residual risk 4). Here we use MmuGather to accumulate the full rewritten range;
        // after the loop, tlb.finish() triggers flush_tlb_mm_range to synchronously shoot down all active CPUs of the parent mm.
        let parent_mm = self
            .outer
            .upgrade()
            .expect("InnerAddressSpace::try_clone called before AddressSpace::new finished");
        let mut parent_tlb = MmuGather::gather(&parent_mm);

        // Only copy the user stack's structural info (metadata); actual user stack page content is handled in the VMA loop below
        unsafe {
            new_guard.user_stack = Some(self.user_stack.as_ref().unwrap().clone_info_only());
        }

        // Copy holes
        new_guard.mappings.vm_holes = self.mappings.vm_holes.clone();

        // Copy other address space attributes
        new_guard.brk = self.brk;
        new_guard.brk_start = self.brk_start;
        new_guard.mmap_min = self.mmap_min;
        new_guard.elf_brk = self.elf_brk;
        new_guard.elf_brk_start = self.elf_brk_start;
        new_guard.start_code = self.start_code;
        new_guard.end_code = self.end_code;
        new_guard.start_data = self.start_data;
        new_guard.end_data = self.end_data;

        let mut parent_cow_remaps: Vec<(VirtAddr, EntryFlags<MMArch>)> = Vec::new();
        let mut child_present_pages = 0usize;
        let clone_result: Result<(), SystemError> = (|| {
            // Iterate over each VMA of the parent process and perform appropriate copying based on VMA attributes
            // Reference Linux: https://code.dragonos.org.cn/xref/linux-6.6.21/mm/memory.c#copy_page_range
            for vma in self.mappings.vmas.iter() {
                // Lock ordering: VMA lock -> page_manager -> shm_manager, to avoid deadlocks from cross-acquisition.
                let vma_guard = vma.lock();

                // VM_DONTCOPY: skip VMAs that should not be copied (e.g., those marked with MADV_DONTFORK)
                if vma_guard.vm_flags().contains(VmFlags::VM_DONTCOPY) {
                    drop(vma_guard);
                    continue;
                }

                let vm_flags = *vma_guard.vm_flags();
                let is_shared = vm_flags.contains(VmFlags::VM_SHARED);
                let region = *vma_guard.region();
                let page_flags = vma_guard.flags();
                let sysv_shm = vma_guard.sysv_shm();

                // Create new VMA
                let mut child_vma = vma_guard.clone_info_only();
                child_vma.vm_flags &= VmFlags::VM_LOCKED_CLEAR_MASK;
                let new_vma = LockedVMA::new(child_vma);
                new_guard.mappings.insert_vma(new_vma.clone());
                drop(vma_guard);

                if let Some(sysv_shm) = sysv_shm {
                    if let Err(err) = sysv_shm.open_vma() {
                        if let Some(removed) = new_guard.mappings.remove_vma(&region) {
                            removed.lock().set_mapped(false);
                        }
                        return Err(err);
                    }
                }

                // Apply different page copy strategies based on VMA type
                let start_page = region.start();
                let end_page = region.end();
                let mut current_page = start_page;

                {
                    let _parent_pt_edit = parent_mm.page_table_edit();
                    let old_mapper = &mut self.user_mapper.utable;
                    let new_mapper = &mut new_guard.user_mapper.utable;
                    let new_vma_mlocked = new_vma.lock().vm_flags().contains(VmFlags::VM_LOCKED);
                    let mut page_manager_guard = page_manager_lock();

                    while current_page < end_page {
                        if let Some((phys_addr, old_flags)) = old_mapper.translate(current_page) {
                            unsafe {
                                if is_shared {
                                    let child_flags =
                                        if page_manager_guard.get(&phys_addr).is_none()
                                            && vm_flags.contains(VmFlags::VM_MIXEDMAP)
                                        {
                                            // Preserve the actual external PTE permission. Using
                                            // VMA page flags here could make a deliberately RO DAX
                                            // PFN writable in the child without pfn_mkwrite.
                                            old_flags
                                        } else {
                                            page_flags
                                        };
                                    if new_mapper
                                        .map_phys(current_page, phys_addr, child_flags)
                                        .is_none()
                                    {
                                        return Err(SystemError::ENOMEM);
                                    }
                                } else {
                                    let cow_flags = page_flags.set_write(false);

                                    if old_flags.has_write() {
                                        if let Some(flush) =
                                            old_mapper.remap(current_page, cow_flags)
                                        {
                                            flush.ignore();
                                            parent_tlb.accumulate_range(current_page);
                                            parent_cow_remaps.push((current_page, old_flags));
                                        }
                                    }

                                    if new_mapper
                                        .map_phys(current_page, phys_addr, cow_flags)
                                        .is_none()
                                    {
                                        return Err(SystemError::ENOMEM);
                                    }
                                }
                                if let Some(page) = page_manager_guard.get(&phys_addr) {
                                    page.write().insert_vma(new_vma.clone(), new_vma_mlocked);
                                }
                                if page_manager_guard.get(&phys_addr).is_some() {
                                    child_present_pages += 1;
                                }
                            }
                        }
                        current_page = VirtAddr::new(current_page.data() + MMArch::PAGE_SIZE);
                    }
                }
            }
            Ok(())
        })();

        if let Err(err) = clone_result {
            {
                let _parent_pt_edit = parent_mm.page_table_edit();
                let old_mapper = &mut self.user_mapper.utable;
                for (page, flags) in parent_cow_remaps.into_iter().rev() {
                    if let Some(flush) = unsafe { old_mapper.remap(page, flags) } {
                        unsafe { flush.ignore() };
                        parent_tlb.accumulate_range(page);
                    } else {
                        warn!("fork rollback lost expected parent PTE at {:?}", page);
                    }
                }
            }
            drop(new_guard);
            parent_tlb.finish();
            return Err(err);
        }

        drop(new_guard);
        new_addr_space.account_present_pages_add(child_present_pages);
        // Complete the parent mm's mm-aware shootdown: INV-3 requires TLB completion before continuing with subsequent logic;
        // since no pages enter pending_pages here, this actually only triggers flush_tlb_mm_range.
        parent_tlb.finish();
        return Ok(new_addr_space);
    }

    /// Check if the stack can be extended
    pub fn can_extend_stack(&self, bytes: usize) -> bool {
        let bytes = page_align_up(bytes);
        let stack = self.user_stack.as_ref().unwrap();
        let new_size = stack.mapped_size + bytes;
        if new_size > stack.max_limit {
            // Don't exceed the maximum stack size
            return false;
        }
        return true;
    }

    /// Extend the user stack
    /// ## Parameters
    ///
    /// - `bytes`: extension size
    pub fn extend_stack(&mut self, mut bytes: usize) -> Result<(), SystemError> {
        // log::debug!("extend user stack");

        // Layout
        // -------------- high->sp
        // | stack pages|
        // |------------|
        // | stack pages|
        // |------------|
        // | not mapped |
        // -------------- low

        let prot_flags = ProtFlags::PROT_READ | ProtFlags::PROT_WRITE | ProtFlags::PROT_EXEC;
        let map_flags = MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS | MapFlags::MAP_GROWSDOWN;
        let stack = self.user_stack.as_mut().unwrap();

        bytes = page_align_up(bytes);
        stack.mapped_size += bytes;
        // map new stack pages
        let extend_stack_start = stack.stack_bottom - stack.mapped_size;

        self.map_anonymous(
            extend_stack_start,
            bytes,
            prot_flags,
            map_flags,
            false,
            false,
        )?;
        return Ok(());
    }

    /// Check whether this address space is the current process's address space
    #[inline]
    pub fn is_current(&self) -> bool {
        return self.user_mapper.utable.is_current();
    }

    /// Obtain the outer `Arc<AddressSpace>`.
    ///
    /// Back-filled during `AddressSpace::new` construction; upgrade should always succeed on normal paths.
    /// Returns `None` only in extreme scenarios (Weak not yet assigned / mm already destroyed).
    #[inline]
    pub fn outer_addr_space(&self) -> Option<Arc<AddressSpace>> {
        self.outer.upgrade()
    }

    pub(super) fn has_mlock_quota() -> bool {
        let pcb = ProcessManager::current_pcb();
        pcb.get_rlimit(RLimitID::Memlock).rlim_cur != 0
            || pcb.cred().has_capability(CAPFlags::CAP_IPC_LOCK)
    }

    pub(super) fn check_mlock_rlimit_for_pages(
        &self,
        new_pages: usize,
        error: SystemError,
    ) -> Result<(), SystemError> {
        let pcb = ProcessManager::current_pcb();
        if pcb.cred().has_capability(CAPFlags::CAP_IPC_LOCK) {
            return Ok(());
        }

        let total_pages = self.locked_vm.checked_add(new_pages).ok_or(error.clone())?;
        let total_bytes = (total_pages as u128) * (MMArch::PAGE_SIZE as u128);
        let rlimit = pcb.get_rlimit(RLimitID::Memlock).rlim_cur;
        if total_bytes > rlimit as u128 {
            return Err(error);
        }

        Ok(())
    }

    pub(super) fn check_rlimit_as_for_bytes(&self, len: usize) -> Result<(), SystemError> {
        self.check_rlimit_as_for_growth(len)
    }

    pub(super) fn check_rlimit_as_for_region(
        &self,
        region: VirtRegion,
        len: usize,
        map_flags: MapFlags,
    ) -> Result<(), SystemError> {
        let covered = if map_flags.contains(MapFlags::MAP_FIXED) {
            self.covered_vma_bytes(region)
        } else {
            0
        };
        self.check_rlimit_as_for_growth(len.saturating_sub(covered))
    }

    fn check_rlimit_as_for_growth(&self, growth: usize) -> Result<(), SystemError> {
        if growth == 0 {
            return Ok(());
        }

        if !ProcessManager::initialized() {
            return Ok(());
        }

        let rlim_as = ProcessManager::current_pcb()
            .get_rlimit(RLimitID::As)
            .rlim_cur as usize;
        if rlim_as == usize::MAX {
            return Ok(());
        }

        let limit_pages = rlim_as >> MMArch::PAGE_SHIFT;
        let used_pages = self.vma_usage_bytes() >> MMArch::PAGE_SHIFT;
        let growth_pages = growth
            .checked_add(MMArch::PAGE_SIZE - 1)
            .ok_or(SystemError::ENOMEM)?
            >> MMArch::PAGE_SHIFT;
        if used_pages
            .checked_add(growth_pages)
            .is_none_or(|v| v > limit_pages)
        {
            Err(SystemError::ENOMEM)
        } else {
            Ok(())
        }
    }

    fn covered_vma_bytes(&self, region: VirtRegion) -> usize {
        self.mappings
            .conflicts(region)
            .into_iter()
            .filter_map(|vma| vma.lock().region().intersect(&region))
            .fold(0usize, |total, intersection| {
                total.saturating_add(intersection.size())
            })
    }

    fn mlock_fault_flags(vm_flags: VmFlags) -> Option<FaultFlags> {
        if vm_flags.contains(VmFlags::VM_WRITE) {
            Some(FaultFlags::FAULT_FLAG_WRITE)
        } else if vm_flags.contains(VmFlags::VM_READ) {
            Some(FaultFlags::empty())
        } else if vm_flags.contains(VmFlags::VM_EXEC) {
            Some(FaultFlags::FAULT_FLAG_INSTRUCTION)
        } else {
            None
        }
    }

    fn add_present_page_mlock_ref(&mut self, addr: VirtAddr, vma: &Arc<LockedVMA>) {
        if let Some((paddr, _)) = self.user_mapper.utable.translate(addr) {
            let vm_flags = *vma.lock().vm_flags();
            let mut page_manager_guard = page_manager_lock();
            if let PresentPfn::Managed(page) =
                LockedVMA::classify_present_pfn(&mut page_manager_guard, paddr, vm_flags)
            {
                page.write().add_mlocked_vma_ref(vma);
            }
        }
    }

    pub(super) fn update_present_page_mlock_refs(
        &mut self,
        vma: &Arc<LockedVMA>,
        start: VirtAddr,
        end: VirtAddr,
        old_locked: bool,
        new_locked: bool,
    ) {
        if old_locked == new_locked {
            return;
        }

        let mut pages_to_reclassify = Vec::new();
        let mut vaddr = start;
        while vaddr < end {
            if let Some((paddr, _)) = self.user_mapper.utable.translate(vaddr) {
                let vm_flags = *vma.lock().vm_flags();
                let page = {
                    let mut page_manager_guard = page_manager_lock();
                    match LockedVMA::classify_present_pfn(&mut page_manager_guard, paddr, vm_flags)
                    {
                        PresentPfn::Managed(page) => Some(page),
                        PresentPfn::External => None,
                    }
                };
                let Some(page) = page else {
                    vaddr = VirtAddr::new(vaddr.data() + MMArch::PAGE_SIZE);
                    continue;
                };
                {
                    let mut page_guard = page.write();
                    if new_locked {
                        page_guard.add_mlocked_vma_ref(vma);
                    } else {
                        page_guard.remove_mlocked_vma_ref(vma);
                    }
                }
                if !new_locked {
                    pages_to_reclassify.push(page);
                }
            }
            vaddr = VirtAddr::new(vaddr.data() + MMArch::PAGE_SIZE);
        }

        for page in pages_to_reclassify {
            Self::remove_page_unevictable_if_unneeded(&page);
        }
    }

    fn populate_vma_page(
        &mut self,
        vma: Arc<LockedVMA>,
        addr: VirtAddr,
        fault_flags: FaultFlags,
    ) -> Result<(), SystemError> {
        let mm = self.outer_addr_space().ok_or(SystemError::EFAULT)?;
        let fault = unsafe {
            let message =
                PageFaultMessage::new(vma, addr, fault_flags, &mut self.user_mapper.utable, mm);
            PageFaultHandler::handle_mm_fault(message)
        };

        if fault.reason.contains(VmFaultReason::VM_FAULT_COMPLETED) {
            Ok(())
        } else if fault.reason.contains(VmFaultReason::VM_FAULT_OOM) {
            Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
        } else {
            Err(SystemError::ENOMEM)
        }
    }

    fn populate_vma_intersection(
        &mut self,
        vma: Arc<LockedVMA>,
        intersection: VirtRegion,
        vm_flags: VmFlags,
        fault_in_missing: bool,
    ) -> Result<(), SystemError> {
        if vm_flags.is_mlock_population_unsupported() {
            return Ok(());
        }

        let fault_flags = if fault_in_missing {
            Some(Self::mlock_fault_flags(vm_flags).ok_or(SystemError::ENOMEM)?)
        } else {
            None
        };
        let mut addr = intersection.start();
        while addr < intersection.end() {
            if self.user_mapper.utable.translate(addr).is_some() {
                if vm_flags.contains(VmFlags::VM_LOCKED) {
                    self.add_present_page_mlock_ref(addr, &vma);
                }
            } else if fault_in_missing {
                self.populate_vma_page(vma.clone(), addr, fault_flags.unwrap())?;
            }
            addr = VirtAddr::new(addr.data() + MMArch::PAGE_SIZE);
        }
        Ok(())
    }

    pub(super) fn populate_vma_range(
        &mut self,
        start: VirtAddr,
        len: usize,
        fault_in_missing: bool,
    ) -> Result<(), SystemError> {
        let target = Self::checked_user_region(start, len)?;
        let mut vmas = self.mappings.conflicts(target);
        vmas.sort_by_key(|vma| vma.lock().region().start().data());

        let mut cursor = target.start();
        for vma in vmas {
            let (region, vm_flags) = {
                let guard = vma.lock();
                (*guard.region(), *guard.vm_flags())
            };
            let Some(intersection) = region.intersect(&target) else {
                continue;
            };
            if intersection.start() > cursor {
                return Err(SystemError::ENOMEM);
            }
            if intersection.end() <= cursor {
                continue;
            }

            self.populate_vma_intersection(vma, intersection, vm_flags, fault_in_missing)?;

            cursor = intersection.end();
            if cursor >= target.end() {
                break;
            }
        }

        if cursor != target.end() {
            return Err(SystemError::ENOMEM);
        }

        Ok(())
    }

    /// Populate the current VMAs intersecting an already validated mlock
    /// range.  Unlike the commit-time coverage check, holes created after the
    /// mlock flags were committed are skipped, matching Linux __mm_populate().
    pub(super) fn populate_mlock_range_post_commit(
        &mut self,
        start: VirtAddr,
        len: usize,
    ) -> Result<(), SystemError> {
        let target = Self::checked_user_region(start, len)?;
        let mut vmas = self.mappings.conflicts(target);
        vmas.sort_by_key(|vma| vma.lock().region().start().data());

        for vma in vmas {
            let (region, vm_flags) = {
                let guard = vma.lock();
                (*guard.region(), *guard.vm_flags())
            };
            let Some(intersection) = region.intersect(&target) else {
                continue;
            };
            let fault_in_missing = !vm_flags.contains(VmFlags::VM_LOCKONFAULT);
            self.populate_vma_intersection(vma, intersection, vm_flags, fault_in_missing)?;
        }
        Ok(())
    }

    /// Best-effort population used after mlockall(MCL_CURRENT).  Address-space
    /// holes and per-VMA failures do not prevent later VMAs from being visited.
    pub(super) fn populate_mlockall_post_commit(&mut self) {
        let vmas = self.mappings.iter_vmas().cloned().collect::<Vec<_>>();
        for vma in vmas {
            let (region, vm_flags) = {
                let guard = vma.lock();
                (*guard.region(), *guard.vm_flags())
            };
            let fault_in_missing = !vm_flags.contains(VmFlags::VM_LOCKONFAULT);
            let _ = self.populate_vma_intersection(vma, region, vm_flags, fault_in_missing);
        }
    }

    pub(super) fn best_effort_locked_population(
        &mut self,
        start: VirtAddr,
        len: usize,
        vm_flags: VmFlags,
    ) {
        if len == 0 || !vm_flags.contains(VmFlags::VM_LOCKED) {
            return;
        }

        let fault_in_missing = !vm_flags.contains(VmFlags::VM_LOCKONFAULT);
        let _ = self.populate_vma_range(start, len, fault_in_missing);
    }

    pub(super) fn post_map_population(&mut self, start: VirtAddr, len: usize, map_flags: MapFlags) {
        let Some(vma) = self.mappings.contains(start) else {
            return;
        };

        let vm_flags = *vma.lock().vm_flags();
        let fault_in_missing = map_flags.contains(MapFlags::MAP_POPULATE)
            || (vm_flags.contains(VmFlags::VM_LOCKED)
                && !vm_flags.contains(VmFlags::VM_LOCKONFAULT));
        if fault_in_missing || vm_flags.contains(VmFlags::VM_LOCKED) {
            let _ = self.populate_vma_range(start, len, fault_in_missing);
        }
    }
}
