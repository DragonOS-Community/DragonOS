use super::*;
use crate::mm::fault::{FaultFlags, PageFaultHandler, PageFaultMessage};
use crate::mm::VmFaultReason;

struct DisarmTarget {
    vaddr: usize,
    site: Arc<UprobeSite>,
    old_byte: u8,
    consumers: Vec<Arc<UprobeConsumer>>,
}

/// Remove every uprobe site whose instruction lies in `region` before a VMA/PTE
/// mutation commits.  The caller owns `AddressSpace::write()`, so mapping
/// identity and the restored byte are checked against one stable VMA view.
///
/// The XOL VMA is kernel-owned and immutable while the address space lives.
/// User VMA operations which overlap it are rejected before any probe byte or
/// mapping is changed.
fn validate_uprobe_change_range(mm: &AddressSpace, region: VirtRegion) -> Result<(), SystemError> {
    if mm.xol_pool.overlaps(region) {
        Err(SystemError::EBUSY)
    } else {
        Ok(())
    }
}

fn disarm_uprobe_change_range_locked(
    mm: &Arc<AddressSpace>,
    inner: &mut InnerAddressSpace,
    region: VirtRegion,
) {
    let candidate_start = region
        .start()
        .data()
        .saturating_sub(UPROBE_INSN_COPY_SIZE - 1);
    let candidate_sites = mm
        .uprobe_list
        .range(candidate_start..region.end().data())
        .into_iter()
        .filter_map(|(vaddr, site)| {
            let instruction_end = vaddr.checked_add(site.insn_analysis.insn_len)?;
            if vaddr >= region.end().data() || instruction_end <= region.start().data() {
                return None;
            }
            Some((vaddr, site))
        })
        .collect::<Vec<_>>();
    let targets: Vec<DisarmTarget> = candidate_sites
        .into_iter()
        .map(|(vaddr, site)| {
            let consumers = site
                .members
                .lock()
                .values()
                .map(|member| member.consumer.clone())
                .collect();
            DisarmTarget {
                vaddr,
                old_byte: site.old_instruction[0],
                site,
                consumers,
            }
        })
        .collect();

    let mut disarmed = Vec::new();
    for DisarmTarget {
        vaddr: probe_vaddr,
        site,
        old_byte,
        consumers,
    } in targets
    {
        if site
            .state
            .compare_exchange(
                UprobeSiteState::Armed as u8,
                UprobeSiteState::Disarming as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            continue;
        }

        // Keep the hit-table entry visible until the original byte is restored.
        // A concurrent old #BP can therefore still find the site and execute its
        // strongly held XOL lease. VMA withdrawal does not suppress callbacks
        // for an instruction which already trapped before the mapping change.
        if site_mapping_still_matches(inner, &site) {
            let page_base = probe_vaddr & !(MMArch::PAGE_SIZE - 1);
            let page_offset = probe_vaddr & (MMArch::PAGE_SIZE - 1);
            restore_breakpoint_byte(mm, inner, page_base, page_offset, old_byte);
        }
        disarmed.push((probe_vaddr, site, old_byte, consumers));
    }

    if !disarmed.is_empty() {
        // Besides invalidating translations, this synchronous shootdown is the
        // grace point for CPUs which executed INT3 before the bytes above were
        // restored. Such a CPU runs #BP with interrupts disabled and cannot
        // acknowledge the IPI until it has acquired the hit-table entry and
        // its strongly held XOL slot lease.
        // The acknowledgement, not the flushed address coverage, provides
        // the grace point. Flush one affected page here; the VMA operation
        // performs its own range-wide TLB invalidation when it commits.
        let rendezvous_page = disarmed[0].0 & !(MMArch::PAGE_SIZE - 1);
        mm.flush_tlb_range(
            VirtAddr::new(rendezvous_page),
            VirtAddr::new(rendezvous_page + MMArch::PAGE_SIZE),
            MMArch::PAGE_SHIFT as u8,
            false,
        );
    }

    for (probe_vaddr, site, _, consumers) in disarmed {
        // Conditional removal prevents an old lifecycle delta from deleting a
        // newer site installed at the same virtual address.
        mm.uprobe_list.remove_if(probe_vaddr, &site);
        site.members.lock().clear();
        site.withdraw_participants();
        for consumer in consumers {
            consumer.forget_site(mm.id(), probe_vaddr, &site);
        }
        site.state
            .store(UprobeSiteState::Dead as u8, Ordering::Release);
        let page_base = probe_vaddr & !(MMArch::PAGE_SIZE - 1);
        let mut pages = mm.uprobe_page_state.lock_irqsave();
        if let Some(state) = pages.get_mut(&page_base) {
            debug_assert!(state.refcount != 0, "uprobe page refcount underflow");
            if state.refcount <= 1 {
                pages.remove(&page_base);
            } else {
                state.refcount -= 1;
            }
        } else {
            debug_assert!(false, "armed uprobe site without page state");
        }
    }
}

