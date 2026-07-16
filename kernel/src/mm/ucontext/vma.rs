use super::*;

/// Classification of a present user PTE in a VMA.
///
/// A missing `Page` is accepted only for VM_MIXEDMAP. Keeping this check in one
/// place prevents device PFNs from accidentally reaching rmap, LRU, mlock, or
/// the frame allocator.
#[derive(Debug)]
pub enum PresentPfn {
    Managed(Arc<Page>),
    External(PhysAddr),
}

/// A locked VMA (Virtual Memory Area)
///
/// Note: benchmark to determine whether SpinLock or RwLock performs better.
#[derive(Debug)]
pub struct LockedVMA {
    /// Used for hash computation, avoiding the need to acquire the VMA lock for hashing.
    id: usize,
    state_seq: AtomicU64,
    vma: Mutex<VMA>,
}

impl core::hash::Hash for LockedVMA {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

impl PartialEq for LockedVMA {
    fn eq(&self, other: &Self) -> bool {
        self.id.eq(&other.id)
    }
}

impl Eq for LockedVMA {}

#[allow(dead_code)]
impl LockedVMA {
    pub(crate) fn classify_present_pfn(
        page_manager: &mut PageManager,
        paddr: PhysAddr,
        vm_flags: VmFlags,
    ) -> PresentPfn {
        if let Some(page) = page_manager.get(&paddr) {
            return PresentPfn::Managed(page);
        }
        assert!(
            vm_flags.contains(VmFlags::VM_MIXEDMAP),
            "unmanaged PFN {paddr:?} installed in a non-mixed VMA"
        );
        PresentPfn::External(paddr)
    }

    pub fn new(vma: VMA) -> Arc<Self> {
        let r = Arc::new(Self {
            id: LOCKEDVMA_ID_ALLOCATOR.lock().alloc().unwrap(),
            state_seq: AtomicU64::new(0),
            vma: Mutex::new(vma),
        });
        r.vma.lock().self_ref = Arc::downgrade(&r);
        return r;
    }

    pub fn id(&self) -> usize {
        self.id
    }

    pub fn state_seq(&self) -> u64 {
        self.state_seq.load(Ordering::Acquire)
    }

    fn bump_state_seq(&self) {
        self.state_seq.fetch_add(1, Ordering::AcqRel);
    }

    pub fn lock(&self) -> MutexGuard<'_, VMA> {
        return self.vma.lock();
    }

    pub(super) fn prepare_split_lifecycle(
        &self,
        intersection: VirtRegion,
    ) -> Result<VmaSplitLifecycle, VmaSplitFailure> {
        let (original_region, sysv_shm) = {
            let guard = self.lock();
            if intersection == *guard.region() {
                return Ok(VmaSplitLifecycle::none());
            }
            (*guard.region(), guard.sysv_shm())
        };
        let Some(sysv_shm) = sysv_shm else {
            return Ok(VmaSplitLifecycle::none());
        };

        let mut lifecycle = VmaSplitLifecycle {
            sysv_shm: Some(sysv_shm.clone()),
            open_count: 0,
            committed: false,
        };
        if original_region.before(&intersection).is_some() {
            if let Err(err) = sysv_shm.open_vma() {
                return Err(lifecycle.failure(err));
            }
            lifecycle.open_count += 1;
        }
        if original_region.after(&intersection).is_some() {
            if let Err(err) = sysv_shm.open_vma() {
                return Err(lifecycle.failure(err));
            }
            lifecycle.open_count += 1;
        }
        Ok(lifecycle)
    }

    /// Adjust the flags of the pages in the current VMA
    ///
    /// TODO: add the ability to adjust the physical address mapped by a virtual page
    ///
    /// @param flags the new flags
    /// @param mapper the page table mapper
    /// @param flusher the PTE flusher
    ///
    pub fn remap(
        &self,
        flags: EntryFlags<MMArch>,
        mapper: &mut PageMapper,
        mut flusher: impl Flusher<MMArch>,
    ) -> Result<(), SystemError> {
        let mut guard = self.lock();
        let mut page_manager = page_manager_lock();
        for page in guard.region.pages() {
            if let Some((paddr, _)) = mapper.translate(page.virt_address()) {
                let page_flags = if page_manager.get(&paddr).is_none()
                    && guard.vm_flags().contains(VmFlags::VM_MIXEDMAP)
                {
                    // mprotect must not bypass the filesystem's pfn_mkwrite
                    // transaction. A subsequent write fault performs the
                    // external mapping upgrade and PTE identity revalidation.
                    flags.set_write(false)
                } else {
                    flags
                };
                let r = unsafe {
                    mapper
                        .remap(page.virt_address(), page_flags)
                        .expect("Failed to remap")
                };
                flusher.consume(r);
            }
        }
        guard.flags = flags;
        return Ok(());
    }

