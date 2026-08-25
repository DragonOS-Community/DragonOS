use super::*;
use crate::filesystem::vfs::VmaOpenRollback;
#[cfg(target_arch = "x86_64")]
use crate::mm::page::PageEntry;
#[cfg(target_arch = "x86_64")]
use ::uprobe::UPROBE_INSN_COPY_SIZE;

struct MremapExecRange {
    vma: Arc<LockedVMA>,
    apply_region: VirtRegion,
    original_entry_execute: bool,
}

#[cfg(target_arch = "x86_64")]
fn publication_pte_region(vma_region: VirtRegion, apply_region: VirtRegion) -> VirtRegion {
    let prefix = apply_region
        .start()
        .data()
        .saturating_sub(UPROBE_INSN_COPY_SIZE - 1)
        & !(MMArch::PAGE_SIZE - 1);
    let suffix = apply_region
        .end()
        .data()
        .checked_add(UPROBE_INSN_COPY_SIZE - 1)
        .map(page_align_up)
        .unwrap_or(apply_region.end().data());
    let pte_start = VirtAddr::new(core::cmp::max(prefix, vma_region.start().data()));
    let pte_end = VirtAddr::new(core::cmp::min(suffix, vma_region.end().data()));
    VirtRegion::new(pte_start, pte_end - pte_start)
}

/// A local, explicit plan for the executable mappings temporarily withdrawn
/// during mremap. It never changes logical VM_EXEC and never survives the outer
/// mremap call: AddressSpace performs locked best-effort uprobe application and
/// then consumes the plan to restore the captured entry permission.
#[derive(Default)]
pub(super) struct MremapExecPublication {
    // A move can defer at most the retained source and the new target. An
    // in-place grow and old_len==0 use only one slot. Keeping this fixed-size
    // makes the post-commit publication path allocation-free.
    ranges: [Option<MremapExecRange>; 2],
}

impl MremapExecPublication {
    #[cfg(target_arch = "x86_64")]
    fn defer_range(
        &mut self,
        inner: &mut InnerAddressSpace,
        mm: &Arc<AddressSpace>,
        vma: &Arc<LockedVMA>,
        region: VirtRegion,
    ) {
        if self
            .ranges
            .iter()
            .flatten()
            .any(|range| Arc::ptr_eq(&range.vma, vma) && range.apply_region == region)
        {
            return;
        }

        let prior_entry_execute = self
            .ranges
            .iter()
            .flatten()
            .find(|range| Arc::ptr_eq(&range.vma, vma))
            .map(|range| range.original_entry_execute);
        let original_entry_execute = {
            let mut guard = vma.lock();
            let original = prior_entry_execute.unwrap_or_else(|| guard.flags().has_execute());
            guard.set_entry_execute(false);
            original
        };
        // Locked uprobe capture may fault the page containing an instruction
        // which starts immediately before `region` or ends immediately after
        // it. Since the entry permission is VMA-wide, publish every such page
        // back to the captured state instead of leaving a faulted neighbour NX.
        let slot = self
            .ranges
            .iter_mut()
            .find(|slot| slot.is_none())
            .expect("mremap exec publication exceeds source+target bound");
        *slot = Some(MremapExecRange {
            vma: vma.clone(),
            apply_region: region,
            original_entry_execute,
        });

        let mut changed = false;
        {
            let _pt_edit = mm.page_table_edit();
            let mapper = &mut inner.user_mapper.utable;
            let mut address = region.start();
            while address < region.end() {
                if let Some((paddr, flags)) = mapper.translate(address) {
                    if flags.has_execute() {
                        let table = mapper
                            .get_table(address, 0)
                            .expect("present mremap barrier must have a leaf table");
                        let index = table
                            .index_of(address)
                            .expect("present mremap barrier must have a leaf index");
                        unsafe {
                            table.set_entry(index, PageEntry::new(paddr, flags.set_execute(false)));
                        }
                        changed = true;
                    }
                }
                address += MMArch::PAGE_SIZE;
            }
        }
        if changed {
            mm.flush_tlb_range(
                region.start(),
                region.end(),
                MMArch::PAGE_SHIFT as u8,
                false,
            );
        }
    }