/// One complete prepare/mutate/finish boundary for a live VMA change.
///
/// The value is private to the outer `AddressSpace` operation. It cannot be
/// cloned, and dropping it before `finish` is a control-flow bug. Reconcile is
/// intentionally explicit because it may fault and must never run from Drop.
pub(crate) struct PreparedUprobeChange {
    prepared_ranges: Vec<VirtRegion>,
    finished: bool,
}

impl PreparedUprobeChange {
    pub(crate) fn validate(mm: &AddressSpace, ranges: &[VirtRegion]) -> Result<(), SystemError> {
        for region in ranges {
            validate_uprobe_change_range(mm, *region)?;
        }
        Ok(())
    }

    pub(crate) fn prepare(
        mm: &Arc<AddressSpace>,
        inner: &mut InnerAddressSpace,
        ranges: &[VirtRegion],
    ) -> Result<Self, SystemError> {
        // Validate every range before withdrawing any site, so a later XOL
        // overlap cannot leave an earlier range requiring hidden rollback.
        Self::validate(mm, ranges)?;
        for region in ranges {
            disarm_uprobe_change_range_locked(mm, inner, *region);
        }
        Ok(Self {
            prepared_ranges: ranges.to_vec(),
            finished: false,
        })
    }

    /// Withdraw sites after a PTE mutation has already made the affected
    /// instruction unreachable (for example mprotect-to-NX or DONTNEED zap).
    /// The caller must validate the ranges before committing the mutation.
    pub(crate) fn prepare_after_pte_commit(
        mm: &Arc<AddressSpace>,
        inner: &mut InnerAddressSpace,
        ranges: &[VirtRegion],
    ) -> Self {
        debug_assert!(Self::validate(mm, ranges).is_ok());
        for region in ranges {
            disarm_uprobe_change_range_locked(mm, inner, *region);
        }
        Self {
            prepared_ranges: ranges.to_vec(),
            finished: false,
        }
    }

    /// Reconcile every surviving file VMA before the caller releases this
    /// address space's write lock.
    ///
    /// Normal faults and registrations therefore complete before sibling
    /// threads can observe the post-mutation executable mapping. Ranges which
    /// encounter a retry or resource error remain in the returned transaction
    /// for Linux-style best-effort processing after the lock is released.
    pub(crate) fn finish_locked(
        mut self,
        mm: &Arc<AddressSpace>,
        inner: &mut RwSemWriteGuard<'_, InnerAddressSpace>,
    ) -> Option<Self> {
        let mut fallback = Vec::new();
        for region in core::mem::take(&mut self.prepared_ranges) {
            let vmas = inner.mappings.conflicts(region);
            let mut needs_fallback = false;
            for vma in &vmas {
                needs_fallback |= uprobe_apply_vma_range_locked(mm, inner, vma, region);
            }
            if needs_fallback {
                fallback.push(region);
            }
        }
        self.prepared_ranges = fallback;
        if self.prepared_ranges.is_empty() {
            self.finished = true;
            None
        } else {
            Some(self)
        }
    }

    pub(crate) fn finish(mut self, mm: &Arc<AddressSpace>) {
        let ranges = core::mem::take(&mut self.prepared_ranges);
        self.finished = true;
        for region in ranges {
            uprobe_apply_to_range(mm, region);
        }
    }
}

impl Drop for PreparedUprobeChange {
    fn drop(&mut self) {
        debug_assert!(self.finished, "prepared uprobe VMA change was not finished");
    }
}

