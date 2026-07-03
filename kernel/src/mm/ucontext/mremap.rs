use super::*;

impl InnerAddressSpace {
    /// Remap a memory region
    ///
    /// # Parameters
    ///
    /// - `old_vaddr`: starting address of the original mapping
    /// - `old_len`: length of the original mapping
    /// - `new_len`: length of the remapped region
    /// - `mremap_flags`: remap flags
    /// - `new_vaddr`: starting address of the remapped region
    /// - `vm_flags`: old memory region flags
    ///
    /// # Returns
    ///
    /// Returns the starting virtual page frame address of the remapped region
    ///
    /// # Errors
    ///
    /// - `EINVAL`: invalid argument
    pub(super) fn mremap(
        &mut self,
        old_vaddr: VirtAddr,
        mut old_len: usize,
        new_len: usize,
        mremap_flags: MremapFlags,
        new_vaddr: VirtAddr,
        vm_flags: VmFlags,
    ) -> Result<MremapOutcome, MremapFailure> {
        let fixed_new_region = if mremap_flags.contains(MremapFlags::MREMAP_FIXED) {
            if !new_vaddr.check_aligned(MMArch::PAGE_SIZE) {
                return Err(SystemError::EINVAL.into());
            }
            let new_region = Self::checked_user_region(new_vaddr, new_len)?;
            let old_end = old_vaddr.data().wrapping_add(old_len);
            let new_end = new_vaddr
                .data()
                .checked_add(new_len)
                .ok_or(SystemError::EINVAL)?;
            if old_end > new_vaddr.data() && new_end > old_vaddr.data() {
                return Err(SystemError::EINVAL.into());
            }
            if old_len != 0 {
                let old_region = Self::checked_user_region(old_vaddr, old_len)?;
                debug_assert!(!old_region.collide(&new_region));
            }
            Some(new_region)
        } else {
            None
        };
        // Initialise memory region protection flags
        let prot_flags: ProtFlags = vm_flags.into();
        let mut notifications = VmaCloseNotifications::default();
        macro_rules! mremap_fail {
            ($err:expr) => {
                return Err(MremapFailure {
                    err: $err,
                    notifications,
                })
            };
        }
        macro_rules! mremap_try {
            ($expr:expr) => {
                match $expr {
                    Ok(value) => value,
                    Err(err) => mremap_fail!(err),
                }
            };
        }

        if mremap_flags.contains(MremapFlags::MREMAP_FIXED)
            && self.mappings.contains(old_vaddr).is_none()
        {
            mremap_fail!(SystemError::EFAULT);
        }

        if mremap_flags.contains(MremapFlags::MREMAP_FIXED) {
            let start_page = VirtPageFrame::new(new_vaddr);
            let page_count = PageFrameCount::from_bytes(new_len).unwrap();
            match self.munmap_collect(start_page, page_count) {
                Ok(close_notifications) => notifications.extend(close_notifications),
                Err(failure) => {
                    notifications.extend(failure.notifications);
                    mremap_fail!(failure.err);
                }
            }
        }
        if mremap_flags.contains(MremapFlags::MREMAP_FIXED) && old_len > new_len {
            match self.munmap_collect(
                VirtPageFrame::new(old_vaddr + new_len),
                PageFrameCount::from_bytes(old_len - new_len).unwrap(),
            ) {
                Ok(close_notifications) => notifications.extend(close_notifications),
                Err(failure) => {
                    notifications.extend(failure.notifications);
                    mremap_fail!(failure.err);
                }
            }
            old_len = new_len;
        }
        // Read backing info of the old VMA (file/shared-anon) and the page offset base.
        // MREMAP_FIXED may have already split the target interval and shrink tail above; re-query
        // the source VMA to avoid using an old cache that may be invalidated after split.
        let Some(old_vma) = self.mappings.contains(old_vaddr) else {
            mremap_fail!(SystemError::EFAULT);
        };
        let (old_region, vm_file, shared_anon, base_pgoff, sysv_shm) = {
            let g = old_vma.lock();
            let region = *g.region();
            let vma_start = region.start();
            let off_pages =
                (old_vaddr.data().saturating_sub(vma_start.data())) >> MMArch::PAGE_SHIFT;
            let base = g
                .backing_page_offset()
                .unwrap_or(0)
                .saturating_add(off_pages);
            (
                region,
                g.vm_file(),
                g.shared_anon.clone(),
                base,
                g.sysv_shm(),
            )
        };

        // Construct target mapping flags: mremap must preserve shared/private semantics and distinguish anon/file.
        let mut map_flags: MapFlags = vm_flags.into();
        if map_flags.contains(MapFlags::MAP_SHARED) {
            // ok
        } else {
            map_flags |= MapFlags::MAP_PRIVATE;
        }
        if vm_file.is_none() {
            map_flags |= MapFlags::MAP_ANONYMOUS;
        }
        if mremap_flags.contains(MremapFlags::MREMAP_FIXED) {
            map_flags |= MapFlags::MAP_FIXED;
        }

        let dontunmap_flag = mremap_flags.contains(MremapFlags::MREMAP_DONTUNMAP);
        let locked_source = vm_flags.contains(VmFlags::VM_LOCKED);
        let sysv_mremap = sysv_shm.is_some();
        let source_len = old_len;
        let Some(max_old_len) = old_region.end().data().checked_sub(old_vaddr.data()) else {
            mremap_fail!(SystemError::EINVAL);
        };
        if source_len > max_old_len {
            mremap_fail!(SystemError::EFAULT);
        }
        let source_region = VirtRegion::new(old_vaddr, source_len);
        if dontunmap_flag {
            if vm_flags.intersects(VmFlags::VM_DONTEXPAND | VmFlags::VM_PFNMAP) {
                mremap_fail!(SystemError::EINVAL);
            }
            let Some(old_end) = old_vaddr.data().checked_add(old_len) else {
                mremap_fail!(SystemError::EINVAL);
            };
            let Some(new_end) = new_vaddr.data().checked_add(new_len) else {
                mremap_fail!(SystemError::EINVAL);
            };
            if old_end > new_vaddr.data() && new_end > old_vaddr.data() {
                mremap_fail!(SystemError::EINVAL);
            }
        }
        if locked_source {
            let additional_locked_pages = if old_len == 0 {
                new_len >> MMArch::PAGE_SHIFT
            } else if dontunmap_flag {
                0
            } else if new_len > old_len {
                (new_len - old_len) >> MMArch::PAGE_SHIFT
            } else {
                0
            };
            if additional_locked_pages != 0 {
                mremap_try!(self.check_mlock_rlimit_for_pages(
                    additional_locked_pages,
                    SystemError::EAGAIN_OR_EWOULDBLOCK,
                ));
            }
        }
        let as_delta = if old_len == 0 || dontunmap_flag {
            new_len
        } else {
            new_len.saturating_sub(old_len)
        };
        if as_delta != 0 {
            mremap_try!(self.check_rlimit_as_for_bytes(as_delta));
        }

        // Whether moving is allowed (Linux: only MAYMOVE / FIXED can move)
        let can_move = mremap_flags.contains(MremapFlags::MREMAP_MAYMOVE)
            || mremap_flags.contains(MremapFlags::MREMAP_FIXED);

        // Linux: old_len==0 means “copy/duplicate-map” a shared region (DOS-emu legacy).
        // - Only allowed for shared mappings
        // - Return ENOMEM without MAYMOVE/FIXED
        if old_len == 0 {
            if !vm_flags.intersects(VmFlags::VM_SHARED | VmFlags::VM_MAYSHARE) {
                mremap_fail!(SystemError::EINVAL);
            }
            if !can_move {
                mremap_fail!(SystemError::ENOMEM);
            }
        }

        if mremap_flags.contains(MremapFlags::MREMAP_FIXED) {
            if let Err(err) = check_mmap_min_addr(new_vaddr, self.mmap_min) {
                mremap_fail!(err);
            }
        }

        // When moving is not allowed, only try in-place expansion.
        if !can_move {
            if new_len <= old_len {
                return Ok(MremapOutcome {
                    addr: old_vaddr,
                    notifications: VmaCloseNotifications::default(),
                });
            }

            // Linux only allows in-place expansion when the old range reaches the VMA end.
            if old_len != max_old_len {
                mremap_fail!(SystemError::ENOMEM);
            }

            let grow = new_len - old_len;
            let Some(grown_region_size) = old_region.size().checked_add(grow) else {
                mremap_fail!(SystemError::ENOMEM);
            };
            let Some(grown_end) = old_region.start().data().checked_add(grown_region_size) else {
                mremap_fail!(SystemError::ENOMEM);
            };
            if grown_end > MMArch::USER_END_VADDR.data() {
                mremap_fail!(SystemError::ENOMEM);
            }
            let locked_vm_after_grow = if locked_source {
                let Some(locked_vm_after_grow) =
                    self.locked_vm.checked_add(grow >> MMArch::PAGE_SHIFT)
                else {
                    mremap_fail!(SystemError::ENOMEM);
                };
                Some(locked_vm_after_grow)
            } else {
                None
            };
            let grow_region = VirtRegion::new(old_vaddr + old_len, grow);
            if self.mappings.has_conflict(grow_region) {
                mremap_fail!(SystemError::ENOMEM);
            }

            let Some(removed) = self.mappings.remove_vma(&old_region) else {
                mremap_fail!(SystemError::EINVAL);
            };
            removed.lock().set_region_size(grown_region_size);
            self.mappings.insert_vma(removed);
            if let Some(locked_vm_after_grow) = locked_vm_after_grow {
                self.locked_vm = locked_vm_after_grow;
                self.best_effort_locked_population(old_vaddr + old_len, grow, vm_flags);
            }
            return Ok(MremapOutcome {
                addr: old_vaddr,
                notifications: VmaCloseNotifications::default(),
            });
        }

        // Need to create a new mapping and migrate (FIXED or MAYMOVE).
        // Note: must avoid touching user addresses while holding the address space write lock
        // (would trigger page fault recursive deadlock).
        // Linux mremap is implemented by moving/copying page table entries, not byte copying.

        let new_region: VirtRegion = if let Some(new_region) = fixed_new_region {
            new_region
        } else if dontunmap_flag {
            let (region, close_notifications) =
                match self.find_free_at_collect(self.mmap_min, new_vaddr, new_len, map_flags) {
                    Ok(outcome) => outcome,
                    Err(failure) => {
                        notifications.extend(failure.notifications);
                        mremap_fail!(failure.err);
                    }
                };
            notifications.extend(close_notifications);
            region
        } else {
            let Some(new_region) = self.mappings.find_free(self.mmap_min, new_len) else {
                mremap_fail!(SystemError::ENOMEM);
            };
            new_region
        };

        let entry_flags = EntryFlags::from_prot_flags(prot_flags, true);
        let Some(mm) = self.outer_addr_space() else {
            mremap_fail!(SystemError::EFAULT);
        };
        let remove_source_vma_on_commit =
            !dontunmap_flag && old_len != 0 && new_region.start() != old_vaddr;
        let split_source_on_commit = old_len != 0 && source_region != old_region && !dontunmap_flag;

        let locked_vm_after_move_commit = if locked_source {
            let new_pages = new_len >> MMArch::PAGE_SHIFT;
            let old_pages = source_len >> MMArch::PAGE_SHIFT;
            if old_len == 0 {
                let Some(locked_vm_after_commit) = self.locked_vm.checked_add(new_pages) else {
                    mremap_fail!(SystemError::ENOMEM);
                };
                Some(locked_vm_after_commit)
            } else if dontunmap_flag {
                // Linux move_vma() clears VM_LOCKED on the old VMA for
                // MREMAP_DONTUNMAP but deliberately leaves mm->locked_vm
                // unchanged because the source range is not unmapped.
                Some(self.locked_vm)
            } else {
                let Some(locked_after_add) = self.locked_vm.checked_add(new_pages) else {
                    mremap_fail!(SystemError::ENOMEM);
                };
                let Some(locked_vm_after_commit) = locked_after_add.checked_sub(old_pages) else {
                    mremap_fail!(SystemError::ENOMEM);
                };
                Some(locked_vm_after_commit)
            }
        } else {
            None
        };
        let mut source_split_lifecycle = if split_source_on_commit {
            match old_vma.prepare_split_lifecycle(source_region) {
                Ok(lifecycle) => Some(lifecycle),
                Err(failure) => {
                    let err = failure.rollback_into(&mut notifications);
                    mremap_fail!(err);
                }
            }
        } else {
            None
        };
        if let Some(sysv_shm) = sysv_shm.as_ref() {
            if let Err(err) = sysv_shm.open_vma() {
                if let Some(lifecycle) = source_split_lifecycle.take() {
                    lifecycle.rollback_into(&mut notifications);
                }
                mremap_fail!(err);
            }
        }

        // Create the target VMA (initially without mapping physical pages; existing PTEs will be
        // moved/copied below).
        let new_vma: Arc<LockedVMA> = {
            let vma = LockedVMA::new(VMA::new(
                new_region,
                vm_flags,
                entry_flags,
                vm_file.clone(),
                if vm_file.is_some() || shared_anon.is_some() {
                    Some(base_pgoff)
                } else {
                    None
                },
                false,
            ));
            if let Some(shared) = shared_anon.clone() {
                let mut vg = vma.lock();
                vg.shared_anon = Some(shared);
                vg.backing_pgoff = Some(base_pgoff);
            }
            if let Some(sysv_shm) = sysv_shm.clone() {
                vma.lock().set_sysv_shm(Some(sysv_shm));
            }
            vma
        };

        // Linux mremap moves/duplicates an existing VMA; it does not call the
        // filesystem mmap hook again. The file mapping was already accepted
        // when the source VMA was created.
        self.mappings.insert_vma(new_vma.clone());
        let move_len = core::cmp::min(source_len, new_len);

        // mremap does not free physical pages; old PTEs are migrated to the new VMA, while
        // old_len==0 keeps the legacy duplicate-mapping behavior.
        // using MmuGather here is solely for a unified cross-core TLB shootdown at the end.
        let mut tlb = MmuGather::gather(&mm);

        // Migrate/copy existing page table mappings.
        // Phase A: install target PTEs completely first, without destroying source PTEs;
        // on failure only delete target PTEs.
        // Phase B: after all target PTEs are installed successfully, remove source PTEs
        // infallibly and switch vma_set.
        // Linux MREMAP_DONTUNMAP preserves the old VMA, but page tables are still migrated;
        // source PTEs must not be kept long-term.
        let mapper = &mut self.user_mapper.utable;
        let old_vma = old_vma.clone();
        let mut installed_target_pte = false;
        let mut installed_target_present_pages = 0usize;
        let mut removed_source_present_pages = 0usize;

        {
            let _pt_edit = mm.page_table_edit();
            let mut page_manager_guard = page_manager_lock();
            let mut migrated = Vec::new();
            let mut err = None;
            let mut off = 0usize;
            while off < move_len {
                let src = old_vaddr + off;
                let dst = new_region.start() + off;
                if let Some((paddr, src_flags)) = mapper.translate(src) {
                    let Some(flush) = (unsafe { mapper.map_phys(dst, paddr, src_flags) }) else {
                        err = Some(SystemError::ENOMEM);
                        break;
                    };
                    unsafe { flush.ignore() };
                    tlb.accumulate_range(dst);
                    installed_target_pte = true;
                    installed_target_present_pages += 1;
                    page_manager_guard
                        .get_unwrap(&paddr)
                        .write()
                        .insert_vma(new_vma.clone(), locked_source);

                    migrated.push((src, dst, paddr, src_flags));
                }
                off += MMArch::PAGE_SIZE;
            }

            if let Some(err) = err {
                for (_src, dst, paddr, _src_flags) in migrated.into_iter().rev() {
                    if let Some((_unmapped_paddr, _flags, flush)) =
                        unsafe { mapper.unmap_phys_preserve_tables(dst) }
                    {
                        unsafe { flush.ignore() };
                        tlb.accumulate_range(dst);
                    }
                    if let Some(page) = page_manager_guard.get(&paddr) {
                        page.write().remove_vma(new_vma.as_ref());
                    }
                }

                self.mappings.remove_vma(&new_region);
                drop(page_manager_guard);
                tlb.finish();
                if let Some(sysv_shm) = sysv_shm.as_ref() {
                    notifications.sysv.push(sysv_shm.clone());
                }
                if let Some(lifecycle) = source_split_lifecycle.take() {
                    lifecycle.rollback_into(&mut notifications);
                }
                mremap_fail!(err);
            }

            if old_len != 0 {
                for (src, _dst, paddr, _src_flags) in migrated {
                    if let Some((_paddr2, _flags2, flush)) =
                        unsafe { mapper.unmap_phys_preserve_tables(src) }
                    {
                        unsafe { flush.ignore() };
                        tlb.accumulate_range(src);
                        removed_source_present_pages += 1;
                    } else {
                        panic!("mremap commit lost expected source PTE at {:?}", src);
                    }

                    let page = page_manager_guard.get_unwrap(&paddr);
                    let mut pg = page.write();
                    pg.remove_vma(old_vma.as_ref());
                }
            }
        }
        if installed_target_pte {
            new_vma.lock().set_mapped(true);
        }
        if installed_target_present_pages >= removed_source_present_pages {
            mm.account_present_pages_add(
                installed_target_present_pages - removed_source_present_pages,
            );
        } else {
            mm.account_present_pages_sub(
                removed_source_present_pages - installed_target_present_pages,
            );
        }

        if sysv_mremap || remove_source_vma_on_commit || (locked_source && dontunmap_flag) {
            let mut source_vma = old_vma.clone();
            let mut split_before = None;
            let mut split_after = None;

            if split_source_on_commit {
                let removed = self
                    .mappings
                    .remove_vma(&old_region)
                    .expect("validated mremap source VMA must exist");
                debug_assert!(Arc::ptr_eq(&removed, &old_vma));
                let split_result = removed
                    .extract(source_region, &self.user_mapper.utable)
                    .expect("validated mremap source region must split");
                source_vma = split_result.middle;
                split_before = split_result.prev;
                split_after = split_result.after;
            }

            if locked_source && dontunmap_flag {
                self.update_present_page_mlock_refs(
                    &source_vma,
                    old_region.start(),
                    old_region.end(),
                    true,
                    false,
                );
                let clear_locked = |vma: &Arc<LockedVMA>| {
                    let mut guard = vma.lock();
                    let unlocked_flags = *guard.vm_flags() & VmFlags::VM_LOCKED_CLEAR_MASK;
                    guard.set_vm_flags(unlocked_flags);
                    guard.set_flags();
                };
                clear_locked(&source_vma);
            }

            if let Some(before) = split_before {
                self.mappings.insert_vma(before);
            }
            if let Some(after) = split_after {
                self.mappings.insert_vma(after);
            }
            if let Some(lifecycle) = source_split_lifecycle.take() {
                lifecycle.commit();
            }
            if remove_source_vma_on_commit {
                if split_source_on_commit {
                    source_vma.unmap(&mut self.user_mapper.utable, &mut tlb);
                    source_vma.lock().set_mapped(false);
                    if let Some(notification) = Self::collect_vma_close(&source_vma, source_region)
                    {
                        notifications.vma.push(notification);
                    }
                    if let Some(notification) = Self::collect_sysv_shm_close(&source_vma) {
                        notifications.sysv.push(notification);
                    }
                } else {
                    let removed = self
                        .mappings
                        .remove_vma(&old_region)
                        .expect("validated mremap source VMA must exist");
                    removed.unmap(&mut self.user_mapper.utable, &mut tlb);
                    removed.lock().set_mapped(false);
                    if let Some(notification) = Self::collect_vma_close(&removed, old_region) {
                        notifications.vma.push(notification);
                    }
                    if let Some(notification) = Self::collect_sysv_shm_close(&removed) {
                        notifications.sysv.push(notification);
                    }
                }
            }
            if split_source_on_commit && !remove_source_vma_on_commit {
                self.mappings.insert_vma(source_vma);
            }

            if let Some(locked_vm_after_commit) = locked_vm_after_move_commit {
                self.locked_vm = locked_vm_after_commit;
            }
            tlb.finish();

            if locked_source && new_len > old_len {
                self.best_effort_locked_population(
                    new_region.start() + old_len,
                    new_len - old_len,
                    vm_flags,
                );
            }

            return Ok(MremapOutcome {
                addr: new_region.start(),
                notifications,
            });
        }

        if let Some(locked_vm_after_commit) = locked_vm_after_move_commit {
            self.locked_vm = locked_vm_after_commit;
        }
        tlb.finish();

        if locked_source && new_len > old_len {
            self.best_effort_locked_population(
                new_region.start() + old_len,
                new_len - old_len,
                vm_flags,
            );
        }

        Ok(MremapOutcome {
            addr: new_region.start(),
            notifications,
        })
    }
}