    fn discard_vma(&mut self, vma: &Arc<LockedVMA>) {
        for slot in &mut self.ranges {
            if slot
                .as_ref()
                .is_some_and(|range| Arc::ptr_eq(&range.vma, vma))
            {
                *slot = None;
            }
        }
    }

    /// `VMA::set_flags()` can recompute executable entry flags while clearing
    /// VM_LOCKED on a retained DONTUNMAP source. Reassert the plan before any
    /// locked fault/install path consumes those flags.
    fn reassert_deferred_entry_flags(&self) {
        for range in self.ranges.iter().flatten() {
            range.vma.lock().set_entry_execute(false);
        }
    }

    /// `VMA::extract()` clones the source entry flags after the publication
    /// barrier is active. Fragments outside the moved range were never
    /// withdrawn, so restore their captured entry permission before exposing
    /// them as independent VMAs.
    fn restore_split_fragment(&self, source: &Arc<LockedVMA>, fragment: &Arc<LockedVMA>) {
        let Some(range) = self
            .ranges
            .iter()
            .flatten()
            .find(|range| Arc::ptr_eq(&range.vma, source))
        else {
            return;
        };
        let mut guard = fragment.lock();
        let restore = range.original_entry_execute && guard.vm_flags().contains(VmFlags::VM_EXEC);
        guard.set_entry_execute(restore);
    }

    pub(super) fn ranges(&self) -> impl Iterator<Item = (&Arc<LockedVMA>, VirtRegion)> {
        self.ranges
            .iter()
            .flatten()
            .map(|range| (&range.vma, range.apply_region))
    }

    /// Restore only the execute permission captured for each still-current
    /// logical executable VMA, and complete one unified shootdown.
    #[cfg(target_arch = "x86_64")]
    pub(super) fn publish(self, inner: &mut InnerAddressSpace, mm: &Arc<AddressSpace>) {
        // Preserve the global VMA -> page_table_edit lock order. No VMA lock
        // is acquired while page-table mutation serialization is held.
        let mut publications = [None; 2];
        for (index, range) in self.ranges.iter().enumerate() {
            let Some(range) = range else {
                continue;
            };
            if inner
                .mappings
                .contains(range.apply_region.start())
                .is_some_and(|current| Arc::ptr_eq(&current, &range.vma))
            {
                let publication = {
                    let mut guard = range.vma.lock();
                    let restore =
                        range.original_entry_execute && guard.vm_flags().contains(VmFlags::VM_EXEC);
                    let pte_region = publication_pte_region(*guard.region(), range.apply_region);
                    guard.set_entry_execute(restore);
                    (pte_region, restore)
                };
                publications[index] = Some(publication);
            }
        }
        let mut tlb = MmuGather::gather(mm);
        {
            let _pt_edit = mm.page_table_edit();
            let mapper = &mut inner.user_mapper.utable;
            for publication in publications {
                let Some((pte_region, restore_execute)) = publication else {
                    continue;
                };
                let mut address = pte_region.start();
                while address < pte_region.end() {
                    if let Some((paddr, flags)) = mapper.translate(address) {
                        if flags.has_execute() != restore_execute {
                            let table = mapper
                                .get_table(address, 0)
                                .expect("present mremap publication must have a leaf table");
                            let index = table
                                .index_of(address)
                                .expect("present mremap publication must have a leaf index");
                            unsafe {
                                table.set_entry(
                                    index,
                                    PageEntry::new(paddr, flags.set_execute(restore_execute)),
                                );
                            }
                            tlb.accumulate_range(address);
                        }
                    }
                    address += MMArch::PAGE_SIZE;
                }
            }
        }
        tlb.finish();
    }
}

