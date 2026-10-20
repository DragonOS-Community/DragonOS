//! Real mapped-page ownership tests; failed fixtures are rescued only after
//! recording failure. Rescue is never evidence of successful retirement.
use crate::{
    arch::MMArch,
    cgroup::memory::{MemoryAccount, MemoryKind, MemoryStat},
    filesystem::page_cache::PageCache,
    libs::mutex::Mutex,
    mm::{
        allocator::page_frame::{PageFrameCount, VirtPageFrame},
        mmu_gather::MmuGather,
        page::{
            allocate_registered_data_page, detach_pages_from_manager_batched,
            detach_pages_from_reclaimer_batched, page_manager_lock, page_reclaimer_lock,
            retire_unmapped_page, EntryFlags, Page, PageFlags, PageType,
        },
        syscall::{MadvFlags, MapFlags, ProtFlags},
        ucontext::{oom_reap::OomReapBatchResult, AddressSpace},
        MemoryManagementArch, VirtAddr,
    },
    process::namespace::user_namespace::INIT_USER_NAMESPACE,
};
use alloc::{
    format,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    fmt::Write,
    sync::atomic::{AtomicU64, Ordering},
};
use system_error::SystemError;

static TEST_LOCK: Mutex<()> = Mutex::new(());
static NEXT_ACCOUNT: AtomicU64 = AtomicU64::new(0x0139_0000_0000_0000);

struct Fixture {
    mm: Arc<AddressSpace>,
    address: VirtAddr,
    count: usize,
    account: Arc<MemoryAccount>,
    pages: Vec<Weak<Page>>,
}

impl Fixture {
    fn new(count: usize, lru: bool) -> Result<Self, SystemError> {
        let mm = AddressSpace::new(false, INIT_USER_NAMESPACE.clone(), 1)
            .map_err(|_| SystemError::ENOMEM)?;
        let address = mm
            .map_anonymous_wait(
                VirtAddr::new(0),
                count * MMArch::PAGE_SIZE,
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS,
                true,
                false,
            )?
            .virt_address();
        let account = MemoryAccount::new(NEXT_ACCOUNT.fetch_add(1, Ordering::Relaxed), None)?;
        let mut fixture = Self {
            mm,
            address,
            count,
            account,
            pages: Vec::new(),
        };
        for index in 0..count {
            let charge = fixture
                .account
                .try_charge(MemoryKind::Anon, 1)
                .map_err(|_| SystemError::ENOMEM)?;
            let page = allocate_registered_data_page(
                PageType::Normal,
                if lru {
                    PageFlags::PG_LRU
                } else {
                    PageFlags::empty()
                },
                Some(charge),
            )?;
            fixture.pages.push(Arc::downgrade(&page));
            unsafe {
                page.write().as_slice_mut().fill(0);
            }
            attach(&fixture.mm, address + index * MMArch::PAGE_SIZE, &page)?;
        }
        Ok(fixture)
    }
    fn unmap(&self, first: usize, count: usize) -> bool {
        self.mm
            .munmap_wait(
                VirtPageFrame::new(self.address + first * MMArch::PAGE_SIZE),
                PageFrameCount::new(count),
            )
            .is_ok()
    }
    fn freed(&self) -> bool {
        self.pages.iter().all(|page| page.upgrade().is_none())
            && self.account.current_pages() == 0
            && lru_stats(&self.account) == 0
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.unmap(0, self.count);
        // Baseline-only rescue after the case has already captured its result.
        for page in self.pages.iter().filter_map(Weak::upgrade) {
            if page.read().map_count() != 0 {
                continue;
            }
            detach_pages_from_manager_batched(core::iter::once(&page));
            detach_pages_from_reclaimer_batched(core::iter::once(&page));
        }
    }
}

fn lru_stats(account: &MemoryAccount) -> u64 {
    let s = account.snapshot();
    s.stats[MemoryStat::InactiveAnon as usize] + s.stats[MemoryStat::ActiveAnon as usize]
}