    /// Unmap the entire VMA, stashing physical pages pending release into `tlb` and accumulating the TLB flush range.
    ///
    /// The caller must complete all PTE clears before calling `tlb.finish()`, which uniformly
    /// performs cross-core TLB shootdown first and then frees physical pages (INV-3).
    pub fn unmap(&self, mapper: &mut PageMapper, tlb: &mut MmuGather<'_>) {
        // todo: if the current VMA is associated with a file, complete the file-related logic
        let (region, should_wakeup_writeback, mm, vm_flags) = {
            let mut self_guard = self.lock();
            let region = *self_guard.region();
            let mm = self_guard.address_space().and_then(|mm| mm.upgrade());
            self_guard.mapped = false;
            let should_wakeup_writeback = self_guard.vm_file().is_some()
                && self_guard
                    .vm_flags()
                    .contains(VmFlags::VM_SHARED | VmFlags::VM_WRITE);
            (region, should_wakeup_writeback, mm, *self_guard.vm_flags())
        };

        let mut pages_to_reclassify = Vec::new();
        let mut unmapped_present_pages = 0usize;
        {
            let mut page_manager_guard = page_manager_lock();
            for page in region.pages() {
                if mapper.translate(page.virt_address()).is_none() {
                    continue;
                }
                let (paddr, _, flush, freed_tables) =
                    unsafe { mapper.unmap_phys_with_freed_tables(page.virt_address(), true) }
                        .expect("Failed to unmap, beacuse of some page is not mapped");

                // Remove the current VMA from anon_vma
                let PresentPfn::Managed(page_arc) =
                    Self::classify_present_pfn(&mut page_manager_guard, paddr, vm_flags)
                else {
                    unsafe { flush.ignore() };
                    tlb.accumulate_range(page.virt_address());
                    if freed_tables {
                        tlb.note_pt_table_freed();
                    }
                    continue;
                };
                {
                    let mut page_guard = page_arc.write();
                    page_guard.remove_vma(self);
                }
                pages_to_reclassify.push((paddr, page_arc));

                // Local PTE cleared; no immediate invlpg. Final TLB invalidation is performed uniformly by MmuGather.
                unsafe { flush.ignore() };
                tlb.accumulate_range(page.virt_address());
                unmapped_present_pages += 1;
                if freed_tables {
                    tlb.note_pt_table_freed();
                }
            }
        }

        for (_, page_arc) in &pages_to_reclassify {
            InnerAddressSpace::remove_page_unevictable_if_unneeded(page_arc);
        }

        if let Some(mm) = mm.as_ref() {
            mm.account_present_pages_sub(unmapped_present_pages);
        }

        let mut page_manager_guard = page_manager_lock();
        for (paddr, page_arc) in pages_to_reclassify {
            // The physical page's VMA list length is 0 and it is not marked as non-reclaimable, so it can be freed.
            // TODO: LRU-based physical page reclamation in the future
            let can_dealloc = page_arc.read().can_deallocate_after_vma_unmap();

            if can_dealloc {
                // Remove this `Arc<Page>` from page_manager, deferring the drop until after TLB shootdown.
                // This guarantees INV-3: other cores can no longer reach the returned-to-buddy physical page via stale TLB.
                if let Some(p) = page_manager_guard.remove_page(&paddr) {
                    tlb.stash_page(p);
                }
            }
        }

        // When the VMA corresponds to a shared file write mapping, wake up the dirty page writeback thread
        if should_wakeup_writeback {
            crate::mm::page::PageReclaimer::wakeup_claim_thread();
        }
    }