pub(super) struct MremapRequest {
    pub old_vaddr: VirtAddr,
    pub old_len: usize,
    pub new_len: usize,
    pub flags: MremapFlags,
    pub new_vaddr: VirtAddr,
}

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
    /// # Returns
    ///
    /// Returns the starting virtual page frame address of the remapped region
    ///
    /// # Errors
    ///
    /// - `EINVAL`: invalid argument
    pub(super) fn mremap<F>(
        &mut self,
        request: MremapRequest,
        mut before_mutation: F,
    ) -> Result<MremapOutcome, MremapFailure>
    where
        F: FnMut(&mut Self, &[VirtRegion]) -> Result<(), SystemError>,
    {
        let MremapRequest {
            old_vaddr,
            mut old_len,
            new_len,
            flags: mremap_flags,
            new_vaddr,
        } = request;
        let mut notifications = VmaCloseNotifications::default();
        let mut exec_publication = MremapExecPublication::default();
        macro_rules! mremap_fail {
            ($err:expr) => {
                return Err(MremapFailure {
                    err: $err,
                    notifications,
                    exec_publication,
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

        // Linux holds mmap_write_lock from the initial lookup through every
        // validation and mutation. Keep the source identity and flags in this
        // same transaction instead of accepting a syscall-layer snapshot.
        let Some(initial_vma) = self.mappings.contains(old_vaddr) else {
            mremap_fail!(SystemError::EFAULT);
        };
        let (initial_region, initial_vm_flags) = {
            let guard = initial_vma.lock();
            (*guard.region(), *guard.vm_flags())
        };

        // Huge-page remapping is not implemented yet. Like Linux, reject it
        // before MREMAP_FIXED can destroy the destination.
        if initial_vm_flags.contains(VmFlags::VM_HUGETLB) {
            log::error!("mremap: huge-page mappings are not supported");
            mremap_fail!(SystemError::ENOSYS);
        }

        // Linux performs the source lookup before validating an explicit
        // target, then validates that target before any destination unmap.
        if mremap_flags.intersects(MremapFlags::MREMAP_FIXED | MremapFlags::MREMAP_DONTUNMAP) {
            if !new_vaddr.check_aligned(MMArch::PAGE_SIZE) {
                mremap_fail!(SystemError::EINVAL);
            }
            let Some(new_end) = new_vaddr.data().checked_add(new_len) else {
                mremap_fail!(SystemError::EINVAL);
            };
            if new_end > MMArch::USER_END_VADDR.data() {
                mremap_fail!(SystemError::EINVAL);
            }
            let old_end = old_vaddr.data().wrapping_add(old_len);
            if old_end > new_vaddr.data() && new_end > old_vaddr.data() {
                mremap_fail!(SystemError::EINVAL);
            }
        }
        let fixed_new_region = if mremap_flags.contains(MremapFlags::MREMAP_FIXED) {
            let new_region = mremap_try!(Self::checked_user_region(new_vaddr, new_len));
            Some(new_region)
        } else {
            None
        };

        // A plain equal-size remap is a no-op, and a plain shrink only unmaps
        // the tail. Both precede vma_to_resize() and therefore remain valid for
        // VM_DONTEXPAND/VM_PFNMAP mappings on Linux.
        if !mremap_flags.intersects(MremapFlags::MREMAP_FIXED | MremapFlags::MREMAP_DONTUNMAP)
            && old_len == new_len
        {
            return Ok(MremapOutcome {
                addr: old_vaddr,
                notifications,
                exec_publication,
                post_commit_population: None,
            });
        }

        // XOL mappings are kernel-owned and their pool records a fixed virtual
        // slot address. Unlike a generic VM_DONTEXPAND mapping, relocating or
        // duplicating one would corrupt that ownership metadata, so reject it
        // before any fixed-target side effect.
        #[cfg(target_arch = "x86_64")]
        if self.outer_addr_space().is_some_and(|mm| {
            let plain_shrink = old_len > new_len
                && !mremap_flags
                    .intersects(MremapFlags::MREMAP_FIXED | MremapFlags::MREMAP_DONTUNMAP);
            (!plain_shrink && mm.xol_pool.overlaps(initial_region))
                || fixed_new_region.is_some_and(|target| mm.xol_pool.overlaps(target))
                || (old_len > new_len
                    && mm
                        .xol_pool
                        .overlaps(VirtRegion::new(old_vaddr + new_len, old_len - new_len)))
        }) {
            mremap_fail!(if mremap_flags.contains(MremapFlags::MREMAP_DONTUNMAP) {
                SystemError::EINVAL
            } else {
                SystemError::EFAULT
            });
        }

        if old_len > new_len && !mremap_flags.contains(MremapFlags::MREMAP_FIXED) {
            let prepared = match self.prepare_munmap(
                VirtPageFrame::new(old_vaddr + new_len),
                PageFrameCount::from_bytes(old_len - new_len).unwrap(),
            ) {
                Ok(prepared) => prepared,
                Err(failure) => {
                    notifications.extend(failure.notifications);
                    mremap_fail!(failure.err);
                }
            };
            if let Err(err) = before_mutation(self, &prepared.affected_ranges()) {
                notifications.extend(prepared.rollback());
                mremap_fail!(err);
            }
            notifications.extend(self.commit_munmap(prepared));
            return Ok(MremapOutcome {
                addr: old_vaddr,
                notifications,
                exec_publication,
                post_commit_population: None,
            });
        }

        if mremap_flags.contains(MremapFlags::MREMAP_FIXED) {
            let start_page = VirtPageFrame::new(new_vaddr);
            let page_count = PageFrameCount::from_bytes(new_len).unwrap();
            let prepared = match self.prepare_munmap(start_page, page_count) {
                Ok(prepared) => prepared,
                Err(failure) => {
                    notifications.extend(failure.notifications);
                    mremap_fail!(failure.err);
                }
            };
            if let Err(err) = before_mutation(self, &prepared.affected_ranges()) {
                notifications.extend(prepared.rollback());
                mremap_fail!(err);
            }
            notifications.extend(self.commit_munmap(prepared));
        }
        if mremap_flags.contains(MremapFlags::MREMAP_FIXED) && old_len > new_len {
            let prepared = match self.prepare_munmap(
                VirtPageFrame::new(old_vaddr + new_len),
                PageFrameCount::from_bytes(old_len - new_len).unwrap(),
            ) {
                Ok(prepared) => prepared,
                Err(failure) => {
                    notifications.extend(failure.notifications);
                    mremap_fail!(failure.err);
                }
            };
            if let Err(err) = before_mutation(self, &prepared.affected_ranges()) {
                notifications.extend(prepared.rollback());
                mremap_fail!(err);
            }
            notifications.extend(self.commit_munmap(prepared));
            old_len = new_len;
        }
        // Read backing info of the old VMA (file/shared-anon) and the page offset base.
        // MREMAP_FIXED may have already split the target interval and shrink tail above; re-query
        // the source VMA to avoid using an old cache that may be invalidated after split.
        let Some(old_vma) = self.mappings.contains(old_vaddr) else {
            mremap_fail!(SystemError::EFAULT);
        };
        let (old_region, vm_flags, vm_file, shared_anon, base_pgoff, sysv_shm) = {
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
                *g.vm_flags(),
                g.vm_file(),
                g.shared_anon.clone(),
                base,
                g.sysv_shm(),
            )
        };
        let prot_flags: ProtFlags = vm_flags.into();

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
        let can_move = mremap_flags.contains(MremapFlags::MREMAP_MAYMOVE)
            || mremap_flags.contains(MremapFlags::MREMAP_FIXED);

        // Linux checks the old_len==0 legacy duplicate rule before the
        // special-mapping and source-span checks in vma_to_resize().
        if old_len == 0 && !vm_flags.intersects(VmFlags::VM_SHARED | VmFlags::VM_MAYSHARE) {
            mremap_fail!(SystemError::EINVAL);
        }
        if dontunmap_flag && vm_flags.intersects(VmFlags::VM_DONTEXPAND | VmFlags::VM_PFNMAP) {
            mremap_fail!(SystemError::EINVAL);
        }
        let Some(max_old_len) = old_region.end().data().checked_sub(old_vaddr.data()) else {
            mremap_fail!(SystemError::EINVAL);
        };
        if source_len > max_old_len {
            mremap_fail!(SystemError::EFAULT);
        }
        if new_len != old_len && vm_flags.intersects(VmFlags::VM_DONTEXPAND | VmFlags::VM_PFNMAP) {
            mremap_fail!(SystemError::EFAULT);
        }
        let source_region = VirtRegion::new(old_vaddr, source_len);
        if dontunmap_flag {
            debug_assert!(
                old_vaddr.data().wrapping_add(old_len) <= new_vaddr.data()
                    || new_vaddr.data() + new_len <= old_vaddr.data()
            );
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

        if old_len == 0 && !can_move {
            mremap_fail!(SystemError::ENOMEM);
        }

        // Linux: old_len==0 means “copy/duplicate-map” a shared region (DOS-emu legacy).
        // - Only allowed for shared mappings
        // - Return ENOMEM without MAYMOVE/FIXED
        if mremap_flags.contains(MremapFlags::MREMAP_FIXED) {
            if let Err(err) = check_mmap_min_addr(new_vaddr, self.mmap_min) {
                mremap_fail!(err);
            }
        }
        let Some(mm) = self.outer_addr_space() else {
            mremap_fail!(SystemError::EFAULT);
        };

        // When moving is not allowed, only try in-place expansion.
        if !can_move {
            if new_len <= old_len {
                return Ok(MremapOutcome {
                    addr: old_vaddr,
                    notifications: VmaCloseNotifications::default(),
                    exec_publication: MremapExecPublication::default(),
                    post_commit_population: None,
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
            #[cfg(target_arch = "x86_64")]
            if vm_file.as_ref().is_some_and(|file| {
                base_pgoff
                    .checked_mul(MMArch::PAGE_SIZE)
                    .and_then(|start| start.checked_add(old_len))
                    .is_some_and(|file_start_byte| {
                        super::uprobe::requires_exec_publication_barrier(
                            &mm,
                            file,
                            vm_flags,
                            file_start_byte,
                            grow,
                        )
                    })
            }) {
                let grown_vma = self
                    .mappings
                    .contains(old_vaddr)
                    .expect("committed in-place mremap VMA disappeared");
                exec_publication.defer_range(self, &mm, &grown_vma, grow_region);
            }
            if let Some(locked_vm_after_grow) = locked_vm_after_grow {
                self.locked_vm = locked_vm_after_grow;
            }
            return Ok(MremapOutcome {
                addr: old_vaddr,
                notifications: VmaCloseNotifications::default(),
                exec_publication,
                post_commit_population: locked_source.then(|| {
                    let vma = self
                        .mappings
                        .contains(old_vaddr)
                        .expect("committed in-place mremap VMA disappeared");
                    (grow_region, Arc::downgrade(&vma))
                }),
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

        // A moved executable PTE can be observed by another CPU immediately;
        // AddressSpace::write() only serializes software page-table updates.
        // Publish eligible uprobe targets as NX until the outer owner installs
        // matching sites under the same write guard. A concurrent instruction
        // fetch then blocks in the fault path instead of escaping unprobed.
        #[cfg(target_arch = "x86_64")]
        let defer_target_execute = vm_file.as_ref().is_some_and(|file| {
            base_pgoff
                .checked_mul(MMArch::PAGE_SIZE)
                .is_some_and(|file_start_byte| {
                    super::uprobe::requires_exec_publication_barrier(
                        &mm,
                        file,
                        vm_flags,
                        file_start_byte,
                        new_len,
                    )
                })
        });
        #[cfg(not(target_arch = "x86_64"))]
        let defer_target_execute = false;
        #[cfg(target_arch = "x86_64")]
        let defer_source_execute = old_len != 0
            && vm_file.as_ref().is_some_and(|file| {
                base_pgoff
                    .checked_mul(MMArch::PAGE_SIZE)
                    .is_some_and(|file_start_byte| {
                        super::uprobe::requires_exec_publication_barrier(
                            &mm,
                            file,
                            vm_flags,
                            file_start_byte,
                            source_len,
                        )
                    })
            });
        #[cfg(target_arch = "x86_64")]
        let defer_target_execute = defer_target_execute || defer_source_execute;
        let entry_flags = EntryFlags::from_prot_flags(prot_flags, true);
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
        if old_len != 0 {
            #[cfg(target_arch = "x86_64")]
            if defer_source_execute {
                exec_publication.defer_range(self, &mm, &old_vma, source_region);
            }
            if let Err(err) = before_mutation(self, &[source_region]) {
                #[cfg(target_arch = "x86_64")]
                {
                    let publication = core::mem::take(&mut exec_publication);
                    publication.publish(self, &mm);
                }
                if let Some(sysv_shm) = sysv_shm.as_ref() {
                    notifications.sysv.push(sysv_shm.clone());
                }
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
            #[cfg(target_arch = "x86_64")]
            if defer_target_execute {
                exec_publication.defer_range(self, &mm, &vma, new_region);
            }
            vma
        };

        // Like Linux vm_ops->open, this is a lifetime notification for the new
        // VMA, not another filesystem mmap admission check.
        let target_vma_open_rollback = vm_file
            .as_ref()
            .map(|file| file.with_io_fs(|fs| fs.vma_open(file, new_region, vm_flags)));
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
                    let target_flags = if defer_target_execute {
                        src_flags.set_execute(false)
                    } else {
                        src_flags
                    };
                    let Some(flush) = (unsafe { mapper.map_phys(dst, paddr, target_flags) }) else {
                        err = Some(SystemError::ENOMEM);
                        break;
                    };
                    unsafe { flush.ignore() };
                    tlb.accumulate_range(dst);
                    installed_target_pte = true;
                    if let PresentPfn::Managed(page) =
                        LockedVMA::classify_present_pfn(&mut page_manager_guard, paddr, vm_flags)
                    {
                        installed_target_present_pages += 1;
                        page.write().insert_vma(new_vma.clone(), locked_source);
                    }

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
                exec_publication.discard_vma(&new_vma);
                drop(page_manager_guard);
                tlb.finish();
                if let (Some(file), Some(VmaOpenRollback::Close)) =
                    (vm_file.as_ref(), target_vma_open_rollback)
                {
                    notifications.vma.push(VmaCloseNotification {
                        file: file.clone(),
                        region: new_region,
                        vm_flags,
                    });
                }
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
                    } else {
                        panic!("mremap commit lost expected source PTE at {:?}", src);
                    }

                    if let PresentPfn::Managed(page) =
                        LockedVMA::classify_present_pfn(&mut page_manager_guard, paddr, vm_flags)
                    {
                        removed_source_present_pages += 1;
                        page.write().remove_vma(old_vma.as_ref());
                    }
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
                exec_publication.restore_split_fragment(&old_vma, &before);
                self.mappings.insert_vma(before);
            }
            if let Some(after) = split_after {
                exec_publication.restore_split_fragment(&old_vma, &after);
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
            if !dontunmap_flag {
                exec_publication.discard_vma(&old_vma);
            }
            exec_publication.reassert_deferred_entry_flags();
            tlb.finish();

            return Ok(MremapOutcome {
                addr: new_region.start(),
                notifications,
                exec_publication,
                post_commit_population: (locked_source && new_len > old_len).then(|| {
                    let vma = self
                        .mappings
                        .contains(new_region.start())
                        .expect("committed moved mremap VMA disappeared");
                    (
                        VirtRegion::new(new_region.start() + old_len, new_len - old_len),
                        Arc::downgrade(&vma),
                    )
                }),
            });
        }

        if let Some(locked_vm_after_commit) = locked_vm_after_move_commit {
            self.locked_vm = locked_vm_after_commit;
        }
        if !dontunmap_flag {
            exec_publication.discard_vma(&old_vma);
        }
        exec_publication.reassert_deferred_entry_flags();
        tlb.finish();

        Ok(MremapOutcome {
            addr: new_region.start(),
            notifications,
            exec_publication,
            post_commit_population: (locked_source && new_len > old_len).then(|| {
                let vma = self
                    .mappings
                    .contains(new_region.start())
                    .expect("committed moved mremap VMA disappeared");
                (
                    VirtRegion::new(new_region.start() + old_len, new_len - old_len),
                    Arc::downgrade(&vma),
                )
            }),
        })
    }
}
