//! Non-blocking MM part of scoped OOM reaping. No worker or kill authorization
//! is installed here. Like Linux's oom_reap_task_mm, VMAs/backing stay alive.
use super::*;
use crate::{
    filesystem::page_cache::PageCache,
    mm::page::{
        retirement::{detach_unmapped_page_locked, RetiredPageOwners},
        try_page_manager_lock, try_page_reclaimer_lock, PageTable,
    },
};

const BATCH: usize = 32;

/// Shared shmem has file operations (Linux shmem_anon_vm_ops), not the
/// anonymous vm_ops == NULL case selected for OOM reaping.
pub(crate) fn is_reapable_vma(anonymous: bool, flags: VmFlags) -> bool {
    (anonymous || !flags.contains(VmFlags::VM_SHARED))
        && !flags.intersects(VmFlags::VM_HUGETLB | VmFlags::VM_PFNMAP)
}

pub(crate) use crate::mm::oom::OomReapAuthorization;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum OomReapBatchResult {
    Continue {
        next: VirtAddr,
        expected_gen: u64,
        unmapped: usize,
    },
    Restart {
        expected_gen: u64,
        unmapped: usize,
    },
    Busy {
        resume: VirtAddr,
        expected_gen: u64,
        unmapped: usize,
    },
    /// A selected leaf cannot safely be processed by the supported protocol.
    Incomplete {
        resume: VirtAddr,
        expected_gen: u64,
        unmapped: usize,
    },
    PassEnd,
    Protected,
}

#[derive(Default)]
struct Retirement {
    page: Option<Arc<Page>>,
    owners: RetiredPageOwners,
    cache: Option<Arc<PageCache>>,
    rmap_backing: Option<hashbrown::HashMap<u64, Arc<LockedVMA>>>,
}

enum Walk {
    Leaf(PhysAddr, EntryFlags<MMArch>),
    Hole(VirtAddr),
    Budget,
    Unsupported,
}

/// Query actual tables under both MM and page-table edit protection. Empty
/// subtrees are skipped by their own level, not by a stale VMA snapshot.
fn walk_leaf(mapper: &PageMapper, address: VirtAddr, end: VirtAddr, budget: &mut usize) -> Walk {
    let mut table: PageTable<MMArch> = mapper.table();
    loop {
        if *budget == 0 {
            return Walk::Budget;
        }
        *budget -= 1;
        let Some(index) = table.index_of(address) else {
            return Walk::Unsupported;
        };
        let Some(entry) = (unsafe { table.entry(index) }) else {
            return Walk::Unsupported;
        };
        if !entry.present() {
            let shift = MMArch::PAGE_SHIFT + table.level() * MMArch::PAGE_ENTRY_SHIFT;
            let span = 1usize << shift;
            let next = (address.data() & !(span - 1))
                .saturating_add(span)
                .min(end.data());
            return Walk::Hole(VirtAddr::new(next));
        }
        if MMArch::entry_is_leaf(table.level(), entry.flags().data()) {
            if table.level() != 0 {
                return Walk::Unsupported;
            }
            return match entry.address() {
                Ok(phys) => Walk::Leaf(phys, entry.flags()),
                Err(_) => Walk::Unsupported,
            };
        }
        let Some(next) = (unsafe { table.next_level_table(index) }) else {
            return Walk::Unsupported;
        };
        table = next;
    }
}

enum Stop {
    Continue,
    Busy,
    Incomplete,
}