    /// Unmap an intersecting sub-range of this VMA while keeping the VMA metadata itself alive.
    ///
    /// This is used by file truncate/invalidate paths: future access should fault back in against
    /// the updated file size/content instead of tearing down the VMA object.
    pub fn unmap_range(
        &self,
        region: VirtRegion,
        mapper: &PageMapper,
        tlb: &mut MmuGather<'_>,
        mode: UnmapMappingMode,
    ) {
        let self_guard = self.lock();
        let Some(intersection) = self_guard.region().intersect(&region) else {
            return;
        };
        let mm = self_guard.address_space().and_then(|mm| mm.upgrade());
        let vma_start = self_guard.region().start();
        let backing_pgoff = self_guard.backing_page_offset();
        let vm_flags = *self_guard.vm_flags();
        let file_page_cache = self_guard
            .vm_file()
            .and_then(|file| file.inode().page_cache());
        drop(self_guard);

        let mut pages_to_reclassify = Vec::new();
        let mut unmapped_present_pages = 0usize;
        {
            let mut page_manager_guard = page_manager_lock();
            for page in intersection.pages() {
                let virt = page.virt_address();
                let Some((paddr, _)) = mapper.translate(virt) else {
                    continue;
                };

                let PresentPfn::Managed(page_arc) =
                    Self::classify_present_pfn(&mut page_manager_guard, paddr, vm_flags)
                else {
                    // A raw mixed PFN belongs to this file VMA. CacheOnly and
                    // EvenCow both zap it; private COW pages are managed and are
                    // filtered below by PageType.
                    let Some((_paddr, _, flush)) =
                        (unsafe { mapper.unmap_phys_preserve_tables(virt) })
                    else {
                        continue;
                    };
                    unsafe { flush.ignore() };
                    tlb.accumulate_range(virt);
                    continue;
                };
                if let Some(page_cache) = file_page_cache.as_ref() {
                    let Some(base_pgoff) = backing_pgoff else {
                        continue;
                    };
                    let pgoff =
                        base_pgoff + ((virt.data() - vma_start.data()) >> MMArch::PAGE_SHIFT);
                    let page_guard = page_arc.read();
                    let is_target_page = match page_guard.page_type() {
                        PageType::File(info) if info.index == pgoff => info
                            .page_cache
                            .upgrade()
                            .is_some_and(|mapped_cache| Arc::ptr_eq(&mapped_cache, page_cache)),
                        // Truncate must also zap private COW pages. For file VMAs those pages are
                        // represented as normal pages, while shared file mappings remain page-cache
                        // backed and are covered by the PageType::File branch above.
                        PageType::Normal if mode == UnmapMappingMode::EvenCow => true,
                        _ => false,
                    };
                    drop(page_guard);
                    if !is_target_page {
                        continue;
                    }
                }

                let Some((paddr, _, flush)) = (unsafe { mapper.unmap_phys_preserve_tables(virt) })
                else {
                    continue;
                };

                {
                    let mut page_guard = page_arc.write();
                    page_guard.remove_vma(self);
                }
                pages_to_reclassify.push((paddr, page_arc));

                unsafe { flush.ignore() };
                tlb.accumulate_range(virt);
                unmapped_present_pages += 1;
            }
        }

        for (_, page_arc) in &pages_to_reclassify {
            InnerAddressSpace::remove_page_unevictable_if_unneeded(page_arc);
        }

        if let Some(mm) = mm.as_ref() {
            mm.account_present_pages_sub(unmapped_present_pages);
        }

        let mut page_manager_guard = page_manager_lock();
        for (paddr, page_arc) in pages_to_reclassify {
            let can_dealloc = page_arc.read().can_deallocate_after_vma_unmap();
            if can_dealloc {
                if let Some(p) = page_manager_guard.remove_page(&paddr) {
                    tlb.stash_page(p);
                }
            }
        }
    }

    pub fn mapped(&self) -> bool {
        return self.vma.lock().mapped;
    }

