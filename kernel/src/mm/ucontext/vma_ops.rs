use super::*;

impl InnerAddressSpace {
    /// Unmap a region in the process's address space
    ///
    /// # Parameters
    ///
    /// - `start_page`: the starting page frame
    /// - `page_count`: the number of page frames to unmap
    ///
    /// # Errors
    ///
    /// - `EINVAL`: invalid argument
    /// - `ENOMEM`: out of memory
    /// - `EFAULT`: VMA state is invalid
    pub fn munmap(
        &mut self,
        start_page: VirtPageFrame,
        page_count: PageFrameCount,
    ) -> Result<(), SystemError> {
        match self.munmap_collect(start_page, page_count) {
            Ok(notifications) => {
                Self::notify_close_notifications(notifications);
                Ok(())
            }
            Err(failure) => {
                Self::notify_close_notifications(failure.notifications);
                Err(failure.err)
            }
        }
    }

    pub(super) fn munmap_collect(
        &mut self,
        start_page: VirtPageFrame,
        page_count: PageFrameCount,
    ) -> Result<VmaCloseNotifications, VmaOpFailure> {
        defer!({
            compiler_fence(Ordering::SeqCst);
        });

        // Get the VMAs associated with the unmap operation (the user-specified region may span multiple VMAs)
        let region_to_unmap = VirtRegion::new(start_page.virt_address(), page_count.bytes());
        let vmas_related: Vec<Arc<LockedVMA>> = self.mappings.conflicts(region_to_unmap);

        // Use MmuGather: clear PTEs + stash pages first, then unified shootdown, and finally free physical pages (INV-3)
        let mm = self.outer_addr_space().ok_or(SystemError::EFAULT)?;
        let mut tlb = MmuGather::gather(&mm);
        let mut notifications = VmaCloseNotifications::default();
        let mut plans: Vec<MunmapVmaPlan> = Vec::with_capacity(vmas_related.len());
        let mut unmapped_vmas: Vec<Arc<LockedVMA>> = Vec::with_capacity(vmas_related.len());
        let mut locked_vm_after_commit = self.locked_vm;

        // Iterate over each related VMA, split the current VMA into possibly three segments, then delete the segment that intersects with the target range.
        // Diagram: for each VMA that intersects region_to_unmap, split it into three segments (before / intersection / after) along the intersection,
        // then unmap only the intersection segment; before/after are re-inserted into mappings.
        //
        //          cur_vma.region (original VMA)
        //      [------------------------------]
        //                region_to_unmap
        //            [----------]
        //                 ||
        //                 \/
        //      before         intersection          after
        //   [--------]      [----------]         [--------]
        //      keep            unmap                keep
        //
        // Note: the user-specified region_to_unmap may span multiple VMAs, so each related VMA must be processed individually.
        //
        // Phase 1 only does validation and SysV split side pre-open, without modifying mappings. This way if a later
        // VMA's open_vma fails due to RMID/reference limits, the earlier VMAs will not have already been removed.
        for cur_vma in vmas_related {
            let (original_region, intersection, locked) = {
                let guard = cur_vma.lock();
                let original_region = *guard.region();
                let Some(intersection) = original_region.intersect(&region_to_unmap) else {
                    for plan in plans {
                        plan.split_lifecycle.rollback_into(&mut notifications);
                    }
                    return Err(VmaOpFailure {
                        err: SystemError::EFAULT,
                        notifications,
                    });
                };
                (
                    original_region,
                    intersection,
                    guard.vm_flags().contains(VmFlags::VM_LOCKED),
                )
            };
            let locked_vm_after_unmap = if locked {
                let Some(next_locked_vm) =
                    locked_vm_after_commit.checked_sub(intersection.size() >> MMArch::PAGE_SHIFT)
                else {
                    for plan in plans {
                        plan.split_lifecycle.rollback_into(&mut notifications);
                    }
                    return Err(VmaOpFailure {
                        err: SystemError::EFAULT,
                        notifications,
                    });
                };
                locked_vm_after_commit = next_locked_vm;
                Some(locked_vm_after_commit)
            } else {
                None
            };

            let split_lifecycle = match cur_vma.prepare_split_lifecycle(intersection) {
                Ok(lifecycle) => lifecycle,
                Err(failure) => {
                    for plan in plans {
                        plan.split_lifecycle.rollback_into(&mut notifications);
                    }
                    let err = failure.rollback_into(&mut notifications);
                    return Err(VmaOpFailure { err, notifications });
                }
            };

            plans.push(MunmapVmaPlan {
                original_region,
                intersection,
                locked_vm_after_unmap,
                split_lifecycle,
            });
        }

        plans.reverse();
        while let Some(plan) = plans.pop() {
            let cur_vma = match self.mappings.remove_vma(&plan.original_region) {
                Some(vma) => vma,
                None => {
                    plan.split_lifecycle.rollback_into(&mut notifications);
                    for plan in plans {
                        plan.split_lifecycle.rollback_into(&mut notifications);
                    }
                    return Err(VmaOpFailure {
                        err: SystemError::EFAULT,
                        notifications,
                    });
                }
            };
            let (before, after) = {
                let _pt_edit = mm.page_table_edit();
                let Some(split_result) =
                    cur_vma.extract(plan.intersection, &self.user_mapper.utable)
                else {
                    self.mappings.insert_vma(cur_vma.clone());
                    plan.split_lifecycle.rollback_into(&mut notifications);
                    for plan in plans {
                        plan.split_lifecycle.rollback_into(&mut notifications);
                    }
                    return Err(VmaOpFailure {
                        err: SystemError::EFAULT,
                        notifications,
                    });
                };
                let before = split_result.prev;
                let after = split_result.after;
                if let Some(locked_vm_after_unmap) = plan.locked_vm_after_unmap {
                    self.locked_vm = locked_vm_after_unmap;
                }
                cur_vma.unmap(&mut self.user_mapper.utable, &mut tlb);
                (before, after)
            };

            if let Some(notification) = Self::collect_vma_close(&cur_vma, plan.intersection) {
                notifications.vma.push(notification);
            }
            if let Some(notification) = Self::collect_sysv_shm_close(&cur_vma) {
                notifications.sysv.push(notification);
            }

            if let Some(before) = before {
                self.mappings.insert_vma(before);
            }

            if let Some(after) = after {
                self.mappings.insert_vma(after);
            }
            plan.split_lifecycle.commit();
            // Keep the removed VMA alive until after TLB shootdown.  Its drop may
            // destroy the last shared-anon backing object, which can release
            // physical pages that were just unmapped above.
            unmapped_vmas.push(cur_vma);
        }

        // Shootdown first, then free physical pages
        tlb.finish();
        drop(unmapped_vmas);

        Ok(notifications)
    }