/// Populate the instruction pages once while `AddressSpace::write()` remains
/// held. A page-cache invalidation conflict is deliberately returned to the
/// caller: waiting for its retry token under the mm write guard can deadlock
/// against writeback, so the unlocked post-commit path remains the fallback.
fn fault_in_probe_mapping_locked(
    mm: &Arc<AddressSpace>,
    inner: &mut RwSemWriteGuard<'_, InnerAddressSpace>,
    expected: &ExpectedProbeMapping,
) -> Result<(), VmFaultReason> {
    let mut previous_page = None;
    for part in &expected.parts {
        let page = part.region.start().data() & !(MMArch::PAGE_SIZE - 1);
        if previous_page == Some(page) {
            continue;
        }
        previous_page = Some(page);
        let address = VirtAddr::new(page);
        if inner.user_mapper.utable.translate(address).is_some() {
            continue;
        }
        let Some(vma) = part.vma.upgrade() else {
            return Err(VmFaultReason::VM_FAULT_SIGBUS);
        };
        let vm_flags = *vma.lock().vm_flags();
        let fault_flags = InnerAddressSpace::mlock_fault_flags(vm_flags)
            .unwrap_or(FaultFlags::FAULT_FLAG_REMOTE)
            | FaultFlags::FAULT_FLAG_REMOTE;
        let outcome = unsafe {
            let message = PageFaultMessage::new(
                vma,
                address,
                fault_flags | FaultFlags::FAULT_FLAG_ALLOW_RETRY | FaultFlags::FAULT_FLAG_KILLABLE,
                &mut inner.user_mapper.utable,
                mm.clone(),
            );
            PageFaultHandler::handle_mm_fault(message)
        };
        // Never wait on outcome.retry_wait here. A successful fault must have
        // published a present PTE; checking the mapper also accepts filesystem
        // implementations whose success reason is the empty bitset.
        if inner.user_mapper.utable.translate(address).is_none() {
            return Err(outcome.reason);
        }
    }
    Ok(())
}

/// Apply active consumers to one newly committed private executable file VMA
/// before its owner releases `AddressSpace::write()` or starts MAP_POPULATE.
///
/// This helper never reacquires the mm lock, never waits on a fault retry, and
/// never returns an errno to the mmap transaction. The ordinary unlocked
/// range apply remains responsible for retrying deferred consumers.
pub(crate) fn uprobe_apply_new_vma_locked(
    mm: &Arc<AddressSpace>,
    inner: &mut RwSemWriteGuard<'_, InnerAddressSpace>,
    vma: &Arc<LockedVMA>,
) -> bool {
    let region = *vma.lock().region();
    uprobe_apply_vma_range_locked(mm, inner, vma, region)
}