    /// Split the current VMA into at most three segments (before / middle / after).
    ///
    /// ### Parameters
    /// - `region`: the target split region, must be page-aligned and **must fall completely within** the current VMA.
    /// - `utable`: used to look up virtual-to-physical page mappings for updating the page's reverse mapping (anon_vma).
    ///
    /// ### Return Value
    /// - `Some(VMASplitResult)`: split succeeded.
    ///   - `prev`: the VMA before `region` (may be `None`).
    ///   - `middle`: the VMA corresponding to `region` (the original VMA is shrunk to this segment).
    ///   - `after`: the VMA after `region` (may be `None`).
    /// - `None`: `region` is invalid (not fully contained within the current VMA, or no intersection could be formed).
    ///
    /// ### Critical Side Effects
    /// - **`self` is modified in-place to become `middle`**: the current VMA's (`self`) `region` is changed to the given `region` argument,
    ///   and its `backing_pgoff` is adjusted accordingly. **The returned `middle` is the same VMA as `self`** (the same `Arc` instance),
    ///   so after modification `self` *is* `middle`. This is an in-place modification, not a new VMA creation.
    /// - New VMAs (`before`/`after`) may be created, but they start off in an unmapped state.
    /// - The reverse mappings (anon_vma) of the pages covered by `before`/`after` are updated and removed from the original VMA.
    ///
    /// ### Complex / Implicit Logic Notes
    /// - `backing_pgoff` adjustment for `before`/`after`:
    ///   `after` needs to be offset to the corresponding page offset within the original VMA; `before` keeps the original offset.
    /// - Reason for reverse mapping updates:
    ///   After a VMA split, physical pages should belong to the new VMA (`before`/`after`), otherwise page reclamation/sharing decisions will be wrong.
    /// - When `region` exactly matches the VMA, the current VMA is returned directly to avoid an unnecessary split.
    pub fn extract(&self, region: VirtRegion, utable: &PageMapper) -> Option<VMASplitResult> {
        assert!(region.start().check_aligned(MMArch::PAGE_SIZE));
        assert!(region.end().check_aligned(MMArch::PAGE_SIZE));

        let mut guard = self.lock();

        // ============================================================
        // Early check: handle three boundary cases that do not require a split
        // ============================================================
        // This block handles three special cases where no VMA split is needed:
        // 1. region crosses the VMA's lower or upper boundary → return None
        // 2. region does not intersect the current VMA at all → return None
        // 3. region exactly equals the current VMA → return the current VMA directly, no split needed
        {
            // If the given region crosses the VMA's lower or upper boundary, return None (cannot split).
            // The check `region.start() < vma.start || region.end() > vma.end` is deliberately used as the error condition,
            // not the more common “no-intersection” check (`region.end <= vma.start || region.start >= vma.end`).
            // This ensures that the three segments `before`/`after`/`middle` are always a strict partition of the original VMA.
            if unlikely(region.start() < guard.region.start() || region.end() > guard.region.end())
            {
                return None;
            }
            let intersect: Option<VirtRegion> = guard.region.intersect(&region);

            // If the current VMA.region does not intersect the given region, return None directly
            if unlikely(intersect.is_none()) {
                return None;
            }
            let intersect: VirtRegion = intersect.unwrap();

            // If the current VMA.region exactly equals the given region, no split is needed; return the current VMA directly.
            if unlikely(intersect == guard.region) {
                return Some(VMASplitResult::new(
                    None,
                    guard.self_ref.upgrade().unwrap(),
                    None,
                ));
            }
        }

        let before: Option<Arc<LockedVMA>> = guard.region.before(&region).map(|virt_region| {
            let mut vma: VMA = unsafe { guard.clone() };
            vma.region = virt_region;
            vma.mapped = false;
            // backing_pgoff stays unchanged; before VMA uses the original offset
            let vma: Arc<LockedVMA> = LockedVMA::new(vma);
            vma
        });

        let after: Option<Arc<LockedVMA>> = guard.region.after(&region).map(|virt_region| {
            let mut vma: VMA = unsafe { guard.clone() };
            vma.region = virt_region;
            vma.mapped = false;
            // after VMA needs its backing_pgoff adjusted
            // offset of the after region's start address relative to the original VMA's start address (in pages)
            if let Some(original_pgoff) = vma.backing_pgoff {
                let offset_pages =
                    (virt_region.start() - guard.region.start()) >> MMArch::PAGE_SHIFT;
                vma.backing_pgoff = Some(original_pgoff + offset_pages);
            }
            let vma: Arc<LockedVMA> = LockedVMA::new(vma);
            vma
        });

        let vma_mlocked = guard.vm_flags().contains(VmFlags::VM_LOCKED);
        // Reassign the anon_vma of the physical pages in the before and after VMAs
        let mut page_manager_guard = page_manager_lock();
        if let Some(before) = before.clone() {
            let virt_iter = before.lock().region.iter_pages();
            for frame in virt_iter {
                if let Some((paddr, _)) = utable.translate(frame.virt_address()) {
                    if let Some(page) = page_manager_guard.get(&paddr) {
                        let mut page_guard = page.write();
                        page_guard.insert_vma(before.clone(), vma_mlocked);
                        page_guard.remove_vma(self);
                    } else {
                        assert!(guard.vm_flags().contains(VmFlags::VM_MIXEDMAP));
                    }
                    before.lock().mapped = true;
                }
            }
        }
        if let Some(after) = after.clone() {
            let virt_iter = after.lock().region.iter_pages();
            for frame in virt_iter {
                if let Some((paddr, _)) = utable.translate(frame.virt_address()) {
                    if let Some(page) = page_manager_guard.get(&paddr) {
                        let mut page_guard = page.write();
                        page_guard.insert_vma(after.clone(), vma_mlocked);
                        page_guard.remove_vma(self);
                    } else {
                        assert!(guard.vm_flags().contains(VmFlags::VM_MIXEDMAP));
                    }
                    after.lock().mapped = true;
                }
            }
        }

        // Adjust the region and backing_pgoff of the middle VMA
        let original_start = guard.region.start();
        guard.region = region;
        if let Some(original_pgoff) = guard.backing_pgoff {
            let offset_pages = (region.start() - original_start) >> MMArch::PAGE_SHIFT;
            guard.backing_pgoff = Some(original_pgoff + offset_pages);
        }

        return Some(VMASplitResult::new(
            before,
            guard.self_ref.upgrade().unwrap(),
            after,
        ));
    }

    /// Determine whether this VMA is foreign (not belonging to the current process's address space)
    pub fn is_foreign(&self) -> bool {
        let guard = self.lock();
        if let Some(space) = guard.user_address_space.clone() {
            if let Some(space) = space.upgrade() {
                return AddressSpace::is_current(&space);
            } else {
                return true;
            }
        } else {
            return true;
        }
    }