    pub(super) fn detach_sysv_shm(
        &mut self,
        addr: VirtAddr,
    ) -> Result<VmaCloseNotifications, SystemError> {
        if !addr.check_aligned(MMArch::PAGE_SIZE) {
            return Err(SystemError::EINVAL);
        }

        let mut attach_file = None;
        let mut attach_id = None;
        let mut segment_size = 0usize;
        let mut targets: Vec<Arc<LockedVMA>> = Vec::new();

        for vma in self.mappings.iter_vmas_starting_at(addr) {
            let (region, pgoff, sysv, file) = {
                let guard = vma.lock();
                (
                    *guard.region(),
                    guard.backing_page_offset(),
                    guard.sysv_shm(),
                    guard.vm_file(),
                )
            };
            if !targets.is_empty()
                && region.start().data().saturating_sub(addr.data()) >= segment_size
            {
                break;
            }
            let Some(sysv) = sysv else {
                continue;
            };
            if region.start() < addr {
                continue;
            }
            let Some(pgoff) = pgoff else {
                continue;
            };

            if file.is_none() {
                continue;
            };
            let expected_pgoff = (region.start().data() - addr.data()) >> MMArch::PAGE_SHIFT;
            let first_match = attach_file.is_none();
            if first_match {
                if pgoff != expected_pgoff {
                    continue;
                }
                segment_size = page_align_up(sysv.size());
                attach_id = Some(sysv.attach_id());
                attach_file = file;
            } else {
                if region.end().data().saturating_sub(addr.data()) > segment_size {
                    break;
                }
                if attach_id != Some(sysv.attach_id()) {
                    continue;
                }
                if pgoff != expected_pgoff {
                    continue;
                }
                let Some(expected_file) = attach_file.as_ref() else {
                    continue;
                };
                let Some(file) = file else {
                    continue;
                };
                if !Arc::ptr_eq(&file, expected_file) {
                    continue;
                }
            }
            targets.push(vma.clone());
        }

        if targets.is_empty() {
            return Err(SystemError::EINVAL);
        }

        let locked_pages = targets.iter().try_fold(0usize, |acc, target| {
            let guard = target.lock();
            if guard.vm_flags().contains(VmFlags::VM_LOCKED) {
                acc.checked_add(guard.region().size() >> MMArch::PAGE_SHIFT)
                    .ok_or(SystemError::EOVERFLOW)
            } else {
                Ok(acc)
            }
        })?;
        let new_locked_vm = self.locked_vm.checked_sub(locked_pages).ok_or_else(|| {
            error!(
                "shmdt locked_vm accounting underflow: locked_vm={}, pages={}",
                self.locked_vm, locked_pages
            );
            debug_assert!(
                false,
                "shmdt locked_vm accounting underflow: locked_vm={}, pages={}",
                self.locked_vm, locked_pages
            );
            SystemError::EFAULT
        })?;

        let mm = self.outer_addr_space().ok_or(SystemError::EFAULT)?;
        let _pt_edit = mm.page_table_edit();
        let mut tlb = MmuGather::gather(&mm);
        let mut notifications = VmaCloseNotifications::default();

        for target in targets {
            let region = *target.lock().region();
            let vma = self
                .mappings
                .remove_vma(&region)
                .ok_or(SystemError::EFAULT)?;
            if let Some(notification) = Self::collect_vma_close(&vma, region) {
                notifications.vma.push(notification);
            }
            if let Some(notification) = Self::collect_sysv_shm_close(&vma) {
                notifications.sysv.push(notification);
            }
            vma.unmap(&mut self.user_mapper.utable, &mut tlb);
        }
        self.locked_vm = new_locked_vm;
        tlb.finish();

        Ok(notifications)
    }

    pub(super) fn collect_sysv_shm_close(vma: &Arc<LockedVMA>) -> Option<Arc<SysVShmAttach>> {
        vma.lock().sysv_shm()
    }

    pub(super) fn notify_sysv_shm_close(notification: Arc<SysVShmAttach>) {
        notification.close_vma();
    }

    pub(crate) fn notify_close_notifications(notifications: VmaCloseNotifications) {
        for notification in notifications.vma {
            Self::notify_vma_close(notification);
        }
        for notification in notifications.sysv {
            Self::notify_sysv_shm_close(notification);
        }
    }

