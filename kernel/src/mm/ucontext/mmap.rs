use super::*;

impl InnerAddressSpace {
    /// Perform anonymous page mapping
    ///
    /// ## Parameters
    ///
    /// - `start_vaddr`: Start address of the mapping
    /// - `len`: Length of the mapping
    /// - `prot_flags`: Protection flags
    /// - `map_flags`: Map flags
    /// - `round_to_min`: Whether to align `start_vaddr` to `mmap_min`. If `true` and `start_vaddr`
    ///   is non-zero, it is aligned to `mmap_min`; otherwise, it is only rounded down to the page boundary.
    /// - `allocate_at_once`: Whether to allocate physical space immediately
    ///
    /// ## Returns
    ///
    /// Returns the starting virtual page frame of the mapping
    pub fn map_anonymous(
        &mut self,
        start_vaddr: VirtAddr,
        len: usize,
        prot_flags: ProtFlags,
        map_flags: MapFlags,
        round_to_min: bool,
        allocate_at_once: bool,
    ) -> Result<VirtPageFrame, SystemError> {
        let (page, notifications) = match self.map_anonymous_collect(
            start_vaddr,
            len,
            prot_flags,
            map_flags,
            round_to_min,
            allocate_at_once,
        ) {
            Ok(outcome) => outcome,
            Err(failure) => {
                debug_assert!(
                    failure.notifications.is_empty(),
                    "locked map_anonymous caller must not replace existing VMAs"
                );
                if !failure.notifications.is_empty() {
                    error!("locked map_anonymous failed after replacing existing VMAs");
                    return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                }
                return Err(failure.err);
            }
        };
        debug_assert!(
            notifications.is_empty(),
            "locked map_anonymous caller must not replace existing VMAs"
        );
        if !notifications.is_empty() {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }
        Ok(page)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn map_anonymous_collect(
        &mut self,
        start_vaddr: VirtAddr,
        len: usize,
        prot_flags: ProtFlags,
        map_flags: MapFlags,
        round_to_min: bool,
        allocate_at_once: bool,
    ) -> Result<(VirtPageFrame, VmaCloseNotifications), MmapFailure> {
        let allocate_at_once = if MMArch::PAGE_FAULT_ENABLED {
            allocate_at_once
        } else {
            true
        };
        // debug!("map_anonymous: start_vaddr = {:?}", start_vaddr);
        // debug!("map_anonymous: len(no align) = {}", len);

        let len = page_align_up(len);

        // debug!("map_anonymous: len = {}", len);

        let fixed_hint = map_flags.intersects(MapFlags::MAP_FIXED | MapFlags::MAP_FIXED_NOREPLACE);
        let (start_page, notifications) = self.mmap_collect(
            AddressSpace::round_mmap_hint(start_vaddr, round_to_min, fixed_hint),
            PageFrameCount::from_bytes(len).unwrap(),
            prot_flags,
            map_flags,
            move |page, count, vm_flags, flags, mapper, flusher| {
                if allocate_at_once {
                    let vma =
                        VMA::zeroed(page, count, vm_flags, flags, mapper, flusher, None, None)?;
                    // For shared anonymous mappings, allocate a stable identity
                    if vm_flags.contains(VmFlags::VM_SHARED) {
                        let mut g = vma.lock();
                        g.shared_anon = Some(AnonSharedMapping::new(count.data()));
                        // Set backing_pgoff to 0 as the base offset for shared-anon mappings.
                        g.backing_pgoff = Some(0);
                    }
                    Ok(vma)
                } else {
                    let vma = LockedVMA::new(VMA::new(
                        VirtRegion::new(page.virt_address(), count.data() * MMArch::PAGE_SIZE),
                        vm_flags,
                        flags,
                        None,
                        None,
                        false,
                    ));
                    if vm_flags.contains(VmFlags::VM_SHARED) {
                        let mut g = vma.lock();
                        g.shared_anon = Some(AnonSharedMapping::new(count.data()));
                        g.backing_pgoff = Some(0);
                    }
                    Ok(vma)
                }
            },
        )?;

        self.post_map_population(start_page.virt_address(), len, map_flags);

        return Ok((start_page, notifications));
    }

    /// Map pages into the process's address space
    ///
    /// # Parameters
    ///
    /// - `addr`: Start address of the mapping. If `None`, the kernel automatically allocates one.
    /// - `page_count`: Number of pages to map
    /// - `prot_flags`: Protection flags
    /// - `map_flags`: Map flags
    /// - `map_func`: Mapping function used to create the VMA
    ///
    /// # Returns
    ///
    /// Returns the starting virtual page frame of the mapping
    ///
    /// # Errors
    ///
    /// - `EINVAL`: Invalid argument
    fn mmap_collect<
        F: FnOnce(
            VirtPageFrame,
            PageFrameCount,
            VmFlags,
            EntryFlags<MMArch>,
            &mut PageMapper,
            &mut dyn Flusher<MMArch>,
        ) -> Result<Arc<LockedVMA>, SystemError>,
    >(
        &mut self,
        addr: Option<VirtAddr>,
        page_count: PageFrameCount,
        prot_flags: ProtFlags,
        map_flags: MapFlags,
        map_func: F,
    ) -> Result<(VirtPageFrame, VmaCloseNotifications), MmapFailure> {
        if page_count == PageFrameCount::new(0) {
            return Err(SystemError::EINVAL.into());
        }
        // debug!("mmap: addr: {addr:?}, page_count: {page_count:?}, prot_flags: {prot_flags:?}, map_flags: {map_flags:?}");

        let vm_flags = VmFlags::from(prot_flags)
            | VmFlags::from(map_flags)
            | self.mlock_future
            | VmFlags::VM_MAYREAD
            | VmFlags::VM_MAYWRITE
            | VmFlags::VM_MAYEXEC;

        if vm_flags.contains(VmFlags::VM_LOCKED) {
            let error = if map_flags.contains(MapFlags::MAP_LOCKED) && !Self::has_mlock_quota() {
                SystemError::EPERM
            } else {
                SystemError::EAGAIN_OR_EWOULDBLOCK
            };
            self.check_mlock_rlimit_for_pages(page_count.data(), error)?;
        }

        let mut notifications = VmaCloseNotifications::default();
        macro_rules! mmap_fail {
            ($err:expr) => {
                return Err(MmapFailure {
                    err: $err,
                    notifications,
                })
            };
        }
        macro_rules! mmap_try {
            ($expr:expr) => {
                match $expr {
                    Ok(value) => value,
                    Err(err) => mmap_fail!(err),
                }
            };
        }

        // First, only resolve the target region; for MAP_FIXED, the destructive replacement is deferred until after the preliminary checks are complete.
        let region = match addr {
            Some(vaddr) => {
                mmap_try!(self.find_free_at_prepare(
                    self.mmap_min,
                    vaddr,
                    page_count.bytes(),
                    map_flags,
                ))
            }
            None => self
                .mappings
                .find_free(self.mmap_min, page_count.bytes())
                .ok_or(SystemError::ENOMEM)?,
        };

        self.check_rlimit_as_for_region(region, page_count.bytes(), map_flags)?;

        if map_flags.contains(MapFlags::MAP_FIXED) && self.mappings.has_conflict(region) {
            let close_notifications = mmap_try!(self.munmap_collect(
                VirtPageFrame::new(region.start()),
                PageFrameCount::from_bytes(region.size()).unwrap(),
            ));
            notifications.extend(close_notifications);
        }

        let page = VirtPageFrame::new(region.start());
        // debug!("mmap: page: {:?}, region={region:?}", page.virt_address());

        let new_locked_vm = if vm_flags.contains(VmFlags::VM_LOCKED) {
            Some(
                self.locked_vm
                    .checked_add(page_count.data())
                    .ok_or(SystemError::ENOMEM)?,
            )
        } else {
            None
        };

        compiler_fence(Ordering::SeqCst);
        // New mapping: the new region had no prior PTE, no TLB invalidation needed.
        // Use DeferredFlusher to silently consume internal PageFlush tokens.
        // If MAP_FIXED support for overwriting existing mappings is added in the future,
        // the caller should first munmap via MmuGather to release old mappings.
        let mut flusher = crate::mm::page::DeferredFlusher::new();
        compiler_fence(Ordering::SeqCst);
        // Map the pages and insert the VMA into the address space's VMA list
        let new_vma = {
            let Some(mm) = self.outer_addr_space() else {
                mmap_fail!(SystemError::EFAULT);
            };
            let _pt_edit = mm.page_table_edit();
            mmap_try!(map_func(
                page,
                page_count,
                vm_flags,
                EntryFlags::from_prot_flags(prot_flags, true),
                &mut self.user_mapper.utable,
                &mut flusher,
            ))
        };
        let new_present_pages = if new_vma.mapped() {
            page_count.data()
        } else {
            0
        };
        self.mappings.insert_vma(new_vma);
        if let Some(new_locked_vm) = new_locked_vm {
            self.locked_vm = new_locked_vm;
        }
        if let Some(mm) = self.outer_addr_space() {
            mm.account_present_pages_add(new_present_pages);
        }

        return Ok((page, notifications));
    }
}