/// Range-scoped variant used by mremap executable-publication plans. Registry
/// discovery covers only the corresponding file window plus the maximum
/// instruction prefix, and only instructions intersecting `apply_region` are
/// installed.
pub(crate) fn uprobe_apply_vma_range_locked(
    mm: &Arc<AddressSpace>,
    inner: &mut RwSemWriteGuard<'_, InnerAddressSpace>,
    vma: &Arc<LockedVMA>,
    apply_region: VirtRegion,
) -> bool {
    let mut needs_fallback = false;
    if uprobe_registry_is_empty() {
        return false;
    }

    let (file, region, file_start_byte) = {
        let guard = vma.lock();
        let Some(file) = guard.vm_file() else {
            return false;
        };
        let Some(pgoff) = guard.backing_page_offset() else {
            return false;
        };
        let Some(file_start_byte) = pgoff.checked_mul(MMArch::PAGE_SIZE) else {
            return false;
        };
        (file, *guard.region(), file_start_byte)
    };
    let Some(page_cache) = file.inode().page_cache() else {
        return false;
    };
    if !region.collide(&apply_region) {
        return false;
    }
    let inode_key = Arc::as_ptr(&page_cache) as usize;
    let Some(apply_intersection) = region.intersect(&apply_region) else {
        return false;
    };
    let Some(apply_file_start) =
        file_start_byte.checked_add(apply_intersection.start().data() - region.start().data())
    else {
        return false;
    };
    let Some(apply_file_end) = apply_file_start.checked_add(apply_intersection.size()) else {
        return false;
    };
    // A probe instruction may start in the preceding continuous file VMA and
    // end in this newly committed VMA. Include only the maximum possible
    // instruction-prefix window; `capture_probe_mapping()` below performs the
    // authoritative mapping-chain and file-offset validation.
    let query_start = apply_file_start.saturating_sub(UPROBE_INSN_COPY_SIZE - 1);
    let matches: Vec<(usize, ConsumerList)> = {
        let registry = UPROBE_REGISTRY.lock_irqsave();
        let Some(offsets) = registry.get(&inode_key) else {
            return false;
        };
        offsets
            .range(query_start..apply_file_end)
            .map(|(offset, consumers)| (*offset, consumers.clone()))
            .collect()
    };

    for (offset, consumers) in matches {
        let probe_vaddr = if offset >= file_start_byte {
            region.start().data().checked_add(offset - file_start_byte)
        } else {
            region.start().data().checked_sub(file_start_byte - offset)
        };
        let Some(probe_vaddr) = probe_vaddr else {
            needs_fallback = true;
            continue;
        };
        for consumer in consumers {
            if !consumer.scope.permits(mm) || !consumer.has_published_epoch() {
                continue;
            }
            let Some(instruction_end) =
                probe_vaddr.checked_add(consumer.definition.analysis.insn_len)
            else {
                needs_fallback = true;
                continue;
            };
            if probe_vaddr >= region.end().data() || instruction_end <= region.start().data() {
                continue;
            }
            if probe_vaddr >= apply_region.end().data()
                || instruction_end <= apply_region.start().data()
            {
                continue;
            }
            let Some(expected) = capture_probe_mapping(inner, &consumer.definition, probe_vaddr)
            else {
                // An ineligible mapping is not a deferred installation. A
                // later protection or mapping change will reconcile it.
                continue;
            };
            if let Err(reason) = fault_in_probe_mapping_locked(mm, inner, &expected) {
                needs_fallback = true;
                log::debug!(
                    "uprobe locked mmap apply deferred at {:#x}: {:?}",
                    probe_vaddr,
                    reason
                );
                continue;
            }
            match uprobe_register_locked(mm, inner, probe_vaddr, &consumer, &expected) {
                Ok(Some(handle)) => {
                    // Pair publication of the reverse site index with the
                    // target's exec-time old-mm scan. If exec scanned before
                    // publication, this postcheck observes the new mm and
                    // rolls back without recursively taking mm.write(); if it
                    // still observes the old mm, the later exec scan sees the
                    // already-published site.
                    if consumer.scope.permits(mm) {
                        handle.persist();
                    } else {
                        handle.rollback_locked(mm, inner);
                    }
                }
                Ok(None) | Err(LockedRegisterError::System(SystemError::ENOENT)) => {}
                Err(LockedRegisterError::PageContended(page)) => {
                    needs_fallback = true;
                    log::debug!(
                        "uprobe locked mmap apply deferred at {:#x}: Page {:#x} is busy",
                        probe_vaddr,
                        page.phys_address().data()
                    );
                }
                Err(LockedRegisterError::System(error)) => {
                    needs_fallback = true;
                    log::debug!(
                        "uprobe locked mmap apply failed at {:#x}: {:?}",
                        probe_vaddr,
                        error
                    );
                }
            }
        }
    }
    needs_fallback
}

/// 对新映射的文件 VMA 迟到应用注册表中的探针（评审 R9：dlopen / 后续 mmap）。
///
/// 在 mmap 提交且地址空间写锁释放后调用（本函数内部自取 `mm.write()`）。
/// `region_start/size` 为 VMA 的用户地址区间；`file_start_byte` 为 VMA 起始
/// 地址对应的文件偏移（= `backing_pgoff << PAGE_SHIFT`）。
pub fn uprobe_apply_to_new_vma(
    mm: &Arc<AddressSpace>,
    file: &Arc<crate::filesystem::vfs::file::File>,
    region_start: usize,
    region_size: usize,
    file_start_byte: usize,
) {
    let _ = uprobe_apply_to_new_vma_inner(
        mm,
        file,
        region_start,
        region_size,
        file_start_byte,
        false,
        None,
    );
}