fn attach(mm: &Arc<AddressSpace>, address: VirtAddr, page: &Arc<Page>) -> Result<(), SystemError> {
    let mut inner = mm.write();
    let vma = inner
        .mappings
        .contains(address)
        .ok_or(SystemError::EFAULT)?;
    let _pt = mm.page_table_edit();
    let flags =
        EntryFlags::<MMArch>::from_prot_flags(ProtFlags::PROT_READ | ProtFlags::PROT_WRITE, true)
            .set_write(true);
    let flush = unsafe {
        inner
            .user_mapper
            .utable
            .map_phys(address, page.phys_address(), flags)
    }
    .ok_or(SystemError::ENOMEM)?;
    vma.lock().set_mapped(true);
    page.write().insert_vma(vma, address, false);
    mm.account_present_page_add();
    flush.flush();
    Ok(())
}

fn detached(page: &Arc<Page>) -> bool {
    let manager_absent = page_manager_lock()
        .get_registered(&page.phys_address())
        .is_none();
    let lru_absent = page_reclaimer_lock().get(&page.phys_address()).is_none();
    manager_absent && lru_absent && !page.read().flags().contains(PageFlags::PG_LRU)
}

fn last_unmap() -> Result<bool, SystemError> {
    for lru in [false, true] {
        let f = Fixture::new(1, lru)?;
        if !f.unmap(0, 1) || !f.freed() {
            return Ok(false);
        }
    }
    let f = Fixture::new(1, true)?;
    f.pages[0]
        .upgrade()
        .unwrap()
        .write()
        .add_flags(PageFlags::PG_ACTIVE);
    Ok(f.unmap(0, 1) && f.freed())
}

fn partial_unmap() -> Result<bool, SystemError> {
    let f = Fixture::new(3, true)?;
    if !f.unmap(1, 1) || f.pages[1].upgrade().is_some() || f.account.current_pages() != 2 {
        return Ok(false);
    }
    let intact = {
        let space = f.mm.read();
        space.user_mapper.utable.translate(f.address).is_some()
            && space
                .user_mapper
                .utable
                .translate(f.address + 2 * MMArch::PAGE_SIZE)
                .is_some()
    };
    Ok(intact && f.unmap(0, 3) && f.freed())
}

fn madvise_range() -> Result<bool, SystemError> {
    let f = Fixture::new(3, true)?;
    for (index, weak) in f.pages.iter().enumerate() {
        unsafe {
            weak.upgrade().unwrap().write().as_slice_mut()[0] = 0x41 + index as u8;
        }
    }
    f.mm.madvise_wait(
        VirtPageFrame::new(f.address + MMArch::PAGE_SIZE),
        PageFrameCount::ONE,
        MadvFlags::MADV_DONTNEED,
    )?;
    let range_ok = {
        let inner = f.mm.read();
        inner
            .mappings
            .contains(f.address + MMArch::PAGE_SIZE)
            .is_some()
            && inner
                .user_mapper
                .utable
                .translate(f.address + MMArch::PAGE_SIZE)
                .is_none()
            && inner.user_mapper.utable.translate(f.address).is_some()
            && inner
                .user_mapper
                .utable
                .translate(f.address + 2 * MMArch::PAGE_SIZE)
                .is_some()
    };
    let ends_intact = [0usize, 2].into_iter().all(|index| {
        f.pages[index]
            .upgrade()
            .is_some_and(|p| unsafe { p.read().as_slice()[0] == 0x41 + index as u8 })
    });
    let middle_freed = f.pages[1].upgrade().is_none() && f.account.current_pages() == 2;
    Ok(range_ok && ends_intact && middle_freed && f.unmap(0, 3) && f.freed())
}

fn teardown(inactive: bool) -> Result<bool, SystemError> {
    let mut f = Fixture::new(2, true)?;
    if inactive {
        // Drop the actual mapped MM; a fresh empty replacement merely leaves
        // the fixture's failure cleanup available without unsafe field moves.
        let empty = AddressSpace::new(false, INIT_USER_NAMESPACE.clone(), 1)
            .map_err(|_| SystemError::ENOMEM)?;
        drop(core::mem::replace(&mut f.mm, empty));
        Ok(f.freed())
    } else {
        unsafe { f.mm.write().unmap_all() };
        Ok(f.freed())
    }
}