    /// Determine whether the VMA is accessible
    pub fn is_accessible(&self) -> bool {
        let guard = self.lock();
        let vm_access_flags: VmFlags = VmFlags::VM_READ | VmFlags::VM_WRITE | VmFlags::VM_EXEC;
        guard.vm_flags().intersects(vm_access_flags)
    }

    /// Determine whether the VMA is an anonymous mapping
    pub fn is_anonymous(&self) -> bool {
        let guard = self.lock();
        guard.vm_file.is_none()
    }

    /// Determine whether the VMA is a huge page mapping
    pub fn is_hugepage(&self) -> bool {
        // TODO: implement huge page mapping detection logic; huge page mappings are not currently supported
        false
    }

    pub fn file_pgoff_intersection(
        &self,
        start_page_index: usize,
        end_page_index_exclusive: Option<usize>,
    ) -> Option<VirtRegion> {
        let guard = self.lock();
        let vma_pgoff = guard.backing_page_offset()?;
        let vma_pages = guard.region().size() >> MMArch::PAGE_SHIFT;
        let vma_end = vma_pgoff.saturating_add(vma_pages);
        let intersect_start = core::cmp::max(start_page_index, vma_pgoff);
        let intersect_end = match end_page_index_exclusive {
            Some(end) => core::cmp::min(end, vma_end),
            None => vma_end,
        };
        if intersect_start >= intersect_end {
            return None;
        }

        let offset_pages = intersect_start - vma_pgoff;
        let start = guard.region().start() + (offset_pages << MMArch::PAGE_SHIFT);
        Some(VirtRegion::new(
            start,
            (intersect_end - intersect_start) << MMArch::PAGE_SHIFT,
        ))
    }
}

impl Drop for LockedVMA {
    fn drop(&mut self) {
        LOCKEDVMA_ID_ALLOCATOR.lock().free(self.id);
    }
}

/// Result of a VMA split
#[allow(dead_code)]
pub struct VMASplitResult {
    pub prev: Option<Arc<LockedVMA>>,
    pub middle: Arc<LockedVMA>,
    pub after: Option<Arc<LockedVMA>>,
}

pub(super) type VmaSplitSides = (Option<Arc<LockedVMA>>, Option<Arc<LockedVMA>>);

impl VMASplitResult {
    pub fn new(
        prev: Option<Arc<LockedVMA>>,
        middle: Arc<LockedVMA>,
        post: Option<Arc<LockedVMA>>,
    ) -> Self {
        Self {
            prev,
            middle,
            after: post,
        }
    }
}

/// Parameters for physmap operation
#[derive(Debug)]
pub struct PhysmapParams {
    pub phys: PhysPageFrame,
    pub destination: VirtPageFrame,
    pub count: PageFrameCount,
    pub vm_flags: VmFlags,
    pub flags: EntryFlags<MMArch>,
}

/// Virtual Memory Area
#[derive(Debug)]
pub struct VMA {
    /// Virtual address range of this VMA
    pub(super) region: VirtRegion,
    /// VMA flags
    pub(super) vm_flags: VmFlags,
    /// Flags of the page frames within this VMA
    pub(super) flags: EntryFlags<MMArch>,
    /// Whether the page frames within this VMA have been mapped into the page table
    pub(super) mapped: bool,
    /// The user address space that this VMA belongs to
    pub(super) user_address_space: Option<Weak<AddressSpace>>,
    pub(super) self_ref: Weak<LockedVMA>,

    pub(super) vm_file: Option<Arc<File>>,
    /// The offset (in pages) of the VMA's backing object (file/shared-anonymous) relative to the entire backing object
    pub(super) backing_pgoff: Option<usize>,

    pub(super) provider: Provider,
    /// SysV SHM attach identity, used for Linux-style VMA open/close lifecycle.
    pub(super) sysv_shm: Option<Arc<SysVShmAttach>>,
    /// Stable identity of a shared anonymous mapping (used for cross-process futex key sharing)
    pub(crate) shared_anon: Option<Arc<AnonSharedMapping>>,
}

impl core::hash::Hash for VMA {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.region.hash(state);
        self.flags.hash(state);
        self.mapped.hash(state);
    }
}

/// Describes different types of memory providers or resources
#[derive(Debug)]
pub enum Provider {
    Allocated, // TODO: others
}

/// Stable identity of a shared anonymous mapping
#[derive(Debug)]
pub struct AnonSharedMapping {
    pub id: u64,
    /// Fixed backing size in pages, established at creation time.
    /// Linux semantics: mremap() expanding a MAP_SHARED|MAP_ANONYMOUS mapping does not grow the
    /// underlying shmem object; access beyond this size should SIGBUS.
    size_pages: usize,
    // Per-page cache keyed by page index within the backing object; store physical address.
    pages: SpinLock<HashMap<usize, PhysAddr>>,
}