/// Attempt one best-effort site installation without waiting for a contended
/// source page. Mapping hooks and fork replay must never turn an observer into
/// an unbounded wait; explicit consumer activation uses `uprobe_register()`
/// below and retains its strict wait/revalidate contract.
fn uprobe_register_best_effort_once(
    mm: &Arc<AddressSpace>,
    probe_vaddr: usize,
    consumer: &Arc<UprobeConsumer>,
    expected_mapping: &ExpectedProbeMapping,
) -> Result<(), SystemError> {
    let mut inner = mm.write();
    match uprobe_register_locked(mm, &mut inner, probe_vaddr, consumer, expected_mapping) {
        Ok(Some(handle)) => {
            if consumer.scope.permits(mm) {
                handle.persist();
            } else {
                handle.rollback_locked(mm, &mut inner);
            }
            Ok(())
        }
        Ok(None) => Ok(()),
        Err(LockedRegisterError::PageContended(_)) => Err(SystemError::EAGAIN_OR_EWOULDBLOCK),
        Err(LockedRegisterError::System(error)) => Err(error),
    }
}

pub(super) fn uprobe_apply_to_new_vma_inner(
    mm: &Arc<AddressSpace>,
    file: &Arc<crate::filesystem::vfs::file::File>,
    region_start: usize,
    region_size: usize,
    file_start_byte: usize,
    strict: bool,
    only_consumer_id: Option<u64>,
) -> Result<(), SystemError> {
    let inode = file.inode();
    let inode_id = inode.metadata().map(|md| md.inode_id.data()).unwrap_or(0);
    let Some(page_cache) = inode.page_cache() else {
        return Ok(());
    };
    let inode_key = Arc::as_ptr(&page_cache) as usize;
    let region_file_end = file_start_byte
        .checked_add(region_size)
        .ok_or(SystemError::EINVAL)?;
    // 锁内快照：落在新 VMA 文件区间内的消费者列表
    let matches: Vec<(usize, ConsumerList)> = {
        let r = UPROBE_REGISTRY.lock_irqsave();
        let Some(offsets) = r.get(&inode_key) else {
            return Ok(());
        };
        offsets
            .range(file_start_byte..region_file_end)
            .filter_map(|(off, consumers)| {
                let consumers = if let Some(id) = only_consumer_id {
                    consumers
                        .iter()
                        .filter(|consumer| consumer.id == id)
                        .cloned()
                        .collect()
                } else {
                    consumers.clone()
                };
                (!consumers.is_empty()).then_some((*off, consumers))
            })
            .collect()
    };
    if matches.is_empty() {
        return Ok(());
    }

    for (offset, consumers) in matches {
        let probe_vaddr = region_start
            .checked_add(offset - file_start_byte)
            .ok_or(SystemError::EINVAL)?;
        for consumer in consumers {
            if !consumer.scope.permits(mm) || !consumer.has_published_epoch() {
                continue;
            }
            // Ordinary file mmap is lazy. Fault in the one or two pages which
            // contain the complete instruction, pinning each retry to the VMA
            // identity observed here so MAP_FIXED cannot redirect installation.
            let expected_mapping = {
                let inner = mm.read();
                capture_probe_mapping(&inner, &consumer.definition, probe_vaddr)
            };
            let Some(expected_mapping) = expected_mapping else {
                // An incomplete, non-executable, non-contiguous, shared, or
                // writable alias is not an install failure. Keep the consumer
                // registered for a later eligible mapping.
                continue;
            };
            let mut population_error = None;
            for part in &expected_mapping.parts {
                let page_start = part.region.start().data() & !(MMArch::PAGE_SIZE - 1);
                let Some(page_end) = part
                    .region
                    .end()
                    .data()
                    .checked_add(MMArch::PAGE_SIZE - 1)
                    .map(|end| end & !(MMArch::PAGE_SIZE - 1))
                else {
                    population_error = Some(SystemError::EINVAL);
                    break;
                };
                if let Err(e) = mm.populate_uprobe_range_post_commit(
                    VirtAddr::new(page_start),
                    page_end - page_start,
                    Some(part.vma.clone()),
                ) {
                    population_error = Some(e);
                    break;
                }
            }
            if let Some(e) = population_error {
                log::debug!(
                    "uprobe fault-in {:x}+{:#x} in new vma failed: {:?}",
                    inode_id,
                    offset,
                    e
                );
                if strict {
                    return Err(e);
                }
                continue;
            }
            let register_result = if strict {
                uprobe_register(mm, probe_vaddr, &consumer, &expected_mapping).map(|handle| {
                    if let Some(handle) = handle {
                        handle.persist();
                    }
                })
            } else {
                uprobe_register_best_effort_once(mm, probe_vaddr, &consumer, &expected_mapping)
            };
            match register_result {
                Ok(()) => {}
                Err(e) => {
                    log::debug!(
                        "uprobe late-apply {:x}+{:#x} in new vma failed: {:?}",
                        inode_id,
                        offset,
                        e
                    );
                    // ENOENT is a consumer closing concurrently and therefore a
                    // successful absence, not a fork transaction failure.
                    if strict && e != SystemError::ENOENT {
                        return Err(e);
                    }
                }
            }
        }
    }
    Ok(())
}