fn lifetime(which: usize) -> Result<bool, SystemError> {
    let f = Fixture::new(1, true)?;
    let page = f.pages[0].upgrade().unwrap();
    let pin = if which == 0 {
        Some(page.try_pin_access().map_err(|_| SystemError::EBUSY)?)
    } else {
        None
    };
    let extra = if which == 1 { Some(page.clone()) } else { None };
    let snapshot = if which == 2 {
        let mut reclaimer = page_reclaimer_lock();
        let mut remaining = reclaimer.lru.len();
        let mut found = None;
        while remaining > 0 && found.is_none() {
            let slots = reclaimer.reclaim_snapshot(remaining.min(64));
            remaining = remaining.saturating_sub(64);
            for candidate in slots.into_iter().flatten() {
                if Arc::ptr_eq(&candidate, &page) {
                    found = Some(candidate);
                }
            }
        }
        found
    } else {
        None
    };
    if which == 2 && snapshot.is_none() {
        return Ok(false);
    }
    drop(page);
    if !f.unmap(0, 1) {
        return Ok(false);
    }
    let live = f.pages[0].upgrade().unwrap();
    let kept = detached(&live) && f.account.current_pages() == 1 && lru_stats(&f.account) == 0;
    retire_unmapped_page(&live);
    retire_unmapped_page(&live);
    let idempotent = detached(&live) && lru_stats(&f.account) == 0;
    drop(live);
    drop(pin);
    drop(extra);
    drop(snapshot);
    Ok(kept && idempotent && f.freed())
}

fn two_mm(lru: bool, first_finishes_early: bool) -> Result<bool, SystemError> {
    let a = Fixture::new(1, lru)?;
    let b = AddressSpace::new(false, INIT_USER_NAMESPACE.clone(), 1)
        .map_err(|_| SystemError::ENOMEM)?;
    let address = b
        .map_anonymous_wait(
            VirtAddr::new(0),
            MMArch::PAGE_SIZE,
            ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
            MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS,
            true,
            false,
        )?
        .virt_address();
    let page = a.pages[0].upgrade().unwrap();
    attach(&b, address, &page)?;
    drop(page);
    let mut ga = MmuGather::gather(&a.mm);
    {
        let mut inner = a.mm.write();
        let _pt = a.mm.page_table_edit();
        let vma = inner.mappings.contains(a.address).unwrap();
        vma.unmap(&mut inner.user_mapper.utable, &mut ga);
    }
    if first_finishes_early {
        ga.finish();
        let page = a.pages[0].upgrade().unwrap();
        if page_manager_lock()
            .get_registered(&page.phys_address())
            .is_none()
            || page.read().map_count() != 1
            || a.account.current_pages() != 1
        {
            return Ok(false);
        }
        drop(page);
        let done = b
            .munmap_wait(VirtPageFrame::new(address), PageFrameCount::ONE)
            .is_ok();
        return Ok(done && a.freed());
    }
    let done = b
        .munmap_wait(VirtPageFrame::new(address), PageFrameCount::ONE)
        .is_ok();
    let page = a.pages[0].upgrade();
    let pinned_until_a = page.as_ref().is_some_and(detached) && a.account.current_pages() == 1;
    drop(page);
    ga.finish();
    Ok(done && pinned_until_a && a.freed())
}