impl AnonSharedMapping {
    fn new_id() -> u64 {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        return NEXT_ID.fetch_add(1, Ordering::Relaxed);
    }

    pub fn new(size_pages: usize) -> Arc<Self> {
        Arc::new(Self {
            id: Self::new_id(),
            size_pages,
            pages: SpinLock::new(HashMap::new()),
        })
    }

    #[inline(always)]
    pub fn size_pages(&self) -> usize {
        self.size_pages
    }

    /// Get or create a shared page for the given offset atomically.
    /// This prevents the double-allocation race when multiple processes fault the same page.
    pub fn get_or_create_page(&self, pgoff: usize) -> Result<Arc<Page>, SystemError> {
        let mut guard = self.pages.lock_irqsave();
        if let Some(paddr) = guard.get(&pgoff).copied() {
            let mut pm = page_manager_lock();
            return Ok(pm.get_unwrap(&paddr));
        }

        // Allocate while holding the map lock to avoid duplicate creations.
        let mut pm = page_manager_lock();
        let mut allocator = LockedFrameAllocator;
        let page = pm.create_one_page(PageType::Normal, PageFlags::empty(), &mut allocator)?;
        page.write().add_backing_lifetime_pin();
        guard.insert(pgoff, page.phys_address());
        Ok(page)
    }
}

impl Drop for AnonSharedMapping {
    fn drop(&mut self) {
        // When the backing object is destroyed, allow cached pages to be freed.
        let pages: alloc::vec::Vec<PhysAddr> = {
            let guard = self.pages.lock_irqsave();
            guard.values().copied().collect()
        };

        let mut pm = page_manager_lock();
        for paddr in pages {
            if let Some(page) = pm.get(&paddr) {
                let mut pg = page.write();
                pg.remove_backing_lifetime_pin();
                if pg.can_deallocate() {
                    drop(pg);
                    pm.remove_page(&paddr);
                }
            }
        }
    }
}

#[allow(dead_code)]
impl VMA {
    pub fn new(
        region: VirtRegion,
        vm_flags: VmFlags,
        flags: EntryFlags<MMArch>,
        file: Option<Arc<File>>,
        pgoff: Option<usize>,
        mapped: bool,
    ) -> Self {
        VMA {
            region,
            vm_flags,
            flags,
            mapped,
            user_address_space: None,
            self_ref: Weak::default(),
            provider: Provider::Allocated,
            vm_file: file,
            backing_pgoff: pgoff,
            sysv_shm: None,
            shared_anon: None,
        }
    }

    pub fn region(&self) -> &VirtRegion {
        return &self.region;
    }

    pub fn vm_flags(&self) -> &VmFlags {
        return &self.vm_flags;
    }

    pub fn vm_file(&self) -> Option<Arc<File>> {
        return self.vm_file.clone();
    }

    pub fn address_space(&self) -> Option<Weak<AddressSpace>> {
        return self.user_address_space.clone();
    }

    pub fn set_vm_flags(&mut self, vm_flags: VmFlags) {
        let changed = self.vm_flags != vm_flags;
        self.vm_flags = vm_flags;
        if changed {
            if let Some(vma) = self.self_ref.upgrade() {
                vma.bump_state_seq();
            }
        }
    }

    pub fn set_region_size(&mut self, new_region_size: usize) {
        self.region.set_size(new_region_size);
    }

    pub fn set_mapped(&mut self, mapped: bool) {
        self.mapped = mapped;
    }

    pub fn set_flags(&mut self) {
        self.flags = MMArch::vm_get_page_prot(self.vm_flags);
    }

    #[inline(always)]
    pub fn set_sysv_shm(&mut self, sysv_shm: Option<Arc<SysVShmAttach>>) {
        self.sysv_shm = sysv_shm;
    }

    #[inline(always)]
    pub fn sysv_shm(&self) -> Option<Arc<SysVShmAttach>> {
        self.sysv_shm.clone()
    }

    /// # Copy the contents of the current VMA
    ///
    /// ### Safety
    ///
    /// This operation may cause memory leaks, double-free bugs, and other issues if copied incorrectly, so it must be used with care.
    pub unsafe fn clone(&self) -> Self {
        return Self {
            region: self.region,
            vm_flags: self.vm_flags,
            flags: self.flags,
            mapped: self.mapped,
            user_address_space: self.user_address_space.clone(),
            self_ref: self.self_ref.clone(),
            provider: Provider::Allocated,
            backing_pgoff: self.backing_pgoff,
            vm_file: self.vm_file.clone(),
            sysv_shm: self.sysv_shm.clone(),
            shared_anon: self.shared_anon.clone(),
        };
    }