    pub(super) fn collect_vma_close(
        vma: &Arc<LockedVMA>,
        region: VirtRegion,
    ) -> Option<VmaCloseNotification> {
        let (file, vm_flags) = {
            let guard = vma.lock();
            let file = guard.vm_file()?;
            (file, *guard.vm_flags())
        };

        if vm_flags.contains(VmFlags::VM_SHARED | VmFlags::VM_WRITE) {
            Some(VmaCloseNotification {
                file,
                region,
                vm_flags,
            })
        } else {
            None
        }
    }

    pub(super) fn notify_vma_close(notification: VmaCloseNotification) {
        notification.file.with_io_fs(|fs| {
            fs.vma_close(
                &notification.file,
                notification.region,
                notification.vm_flags,
            )
        });
    }

    pub(super) fn mprotect_collect(
        &mut self,
        start_page: VirtPageFrame,
        page_count: PageFrameCount,
        prot_flags: ProtFlags,
    ) -> Result<(), VmaOpFailure> {
        // debug!(
        //     "mprotect: start_page: {:?}, page_count: {:?}, prot_flags:{prot_flags:?}",
        //     start_page,
        //     page_count
        // );
        let mm = self.outer_addr_space().ok_or(SystemError::EFAULT)?;
        let mut tlb = MmuGather::gather(&mm);

        let mapper = &mut self.user_mapper.utable;
        let region = VirtRegion::new(start_page.virt_address(), page_count.bytes());
        // debug!("mprotect: region: {:?}", region);

        let (regions, has_unmapped) = self.mappings.conflicts_with_unmapped(region);
        if has_unmapped {
            return Err(SystemError::ENOMEM.into());
        }
        // debug!("mprotect: regions: {:?}", regions);

        let mut plans: Vec<MprotectVmaPlan> = Vec::with_capacity(regions.len());
        let mut rollback_notifications = VmaCloseNotifications::default();
        for r in &regions {
            // debug!("mprotect: r: {:?}", r);
            let (original_region, new_vm_flags) = {
                let guard = r.lock();
                if !guard.can_have_flags(prot_flags) {
                    for plan in plans {
                        plan.split_lifecycle
                            .rollback_into(&mut rollback_notifications);
                    }
                    return Err(VmaOpFailure {
                        err: SystemError::EACCES,
                        notifications: rollback_notifications,
                    });
                }
                let old_vm_flags = *guard.vm_flags();
                let access_flags = VmFlags::VM_READ | VmFlags::VM_WRITE | VmFlags::VM_EXEC;
                let new_vm_flags = (old_vm_flags & !access_flags) | VmFlags::from(prot_flags);
                if new_vm_flags == old_vm_flags {
                    continue;
                }
                if let Some(file) = guard.vm_file() {
                    if let Err(err) = file.with_io_fs(|fs| fs.mprotect(old_vm_flags, new_vm_flags))
                    {
                        for plan in plans {
                            plan.split_lifecycle
                                .rollback_into(&mut rollback_notifications);
                        }
                        return Err(VmaOpFailure {
                            err,
                            notifications: rollback_notifications,
                        });
                    }
                }
                (*guard.region(), new_vm_flags)
            };
            let intersection = original_region.intersect(&region).unwrap();
            let split_lifecycle = match r.prepare_split_lifecycle(intersection) {
                Ok(lifecycle) => lifecycle,
                Err(failure) => {
                    for plan in plans {
                        plan.split_lifecycle
                            .rollback_into(&mut rollback_notifications);
                    }
                    let err = failure.rollback_into(&mut rollback_notifications);
                    return Err(VmaOpFailure {
                        err,
                        notifications: rollback_notifications,
                    });
                }
            };
            plans.push(MprotectVmaPlan {
                original_region,
                intersection,
                new_vm_flags,
                split_lifecycle,
            });
        }

        for plan in plans {
            let r = match self.mappings.remove_vma(&plan.original_region) {
                Some(vma) => vma,
                None => {
                    plan.split_lifecycle
                        .rollback_into(&mut rollback_notifications);
                    return Err(VmaOpFailure {
                        err: SystemError::EFAULT,
                        notifications: rollback_notifications,
                    });
                }
            };

            let remap_result: Result<VmaSplitSides, SystemError> = {
                let _pt_edit = mm.page_table_edit();
                let split_result = r
                    .extract(plan.intersection, mapper)
                    .expect("Failed to extract VMA");

                let mut r_guard = r.lock();
                r_guard.set_vm_flags(plan.new_vm_flags);

                let new_flags: EntryFlags<MMArch> = MMArch::vm_get_page_prot(plan.new_vm_flags);

                r_guard.remap(new_flags, mapper, &mut tlb);
                Ok((split_result.prev, split_result.after))
            };
            let (before, after) = match remap_result {
                Ok(result) => result,
                Err(err) => {
                    self.mappings.insert_vma(r);
                    plan.split_lifecycle
                        .rollback_into(&mut rollback_notifications);
                    return Err(VmaOpFailure {
                        err,
                        notifications: rollback_notifications,
                    });
                }
            };

            if let Some(before) = before {
                self.mappings.insert_vma(before);
            }
            if let Some(after) = after {
                self.mappings.insert_vma(after);
            }
            self.mappings.insert_vma(r);
            plan.split_lifecycle.commit();
        }

        // Unified shootdown. mprotect does not free physical pages; tlb.finish() mainly flushes the TLB.
        tlb.finish();
        return Ok(());
    }