/// Re-evaluate all file VMAs intersecting `region` after a committed VMA
/// operation.  The VMA snapshot is owned and the address-space lock is dropped
/// before fault-in/registration, preserving the registry -> mm lock boundary.
pub(super) fn collect_file_vma_snapshot(
    mm: &Arc<AddressSpace>,
    region: VirtRegion,
) -> Vec<(Arc<crate::filesystem::vfs::file::File>, usize, usize, usize)> {
    {
        let inner = mm.read();
        inner
            .mappings
            .conflicts(region)
            .into_iter()
            .filter_map(|vma| {
                let guard = vma.lock();
                let file = guard.vm_file()?;
                let pgoff = guard.backing_page_offset()?;
                let vma_region = *guard.region();
                let intersection = vma_region.intersect(&region)?;
                let file_start = pgoff
                    .checked_mul(MMArch::PAGE_SIZE)?
                    .checked_add(intersection.start().data() - vma_region.start().data())?;
                Some((
                    file,
                    intersection.start().data(),
                    intersection.size(),
                    file_start,
                ))
            })
            .collect()
    }
}

pub(crate) fn uprobe_apply_to_range(mm: &Arc<AddressSpace>, region: VirtRegion) {
    if uprobe_registry_is_empty() {
        return;
    }
    // A variable-length x86 instruction can start on the preceding page or
    // VMA and overlap this mutation with only its tail. Reconsider the maximum
    // possible prefix so a disarmed cross-boundary site is only restored once
    // its complete mapping is valid again.
    let start = region
        .start()
        .data()
        .saturating_sub(UPROBE_INSN_COPY_SIZE - 1);
    let region = VirtRegion::new(VirtAddr::new(start), region.end().data() - start);
    for (file, start, size, offset) in collect_file_vma_snapshot(mm, region) {
        uprobe_apply_to_new_vma(mm, &file, start, size, offset);
    }
}

pub(crate) fn uprobe_apply_to_all_vmas(mm: &Arc<AddressSpace>) {
    uprobe_apply_to_range(
        mm,
        VirtRegion::new(VirtAddr::new(0), MMArch::USER_END_VADDR.data()),
    );
}

/// Replay consumers after exec has published its final target mm.
///
/// Consumer activation and this gate form a two-sided publication handshake:
/// an epoch visible here is replayed below, while an epoch published after the
/// gate makes activation observe and scan the already-stable target mm. The
/// gate is allocation-free, so unrelated task-scoped events do not turn every
/// exec into a full address-space walk.
pub(crate) fn uprobe_apply_to_exec_mm(target: &Arc<ProcessControlBlock>, mm: &Arc<AddressSpace>) {
    if !uprobe_registry_has_active_for_exec_mm(target, mm) {
        return;
    }
    uprobe_apply_to_all_vmas(mm);
}