    pub fn clone_info_only(&self) -> Self {
        return Self {
            region: self.region,
            vm_flags: self.vm_flags,
            flags: self.flags,
            mapped: self.mapped,
            user_address_space: None,
            self_ref: Weak::default(),
            provider: Provider::Allocated,
            backing_pgoff: self.backing_pgoff,
            vm_file: self.vm_file.clone(),
            sysv_shm: self.sysv_shm.clone(),
            shared_anon: self.shared_anon.clone(),
        };
    }

    #[inline(always)]
    pub fn flags(&self) -> EntryFlags<MMArch> {
        return self.flags;
    }

    #[inline(always)]
    pub fn backing_page_offset(&self) -> Option<usize> {
        return self.backing_pgoff;
    }

    pub fn pages(&self) -> VirtPageFrameIter {
        return VirtPageFrameIter::new(
            VirtPageFrame::new(self.region.start()),
            VirtPageFrame::new(self.region.end()),
        );
    }

    /// Modify the flags of all existing PTEs covered by this VMA (without changing physical page ownership).
    ///
    /// Does not immediately perform TLB invalidation; the range is accumulated into the passed `MmuGather`,
    /// and the upper layer performs unified shootdown after all modifications are complete
    /// (TLB first, then free/modify, guaranteeing INV-3).
    pub fn remap(
        &mut self,
        flags: EntryFlags<MMArch>,
        mapper: &mut PageMapper,
        tlb: &mut MmuGather<'_>,
    ) {
        let pte_flags = if self.vm_file.is_some()
            && self
                .vm_flags
                .contains(VmFlags::VM_SHARED | VmFlags::VM_WRITE)
        {
            flags.set_write(false)
        } else {
            flags
        };
        let mut page_manager = page_manager_lock();
        for page in self.region.pages() {
            // debug!("remap page {:?}", page.virt_address());
            if let Some((paddr, _)) = mapper.translate(page.virt_address()) {
                let external = self.vm_flags.contains(VmFlags::VM_MIXEDMAP)
                    && page_manager.get(&paddr).is_none();
                let page_flags = if external {
                    // Adding PROT_WRITE must still fault through DAX
                    // pfn_mkwrite/private COW and revalidate the PFN identity.
                    pte_flags.set_write(false)
                } else {
                    pte_flags
                };
                let r = unsafe {
                    mapper
                        .remap(page.virt_address(), page_flags)
                        .expect("Failed to remap")
                };
                unsafe { r.ignore() };
                tlb.accumulate_range(page.virt_address());
            }
            // debug!("consume page {:?}", page.virt_address());
            // debug!("remap page {:?} done", page.virt_address());
        }
        self.flags = flags;
    }

    /// Check whether the current VMA can hold the specified flags
    ///
    /// ## Parameters
    ///
    /// - `prot_flags` the flags to check
    pub fn can_have_flags(&self, prot_flags: ProtFlags) -> bool {
        if prot_flags.contains(ProtFlags::PROT_READ) && !self.vm_flags.contains(VmFlags::VM_MAYREAD)
        {
            return false;
        }
        if prot_flags.contains(ProtFlags::PROT_WRITE)
            && !self.vm_flags.contains(VmFlags::VM_MAYWRITE)
        {
            return false;
        }
        if prot_flags.contains(ProtFlags::PROT_EXEC) && !self.vm_flags.contains(VmFlags::VM_MAYEXEC)
        {
            return false;
        }

        let is_downgrade = (self.flags.has_write() || !prot_flags.contains(ProtFlags::PROT_WRITE))
            && (self.flags.has_execute() || !prot_flags.contains(ProtFlags::PROT_EXEC));

        #[allow(clippy::unneeded_struct_pattern)]
        match self.provider {
            Provider::Allocated { .. } => true,

            #[allow(unreachable_patterns)]
            _ => is_downgrade,
        }
    }

    /// Map a physical address to a virtual address
    ///
    /// @param params physical mapping parameters
    /// @param mapper the page table mapper
    /// @param flusher the PTE flusher
    ///
    /// @return the virtual memory region after mapping
    pub fn physmap(
        params: PhysmapParams,
        mapper: &mut PageMapper,
        mut flusher: impl Flusher<MMArch>,
    ) -> Result<Arc<LockedVMA>, SystemError> {
        let mut cur_phy = params.phys;
        let mut cur_dest = params.destination;

        for _ in 0..params.count.data() {
            // Map the physical page frame to the virtual page frame
            let r = unsafe {
                mapper.map_phys(
                    cur_dest.virt_address(),
                    cur_phy.phys_address(),
                    params.flags,
                )
            }
            .expect("Failed to map phys, may be OOM error");

            // todo: add OOM handling

            // Flush TLB
            flusher.consume(r);

            cur_phy = cur_phy.next();
            cur_dest = cur_dest.next();
        }

        let r: Arc<LockedVMA> = LockedVMA::new(VMA::new(
            VirtRegion::new(
                params.destination.virt_address(),
                params.count.data() * MMArch::PAGE_SIZE,
            ),
            params.vm_flags,
            params.flags,
            None,
            None,
            true,
        ));
        // Add the VMA to anon_vma
        let mut page_manager_guard = page_manager_lock();
        cur_phy = params.phys;
        let vma_mlocked = params.vm_flags.contains(VmFlags::VM_LOCKED);
        for _ in 0..params.count.data() {
            let paddr = cur_phy.phys_address();
            let page = page_manager_guard.get_unwrap(&paddr);
            page.write().insert_vma(r.clone(), vma_mlocked);
            cur_phy = cur_phy.next();
        }

        return Ok(r);
    }