fn backing_negative(shm: bool) -> Result<bool, SystemError> {
    let account = MemoryAccount::new(NEXT_ACCOUNT.fetch_add(1, Ordering::Relaxed), None)?;
    let charge = account
        .try_charge(
            if shm {
                MemoryKind::Shmem
            } else {
                MemoryKind::Anon
            },
            1,
        )
        .map_err(|_| SystemError::ENOMEM)?;
    let page = allocate_registered_data_page(
        if shm { PageType::Shm } else { PageType::Normal },
        PageFlags::PG_LRU,
        Some(charge),
    )?;
    page.write().add_backing_lifetime_pin();
    retire_unmapped_page(&page);
    let kept = page_manager_lock()
        .get_registered(&page.phys_address())
        .is_some()
        && page_reclaimer_lock().get(&page.phys_address()).is_some()
        && account.current_pages() == 1;
    page.write().remove_backing_lifetime_pin();
    // Clean private fixture through explicit normal registry authorities.
    detach_pages_from_manager_batched(core::iter::once(&page));
    detach_pages_from_reclaimer_batched(core::iter::once(&page));
    drop(page);
    Ok(kept && account.current_pages() == 0)
}

fn cache_negative(shmem: bool) -> Result<bool, SystemError> {
    let cache = if shmem {
        PageCache::new_shmem(None, None)
    } else {
        PageCache::new_unowned(None, None)
    };
    let page = cache.get_or_create_page_zero(0)?;
    retire_unmapped_page(&page);
    let kept = page_manager_lock()
        .get_registered(&page.phys_address())
        .is_some()
        && cache
            .manager()
            .get_page_any(0)
            .is_some_and(|p| Arc::ptr_eq(&p, &page));
    let _ = cache.manager().remove_page(0)?;
    detach_pages_from_manager_batched(core::iter::once(&page));
    detach_pages_from_reclaimer_batched(core::iter::once(&page));
    Ok(kept)
}

fn unevictable_negative() -> Result<bool, SystemError> {
    let page = allocate_registered_data_page(PageType::Normal, PageFlags::PG_UNEVICTABLE, None)?;
    retire_unmapped_page(&page);
    let kept = page_manager_lock()
        .get_registered(&page.phys_address())
        .is_some()
        && page.read().intrinsic_unevictable();
    detach_pages_from_manager_batched(core::iter::once(&page));
    Ok(kept)
}

fn oom_plain_ignores_lru_contention() -> Result<bool, SystemError> {
    let f = Fixture::new(1, false)?;
    let authorization = crate::mm::oom::OomReapAuthorization::test_authorization(f.mm.id());
    let reclaimer = page_reclaimer_lock();
    let result = f.mm.try_oom_reap_batch(
        Some(&authorization),
        f.address,
        f.mm.tlb_gen.load(Ordering::SeqCst),
    );
    drop(reclaimer);
    Ok(matches!(
        result,
        OomReapBatchResult::Continue { unmapped: 1, .. }
            | OomReapBatchResult::Incomplete { unmapped: 1, .. }
    ) && f.freed())
}

fn oom_partial_batch_busy() -> Result<bool, SystemError> {
    let f = Fixture::new(2, true)?;
    let second = f.address + MMArch::PAGE_SIZE;
    let before =
        f.mm.read()
            .user_mapper
            .utable
            .translate(second)
            .map(|(phys, flags)| (phys, flags.data()));
    let generation = f.mm.tlb_gen.load(Ordering::SeqCst);
    let authorization = crate::mm::oom::OomReapAuthorization::test_authorization(f.mm.id());
    let page = f.pages[1].upgrade().unwrap();
    let guard = page.write();
    let result =
        f.mm.try_oom_reap_batch(Some(&authorization), f.address, generation);
    let OomReapBatchResult::Busy {
        unmapped: 1,
        resume,
        expected_gen,
    } = result
    else {
        return Ok(false);
    };
    let after = {
        let inner = f.mm.read();
        if inner.user_mapper.utable.translate(f.address).is_some() {
            return Ok(false);
        }
        inner
            .user_mapper
            .utable
            .translate(second)
            .map(|(phys, flags)| (phys, flags.data()))
    };
    let untouched = before == after
        && guard.map_count() == 1
        && guard.flags().contains(PageFlags::PG_LRU)
        && page_manager_lock()
            .get_registered(&page.phys_address())
            .is_some()
        && page_reclaimer_lock().get(&page.phys_address()).is_some();
    drop(guard);
    drop(page);
    if resume != second
        || expected_gen != generation.wrapping_add(1)
        || expected_gen != f.mm.tlb_gen.load(Ordering::SeqCst)
        || f.pages[0].upgrade().is_some()
        || f.account.current_pages() != 1
        || f.mm.resident_pages() != 1
        || !untouched
    {
        return Ok(false);
    }
    let retried =
        f.mm.try_oom_reap_batch(Some(&authorization), resume, expected_gen);
    Ok(matches!(retried, OomReapBatchResult::Continue { unmapped: 1, .. }) && f.freed())
}