    pub fn mprotect(
        &mut self,
        start_page: VirtPageFrame,
        page_count: PageFrameCount,
        prot_flags: ProtFlags,
    ) -> Result<(), SystemError> {
        match self.mprotect_collect(start_page, page_count, prot_flags) {
            Ok(()) => Ok(()),
            Err(failure) => {
                Self::notify_close_notifications(failure.notifications);
                Err(failure.err)
            }
        }
    }

    pub fn mincore(
        &self,
        start_page: VirtPageFrame,
        page_count: PageFrameCount,
        vec: &mut [u8],
    ) -> Result<(), SystemError> {
        let mapper = &self.user_mapper.utable;

        if self.mappings.contains(start_page.virt_address()).is_none() {
            return Err(SystemError::ENOMEM);
        }

        let mut last_vaddr = start_page.virt_address();
        let region = VirtRegion::new(start_page.virt_address(), page_count.bytes());
        let mut vmas = self.mappings.conflicts(region);
        // To ensure correct address contiguity checks, iterate in ascending order of start address
        vmas.sort_by_key(|v| v.lock().region().start().data());
        let mut offset = 0;
        for v in vmas {
            let region = *v.lock().region();
            // Ensure adjacent VMAs are contiguous
            if region.start() != last_vaddr && last_vaddr != start_page.virt_address() {
                return Err(SystemError::ENOMEM);
            }
            let start_vaddr = last_vaddr;
            let end_vaddr = core::cmp::min(region.end(), start_vaddr + page_count.bytes());
            v.do_mincore(mapper, vec, start_vaddr, end_vaddr, offset)?;
            let page_count_this_vma = (end_vaddr - start_vaddr) >> MMArch::PAGE_SHIFT;
            offset += page_count_this_vma;
            last_vaddr = end_vaddr;
        }

        // Verify coverage completeness: if the tail end does not cover the requested range, return ENOMEM
        if last_vaddr != region.end() {
            return Err(SystemError::ENOMEM);
        }

        return Ok(());
    }

    pub fn count_unlocked_pages_for_mlock(
        &self,
        start: VirtAddr,
        len: usize,
    ) -> Result<usize, SystemError> {
        let target = Self::checked_user_region(start, len)?;
        let mut vmas = self.mappings.conflicts(target);
        vmas.sort_by_key(|vma| vma.lock().region().start().data());

        let mut cursor = target.start();
        let mut unlocked_pages = 0usize;
        for vma in vmas {
            let guard = vma.lock();
            let region = *guard.region();
            let Some(intersection) = region.intersect(&target) else {
                continue;
            };
            if intersection.start() > cursor {
                return Err(SystemError::ENOMEM);
            }
            if intersection.end() > cursor {
                if !guard.vm_flags().contains(VmFlags::VM_LOCKED) {
                    unlocked_pages = unlocked_pages
                        .checked_add(intersection.size() >> MMArch::PAGE_SHIFT)
                        .ok_or(SystemError::ENOMEM)?;
                }
                cursor = intersection.end();
            }
            if cursor >= target.end() {
                break;
            }
        }

        if cursor != target.end() {
            return Err(SystemError::ENOMEM);
        }
        Ok(unlocked_pages)
    }

    pub fn count_unlocked_pages_for_mlockall(&self) -> Result<usize, SystemError> {
        let mut unlocked_pages = 0usize;
        for vma in self.mappings.iter_vmas() {
            let guard = vma.lock();
            if !guard.vm_flags().contains(VmFlags::VM_LOCKED) {
                unlocked_pages = unlocked_pages
                    .checked_add(guard.region().size() >> MMArch::PAGE_SHIFT)
                    .ok_or(SystemError::ENOMEM)?;
            }
        }

        Ok(unlocked_pages)
    }

    pub(crate) fn apply_mlockall_current_collect(
        &mut self,
        new_flags: VmFlags,
    ) -> VmaCloseNotifications {
        let ranges = self
            .mappings
            .iter_vmas()
            .map(|vma| {
                let guard = vma.lock();
                (guard.region().start(), guard.region().size())
            })
            .collect::<Vec<_>>();

        let mut notifications = VmaCloseNotifications::default();
        for (start, len) in ranges {
            if let Err(failure) = self.apply_vma_lock_flags_collect(start, len, new_flags) {
                notifications.extend(failure.notifications);
            }
        }

        notifications
    }

    pub fn apply_mlockall_current(&mut self, new_flags: VmFlags) -> Result<(), SystemError> {
        let notifications = self.apply_mlockall_current_collect(new_flags);
        Self::notify_close_notifications(notifications);
        Ok(())
    }

    pub fn set_mlock_future(&mut self, flags: VmFlags) {
        self.mlock_future = flags;
    }