    /// Allocate some physical pages from the page allocator, map them to the specified virtual address, and then create a VMA.
    /// ## Parameters
    ///
    /// - `destination`: the virtual address to map to
    /// - `page_count`: the number of page frames to map
    /// - `vm_flags`: VMA flags
    /// - `flags`: page flags
    /// - `mapper`: the page table mapper
    /// - `flusher`: the PTE flusher
    /// - `file`: the mapped file
    /// - `pgoff`: the backing page offset
    ///
    /// ## Return Value
    /// - Page fault handling info flags
    #[allow(clippy::too_many_arguments)]
    pub fn zeroed(
        destination: VirtPageFrame,
        page_count: PageFrameCount,
        vm_flags: VmFlags,
        flags: EntryFlags<MMArch>,
        mapper: &mut PageMapper,
        mut flusher: impl Flusher<MMArch>,
        file: Option<Arc<File>>,
        pgoff: Option<usize>,
    ) -> Result<Arc<LockedVMA>, SystemError> {
        let mut cur_dest: VirtPageFrame = destination;
        let mut mapped_pages = Vec::new();
        // debug!(
        //     "VMA::zeroed: page_count = {:?}, destination={destination:?}",
        //     page_count
        // );
        for _ in 0..page_count.data() {
            // debug!(
            //     "VMA::zeroed: cur_dest={cur_dest:?}, vaddr = {:?}",
            //     cur_dest.virt_address()
            // );
            let Some(r) = (unsafe { mapper.map(cur_dest.virt_address(), flags) }) else {
                let mut page_manager_guard = page_manager_lock();
                for mapped in mapped_pages.into_iter().rev() {
                    if let Some((paddr, _flags, flush, _freed_tables)) =
                        unsafe { mapper.unmap_phys_with_freed_tables(mapped, true) }
                    {
                        flusher.consume(flush);
                        let _ = page_manager_guard.remove_page(&paddr);
                    }
                }
                return Err(SystemError::ENOMEM);
            };

            // Defer TLB flush; cancel the flush here
            flusher.consume(r);
            mapped_pages.push(cur_dest.virt_address());
            cur_dest = cur_dest.next();
        }
        let r = LockedVMA::new(VMA::new(
            VirtRegion::new(
                destination.virt_address(),
                page_count.data() * MMArch::PAGE_SIZE,
            ),
            vm_flags,
            flags,
            file,
            pgoff,
            true,
        ));
        drop(flusher);
        // debug!("VMA::zeroed: flusher dropped");

        // Zero out this memory and add the VMA to anon_vma
        let mut page_manager_guard = page_manager_lock();
        let virt_iter: VirtPageFrameIter =
            VirtPageFrameIter::new(destination, destination.add(page_count));
        let vma_mlocked = vm_flags.contains(VmFlags::VM_LOCKED);
        for frame in virt_iter {
            let paddr = mapper.translate(frame.virt_address()).unwrap().0;

            // Add the VMA to anon_vma
            let page = page_manager_guard.get_unwrap(&paddr);
            page.write().insert_vma(r.clone(), vma_mlocked);
        }
        // debug!("VMA::zeroed: done");
        return Ok(r);
    }

    pub fn page_address(&self, index: usize) -> Result<VirtAddr, SystemError> {
        if index >= self.backing_pgoff.unwrap() {
            let address =
                self.region.start + ((index - self.backing_pgoff.unwrap()) << MMArch::PAGE_SHIFT);
            if address <= self.region.end() {
                return Ok(address);
            }
        }
        return Err(SystemError::EFAULT);
    }
}

impl Drop for VMA {
    fn drop(&mut self) {
        // When a VMA is dropped, it must have already been unmapped from the page table
        assert!(!self.mapped, "VMA is still mapped");
    }
}

impl PartialEq for VMA {
    fn eq(&self, other: &Self) -> bool {
        return self.region == other.region;
    }
}

impl Eq for VMA {}

impl PartialOrd for VMA {
    fn partial_cmp(&self, other: &Self) -> Option<cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for VMA {
    fn cmp(&self, other: &Self) -> cmp::Ordering {
        return self.region.cmp(&other.region);
    }
}