fn oom(busy: bool) -> Result<bool, SystemError> {
    let f = Fixture::new(1, true)?;
    let authorization = crate::mm::oom::OomReapAuthorization::test_authorization(f.mm.id());
    let generation = f.mm.tlb_gen.load(Ordering::SeqCst);
    if busy {
        let before =
            f.mm.read()
                .user_mapper
                .utable
                .translate(f.address)
                .map(|(phys, flags)| (phys, flags.data()));
        let reclaimer = page_reclaimer_lock();
        let result =
            f.mm.try_oom_reap_batch(Some(&authorization), f.address, generation);
        drop(reclaimer);
        let after =
            f.mm.read()
                .user_mapper
                .utable
                .translate(f.address)
                .map(|(phys, flags)| (phys, flags.data()));
        if !matches!(result, OomReapBatchResult::Busy { unmapped: 0, .. })
            || before != after
            || f.account.current_pages() != 1
            || f.pages[0].upgrade().unwrap().read().map_count() != 1
        {
            return Ok(false);
        }
    }
    let result = f.mm.try_oom_reap_batch(
        Some(&authorization),
        f.address,
        f.mm.tlb_gen.load(Ordering::SeqCst),
    );
    Ok(matches!(
        result,
        OomReapBatchResult::Continue { unmapped: 1, .. }
            | OomReapBatchResult::Incomplete { unmapped: 1, .. }
    ) && f.freed())
}

pub(crate) fn report() -> String {
    let _serial = TEST_LOCK.lock();
    type Check = fn() -> Result<bool, SystemError>;
    let tests: &[(&str, Check)] = &[
        ("last_unmap", last_unmap),
        ("partial_unmap", partial_unmap),
        ("madvise_range", madvise_range),
        ("teardown", || teardown(false)),
        ("inactive_teardown", || teardown(true)),
        ("two_mm_interleaved_lru", || two_mm(true, false)),
        ("two_mm_interleaved_plain", || two_mm(false, false)),
        ("last_mapping_only", || {
            let lru = two_mm(true, true)?;
            let plain = two_mm(false, true)?;
            Ok(lru && plain)
        }),
        ("access_pin_lifetime", || lifetime(0)),
        ("extra_arc_lifetime", || lifetime(1)),
        ("reclaimer_snapshot_lifetime", || lifetime(2)),
        ("file_backing_preserved", || cache_negative(false)),
        ("shmem_backing_preserved", || cache_negative(true)),
        ("shm_backing_pin", || backing_negative(true)),
        ("normal_backing_pin", || backing_negative(false)),
        ("unevictable_preserved", unevictable_negative),
        ("oom_lru", || oom(false)),
        ("oom_reclaimer_busy", || oom(true)),
        (
            "oom_plain_ignores_lru_contention",
            oom_plain_ignores_lru_contention,
        ),
        ("oom_partial_batch_busy", oom_partial_batch_busy),
    ];
    let mut report = String::new();
    let mut failures = 0;
    for (name, test) in tests {
        let result = test();
        let ok = matches!(result, Ok(true));
        failures += usize::from(!ok);
        let _ = writeln!(report, "{name}={}", if ok { "ok" } else { "fail" });
        if let Err(error) = result {
            let _ = writeln!(report, "{name}_error={error:?}");
        }
    }
    format!(
        "status={}\n{report}summary_fail={failures}\n",
        if failures == 0 { "ok" } else { "fail" }
    )
}