    pub(crate) fn clear_all_vma_lock_flags_collect(&mut self) -> VmaCloseNotifications {
        self.mlock_future = VmFlags::VM_NONE;
        let ranges = self
            .mappings
            .iter_vmas()
            .filter_map(|vma| {
                let guard = vma.lock();
                if guard
                    .vm_flags()
                    .intersects(VmFlags::VM_LOCKED | VmFlags::VM_LOCKONFAULT)
                {
                    Some((guard.region().start(), guard.region().size()))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        let mut notifications = VmaCloseNotifications::default();
        for (start, len) in ranges {
            if let Err(failure) = self.apply_vma_lock_flags_collect(start, len, VmFlags::VM_NONE) {
                notifications.extend(failure.notifications);
            }
        }

        notifications
    }

    pub fn clear_all_vma_lock_flags(&mut self) -> Result<(), SystemError> {
        let notifications = self.clear_all_vma_lock_flags_collect();
        Self::notify_close_notifications(notifications);
        Ok(())
    }

    pub(crate) fn apply_vma_lock_flags_collect(
        &mut self,
        start: VirtAddr,
        len: usize,
        new_flags: VmFlags,
    ) -> Result<(), VmaOpFailure> {
        let target = Self::checked_user_region(start, len)?;
        self.count_unlocked_pages_for_mlock(start, len)?;

        let wants_locked = new_flags.contains(VmFlags::VM_LOCKED);
        let mut vmas = self.mappings.conflicts(target);
        vmas.sort_by_key(|vma| vma.lock().region().start().data());

        for cur_vma in vmas {
            let (original_region, intersection, old_flags) = {
                let guard = cur_vma.lock();
                (
                    *guard.region(),
                    guard
                        .region()
                        .intersect(&target)
                        .ok_or(SystemError::EFAULT)?,
                    *guard.vm_flags(),
                )
            };
            if old_flags.is_mlock_flag_unsupported() {
                continue;
            }
            let old_locked = old_flags.contains(VmFlags::VM_LOCKED);
            let committed_flags = (old_flags & VmFlags::VM_LOCKED_CLEAR_MASK) | new_flags;
            if committed_flags == old_flags {
                continue;
            }
            let pages = intersection.size() >> MMArch::PAGE_SHIFT;
            let locked_vm_after = if wants_locked && !old_locked {
                Some(
                    self.locked_vm
                        .checked_add(pages)
                        .ok_or(SystemError::ENOMEM)?,
                )
            } else if !wants_locked && old_locked {
                Some(
                    self.locked_vm
                        .checked_sub(pages)
                        .ok_or(SystemError::ENOMEM)?,
                )
            } else {
                None
            };
            let split_lifecycle = match cur_vma.prepare_split_lifecycle(intersection) {
                Ok(lifecycle) => lifecycle,
                Err(failure) => {
                    let mut notifications = VmaCloseNotifications::default();
                    let err = failure.rollback_into(&mut notifications);
                    return Err(VmaOpFailure { err, notifications });
                }
            };
            let cur_vma = match self.mappings.remove_vma(&original_region) {
                Some(vma) => vma,
                None => {
                    let mut notifications = VmaCloseNotifications::default();
                    split_lifecycle.rollback_into(&mut notifications);
                    return Err(VmaOpFailure {
                        err: SystemError::EFAULT,
                        notifications,
                    });
                }
            };

            let split_result = match cur_vma.extract(intersection, &self.user_mapper.utable) {
                Some(split_result) => split_result,
                None => {
                    self.mappings.insert_vma(cur_vma.clone());
                    let mut notifications = VmaCloseNotifications::default();
                    split_lifecycle.rollback_into(&mut notifications);
                    return Err(VmaOpFailure {
                        err: SystemError::EFAULT,
                        notifications,
                    });
                }
            };
            let (before, after) = (split_result.prev, split_result.after);

            {
                let mut guard = cur_vma.lock();
                guard.set_vm_flags(committed_flags);
            }

            if let Some(locked_vm_after) = locked_vm_after {
                self.locked_vm = locked_vm_after;
            }
            self.update_present_page_mlock_refs(
                &cur_vma,
                intersection.start(),
                intersection.end(),
                old_locked,
                wants_locked,
            );

            if let Some(before) = before {
                self.mappings.insert_vma(before);
            }
            if let Some(after) = after {
                self.mappings.insert_vma(after);
            }
            self.mappings.insert_vma(cur_vma);
            split_lifecycle.commit();
        }

        if !wants_locked {
            self.munlock_vma_pages_range(target.start(), target.end())?;
        }

        Ok(())
    }

    pub fn apply_vma_lock_flags(
        &mut self,
        start: VirtAddr,
        len: usize,
        new_flags: VmFlags,
    ) -> Result<(), SystemError> {
        match self.apply_vma_lock_flags_collect(start, len, new_flags) {
            Ok(()) => Ok(()),
            Err(failure) => {
                Self::notify_close_notifications(failure.notifications);
                Err(failure.err)
            }
        }
    }

    pub(super) fn checked_user_region(
        start: VirtAddr,
        len: usize,
    ) -> Result<VirtRegion, SystemError> {
        if len == 0 {
            return Err(SystemError::EINVAL);
        }
        if !start.check_aligned(MMArch::PAGE_SIZE) {
            return Err(SystemError::EINVAL);
        }
        let end = start.data().checked_add(len).ok_or(SystemError::EINVAL)?;
        if end > MMArch::USER_END_VADDR.data() {
            return Err(SystemError::EINVAL);
        }
        Ok(VirtRegion::new(start, len))
    }

    fn munlock_vma_pages_range(
        &mut self,
        start: VirtAddr,
        end: VirtAddr,
    ) -> Result<(), SystemError> {
        let mapper = &self.user_mapper.utable;
        let mut vaddr = start;
        while vaddr < end {
            if let Some((paddr, _)) = mapper.translate(vaddr) {
                let page = {
                    let mut page_manager_guard = page_manager_lock();
                    page_manager_guard.get(&paddr)
                };
                if let Some(page) = page {
                    Self::remove_page_unevictable_if_unneeded(&page);
                }
            }
            vaddr = VirtAddr::new(vaddr.data() + MMArch::PAGE_SIZE);
        }
        Ok(())
    }

    pub(crate) fn remove_page_unevictable_if_unneeded(page: &Arc<Page>) {
        let mut page_guard = page.write();
        if !page_guard.flags().contains(PageFlags::PG_UNEVICTABLE)
            || page_guard.has_unevictable_source()
        {
            return;
        }

        page_guard.remove_flags(PageFlags::PG_UNEVICTABLE);
        let paddr = page.phys_address();
        let should_reclaim = page_guard.flags().contains(PageFlags::PG_LRU);
        drop(page_guard);
        if should_reclaim {
            page_reclaimer_lock().insert_page(paddr, page);
        }
    }

    fn madvise_uses_range_without_vma_split(behavior: MadvFlags) -> bool {
        behavior == MadvFlags::MADV_DONTNEED
            || behavior == MadvFlags::MADV_DONTNEED_LOCKED
            || behavior == MadvFlags::MADV_WILLNEED
            || behavior == MadvFlags::MADV_COLD
            || behavior == MadvFlags::MADV_PAGEOUT
            || behavior == MadvFlags::MADV_FREE
            || behavior == MadvFlags::MADV_POPULATE_READ
            || behavior == MadvFlags::MADV_POPULATE_WRITE
    }

    pub(super) fn madvise_collect(
        &mut self,
        start_page: VirtPageFrame,
        page_count: PageFrameCount,
        behavior: MadvFlags,
    ) -> Result<(), VmaOpFailure> {
        let mm = self.outer_addr_space().ok_or(SystemError::EFAULT)?;
        let mut tlb = MmuGather::gather(&mm);

        let mapper = &mut self.user_mapper.utable;

        let region = VirtRegion::new(start_page.virt_address(), page_count.bytes());
        let (regions, has_unmapped) = self.mappings.conflicts_with_unmapped(region);

        if behavior == MadvFlags::MADV_DOFORK {
            for vma in &regions {
                if vma.lock().vm_flags().contains(VmFlags::VM_IO) {
                    return Err(SystemError::EINVAL.into());
                }
            }
        }
        if behavior == MadvFlags::MADV_REMOVE {
            return if regions.is_empty() {
                Err(SystemError::ENOMEM.into())
            } else {
                Err(SystemError::EINVAL.into())
            };
        }

        if Self::madvise_uses_range_without_vma_split(behavior) {
            for r in regions {
                let (original_region, vm_flags) = {
                    let guard = r.lock();
                    (*guard.region(), *guard.vm_flags())
                };
                let intersection = original_region.intersect(&region).unwrap();

                let _pt_edit = mm.page_table_edit();
                match behavior {
                    MadvFlags::MADV_DONTNEED | MadvFlags::MADV_DONTNEED_LOCKED => {
                        if vm_flags.contains(VmFlags::VM_PFNMAP)
                            || (behavior == MadvFlags::MADV_DONTNEED
                                && vm_flags.contains(VmFlags::VM_LOCKED))
                        {
                            tlb.finish();
                            return Err(SystemError::EINVAL.into());
                        }
                        r.unmap_range(intersection, mapper, &mut tlb, UnmapMappingMode::EvenCow);
                    }
                    _ => r.do_madvise(behavior, mapper, &mut tlb),
                }
            }
            tlb.finish();
            return if has_unmapped {
                Err(SystemError::ENOMEM.into())
            } else {
                Ok(())
            };
        }

        let mut plans: Vec<MadviseVmaPlan> = Vec::with_capacity(regions.len());
        let mut rollback_notifications = VmaCloseNotifications::default();
        for r in &regions {
            let (original_region, old_flags) = {
                let guard = r.lock();
                (*guard.region(), *guard.vm_flags())
            };
            let new_flags = match r.madvise_updated_flags(behavior) {
                Ok(Some(new_flags)) => new_flags,
                Ok(None) => continue,
                Err(err) => {
                    for plan in plans {
                        plan.split_lifecycle
                            .rollback_into(&mut rollback_notifications);
                    }
                    return Err(VmaOpFailure {
                        err,
                        notifications: rollback_notifications,
                    });
                }
            };
            if new_flags == old_flags {
                continue;
            };
            let intersection = original_region.intersect(&region).unwrap();
            let split_lifecycle = match r.prepare_split_lifecycle(intersection) {
                Ok(lifecycle) => lifecycle,
                Err(failure) => {
                    for plan in plans {
                        plan.split_lifecycle
                            .rollback_into(&mut rollback_notifications);
                    }
                    let err = failure.rollback_into(&mut rollback_notifications);
                    return Err(VmaOpFailure {
                        err,
                        notifications: rollback_notifications,
                    });
                }
            };
            plans.push(MadviseVmaPlan {
                original_region,
                intersection,
                split_lifecycle,
            });
        }

        for plan in plans {
            let r = match self.mappings.remove_vma(&plan.original_region) {
                Some(vma) => vma,
                None => {
                    plan.split_lifecycle
                        .rollback_into(&mut rollback_notifications);
                    return Err(VmaOpFailure {
                        err: SystemError::EFAULT,
                        notifications: rollback_notifications,
                    });
                }
            };

            let madvise_result: Result<VmaSplitSides, SystemError> = {
                let _pt_edit = mm.page_table_edit();
                let split_result = r
                    .extract(plan.intersection, mapper)
                    .expect("Failed to extract VMA");
                r.do_madvise(behavior, mapper, &mut tlb);
                Ok((split_result.prev, split_result.after))
            };
            let (before, after) = match madvise_result {
                Ok(result) => result,
                Err(err) => {
                    self.mappings.insert_vma(r);
                    plan.split_lifecycle
                        .rollback_into(&mut rollback_notifications);
                    return Err(VmaOpFailure {
                        err,
                        notifications: rollback_notifications,
                    });
                }
            };
            if let Some(before) = before {
                self.mappings.insert_vma(before);
            }
            if let Some(after) = after {
                self.mappings.insert_vma(after);
            }
            self.mappings.insert_vma(r);
            plan.split_lifecycle.commit();
        }
        tlb.finish();
        if has_unmapped {
            Err(SystemError::ENOMEM.into())
        } else {
            Ok(())
        }
    }

    pub fn madvise(
        &mut self,
        start_page: VirtPageFrame,
        page_count: PageFrameCount,
        behavior: MadvFlags,
    ) -> Result<(), SystemError> {
        match self.madvise_collect(start_page, page_count, behavior) {
            Ok(()) => Ok(()),
            Err(failure) => {
                Self::notify_close_notifications(failure.notifications);
                Err(failure.err)
            }
        }
    }

    /// Clear the page table entries for file mappings associated with the given inode, preserving the VMA so that future accesses trigger a page fault and handle it according to the latest file size
    pub fn zap_file_mappings(&mut self, inode_id: InodeId) -> Result<(), SystemError> {
        let mut targets: Vec<Arc<LockedVMA>> = Vec::new();
        for vma in self.mappings.iter_vmas() {
            let guard = vma.lock();
            if let Some(file) = guard.vm_file() {
                if file.inode().metadata()?.inode_id == inode_id {
                    targets.push(vma.clone());
                }
            }
        }

        let mm = self.outer_addr_space().ok_or(SystemError::EFAULT)?;
        let _pt_edit = mm.page_table_edit();
        let mut tlb = MmuGather::gather(&mm);
        for vma in targets {
            vma.unmap(&mut self.user_mapper.utable, &mut tlb);
        }
        tlb.finish();
        Ok(())
    }

    /// Create a new user stack
    ///
    /// ## Parameters
    ///
    /// - `size`: the stack size
    pub fn new_user_stack(&mut self, size: usize) -> Result<(), SystemError> {
        assert!(self.user_stack.is_none(), "User stack already exists");
        let stack = UserStack::new(self, None, size)?;
        self.user_stack = Some(stack);
        return Ok(());
    }

    #[inline(always)]
    pub fn user_stack_mut(&mut self) -> Option<&mut UserStack> {
        return self.user_stack.as_mut();
    }

    /// Unmap all mappings in the user address space
    pub unsafe fn unmap_all(&mut self) {
        // Two calling scenarios:
        // 1) Explicit call (still has external `Arc<AddressSpace>` references): `outer.upgrade()` returns `Some`,
        //    taking the normal mm-aware shootdown + release path.
        // 2) `Drop for InnerAddressSpace`: at this point `Arc<AddressSpace>` is inside `drop_slow`,
        //    strong-count is already 0, so `Weak::upgrade()` must return `None`. On this path we have already
        //    cleaned up `active_cpus` inside exit/switch_process; no CPU still holds this mm's TLB,
        //    so we use `MmuGather::gather_teardown()`, skipping cross-core shootdown, only tearing down PTEs
        //    and releasing physical pages in INV-3 order ("flush first, release later").
        let mm_arc = self.outer_addr_space();
        let _pt_edit = mm_arc.as_ref().map(|mm| mm.page_table_edit());
        let mut tlb = match mm_arc.as_ref() {
            Some(mm) => MmuGather::gather(mm),
            None => MmuGather::gather_teardown(),
        };
        // Full-mm flush (fullmm); no need to accumulate ranges.
        tlb.set_fullmm();
        let mut vma_close_notifications = Vec::new();
        let mut sysv_close_notifications = Vec::new();
        let unmapped_vmas = self.mappings.take_all_vmas();
        for vma in &unmapped_vmas {
            let region = *vma.lock().region();
            if let Some(notification) = Self::collect_vma_close(vma, region) {
                vma_close_notifications.push(notification);
            }
            let sysv_close = Self::collect_sysv_shm_close(vma);
            if let Some(notification) = sysv_close {
                sysv_close_notifications.push(notification);
            }
            if vma.mapped() {
                vma.unmap(&mut self.user_mapper.utable, &mut tlb);
            }
        }
        tlb.finish();
        drop(unmapped_vmas);
        for notification in vma_close_notifications {
            Self::notify_vma_close(notification);
        }
        for notification in sysv_close_notifications {
            Self::notify_sysv_shm_close(notification);
        }
    }

    /// Set the process's heap memory space
    ///
    /// ## Parameters
    ///
    /// - `new_brk`: the new end address of the heap. Must be page-aligned, a user space address, and greater than or equal to the current heap start address.
    ///
    /// ## Return Value
    ///
    /// Returns the old heap end address
    pub unsafe fn set_brk(&mut self, new_brk: VirtAddr) -> Result<VirtAddr, SystemError> {
        assert!(new_brk.check_aligned(MMArch::PAGE_SIZE));

        if !new_brk.check_user() || new_brk < self.brk_start {
            return Err(SystemError::EFAULT);
        }

        // Soft limit: RLIMIT_DATA
        let rlim = ProcessManager::current_pcb()
            .get_rlimit(RLimitID::Data)
            .rlim_cur as usize;
        if rlim != usize::MAX {
            let desired = new_brk.data().saturating_sub(self.brk_start.data());
            if desired > rlim {
                return Err(SystemError::ENOMEM);
            }
        }

        let old_brk = self.brk;

        if new_brk > self.brk {
            let len = new_brk - self.brk;
            let brk_region = VirtRegion::new(self.brk, len);
            if self.mappings.has_conflict(brk_region) {
                return Err(SystemError::ENOMEM);
            }
            let prot_flags = ProtFlags::PROT_READ | ProtFlags::PROT_WRITE;
            let map_flags =
                MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS | MapFlags::MAP_FIXED_NOREPLACE;
            self.map_anonymous(old_brk, len, prot_flags, map_flags, true, false)?;

            self.brk = new_brk;
            return Ok(old_brk);
        } else {
            let unmap_len = self.brk - new_brk;
            let unmap_start = new_brk;
            if unmap_len == 0 {
                return Ok(old_brk);
            }
            self.munmap(
                VirtPageFrame::new(unmap_start),
                PageFrameCount::from_bytes(unmap_len).unwrap(),
            )?;
            self.brk = new_brk;
            return Ok(old_brk);
        }
    }

    pub unsafe fn sbrk(&mut self, incr: isize) -> Result<VirtAddr, SystemError> {
        if incr == 0 {
            return Ok(self.brk);
        }

        let new_brk = if incr > 0 {
            self.brk + incr as usize
        } else {
            self.brk - incr.unsigned_abs()
        };

        let new_brk = VirtAddr::new(page_align_up(new_brk.data()));

        let rlim = ProcessManager::current_pcb()
            .get_rlimit(RLimitID::Data)
            .rlim_cur as usize;
        if rlim != usize::MAX {
            let desired = new_brk.data().saturating_sub(self.brk_start.data());
            if desired > rlim {
                return Err(SystemError::ENOMEM);
            }
        }

        return self.set_brk(new_brk);
    }

    pub(super) fn find_free_at_prepare(
        &mut self,
        min_vaddr: VirtAddr,
        vaddr: VirtAddr,
        size: usize,
        flags: MapFlags,
    ) -> Result<VirtRegion, SystemError> {
        self.find_free_at_internal(min_vaddr, vaddr, size, flags, false)
            .map(|(region, _)| region)
            .map_err(|failure| {
                debug_assert!(
                    failure.notifications.is_empty(),
                    "non-collecting find_free_at must not unmap existing VMAs"
                );
                failure.err
            })
    }

    pub(super) fn find_free_at_collect(
        &mut self,
        min_vaddr: VirtAddr,
        vaddr: VirtAddr,
        size: usize,
        flags: MapFlags,
    ) -> Result<(VirtRegion, VmaCloseNotifications), VmaOpFailure> {
        self.find_free_at_internal(min_vaddr, vaddr, size, flags, true)
    }

    pub(super) fn find_free_at_internal(
        &mut self,
        min_vaddr: VirtAddr,
        vaddr: VirtAddr,
        size: usize,
        flags: MapFlags,
        unmap_fixed: bool,
    ) -> Result<(VirtRegion, VmaCloseNotifications), VmaOpFailure> {
        // If no address was specified, search for a free virtual memory range in the current process's address space.
        if vaddr == VirtAddr::new(0)
            && !flags.intersects(MapFlags::MAP_FIXED | MapFlags::MAP_FIXED_NOREPLACE)
        {
            let region = self
                .mappings
                .find_free(min_vaddr, size)
                .ok_or(SystemError::ENOMEM)?;
            return Ok((region, VmaCloseNotifications::default()));
        }

        let end = vaddr.data().checked_add(size).ok_or(SystemError::EINVAL)?;
        if size == 0
            || end > MMArch::USER_END_VADDR.data()
            || !vaddr.check_aligned(MMArch::PAGE_SIZE)
        {
            return Err(SystemError::EINVAL.into());
        }

        if vaddr < min_vaddr {
            if flags.intersects(MapFlags::MAP_FIXED | MapFlags::MAP_FIXED_NOREPLACE) {
                check_mmap_min_addr(vaddr, min_vaddr)?;
            } else {
                let region = self
                    .mappings
                    .find_free(min_vaddr, size)
                    .ok_or(SystemError::ENOMEM)?;
                return Ok((region, VmaCloseNotifications::default()));
            }
        }

        // If an address was specified, check whether the specified address is available.
        let requested = VirtRegion::new(vaddr, size);

        if self
            .mappings
            .first_reservation_conflict(requested)
            .is_some()
        {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK.into());
        }

        let has_conflict = self.mappings.has_conflict(requested);
        if has_conflict {
            if flags.contains(MapFlags::MAP_FIXED_NOREPLACE) {
                // If MAP_FIXED_NOREPLACE was specified and the target address cannot be mapped successfully, abort the mapping without adjusting the address
                return Err(SystemError::EEXIST.into());
            }

            if flags.contains(MapFlags::MAP_FIXED) {
                if !unmap_fixed {
                    return Ok((requested, VmaCloseNotifications::default()));
                }
                // Linux mmap_region() unmaps the whole requested range for MAP_FIXED,
                // because the new mapping may overlap more than the first conflicting VMA.
                let notifications = self.munmap_collect(
                    VirtPageFrame::new(requested.start()),
                    PageFrameCount::from_bytes(requested.size()).unwrap(),
                )?;
                return Ok((requested, notifications));
            }

            // If MAP_FIXED was not specified, adjust the address
            let requested = self
                .mappings
                .find_free(min_vaddr, size)
                .ok_or(SystemError::ENOMEM)?;
            return Ok((requested, VmaCloseNotifications::default()));
        }

        return Ok((requested, VmaCloseNotifications::default()));
    }
}

impl Drop for InnerAddressSpace {
    fn drop(&mut self) {
        unsafe {
            self.unmap_all();
        }
        crate::mm::oom::notify_mm_drop(self.mm_id);
    }
}