/// Restore breakpoint bytes inherited through fork COW before the child mm is
/// exposed without its write lock.
///
/// Child file VMAs are already linked into file-rmap at this point. Keeping
/// their mm write-locked while replacing inherited breakpoint pages prevents
/// a concurrent system-wide registry scan from observing `0xcc` without the
/// corresponding child hit-table entry.
pub(crate) fn fork_restore_inherited_uprobes_locked(
    parent_mm: &Arc<AddressSpace>,
    child_mm: &Arc<AddressSpace>,
    inner: &mut InnerAddressSpace,
) -> Result<(), SystemError> {
    // The child initially shares the parent's private breakpoint pages through
    // normal fork COW.  Restore every inherited 0xcc in one private copy per
    // physical page before the child can run.  Afterwards the registry applies
    // only consumers whose scope actually permits the child mm (system-wide in
    // phase 1; task events have inherit=0).
    let snapshot: BTreeMap<usize, Vec<(usize, u8)>> = {
        let sites = parent_mm
            .uprobe_list
            .range(0..MMArch::USER_END_VADDR.data());
        let mut pages = BTreeMap::<usize, Vec<(usize, u8)>>::new();
        for (vaddr, site) in sites {
            let old_byte = site.old_instruction[0];
            pages
                .entry(vaddr & !(MMArch::PAGE_SIZE - 1))
                .or_default()
                .push((vaddr & (MMArch::PAGE_SIZE - 1), old_byte));
        }
        pages
    };
    if snapshot.is_empty() {
        return Ok(());
    }

    for (page_base, patches) in snapshot {
        let address = VirtAddr::new(page_base);
        let Some(vma) = inner.mappings.contains(address) else {
            // A VM_DONTCOPY source VMA intentionally has no child mapping.
            continue;
        };
        let (old_paddr, entry_flags) = inner
            .user_mapper
            .utable
            .translate(address)
            .ok_or(SystemError::EFAULT)?;
        let old_page = {
            let mut pages = page_manager_lock();
            pages.get(&old_paddr).ok_or(SystemError::EFAULT)?
        };
        let new_page = {
            let mapper = &mut inner.user_mapper.utable;
            let mut pages = page_manager_lock();
            pages
                .copy_page_as_normal(&old_paddr, mapper.allocator_mut())
                .map_err(|_| SystemError::ENOMEM)?
        };
        for (offset, byte) in patches {
            patch_byte_in_phys(&new_page, offset, byte)?;
        }
        {
            let _pt_edit = child_mm.page_table_edit();
            let mapper = &mut inner.user_mapper.utable;
            let table = mapper.get_table(address, 0).ok_or(SystemError::EFAULT)?;
            let index = table.index_of(address).ok_or(SystemError::EFAULT)?;
            unsafe {
                table.set_entry(index, PageEntry::new(new_page.phys_address(), entry_flags));
            }
        }
        let vm_locked = vma.lock().vm_flags().contains(VmFlags::VM_LOCKED);
        new_page.write().insert_vma(vma.clone(), vm_locked);
        old_page.write().remove_vma(vma.as_ref());
        InnerAddressSpace::remove_page_unevictable_if_unneeded(&old_page);
        child_mm.flush_tlb_range(
            address,
            VirtAddr::new(page_base + MMArch::PAGE_SIZE),
            MMArch::PAGE_SHIFT as u8,
            false,
        );
    }
    Ok(())
}

/// Best-effort replay of currently enabled consumers after fork has published
/// a clean child address space.  The inherited breakpoint sanitization above
/// remains part of the fallible fork transaction, but a consumer whose
/// definition no longer matches a private child page must not change fork's
/// result. Concurrent registry scans are idempotent because both paths
/// serialize installation with the child mm write lock.
pub fn fork_inherit_uprobes(child_mm: &Arc<AddressSpace>) {
    // Task-scoped perf events have inherit=0 and can never permit this fresh
    // child mm. The exact system-wide active count participates in the same
    // publication handshake as registry activation: if fork observes zero,
    // a later activation's file-rmap scan will discover the already-linked
    // child VMA; if it observes non-zero, this replay sees the published epoch.
    // Inherited INT3 sanitization is deliberately not guarded by this fast
    // path and remains in fork_restore_inherited_uprobes_locked().
    if !uprobe_registry_has_active_system_wide_consumers() {
        return;
    }
    uprobe_apply_to_all_vmas(child_mm);
}