impl AddressSpace {
    /// No sleeping resource lock, allocation, table retirement or backing
    /// eviction. All pins and empty metadata backings outlive the TLB barrier.
    pub(crate) fn try_oom_reap_batch(
        self: &Arc<Self>,
        authorization: Option<&OomReapAuthorization>,
        cursor: VirtAddr,
        expected_gen: u64,
    ) -> OomReapBatchResult {
        if authorization.is_none_or(|token| token.mm_id() != self.id()) {
            return OomReapBatchResult::Protected;
        }
        let mut pins: [Retirement; BATCH] = core::array::from_fn(|_| Retirement::default());
        let vma_pin;
        let mut unmapped = 0;
        let mut next;
        let mut flush_start: Option<VirtAddr> = None;
        let mut flush_end = cursor;
        let base_gen;
        let stop;
        {
            let Some(inner) = self.try_write() else {
                return OomReapBatchResult::Busy {
                    resume: cursor,
                    expected_gen,
                    unmapped: 0,
                };
            };
            let observed = self.tlb_gen.load(Ordering::SeqCst);
            if observed != expected_gen {
                return OomReapBatchResult::Restart {
                    expected_gen: observed,
                    unmapped: 0,
                };
            }
            if self.is_torn_down() {
                return OomReapBatchResult::PassEnd;
            }
            if inner.mappings.first_reservation_region().is_some() {
                return OomReapBatchResult::Busy {
                    resume: cursor,
                    expected_gen,
                    unmapped: 0,
                };
            }
            let Some(_pt_edit) = self.try_page_table_edit() else {
                return OomReapBatchResult::Busy {
                    resume: cursor,
                    expected_gen,
                    unmapped: 0,
                };
            };
            self.oom_unstable.store(true, Ordering::Release);
            // Select without contains_or_next(), which takes a sleeping VMA lock.
            let previous = inner
                .mappings
                .vmas_by_start
                .range(..=cursor)
                .next_back()
                .map(|(_, v)| v.clone());
            let candidate = previous.or_else(|| {
                inner
                    .mappings
                    .vmas_by_start
                    .range(cursor..)
                    .next()
                    .map(|(_, v)| v.clone())
            });
            let Some(mut candidate) = candidate else {
                return OomReapBatchResult::PassEnd;
            };
            let Some(mut vma_guard) = candidate.try_lock() else {
                return OomReapBatchResult::Busy {
                    resume: cursor,
                    expected_gen,
                    unmapped: 0,
                };
            };
            if vma_guard.region().end() <= cursor {
                drop(vma_guard);
                let following = inner
                    .mappings
                    .vmas_by_start
                    .range((
                        core::ops::Bound::Excluded(cursor),
                        core::ops::Bound::Unbounded,
                    ))
                    .next()
                    .map(|(_, v)| v.clone());
                let Some(following) = following else {
                    return OomReapBatchResult::PassEnd;
                };
                candidate = following;
                let Some(guard) = candidate.try_lock() else {
                    return OomReapBatchResult::Busy {
                        resume: cursor,
                        expected_gen,
                        unmapped: 0,
                    };
                };
                vma_guard = guard;
            }
            let region = *vma_guard.region();
            let vm_flags = *vma_guard.vm_flags();
            let selected = is_reapable_vma(vma_guard.vm_file().is_none(), vm_flags);
            next = VirtAddr::new(cursor.data().max(region.start().data()));
            vma_pin = candidate.clone();
            stop = if !selected {
                next = region.end();
                Stop::Continue
            } else {
                let mut budget = BATCH;
                let mut used = 0;
                loop {
                    if next >= region.end() || budget == 0 || used == BATCH {
                        break Stop::Continue;
                    }
                    let (phys, flags) =
                        match walk_leaf(&inner.user_mapper.utable, next, region.end(), &mut budget)
                        {
                            Walk::Hole(after) => {
                                next = after;
                                continue;
                            }
                            Walk::Budget => break Stop::Continue,
                            Walk::Unsupported => break Stop::Incomplete,
                            Walk::Leaf(phys, flags) => (phys, flags),
                        };
                    let Some(mut manager) = try_page_manager_lock() else {
                        break Stop::Busy;
                    };
                    let Some(page) = manager.get_registered(&phys) else {
                        break Stop::Incomplete;
                    };
                    let slot = &mut pins[used];
                    used += 1;
                    slot.page = Some(page.clone());
                    let Some(mut page_guard) = page.try_write() else {
                        break Stop::Busy;
                    };
                    let is_normal = matches!(page_guard.page_type(), PageType::Normal);
                    match page_guard.page_type() {
                        PageType::Normal => {}
                        PageType::File(info) => {
                            slot.cache = info.page_cache.upgrade();
                            if slot.cache.is_none() {
                                break Stop::Busy;
                            }
                            if flags.has_write() || flags.has_flag(MMArch::ENTRY_FLAG_DIRTY) {
                                break Stop::Incomplete;
                            }
                        }
                        _ => break Stop::Incomplete,
                    }
                    if !page_guard.has_vma(&candidate) {
                        break Stop::Incomplete;
                    }
                    let normal_frame = is_normal && page_guard.individually_owns_frame();
                    // Page exclusivity stabilizes PG_LRU. Ordinary anonymous
                    // pages need no reclaimer lock; only an LRU owner requires
                    // it. Acquire before changing this PTE so contention leaves
                    // the current mapping intact, while earlier batch entries
                    // still pass through the common TLB barrier below.
                    let mut reclaimer =
                        if normal_frame && page_guard.flags().contains(PageFlags::PG_LRU) {
                            let Some(reclaimer) = try_page_reclaimer_lock() else {
                                break Stop::Busy;
                            };
                            Some(reclaimer)
                        } else {
                            None
                        };
                    let cleared = unsafe {
                        inner
                            .user_mapper
                            .utable
                            .unmap_phys_preserve_tables_if(next, |p, f| {
                                p == phys && f.data() == flags.data()
                            })
                    };
                    let Some((_, _, flush)) = cleared else {
                        break Stop::Busy;
                    };
                    unsafe {
                        flush.ignore();
                    }
                    slot.rmap_backing = page_guard.remove_vma_for_oom(&candidate);
                    let still_unevictable = page_guard.intrinsic_unevictable()
                        || page_guard.has_mlocked_vma_refs()
                        || slot
                            .cache
                            .as_ref()
                            .is_some_and(|cache| cache.mapping_unevictable());
                    if !still_unevictable {
                        page_guard.remove_flags(PageFlags::PG_UNEVICTABLE);
                    }
                    if normal_frame {
                        slot.owners = detach_unmapped_page_locked(
                            &page,
                            &mut page_guard,
                            &mut manager,
                            reclaimer.as_deref_mut(),
                        );
                    }
                    self.account_present_page_sub();
                    unmapped += 1;
                    flush_start.get_or_insert(next);
                    next += MMArch::PAGE_SIZE;
                    flush_end = next;
                }
            };
            base_gen = self.tlb_gen.load(Ordering::SeqCst);
            // Guard drops precede TLB acknowledgement and every retirement.
        }
        let expected = if let Some(start) = flush_start {
            self.flush_tlb_range(start, flush_end, MMArch::PAGE_SHIFT as u8, false);
            base_gen.wrapping_add(1)
        } else {
            base_gen
        };
        let actual = self.tlb_gen.load(Ordering::SeqCst);
        for pin in pins {
            drop(pin);
        }
        drop(vma_pin);
        if actual != expected {
            return OomReapBatchResult::Restart {
                expected_gen: actual,
                unmapped,
            };
        }
        match stop {
            Stop::Continue => OomReapBatchResult::Continue {
                next,
                expected_gen: expected,
                unmapped,
            },
            Stop::Busy => OomReapBatchResult::Busy {
                resume: next,
                expected_gen: expected,
                unmapped,
            },
            Stop::Incomplete => OomReapBatchResult::Incomplete {
                resume: next,
                expected_gen: expected,
                unmapped,
            },
        }
    }
}
