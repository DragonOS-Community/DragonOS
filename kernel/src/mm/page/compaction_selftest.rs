//! Deterministic compaction inputs built from real global Buddy frames.
//! Only fixture setup chooses PFNs. Allocation, isolation, migration and final
//! frame ownership use the production paths, with ordinary PTEs and charges.
use super::*;
use crate::{
    arch::CurrentIrqArch,
    cgroup::memory::accounting::MemoryPolicy,
    exception::InterruptArch,
    mm::{
        allocator::page_frame::VirtPageFrame,
        compaction::{try_allocate_in_range, Compactor, Isolation, Limits, Request, Stop},
        dma::{DmaAllocOptions, DmaBuffer, DmaCompaction},
        syscall::{MapFlags, ProtFlags},
        ucontext::AddressSpace,
    },
    process::namespace::user_namespace::INIT_USER_NAMESPACE,
};
use core::fmt::Write;

const ORDER: usize = 4;
const TARGET: usize = 1 << ORDER;
const BACKING: usize = 2 * TARGET;
const SOURCES: usize = BACKING / 2;
static TEST_LOCK: Mutex<()> = Mutex::new(());
static NEXT_ACCOUNT: AtomicUsize = AtomicUsize::new(0x7000_0000);

struct Source {
    physical: PhysAddr,
    address: VirtAddr,
    weak: Weak<Page>,
}

struct Fixture {
    mm: Arc<AddressSpace>,
    account: Arc<MemoryAccount>,
    address: VirtAddr,
    alias: Option<VirtAddr>,
    base: Option<PhysAddr>,
    held: [bool; BACKING],
    removed: [bool; SOURCES],
    sources: Vec<Source>,
    unknown: bool,
}

// Source births need the same manager/LRU capacity discipline as ordinary
// births, but take ownership of a frame already acquired by this fixture.
fn register(page: &Arc<Page>) -> Result<(), SystemError> {
    for _ in 0..128 {
        let mut prepared_manager = prepare_manager_capacity()?;
        let mut prepared_lru = prepare_lru_capacity()?;
        let mut retired_manager = None;
        let mut retired_lru = None;
        let installed = {
            let mut manager = page_manager_lock();
            let mut reclaimer = page_reclaimer_lock();
            if manager.phys2page.len() == manager.phys2page.capacity()
                && prepared_manager
                    .as_ref()
                    .is_some_and(|table| table.capacity() > manager.phys2page.len())
            {
                let mut table = prepared_manager.take().unwrap();
                for (key, value) in manager.phys2page.drain() {
                    assert!(table.insert(key, value).is_none());
                }
                retired_manager = Some(mem::replace(&mut manager.phys2page, table));
            }
            if reclaimer.lru.len() == reclaimer.lru.capacity()
                && prepared_lru
                    .as_ref()
                    .is_some_and(|table| table.capacity() > reclaimer.lru.len())
            {
                retired_lru = Some(reclaimer.lru.install_capacity(prepared_lru.take().unwrap()));
            }
            if manager.phys2page.len() == manager.phys2page.capacity()
                || reclaimer.lru.len() == reclaimer.lru.capacity()
            {
                false
            } else {
                assert!(manager
                    .phys2page
                    .insert(page.phys_address(), page.clone())
                    .is_none());
                reclaimer
                    .lru
                    .insert_prepared(page.phys_address(), page)
                    .expect("prepared fixture LRU birth");
                true
            }
        };
        drop(retired_lru);
        drop(retired_manager);
        if installed {
            return Ok(());
        }
    }
    Err(SystemError::EBUSY)
}

fn pattern(source: usize, offset: usize) -> u8 {
    (source.wrapping_mul(19) ^ offset.wrapping_mul(37) ^ (offset >> 5)) as u8
}

impl Fixture {
    fn new(alias: bool, unknown: bool) -> Result<Self, SystemError> {
        Self::with_range(alias, unknown, None)
    }

    fn with_range(
        alias: bool,
        unknown: bool,
        range: Option<(PhysAddr, PhysAddr)>,
    ) -> Result<Self, SystemError> {
        let mut phase = "fixture";
        let mut range_error = None;
        Self::with_range_detail(alias, unknown, range, &mut phase, &mut range_error)
    }

    fn with_range_detail(
        alias: bool,
        unknown: bool,
        range: Option<(PhysAddr, PhysAddr)>,
        phase: &mut &'static str,
        range_error: &mut Option<Stop>,
    ) -> Result<Self, SystemError> {
        *phase = "fixture_mm_birth";
        let mm = AddressSpace::new(false, INIT_USER_NAMESPACE.clone(), 1)
            .map_err(|_| SystemError::ENOMEM)?;
        *phase = "fixture_account_birth";
        let account =
            MemoryAccount::new(NEXT_ACCOUNT.fetch_add(1, Ordering::Relaxed) as u64, None)?;
        account.set_policy(MemoryPolicy {
            max: Some(SOURCES as u64),
            ..MemoryPolicy::default()
        });
        *phase = "fixture_vma";
        let address = mm
            .map_anonymous_wait(
                VirtAddr::new(0),
                SOURCES * MMArch::PAGE_SIZE,
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS,
                true,
                false,
            )?
            .virt_address();
        *phase = "fixture_alias_vma";
        let alias_address = if alias {
            Some(
                mm.map_anonymous_wait(
                    VirtAddr::new(0),
                    MMArch::PAGE_SIZE,
                    ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                    MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS,
                    true,
                    false,
                )?
                .virt_address(),
            )
        } else {
            None
        };
        let mut fixture = Self {
            mm,
            account,
            address,
            alias: alias_address,
            base: None,
            held: [false; BACKING],
            removed: [false; SOURCES],
            sources: Vec::new(),
            unknown,
        };
        *phase = "fixture_source_metadata";
        fixture
            .sources
            .try_reserve_exact(SOURCES)
            .map_err(|_| SystemError::ENOMEM)?;
        *phase = "fixture_backing_run";
        let (base, count) = if let Some((start, end)) = range {
            let mut entries = 65_536;
            try_allocate_in_range(start, end, ORDER + 1, &mut entries)
                .map_err(|error| {
                    *range_error = Some(error);
                    SystemError::ENOMEM
                })?
                .ok_or_else(|| {
                    *phase = "fixture_backing_unavailable";
                    SystemError::ENOMEM
                })?
                .into_raw_parts()
        } else {
            unsafe {
                LockedFrameAllocator
                    .allocate_below(PageFrameCount::new(BACKING), PhysAddr::new(usize::MAX))
            }
            .ok_or(SystemError::ENOMEM)?
        };
        if count.data() != BACKING {
            unsafe { LockedFrameAllocator.free(base, count) };
            return Err(SystemError::EIO);
        }
        fixture.base = Some(base);
        fixture.held.fill(true);
        for index in 0..SOURCES {
            if unknown && index == 0 {
                continue;
            }
            let physical = base + 2 * index * MMArch::PAGE_SIZE;
            let address = fixture.address + index * MMArch::PAGE_SIZE;
            *phase = "fixture_source_charge";
            let charge = fixture
                .account
                .try_charge(MemoryKind::Anon, 1)
                .map_err(|_| SystemError::ENOMEM)?;
            // The Page constructor consumes its frame even if Arc allocation fails.
            fixture.held[2 * index] = false;
            *phase = "fixture_source_page";
            let page = Page::try_new_with_charge(
                physical,
                PageType::Normal,
                PageFlags::PG_LRU,
                Some(charge),
            )?;
            {
                let mut data = page.write();
                for (offset, byte) in unsafe { data.as_slice_mut() }.iter_mut().enumerate() {
                    *byte = pattern(index, offset);
                }
            }
            fixture.sources.push(Source {
                physical,
                address,
                weak: Arc::downgrade(&page),
            });
            *phase = "fixture_source_register";
            register(&page)?;
            *phase = "fixture_source_mapping";
            fixture.attach(address, &page)?;
            if index == 0 {
                if let Some(alias) = fixture.alias {
                    fixture.attach(alias, &page)?;
                }
            }
        }
        Ok(fixture)
    }

    fn attach(&self, address: VirtAddr, page: &Arc<Page>) -> Result<(), SystemError> {
        let mut space = self.mm.write();
        let vma = space
            .mappings
            .contains(address)
            .ok_or(SystemError::EFAULT)?;
        let _pt = self.mm.page_table_edit();
        let flags = EntryFlags::<MMArch>::from_prot_flags(
            ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
            true,
        )
        .set_write(true)
        .set_dirty(true)
        .set_access(true);
        let flush = unsafe {
            space
                .user_mapper
                .utable
                .map_phys(address, page.phys_address(), flags)
        }
        .ok_or(SystemError::ENOMEM)?;
        vma.lock().set_mapped(true);
        page.write().insert_vma(vma, address, false);
        self.mm.account_present_page_add();
        flush.flush();
        Ok(())
    }

    // Call only after the fixture metadata exists; compaction itself stays cold.
    fn release_holes(&mut self) {
        let base = self.base.unwrap();
        for index in (1..BACKING).step_by(2) {
            self.held[index] = false;
            unsafe { LockedFrameAllocator.free_one(base + index * MMArch::PAGE_SIZE) };
        }
    }

    fn request(&self) -> Request {
        let start = self.base.unwrap();
        Request {
            start,
            end: start + BACKING * MMArch::PAGE_SIZE,
            order: ORDER,
            start_at: None,
            deadline: None,
            limits: Limits::default(),
        }
    }

    fn source_page(&self, index: usize) -> Option<Arc<Page>> {
        let address = self.address + index * MMArch::PAGE_SIZE;
        let physical = self.mm.read().user_mapper.utable.translate(address)?.0;
        page_manager_lock().get_registered(&physical)
    }

    fn valid(&self) -> bool {
        let mut expected: usize = 0;
        for source in &self.sources {
            let index = (source.address.data() - self.address.data()) / MMArch::PAGE_SIZE;
            if self.removed[index] {
                continue;
            }
            expected += 1;
            let Some(page) = self.source_page(index) else {
                return false;
            };
            let data = page.read();
            if page
                .memory_account()
                .is_none_or(|account| !Arc::ptr_eq(account, &self.account))
                || !data.flags().contains(PageFlags::PG_LRU)
                || data.map_count()
                    != if self.alias.is_some() && index == 0 {
                        2
                    } else {
                        1
                    }
                || !unsafe { data.as_slice() }
                    .iter()
                    .enumerate()
                    .all(|(offset, byte)| *byte == pattern(index, offset))
            {
                return false;
            }
        }
        self.account.current_pages() == expected as u64
            && self.account.snapshot().anon == expected as u64
            && self.account.snapshot().peak == (SOURCES - usize::from(self.unknown)) as u64
            && self.mm.resident_pages()
                == expected + usize::from(self.alias.is_some() && !self.removed[0])
    }

    fn finish(self, passed: bool) -> bool {
        let account = self.account.clone();
        drop(self);
        passed && account.current_pages() == 0
    }

    fn retire_first_target(&mut self) -> bool {
        if self.alias.is_some() {
            return false;
        }
        if self
            .mm
            .munmap_wait(
                VirtPageFrame::new(self.address),
                PageFrameCount::new(TARGET / 2),
            )
            .is_err()
        {
            return false;
        }
        if self.unknown && self.held[0] {
            // The unregistered blocker was always this raw owner's frame.
            // Release it normally only after all managed mappings are gone.
            self.held[0] = false;
            unsafe { LockedFrameAllocator.free_one(self.base.unwrap()) };
        }
        self.removed[..TARGET / 2].fill(true);
        true
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(alias) = self.alias {
            let _ = self
                .mm
                .munmap_wait(VirtPageFrame::new(alias), PageFrameCount::ONE);
        }
        let _ = self.mm.munmap_wait(
            VirtPageFrame::new(self.address),
            PageFrameCount::new(SOURCES),
        );
        for source in &self.sources {
            if let Some(page) = source.weak.upgrade() {
                if page.read().map_count() == 0 {
                    retire_unmapped_page(&page);
                }
            }
        }
        if let Some(base) = self.base {
            for (index, held) in self.held.iter().enumerate() {
                if *held {
                    unsafe { LockedFrameAllocator.free_one(base + index * MMArch::PAGE_SIZE) };
                }
            }
        }
    }
}

fn stats_bounded(stats: &crate::mm::compaction::Stats, limits: &Limits) -> bool {
    stats.targets <= limits.targets
        && stats.entries <= limits.entries
        && stats.pages <= limits.pages
        && stats.migrations <= limits.migrations
        && stats.moved <= stats.migrations
}

fn real_fragmented_capture() -> Result<bool, SystemError> {
    let mut fixture = Fixture::new(false, false)?;
    let mut compactor = Compactor::new();
    let request = fixture.request();
    let mut entries = request.limits.entries;
    fixture.release_holes();
    // Measure actual eligible free ownership in the entire 2N domain, not
    // guest-wide counters or merely the number of free calls made by setup.
    let domain =
        Isolation::begin(request.start, ORDER + 1, &mut entries).map_err(|_| SystemError::EBUSY)?;
    let enough_free = domain.free_pages() == TARGET;
    drop(domain);
    entries = request.limits.entries;
    let before = try_allocate_in_range(request.start, request.end, request.order, &mut entries);
    if !matches!(before, Ok(None)) {
        return Ok(false);
    }
    let attempt = compactor.allocate(request);
    let bounded = stats_bounded(&attempt.stats, &request.limits);
    let moved = attempt.stats.moved;
    let Ok(frames) = attempt.outcome else {
        return Ok(false);
    };
    let start = frames.start();
    let end = start + frames.pages() * MMArch::PAGE_SIZE;
    let mut checked = enough_free
        && moved > 0
        && bounded
        && frames.pages() == TARGET
        && start >= request.start
        && end <= request.end
        && start.data().is_multiple_of(TARGET * MMArch::PAGE_SIZE)
        && fixture.valid();
    for source in &fixture.sources {
        if (start..end).contains(&source.physical) {
            let index = (source.address.data() - fixture.address.data()) / MMArch::PAGE_SIZE;
            checked &= source.weak.upgrade().is_none()
                && fixture
                    .source_page(index)
                    .is_some_and(|page| !(start..end).contains(&page.phys_address()));
        }
    }
    let direct = unsafe { MMArch::phys_2_virt(start) }.ok_or(SystemError::EFAULT)?;
    let bytes = unsafe {
        core::slice::from_raw_parts_mut(direct.as_ptr::<u8>(), TARGET * MMArch::PAGE_SIZE)
    };
    bytes.fill(0x5a);
    checked &= bytes.iter().all(|byte| *byte == 0x5a);
    drop(frames);
    let valid = checked && fixture.valid();
    Ok(fixture.finish(valid))
}

fn partial_failure(pin: bool) -> Result<bool, SystemError> {
    let mut fixture = Fixture::new(false, false)?;
    let mut compactor = Compactor::new();
    let mut request = fixture.request();
    request.limits.targets = 1;
    request.limits.no_progress = 1;
    if !pin {
        request.limits.migrations = 1;
    }
    let held = if pin {
        Some(
            fixture
                .source_page(1)
                .ok_or(SystemError::EFAULT)?
                .try_pin_access()
                .map_err(|_| SystemError::EBUSY)?,
        )
    } else {
        None
    };
    fixture.release_holes();
    let attempt = compactor.allocate(request);
    let expected_stop = if pin { Stop::Busy } else { Stop::Budget };
    let mut passed = matches!(attempt.outcome, Err(stop) if stop == expected_stop)
        && attempt.stats.moved > 0
        && stats_bounded(&attempt.stats, &request.limits)
        && fixture.valid()
        && fixture.sources[0].weak.upgrade().is_none();
    drop(held);
    if pin {
        // Transient pin contention must not poison this service's defer state.
        let retry = compactor.allocate(request);
        passed &= retry.outcome.is_ok()
            && stats_bounded(&retry.stats, &request.limits)
            && fixture.valid();
        drop(retry);
    }
    Ok(fixture.finish(passed))
}

fn blocked_unknown() -> Result<bool, SystemError> {
    let mut fixture = Fixture::new(false, true)?;
    let mut compactor = Compactor::new();
    let mut request = fixture.request();
    request.limits.targets = 1;
    request.limits.no_progress = 1;
    fixture.release_holes();
    let attempt = compactor.allocate(request);
    let passed = matches!(attempt.outcome, Err(Stop::Blocked))
        && attempt.stats.moved == 0
        && stats_bounded(&attempt.stats, &request.limits)
        && fixture.valid();
    Ok(fixture.finish(passed))
}

fn rmap_limit() -> Result<bool, SystemError> {
    let mut fixture = Fixture::new(true, false)?;
    let mut compactor = Compactor::new();
    let mut request = fixture.request();
    request.limits.targets = 1;
    request.limits.rmaps = 1;
    fixture.release_holes();
    let attempt = compactor.allocate(request);
    let passed = attempt.outcome.is_err()
        && attempt.stats.moved == 0
        && fixture.valid()
        && fixture.sources[0].weak.upgrade().is_some()
        && stats_bounded(&attempt.stats, &request.limits);
    Ok(fixture.finish(passed))
}

fn deferred_allocation_fastpath() -> Result<(bool, alloc::string::String), SystemError> {
    let mut fixture = Fixture::new(false, true)?;
    let mut compactor = Compactor::new();
    let mut request = fixture.request();
    request.limits.targets = 1;
    request.limits.no_progress = 1;
    fixture.release_holes();
    let first = compactor.allocate(request);
    if !matches!(first.outcome, Err(Stop::Blocked)) || first.stats.moved != 0 {
        let passed = fixture.finish(false);
        return Ok((
            passed,
            alloc::format!(
                "phase=first stop={:?} stats={:?}",
                first.outcome.as_ref().err(),
                first.stats
            ),
        ));
    }
    // A different candidate in the same domain must retain deferral. The
    // second half is genuinely migratable; resetting on seed would move it.
    request.start_at = Some(request.start + TARGET * MMArch::PAGE_SIZE);
    let deferred = compactor.allocate(request);
    let deferred_ok = matches!(deferred.outcome, Err(Stop::Deferred))
        && deferred.stats.targets == 0
        && deferred.stats.moved == 0;
    // munmap/split metadata is allowed to allocate. Protect existing holes
    // during that operation, and collect the retiring pages through the real
    // Buddy quarantine, instead of assuming free calls establish ownership.
    let mut entries = request.limits.entries;
    let mut retirement =
        Isolation::begin(request.start, ORDER, &mut entries).map_err(|_| SystemError::EBUSY)?;
    if !fixture.retire_first_target() {
        drop(retirement);
        let passed = fixture.finish(false);
        return Ok((
            passed,
            alloc::format!(
                "phase=retire first={:?}/{:?} deferred={:?}/{:?}",
                first.outcome.as_ref().err(),
                first.stats,
                deferred.outcome.as_ref().err(),
                deferred.stats
            ),
        ));
    }
    let retired_free = retirement.free_pages();
    let Some(ready_run) = retirement.capture() else {
        drop(retirement);
        let passed = fixture.finish(false);
        return Ok((
            passed,
            alloc::format!("phase=capture retired_free={retired_free}"),
        ));
    };
    let complete =
        retired_free == TARGET && ready_run.start() == request.start && ready_run.pages() == TARGET;
    drop(retirement);
    drop(ready_run);
    let ready = compactor.allocate(request);
    // The freed first target differs from the second-half seed: ordinary
    // allocation must still search the whole original domain before defer.
    let valid = fixture.valid();
    let passed = deferred_ok
        && complete
        && ready
            .outcome
            .as_ref()
            .is_ok_and(|run| run.start() == request.start)
        && ready.stats.migrations == 0
        && valid;
    // Capture scalars only. Formatting occurs after all fixture owners retire,
    // so diagnostics cannot allocate from the holes under investigation.
    let ready_stop = ready.outcome.as_ref().err().copied();
    let ready_start = ready.outcome.as_ref().ok().map(|run| run.start());
    let ready_stats = ready.stats;
    let expected_start = request.start;
    drop(ready);
    let account = fixture.account.clone();
    let passed = fixture.finish(passed);
    let released = account.current_pages() == 0;
    let detail = alloc::format!(
        "phase=complete first={:?}/{:?} deferred={:?}/{:?} ready={:?}/{:?} run={:?} expected={:?} retired_free={} valid={} released={}",
        first.outcome.as_ref().err(), first.stats,
        deferred.outcome.as_ref().err(), deferred.stats,
        ready_stop, ready_stats, ready_start, expected_start, retired_free, valid, released,
    );
    Ok((passed, detail))
}

fn small_budget() -> Result<bool, SystemError> {
    let mut fixture = Fixture::new(false, false)?;
    let mut compactor = Compactor::new();
    let mut request = fixture.request();
    request.limits.entries = 1;
    fixture.release_holes();
    let attempt = compactor.allocate(request);
    let passed = matches!(attempt.outcome, Err(Stop::Budget))
        && attempt.stats.moved == 0
        && stats_bounded(&attempt.stats, &request.limits)
        && fixture.valid()
        && fixture
            .sources
            .iter()
            .all(|source| source.weak.upgrade().is_some());
    Ok(fixture.finish(passed))
}

fn isolation_allocation_free_window() -> Result<bool, SystemError> {
    let mut fixture = Fixture::new(false, false)?;
    let request = fixture.request();
    let mut entries = request.limits.entries;
    fixture.release_holes();
    let mut lease =
        Isolation::begin(request.start, ORDER, &mut entries).map_err(|_| SystemError::EBUSY)?;
    let initial_free = lease.free_pages();
    let second_busy = matches!(
        Isolation::begin(request.start, ORDER, &mut entries),
        Err(Stop::Busy)
    );
    let allocated = unsafe { LockedFrameAllocator.allocate_one() }.ok_or(SystemError::ENOMEM)?;
    let outside =
        allocated < request.start || allocated >= request.start + TARGET * MMArch::PAGE_SIZE;
    unsafe { LockedFrameAllocator.free_one(allocated) };
    let unmapped = fixture
        .mm
        .munmap_wait(VirtPageFrame::new(fixture.address), PageFrameCount::ONE)
        .is_ok();
    if unmapped {
        fixture.removed[0] = true;
    }
    let intercepted = lease.is_free(request.start)
        && lease.free_pages() == initial_free + 1
        && fixture.sources[0].weak.upgrade().is_none();
    let incomplete = lease.capture().is_none();
    drop(lease);
    let passed = initial_free == TARGET / 2
        && second_busy
        && outside
        && unmapped
        && intercepted
        && incomplete
        && fixture.valid();
    Ok(fixture.finish(passed))
}

fn atomic_rejected() -> bool {
    let mut compactor = Compactor::new();
    let request = Request {
        start: PhysAddr::new(0),
        end: PhysAddr::new(32 * MMArch::PAGE_SIZE),
        order: ORDER,
        start_at: None,
        deadline: None,
        limits: Limits::default(),
    };
    let irq = unsafe { CurrentIrqArch::save_and_disable_irq() };
    let attempt = compactor.allocate(request);
    drop(irq);
    matches!(attempt.outcome, Err(Stop::Busy))
        && attempt.stats.targets == 0
        && attempt.stats.entries == 0
        && attempt.stats.pages == 0
        && attempt.stats.migrations == 0
}

// An expired real deadline must not consume ownership or install backoff.
fn deadline_rollback() -> Result<bool, SystemError> {
    let mut fixture = Fixture::new(false, false)?;
    fixture.release_holes();
    let mut compactor = Compactor::new();
    let mut request = fixture.request();
    request.deadline = Some(crate::time::timer::clock());
    let expired = compactor.allocate(request);
    let untouched = matches!(expired.outcome, Err(Stop::Timeout))
        && expired.stats.targets == 0
        && expired.stats.entries == 0
        && expired.stats.migrations == 0
        && fixture.valid()
        && fixture
            .sources
            .iter()
            .all(|source| source.weak.upgrade().is_some());
    // Same owner/domain must still work immediately, without failure deferral.
    request.deadline = None;
    let captured = compactor.allocate(request);
    let recovered = captured.outcome.is_ok() && fixture.valid();
    drop(captured);
    Ok(fixture.finish(untouched && recovered))
}

fn dma_recovery_reserve_guard() -> bool {
    use crate::mm::allocator::reserve::{kernel_level, with_reclaim_reserve, ReserveLevel};
    let before = kernel_level();
    let ordinary = crate::mm::compaction::recovery::context_allowed();
    let denied = with_reclaim_reserve(|| !crate::mm::compaction::recovery::context_allowed());
    before == ReserveLevel::Kernel && ordinary && denied && kernel_level() == before
}

// Isolate the capacity contract with an order-0 DMA request, as used by
// modern virtio-blk rings. Any reclaimed reachable page suffices; high-order
// adjacency and migration are covered by the independent fragmented fixtures.
// The finite prefix denies the baseline without starving the whole guest.
fn dma_capacity_recovery(minimum_expected_base: usize) -> (bool, alloc::string::String) {
    let mut detail = alloc::string::String::new();
    let mut phase = "headroom";
    let result = (|| -> Result<bool, SystemError> {
        struct CleanPages(Vec<Weak<Page>>);
        impl Drop for CleanPages {
            fn drop(&mut self) {
                for weak in &self.0 {
                    if let Some(page) = weak.upgrade() {
                        let lru = page_reclaimer_lock().remove_page_if_same(&page);
                        drop(lru);
                        let manager = page_manager_lock().remove_page_if_same(&page);
                        drop(manager);
                    }
                }
            }
        }
        let free = unsafe { LockedFrameAllocator.usage() }.free().data();
        // Choose from the currently available DMA32 inventory, retaining the
        // same bounded prefix/headroom policy as the other real DMA fixtures.
        // A fixed low-address prefix need not contain any free aligned run.
        let end = dma_fixture_budget(free, None)?
            .checked_mul(MMArch::PAGE_SIZE)
            .ok_or(SystemError::ENOMEM)?
            .min(DMA32_END);
        let mut entries = 65536;
        phase = "fixture_run";
        // The range/isolation helper handles high-order runs. Donate its two
        // frames as independent clean owners; the DMA request below is one page.
        let allocation =
            try_allocate_in_range(PhysAddr::new(0), PhysAddr::new(end), 1, &mut entries);
        let _ = writeln!(
            detail,
            "search_end={end} range_stop={:?} range_found={} scan_remaining={entries}",
            allocation.as_ref().err(),
            allocation.as_ref().is_ok_and(Option::is_some)
        );
        let run = allocation
            .map_err(|_| SystemError::ENOMEM)?
            .ok_or(SystemError::ENOMEM)?;
        let (base, count) = run.into_raw_parts();
        let _ = writeln!(
            detail,
            "base={base:?} pages={} free_before={free}",
            count.data()
        );
        phase = "clean_page_registration";
        let mut raw = RawFrameExtent {
            next: base,
            remaining: count.data(),
        };
        let mut pages = CleanPages(Vec::new());
        pages
            .0
            .try_reserve_exact(count.data())
            .map_err(|_| SystemError::ENOMEM)?;
        // Keep ordinary pins until every source is registered and the prefix owner
        // is born. Their release leaves only the production manager/LRU owners.
        let mut pins = Vec::new();
        pins.try_reserve_exact(count.data())
            .map_err(|_| SystemError::ENOMEM)?;
        while raw.remaining != 0 {
            let physical = raw.next;
            raw.next += MMArch::PAGE_SIZE;
            raw.remaining -= 1;
            let page = Page::try_new(
                physical,
                PageType::File(FileMapInfo {
                    page_cache: Weak::new(),
                    index: pins.len(),
                }),
                PageFlags::PG_LRU,
            )?;
            pages.0.push(Arc::downgrade(&page));
            register(&page)?;
            pins.push(page);
        }
        // Only the first donated page is device-reachable. The second is a
        // clean negative control which must survive the constrained scan.
        let mask_end = base.data() + MMArch::PAGE_SIZE;
        let mut prefix = PrefixReservations::new(mask_end)?;
        phase = "prefix_hold";
        prefix.hold_remaining()?;
        let _ = writeln!(detail, "mask_end={mask_end} held_pages={}", prefix.pages);
        let options = DmaAllocOptions {
            dma_mask: Some((mask_end - 1) as u64),
            use_pool: false,
            ..DmaAllocOptions::default()
        };
        phase = "baseline";
        let baseline = DmaBuffer::try_alloc_pages(1, options);
        let unavailable = matches!(baseline, Err(SystemError::ENOMEM));
        let _ = writeln!(
            detail,
            "baseline={:?} own_live={}",
            baseline.as_ref().err(),
            pages
                .0
                .iter()
                .filter(|page| page.strong_count() != 0)
                .count()
        );
        drop(baseline);
        phase = "fixture_recency";
        // Put the outside page before the inside page in cold-to-hot order.
        // This controls the input without scanning or retiring unrelated pages,
        // so the real request remains meaningful even with a large global LRU.
        let placed = {
            let mut reclaimer = page_reclaimer_lock();
            reclaimer.lru.place_cold_for_selftest(&pins[0])
                && reclaimer.lru.place_cold_for_selftest(&pins[1])
        };
        if !placed {
            return Err(SystemError::EBUSY);
        }
        drop(pins);
        phase = "allowed_attempt";
        let before = unsafe { LockedFrameAllocator.usage() }.free().data();
        let recovered = DmaBuffer::try_alloc_pages(
            1,
            DmaAllocOptions {
                compaction: DmaCompaction::Allowed,
                ..options
            },
        );
        let after = unsafe { LockedFrameAllocator.usage() }.free().data();
        let _ = writeln!(
            detail,
            "allowed={:?} free_before={before} free_after={after} own_live={}",
            recovered.as_ref().err(),
            pages
                .0
                .iter()
                .filter(|page| page.strong_count() != 0)
                .count()
        );
        let passed = base.data() >= minimum_expected_base
            && unavailable
            && pages.0[0].strong_count() == 0
            && pages.0[1].strong_count() != 0
            && recovered.as_ref().is_ok_and(|buffer| {
                buffer.page_count() == PageFrameCount::ONE
                    && buffer.paddr() + MMArch::PAGE_SIZE <= mask_end
                    && buffer.as_slice().iter().all(|byte| *byte == 0)
            });
        drop(recovered);
        drop(prefix);
        drop(pages);
        phase = "complete";
        Ok(passed)
    })();
    let _ = writeln!(detail, "phase={phase} error={:?}", result.as_ref().err());
    (result.unwrap_or(false), detail)
}

// Preserve the capacity/reclaim contract after all allocatable low-prefix
// frames have real owners. Only fixture placement changes; the one-page DMA
// mask, outside clean control, and every capacity assertion remain identical.
fn dma_capacity_occupied_prefix() -> (bool, alloc::string::String) {
    const PREFIX_END: usize = 64 * 1024 * 1024;
    let mut held_pages = 0;
    let mut recovery_detail = alloc::string::String::new();
    let result = (|| -> Result<bool, SystemError> {
        // Prepare reservation metadata before taking the headroom snapshot.
        let mut held = PrefixReservations::new(PREFIX_END)?;
        let free = unsafe { LockedFrameAllocator.usage() }.free().data();
        let dma32_free = LockedFrameAllocator::diagnostic_snapshot()
            .ok_or(SystemError::ENOMEM)?
            .dma32_free_pages();
        // Holding at most 64MiB leaves the inner capacity fixture's existing
        // 128MiB admission floor in both global and DMA32 inventories. The
        // inner fixture still recalculates its budget after this reservation.
        if free < 3 * DMA_HEADROOM_PAGES || dma32_free < 3 * DMA_HEADROOM_PAGES {
            return Err(SystemError::ENOMEM);
        }
        held.hold_remaining()?;
        held_pages = held.pages;
        let (passed, detail) = dma_capacity_recovery(PREFIX_END);
        recovery_detail = detail;
        drop(held);
        Ok(passed)
    })();
    let error = result.as_ref().err();
    (result.as_ref().copied().unwrap_or(false), alloc::format!(
        "error={error:?} held_pages={held_pages} prefix_end={PREFIX_END} recovery=[{recovery_detail}]"
    ))
}

fn invalid_seeds() -> bool {
    let mut compactor = Compactor::new();
    let start = PhysAddr::new(TARGET * MMArch::PAGE_SIZE);
    let end = start + BACKING * MMArch::PAGE_SIZE;
    // Below domain, at its exclusive end, unaligned, and a page-aligned seed
    // whose order alignment would leave no complete target inside the domain.
    [
        start - MMArch::PAGE_SIZE,
        end,
        start + 1,
        end - MMArch::PAGE_SIZE,
    ]
    .into_iter()
    .all(|seed| {
        let attempt = compactor.allocate(Request {
            start,
            end,
            order: ORDER,
            start_at: Some(seed),
            deadline: None,
            limits: Limits::default(),
        });
        matches!(attempt.outcome, Err(Stop::Invalid))
            && attempt.stats.targets == 0
            && attempt.stats.entries == 0
            && attempt.stats.pages == 0
            && attempt.stats.migrations == 0
    })
}

// Cold-call metadata ownership only: the tail deliberately remains raw.
// Real Page/PTE/charge migration is covered by the independent cases above.
fn cold_workspace_outside_target() -> Result<(bool, alloc::string::String), SystemError> {
    const COLD_ORDER: usize = 9;
    const COLD_PAGES: usize = 1 << COLD_ORDER;
    const HEAD_PAGES: usize = 4;
    let (base, count) = unsafe {
        LockedFrameAllocator
            .allocate_below(PageFrameCount::new(COLD_PAGES), PhysAddr::new(usize::MAX))
    }
    .ok_or(SystemError::ENOMEM)?;
    let mut tail = RawFrameExtent {
        next: base,
        remaining: count.data(),
    };
    if count.data() != COLD_PAGES {
        return Err(SystemError::EIO);
    }
    tail.next = base + HEAD_PAGES * MMArch::PAGE_SIZE;
    tail.remaining -= HEAD_PAGES;
    unsafe { LockedFrameAllocator.free(base, PageFrameCount::new(HEAD_PAGES)) };
    let mut entries = 65_536;
    let before =
        Isolation::begin(base, COLD_ORDER, &mut entries).map_err(|_| SystemError::EBUSY)?;
    let before_free = before.free_pages();
    drop(before);
    let request = Request {
        start: base,
        end: base + COLD_PAGES * MMArch::PAGE_SIZE,
        order: COLD_ORDER,
        start_at: None,
        deadline: None,
        limits: Limits {
            targets: 1,
            no_progress: 1,
            ..Limits::default()
        },
    };
    let (stop, stats, reached, cold_free, mut retirement, initial_capture) = {
        // Both measurement and production isolation are allocation-free. This
        // production owner is genuinely cold, with no prepared workspace.
        let mut cold = Compactor::new();
        let attempt = cold.allocate(request);
        let stop = attempt.outcome.as_ref().err().copied();
        let stats = attempt.stats;
        // The raw tail is a real blocker. A working cold API must reach it with
        // bounded work rather than fail to allocate its own metadata.
        let reached = matches!(attempt.outcome, Err(Stop::Blocked))
            && stats.targets == 1
            && stats.moved == 0
            && stats_bounded(&stats, &request.limits);
        drop(attempt);
        entries = 65_536;
        let mut retirement =
            Isolation::begin(base, COLD_ORDER, &mut entries).map_err(|_| SystemError::EBUSY)?;
        let cold_free = retirement.free_pages();
        // Protect all free target pages while the raw tail is returned; ordinary
        // Buddy metadata cannot consume the ownership being measured.
        drop(tail);
        let initial_capture = retirement.capture();
        (stop, stats, reached, cold_free, retirement, initial_capture)
    }; // The old workspace owner retired here; new Compactors own no frames.
    let captured_while_cold = initial_capture.is_some();
    // A second capture after retiring the cold owner is cleanup evidence only,
    // never a passing substitute for capturing while it was still alive.
    let cleanup_run = initial_capture.or_else(|| retirement.capture());
    let cleaned = cleanup_run
        .as_ref()
        .is_some_and(|run| run.start() == base && run.pages() == COLD_PAGES);
    drop(retirement);
    drop(cleanup_run);
    let passed = before_free == HEAD_PAGES
        && cold_free == HEAD_PAGES
        && reached
        && captured_while_cold
        && cleaned;
    Ok((passed, alloc::format!(
        "before_free={before_free} cold_free={cold_free} capture_while_cold={captured_while_cold} cleanup={cleaned} stop={stop:?} stats={stats:?}"
    )))
}

// The consumer requests one derived inclusive DMA prefix. Its bound comes
// from a run the fixture really owns, never a fixed boot physical layout.
const DMA_HEADROOM_PAGES: usize = 64 * 1024 * 1024 / MMArch::PAGE_SIZE;
const DMA32_END: usize = 1usize << 32;
// Sixteen real holes leave eight pages of ordinary allocator headroom for
// recovery metadata/destinations while every eight-page run remains split by
// managed source pages. No recovery owner is prewarmed by this fixture.
const DMA_REQUEST_PAGES: usize = TARGET / 2;

struct PrefixReservations {
    runs: Vec<(PhysAddr, PageFrameCount)>,
    pages: usize,
    end: usize,
}

impl PrefixReservations {
    fn new(end: usize) -> Result<Self, SystemError> {
        let mut runs = Vec::new();
        // Prepare the worst-case owner count before consuming the prefix.
        runs.try_reserve_exact(end / MMArch::PAGE_SIZE)
            .map_err(|_| SystemError::ENOMEM)?;
        Ok(Self {
            runs,
            pages: 0,
            end,
        })
    }

    fn hold_remaining(&mut self) -> Result<(), SystemError> {
        let max = PhysAddr::new(self.end - 1);
        for order in (0..=14).rev() {
            let count = PageFrameCount::new(1 << order);
            loop {
                if self.runs.len() == self.runs.capacity() {
                    return Err(SystemError::ENOMEM);
                }
                let Some((base, actual)) =
                    (unsafe { LockedFrameAllocator.allocate_below(count, max) })
                else {
                    break;
                };
                self.runs.push((base, actual));
                self.pages += actual.data();
                if actual != count
                    || self.pages > self.end / MMArch::PAGE_SIZE
                    || base.data() + actual.data() * MMArch::PAGE_SIZE > self.end
                {
                    return Err(SystemError::EIO);
                }
            }
        }
        Ok(())
    }
}

impl Drop for PrefixReservations {
    fn drop(&mut self) {
        for (base, count) in self.runs.drain(..).rev() {
            unsafe { LockedFrameAllocator.free(base, count) };
        }
    }
}

fn dma_recovery_attempt_bound(buckets: usize, mask_entries: usize) -> Option<usize> {
    if buckets == 0 || mask_entries == 0 {
        return None;
    }
    let deferred_attempts =
        1usize.checked_shl(u32::from(crate::mm::compaction::MAX_DEFER_SHIFT))?;
    mask_entries
        .checked_mul(deferred_attempts)?
        .checked_add(buckets.div_ceil(COMPACTION_DISCOVERY_BUCKETS))?
        .checked_add(1)
}

/// This snapshot is an acceptance budget for the controlled fixture, not an
/// arbitrary concurrent workload's liveness guarantee. No Page owner escapes.
fn dma_recovery_inventory(max: PhysAddr) -> Result<(usize, usize, usize, usize), SystemError> {
    let mut manager = page_manager_lock();
    let len = manager.phys2page.len();
    let buckets = manager.phys2page.raw_table().buckets();
    // Count every same-mask identity as an upper bound. Repeating the runtime
    // try-read/eligibility filter could miss a temporarily busy future source.
    let mask_entries = manager
        .phys2page
        .keys()
        .filter(|physical| **physical <= max)
        .count();
    let bound = dma_recovery_attempt_bound(buckets, mask_entries).ok_or(SystemError::EOVERFLOW)?;
    Ok((len, buckets, mask_entries, bound))
}

/// Both pressure fixtures must leave headroom in the DMA-addressable zone,
/// not merely in Normal memory which cannot satisfy their bounded requests.
fn dma_fixture_budget(free: usize, reservation_limit: Option<usize>) -> Result<usize, SystemError> {
    if free < 2 * DMA_HEADROOM_PAGES {
        return Err(SystemError::ENOMEM);
    }
    let dma32_free = LockedFrameAllocator::diagnostic_snapshot()
        .ok_or(SystemError::ENOMEM)?
        .dma32_free_pages();
    Ok((free / 2)
        .min(dma32_free / 2)
        .min(free - DMA_HEADROOM_PAGES)
        .min(reservation_limit.unwrap_or(usize::MAX)))
}

/// Exercise actual registry discovery and the real Service implementation with
/// a private owner. The global DMA consumer's cursor/backoff is never changed.
fn dma_discovery_deferral_progress() -> Result<bool, SystemError> {
    let free = unsafe { LockedFrameAllocator.usage() }.free().data();
    let end = dma_fixture_budget(free, None)?
        .checked_mul(MMArch::PAGE_SIZE)
        .ok_or(SystemError::ENOMEM)?
        .min(DMA32_END);
    let mut fixture =
        Fixture::with_range(false, true, Some((PhysAddr::new(0), PhysAddr::new(end))))?;
    let base = fixture.base.unwrap();
    let end = base + BACKING * MMArch::PAGE_SIZE;
    let mut prefix = PrefixReservations::new(end.data())?;
    prefix.hold_remaining()?;
    fixture.release_holes();
    let source = fixture.sources[0].physical;
    let (buckets, seeded_cursor) = {
        let mut manager = page_manager_lock();
        let table = manager.phys2page.raw_table();
        let buckets = table.buckets();
        let mut seeded_cursor = None;
        for index in 0..buckets {
            if unsafe { table.is_bucket_full(index) && table.bucket(index).as_ref().0 == source } {
                seeded_cursor = Some(index);
                break;
            }
        }
        (buckets, seeded_cursor)
    };
    let Some(seeded_cursor) = seeded_cursor else {
        return Ok(false);
    };
    let mut seen = [false; SOURCES];
    let mut cursor = 0;
    let mut visited = 0;
    // Empty windows consume up to 256 buckets; each returned source adds at
    // most one interrupted window. This bound follows the real scan contract.
    for _ in 0..buckets.div_ceil(COMPACTION_DISCOVERY_BUCKETS) + SOURCES + 1 {
        let before = cursor;
        let found = super::compaction_candidate(&mut cursor, base, end - 1)?;
        let advanced = (cursor + buckets - before) % buckets;
        visited += if advanced == 0 { buckets } else { advanced };
        if let Some(physical) = found {
            let offset = physical.data() - base.data();
            if !offset.is_multiple_of(2 * MMArch::PAGE_SIZE) {
                return Ok(false);
            }
            let index = offset / (2 * MMArch::PAGE_SIZE);
            if index >= SOURCES || index == 0 {
                return Ok(false);
            }
            if seen[index] {
                break;
            }
            seen[index] = true;
        }
        if visited >= buckets {
            break;
        }
    }
    let consumed = !seen[0] && seen[1..].iter().all(|seen| *seen);
    let deferred = crate::mm::compaction::recovery::deferred_discovery_selftest(
        source,
        seeded_cursor,
        ORDER + 1,
        end - 1,
    );
    let budget_edges = dma_recovery_attempt_bound(0, SOURCES).is_none()
        && dma_recovery_attempt_bound(buckets, 0).is_none()
        && dma_recovery_attempt_bound(buckets, usize::MAX).is_none();
    let passed = fixture.finish(consumed && deferred && budget_edges);
    drop(prefix);
    Ok(passed)
}

#[derive(Default)]
struct DmaRecoveryDetail {
    phase: &'static str,
    error: Option<SystemError>,
    free_before: usize,
    search_end: usize,
    prefix_end: Option<usize>,
    free_after_reservation: Option<usize>,
    prefix_pages: usize,
    foreign_limit: usize,
    foreign_runs: usize,
    fixture_base: Option<PhysAddr>,
    fixture_range_stop: Option<Stop>,
    isolated_free: Option<usize>,
    baseline_allocation: Option<Result<(usize, usize), SystemError>>,
    baseline_identities: Option<bool>,
    baseline_valid: Option<bool>,
    atomic_allocation: Option<Result<(usize, usize), SystemError>>,
    noio_allocation: Option<Result<(usize, usize), SystemError>>,
    registry_len: Option<usize>,
    registry_buckets: Option<usize>,
    registry_mask_entries: Option<usize>,
    recovery_attempt_bound: Option<usize>,
    recovery_attempts: usize,
    recovery_last_error: Option<SystemError>,
    recovered_start: Option<usize>,
    recovered_pages: Option<usize>,
    recovered_moved: Option<usize>,
    recovered_zero: Option<bool>,
    recovered_alignment: Option<bool>,
    recovered_valid_before: Option<bool>,
    recovered_valid_after: Option<bool>,
    recovered_checked: Option<bool>,
    finish_account_pages: Option<u64>,
}

fn dma_allocation_detail(
    result: &Result<DmaBuffer, SystemError>,
) -> Result<(usize, usize), SystemError> {
    result
        .as_ref()
        .map(|buffer| (buffer.paddr(), buffer.page_count().data()))
        .map_err(|error| error.clone())
}

/// Establish a failed ordinary-allocation baseline, including the IRQ/NOIO
/// policy checks. Background owners may release another legal run under the
/// same mask; retain such ordinary successes rather than free-and-retry.
/// Do not use this helper for recovery: migrations of other legal targets may
/// consume this fixture's holes, and the DMA API does not promise its sources.
fn dma_fixture_allocation(
    fixture: &Fixture,
    foreign: &mut Vec<DmaBuffer>,
    prefix_end: usize,
    mut allocate: impl FnMut() -> Result<DmaBuffer, SystemError>,
) -> Result<DmaBuffer, SystemError> {
    loop {
        if let Some(buffer) =
            retain_foreign_dma_allocation(fixture, foreign, prefix_end, allocate()?)?
        {
            return Ok(buffer);
        }
    }
}

/// Validate one real allocation and retain unrelated successes. A target run
/// covers an original fixture source; ordinary foreign frees are not migration
/// evidence and must not be released into a free-and-retry loop.
fn retain_foreign_dma_allocation(
    fixture: &Fixture,
    foreign: &mut Vec<DmaBuffer>,
    prefix_end: usize,
    mut buffer: DmaBuffer,
) -> Result<Option<DmaBuffer>, SystemError> {
    let start = buffer.paddr();
    let bytes = DMA_REQUEST_PAGES * MMArch::PAGE_SIZE;
    let end = start.checked_add(bytes).ok_or(SystemError::EOVERFLOW)?;
    if buffer.page_count().data() != DMA_REQUEST_PAGES
        || buffer.len() != bytes
        || !start.is_multiple_of(bytes)
        || end > prefix_end
        || !buffer.as_slice().iter().all(|byte| *byte == 0)
        || !fixture.valid()
    {
        return Err(SystemError::EIO);
    }
    if fixture
        .sources
        .iter()
        .any(|source| (start..end).contains(&source.physical.data()))
    {
        return Ok(Some(buffer));
    }
    if foreign.len() >= prefix_end / bytes || foreign.len() == foreign.capacity() {
        return Err(SystemError::EOVERFLOW);
    }
    buffer.as_mut_slice().fill(0x63);
    if !fixture.valid() {
        return Err(SystemError::EIO);
    }
    foreign.push(buffer);
    Ok(None)
}

// baseline, atomic exclusion, NOIO exclusion, recovery/ownership respectively.
fn dma_automatic_recovery(
    reservation_limit: Option<usize>,
) -> ((bool, bool, bool, bool), alloc::string::String) {
    let mut detail = DmaRecoveryDetail::default();
    let result = (|| -> Result<(bool, bool, bool, bool), SystemError> {
        detail.phase = "initial_headroom";
        detail.free_before = unsafe { LockedFrameAllocator.usage() }.free().data();
        // Include external layout owners while preserving real DMA32 headroom.
        let budget = dma_fixture_budget(detail.free_before, reservation_limit)?;
        detail.search_end = budget
            .checked_mul(MMArch::PAGE_SIZE)
            .ok_or(SystemError::ENOMEM)?
            .min(DMA32_END);
        let mut fixture = Fixture::with_range_detail(
            false,
            false,
            Some((PhysAddr::new(0), PhysAddr::new(detail.search_end))),
            &mut detail.phase,
            &mut detail.fixture_range_stop,
        )?;
        detail.fixture_base = fixture.base;
        let prefix_end = fixture
            .base
            .unwrap()
            .data()
            .checked_add(BACKING * MMArch::PAGE_SIZE)
            .ok_or(SystemError::ENOMEM)?;
        detail.prefix_end = Some(prefix_end);
        detail.phase = "prefix_metadata";
        // Prepare all incidental owners before consuming any prefix frames.
        // No success can grow this vector during a pressure/IRQ-policy check.
        let foreign_limit = prefix_end / (DMA_REQUEST_PAGES * MMArch::PAGE_SIZE);
        let mut foreign = Vec::new();
        foreign
            .try_reserve_exact(foreign_limit)
            .map_err(|_| SystemError::ENOMEM)?;
        detail.foreign_limit = foreign_limit;
        let mut prefix = PrefixReservations::new(prefix_end)?;
        detail.phase = "prefix_reservation";
        let reservation = prefix.hold_remaining();
        detail.prefix_pages = prefix.pages;
        detail.free_after_reservation = Some(unsafe { LockedFrameAllocator.usage() }.free().data());
        reservation?;
        detail.phase = "post_reservation_headroom";
        if unsafe { LockedFrameAllocator.usage() }.free().data() < DMA_HEADROOM_PAGES {
            return Err(SystemError::ENOMEM);
        }
        fixture.release_holes();
        detail.phase = "fixture_isolation";
        let mut entries = 65_536;
        let domain = Isolation::begin(fixture.base.unwrap(), ORDER + 1, &mut entries)
            .map_err(|_| SystemError::EBUSY)?;
        detail.isolated_free = Some(domain.free_pages());
        let enough = domain.free_pages() == TARGET;
        drop(domain);
        let ordinary = DmaAllocOptions {
            dma_mask: Some((prefix_end - 1) as u64),
            use_pool: false,
            ..DmaAllocOptions::default()
        };
        let original_identities = || {
            fixture
                .sources
                .iter()
                .all(|source| source.weak.upgrade().is_some())
        };
        detail.phase = "baseline";
        // Keep every predicate observable instead of letting && hide the reason.
        // Formatting happens only after all real frame owners have been released.
        let baseline_allocation =
            dma_fixture_allocation(&fixture, &mut foreign, prefix_end, || {
                DmaBuffer::try_alloc_pages(DMA_REQUEST_PAGES, ordinary)
            });
        detail.foreign_runs = foreign.len();
        detail.baseline_allocation = Some(dma_allocation_detail(&baseline_allocation));
        let allocation_failed = matches!(baseline_allocation, Err(SystemError::ENOMEM));
        drop(baseline_allocation);
        let identities = original_identities();
        let valid = fixture.valid();
        detail.baseline_identities = Some(identities);
        detail.baseline_valid = Some(valid);
        let baseline = enough && allocation_failed && identities && valid;
        if !baseline {
            return Ok((false, false, false, false));
        }
        let allowed = DmaAllocOptions {
            compaction: DmaCompaction::Allowed,
            ..ordinary
        };
        detail.phase = "atomic";
        let atomic_result = dma_fixture_allocation(&fixture, &mut foreign, prefix_end, || {
            let irq = unsafe { CurrentIrqArch::save_and_disable_irq() };
            let allocation = DmaBuffer::try_alloc_pages(DMA_REQUEST_PAGES, allowed);
            drop(irq);
            allocation
        });
        detail.foreign_runs = foreign.len();
        detail.atomic_allocation = Some(dma_allocation_detail(&atomic_result));
        let atomic = matches!(atomic_result, Err(SystemError::ENOMEM))
            && original_identities()
            && fixture.valid();
        detail.phase = "noio";
        let noio_result = dma_fixture_allocation(&fixture, &mut foreign, prefix_end, || {
            let scope = crate::cgroup::memory::io::MemoryIoScope::enter_noio();
            let allocation = DmaBuffer::try_alloc_pages(DMA_REQUEST_PAGES, allowed);
            drop(scope);
            allocation
        });
        detail.foreign_runs = foreign.len();
        detail.noio_allocation = Some(dma_allocation_detail(&noio_result));
        let noio = matches!(noio_result, Err(SystemError::ENOMEM))
            && original_identities()
            && fixture.valid();

        // Each production attempt examines a bounded registry window and at most
        // one candidate. Repeated callers advance its real cursor/defer state;
        // there is no fixture seed or forced allocation failure.
        detail.phase = "recovery_inventory";
        let (len, buckets, mask_entries, attempt_bound) =
            dma_recovery_inventory(PhysAddr::new(prefix_end - 1))?;
        detail.registry_len = Some(len);
        detail.registry_buckets = Some(buckets);
        detail.registry_mask_entries = Some(mask_entries);
        detail.recovery_attempt_bound = Some(attempt_bound);
        detail.phase = "automatic_recovery";
        let mut recovered = None;
        for _ in 0..attempt_bound {
            // The first legal run satisfies the consumer, regardless of which
            // target supplied it. Retaining recovery successes while demanding
            // this fixture can consume all its holes as migration destinations.
            detail.recovery_attempts += 1;
            let allocation = DmaBuffer::try_alloc_pages(DMA_REQUEST_PAGES, allowed);
            match allocation {
                Ok(buffer) => {
                    recovered = Some(buffer);
                    break;
                }
                Err(SystemError::ENOMEM) => detail.recovery_last_error = Some(SystemError::ENOMEM),
                Err(error) => {
                    detail.recovery_last_error = Some(error.clone());
                    return Err(error);
                }
            }
        }
        let Some(mut buffer) = recovered else {
            return Ok((baseline, atomic, noio, false));
        };
        detail.phase = "recovered_ownership";
        let start = PhysAddr::new(buffer.paddr());
        let end = PhysAddr::new(
            start
                .data()
                .checked_add(DMA_REQUEST_PAGES * MMArch::PAGE_SIZE)
                .ok_or(SystemError::EOVERFLOW)?,
        );
        let mut moved = 0;
        detail.recovered_start = Some(start.data());
        detail.recovered_pages = Some(buffer.page_count().data());
        detail.recovered_zero = Some(buffer.as_slice().iter().all(|byte| *byte == 0));
        detail.recovered_alignment = Some(
            start
                .data()
                .is_multiple_of(DMA_REQUEST_PAGES * MMArch::PAGE_SIZE),
        );
        detail.recovered_valid_before = Some(fixture.valid());
        let mut checked = buffer.page_count().data() == DMA_REQUEST_PAGES
            && buffer.len() == DMA_REQUEST_PAGES * MMArch::PAGE_SIZE
            && start
                .data()
                .is_multiple_of(DMA_REQUEST_PAGES * MMArch::PAGE_SIZE)
            && end.data() <= prefix_end
            && buffer.as_slice().iter().all(|byte| *byte == 0)
            && fixture.valid();
        for source in &fixture.sources {
            if (start..end).contains(&source.physical) {
                moved += 1;
                let index = (source.address.data() - fixture.address.data()) / MMArch::PAGE_SIZE;
                checked &= source.weak.upgrade().is_none()
                    && fixture
                        .source_page(index)
                        .is_some_and(|page| !(start..end).contains(&page.phys_address()));
            }
        }
        detail.recovered_moved = Some(moved);
        // A public allocation may also capture an ordinary free run. Strong
        // migration/identity assertions belong to real_fragmented_capture;
        // any fixture source covered here must still have moved safely above.
        buffer.as_mut_slice().fill(0xa7);
        checked &= buffer.as_slice().iter().all(|byte| *byte == 0xa7)
            && fixture.valid()
            && foreign
                .iter()
                .all(|held| held.as_slice().iter().all(|byte| *byte == 0x63));
        detail.recovered_valid_after = Some(fixture.valid());
        detail.recovered_checked = Some(checked);
        drop(buffer);
        let account = fixture.account.clone();
        let passed = fixture.finish(checked);
        detail.finish_account_pages = Some(account.current_pages());
        drop(prefix);
        detail.phase = "complete";
        Ok((baseline, atomic, noio, passed))
    })();
    if let Err(ref error) = result {
        detail.error = Some(error.clone());
    }
    // The closure's fixture, reservations and allocation owners are all gone.
    (
        result.unwrap_or((false, false, false, false)),
        alloc::format!(
            "phase={} error={:?} free_before={} search_end={} prefix_end={:?} free_after={:?} prefix_pages={} foreign_limit={} foreign_runs={} fixture_base={:?} range_stop={:?} isolated_free={:?} baseline_allocation={:?} baseline_identities={:?} baseline_valid={:?} atomic_allocation={:?} noio_allocation={:?} registry_len={:?} registry_buckets={:?} registry_mask_entries={:?} recovery_attempt_bound={:?} recovery_attempts={} recovery_last_error={:?} recovered_start={:?} recovered_pages={:?} recovered_moved={:?} recovered_zero={:?} recovered_alignment={:?} recovered_valid_before={:?} recovered_valid_after={:?} recovered_checked={:?} finish_account_pages={:?}",
            detail.phase, detail.error, detail.free_before, detail.search_end, detail.prefix_end, detail.free_after_reservation,
            detail.prefix_pages, detail.foreign_limit, detail.foreign_runs,
            detail.fixture_base, detail.fixture_range_stop, detail.isolated_free,
            detail.baseline_allocation, detail.baseline_identities, detail.baseline_valid,
            detail.atomic_allocation, detail.noio_allocation, detail.registry_len,
            detail.registry_buckets, detail.registry_mask_entries, detail.recovery_attempt_bound,
            detail.recovery_attempts, detail.recovery_last_error, detail.recovered_start, detail.recovered_pages,
            detail.recovered_moved, detail.recovered_zero, detail.recovered_alignment,
            detail.recovered_valid_before, detail.recovered_valid_after,
            detail.recovered_checked, detail.finish_account_pages,
        ),
    )
}

/// Occupying an ordinary low RAM interval must not prevent the test from
/// constructing its independent domain. The same real DMA consumer is used.
fn dma_occupied_layout() -> ((bool, bool, bool, bool, bool), alloc::string::String) {
    const START: usize = 32 * 1024 * 1024;
    const END: usize = 64 * 1024 * 1024;
    const RUNS: usize = (END - START) / (BACKING * MMArch::PAGE_SIZE);
    let initial_free = unsafe { LockedFrameAllocator.usage() }.free().data();
    let mut held = Vec::new();
    let mut held_pages = 0;
    let mut phase = "layout_metadata";
    let mut range_error = None;
    let mut unavailable = false;
    let mut recovery_detail = alloc::string::String::new();
    let result = (|| -> Result<(bool, bool, bool, bool, bool), SystemError> {
        held.try_reserve_exact(RUNS)
            .map_err(|_| SystemError::ENOMEM)?;
        phase = "layout_ownership";
        for _ in 0..=RUNS {
            let mut entries = 65_536;
            let run = try_allocate_in_range(
                PhysAddr::new(START),
                PhysAddr::new(END),
                ORDER + 1,
                &mut entries,
            )
            .map_err(|error| {
                range_error = Some(error);
                SystemError::ENOMEM
            })?;
            let Some(run) = run else { break };
            // A layout probe must also obey the aggregate pressure budget.
            if held.len() == RUNS || held_pages + run.pages() > initial_free / 2 {
                drop(run);
                return Err(SystemError::ENOMEM);
            }
            held_pages += run.pages();
            held.push(run);
        }
        // This is the original fixed-range setup, not a consumer failure
        // injection. Its actual phase proves the legal occupied layout.
        let fixed = Fixture::with_range_detail(
            false,
            false,
            Some((PhysAddr::new(START), PhysAddr::new(END))),
            &mut phase,
            &mut range_error,
        );
        unavailable =
            matches!(fixed, Err(SystemError::ENOMEM)) && phase == "fixture_backing_unavailable";
        drop(fixed);
        if !unavailable {
            return Ok((false, false, false, false, false));
        }
        let limit = (initial_free / 2)
            .checked_sub(held_pages)
            .ok_or(SystemError::ENOMEM)?;
        let ((baseline, atomic, noio, recovered), detail) = dma_automatic_recovery(Some(limit));
        recovery_detail = detail;
        phase = "complete";
        Ok((unavailable, baseline, atomic, noio, recovered))
    })();
    // No retained raw owner or arbitrary test failure changes consumer policy.
    drop(held);
    let error = result.as_ref().err().cloned();
    (result.unwrap_or((false, false, false, false, false)), alloc::format!(
        "phase={phase} error={error:?} held_pages={held_pages} range_stop={range_error:?} fixed_unavailable={unavailable} recovery=[{recovery_detail}]"
    ))
}

// Wait using the ordinary guest timer, never a test-only worker wake or pause.
fn background_idle_until(
    deadline: u64,
) -> Result<Option<crate::mm::compaction::background::selftest::Observation>, SystemError> {
    loop {
        let observed = crate::mm::compaction::background::selftest::observe();
        if observed.pending == 0 && observed.active == 0 && observed.deadline == 0 {
            return Ok(Some(observed));
        }
        if crate::time::timer::clock() >= deadline {
            return Ok(None);
        }
        crate::time::sleep::nanosleep(crate::time::PosixTimeSpec::new(0, 1_000_000))?;
    }
}

// Prepare capacity before holding the registry. These are real fixed-frame
// owners, not a candidate override: they prevent unrelated data from consuming
// this fixture's free holes while the real worker discovers its sources.
fn pin_foreign_dma_sources(
    fixture: &Fixture,
    end: usize,
) -> Result<(Vec<super::access::PageAccessPin>, usize), SystemError> {
    for _ in 0..8 {
        let len = page_manager_lock().phys2page.len();
        let mut pins = Vec::new();
        pins.try_reserve_exact(len)
            .map_err(|_| SystemError::ENOMEM)?;
        let Some(mut manager) = try_page_manager_lock() else {
            crate::sched::sched_yield();
            continue;
        };
        if manager.phys2page.len() > pins.capacity() {
            drop(manager);
            continue;
        }
        let buckets = manager.phys2page.raw_table().buckets();
        let mut busy = false;
        for (physical, page) in &manager.phys2page {
            if physical.data() >= end
                || fixture
                    .sources
                    .iter()
                    .any(|source| source.physical == *physical)
            {
                continue;
            }
            match page.try_pin_access() {
                Ok(pin) => pins.push(pin),
                Err(_) => {
                    busy = true;
                    break;
                }
            }
        }
        drop(manager);
        if !busy {
            return Ok((pins, buckets));
        }
        // Page owners and their pin counts must retire outside the registry.
        drop(pins);
        crate::sched::sched_yield();
    }
    Err(SystemError::EBUSY)
}

fn dma_background_recovery() -> ((bool, bool, bool), alloc::string::String) {
    use crate::mm::compaction::background::selftest::observe;
    let mut detail = alloc::string::String::new();
    let mut phase = "initial_idle";
    let mut rounds = 0;
    let mut exit_reason = "setup";
    let result = (|| -> Result<(bool, bool, bool), SystemError> {
        let initial =
            background_idle_until(crate::time::timer::next_n_us_timer_jiffies(2_000_000))?
                .ok_or(SystemError::ETIMEDOUT)?;
        let free = unsafe { LockedFrameAllocator.usage() }.free().data();
        if free < 2 * DMA_HEADROOM_PAGES {
            return Err(SystemError::ENOMEM);
        }
        let end = (free / 2)
            .checked_mul(MMArch::PAGE_SIZE)
            .ok_or(SystemError::ENOMEM)?
            .min(DMA32_END);
        phase = "fixture";
        let mut fixture =
            Fixture::with_range(false, false, Some((PhysAddr::new(0), PhysAddr::new(end))))?;
        let prefix_end = fixture.base.unwrap().data() + BACKING * MMArch::PAGE_SIZE;
        let mut prefix = PrefixReservations::new(prefix_end)?;
        let mut foreign = Vec::new();
        foreign
            .try_reserve_exact(prefix_end / (DMA_REQUEST_PAGES * MMArch::PAGE_SIZE))
            .map_err(|_| SystemError::ENOMEM)?;
        phase = "foreign_pins";
        let (foreign_pins, buckets) = pin_foreign_dma_sources(&fixture, prefix_end)?;
        let round_bound = buckets.div_ceil(super::COMPACTION_DISCOVERY_BUCKETS) + SOURCES + 1;
        prefix.hold_remaining()?;
        let exercise = (|| -> Result<(bool, bool, bool), SystemError> {
            fixture.release_holes();
            let ordinary = DmaAllocOptions {
                dma_mask: Some((prefix_end - 1) as u64),
                use_pool: false,
                ..DmaAllocOptions::default()
            };
            phase = "baseline";
            let baseline = dma_fixture_allocation(&fixture, &mut foreign, prefix_end, || {
                DmaBuffer::try_alloc_pages(DMA_REQUEST_PAGES, ordinary)
            });
            let unavailable = matches!(baseline, Err(SystemError::ENOMEM));
            drop(baseline);
            if !unavailable || !fixture.valid() {
                return Ok((false, false, false));
            }
            let mut recovered = None;
            // Each new failure authorizes one round. With foreign data pinned, a
            // complete registry traversal plus the fixture identities covers the
            // eligible input. Include the real maximum slot cooldown per round.
            let deadline = crate::time::timer::next_n_us_timer_jiffies(
                (round_bound as u64).saturating_mul(1_100_000),
            );
            exit_reason = "request_bound";
            for _ in 0..round_bound {
                if crate::time::timer::clock() >= deadline {
                    exit_reason = "deadline";
                    break;
                }
                phase = "real_contended_request";
                let before = observe();
                // Sources are already migratable. Contend only the existing
                // synchronous owner so an immediately scheduled worker can
                // proceed, rather than racing the caller's source-pin release.
                // The helper returns (and releases its guard) before any wait.
                let allocation =
                    crate::mm::compaction::recovery::selftest::allocate_with_direct_contention(
                        DMA_REQUEST_PAGES,
                        DmaAllocOptions {
                            compaction: DmaCompaction::DirectAndBackground,
                            ..ordinary
                        },
                    );
                if let Ok(buffer) = allocation {
                    if let Some(buffer) =
                        retain_foreign_dma_allocation(&fixture, &mut foreign, prefix_end, buffer)?
                    {
                        recovered = Some(buffer);
                        exit_reason = "fixture_capture";
                        break;
                    }
                    // This fast-path success authorizes no background work.
                    // Retain its owner and continue within the original bound.
                    continue;
                }
                if !matches!(allocation, Err(SystemError::ENOMEM)) {
                    return Err(allocation.err().unwrap());
                }
                if observe().requests == before.requests {
                    exit_reason = "no_fragmented_handoff";
                    break;
                }
                rounds += 1;
                phase = "worker_consumption";
                let Some(after) = background_idle_until(deadline)? else {
                    exit_reason = "worker_deadline";
                    break;
                };
                if after.moved > initial.moved {
                    // This retry has no direct-recovery permission. It cannot
                    // manufacture the migration evidence attributed to the worker.
                    match dma_fixture_allocation(&fixture, &mut foreign, prefix_end, || {
                        DmaBuffer::try_alloc_pages(DMA_REQUEST_PAGES, ordinary)
                    }) {
                        Ok(buffer) => {
                            recovered = Some(buffer);
                            exit_reason = "disabled_capture";
                            break;
                        }
                        Err(SystemError::ENOMEM) => {}
                        Err(error) => return Err(error),
                    }
                }
            }
            phase = "final_idle";
            let final_state =
                background_idle_until(crate::time::timer::next_n_us_timer_jiffies(3_000_000))?
                    .ok_or(SystemError::ETIMEDOUT)?;
            let triggered = unavailable
                && final_state.requests > initial.requests
                && final_state.rounds > initial.rounds;
            let migrated =
                final_state.compactions > initial.compactions && final_state.moved > initial.moved;
            let ownership = recovered.as_ref().is_some_and(|buffer| {
                buffer.page_count().data() == DMA_REQUEST_PAGES
                    && buffer
                        .paddr()
                        .is_multiple_of(DMA_REQUEST_PAGES * MMArch::PAGE_SIZE)
                    && buffer.paddr() + buffer.len() <= prefix_end
                    && buffer.as_slice().iter().all(|byte| *byte == 0)
            }) && fixture.valid();
            if let Some(buffer) = recovered.as_mut() {
                buffer.as_mut_slice().fill(0x96);
            }
            let contents = recovered
                .as_ref()
                .is_some_and(|buffer| buffer.as_slice().iter().all(|byte| *byte == 0x96))
                && fixture.valid();
            phase = "idle_observation";
            crate::time::sleep::nanosleep(crate::time::PosixTimeSpec::new(0, 20_000_000))?;
            let quiet = observe();
            let idle = quiet.requests == final_state.requests
                && quiet.rounds == final_state.rounds
                && quiet.moved == final_state.moved
                && quiet.pending == 0
                && quiet.active == 0
                && quiet.deadline == 0;
            let _ = writeln!(detail, "requests={} rounds={} compactions={} moved={} legal_requests={rounds} captured={} idle={idle} foreign_runs={} discovery_progress={}",
                final_state.requests - initial.requests, final_state.rounds - initial.rounds,
                final_state.compactions - initial.compactions, final_state.moved - initial.moved, recovered.is_some(), foreign.len(), final_state.discovery_progress - initial.discovery_progress);
            let fixture_moved = fixture
                .sources
                .iter()
                .filter(|source| {
                    let index =
                        (source.address.data() - fixture.address.data()) / MMArch::PAGE_SIZE;
                    fixture
                        .source_page(index)
                        .is_some_and(|page| page.phys_address() != source.physical)
                })
                .count();
            let stops: [u64; 7] =
                core::array::from_fn(|index| final_state.stops[index] - initial.stops[index]);
            let final_buckets = page_manager_lock().phys2page.raw_table().buckets();
            let _ = writeln!(detail, "foreign_pins={} buckets={buckets}/{final_buckets} round_bound={round_bound} fixture_moved={fixture_moved} stops={stops:?}", foreign_pins.len());
            drop(recovered);
            Ok((
                triggered,
                migrated && ownership && contents && buckets == final_buckets,
                idle,
            ))
        })();
        // The exercise may leave on any error, including with an in-flight
        // one-shot request. Quiesce using a fresh cleanup budget before
        // releasing any fixture owner or foreign physical-access pin.
        let cleanup = background_idle_until(crate::time::timer::next_n_us_timer_jiffies(3_000_000));
        let quiescent = matches!(&cleanup, Ok(Some(_)));
        if exercise.is_err() || !quiescent {
            let state = observe();
            let stops: [u64; 7] =
                core::array::from_fn(|index| state.stops[index] - initial.stops[index]);
            let fixture_moved = fixture
                .sources
                .iter()
                .filter(|source| {
                    let index =
                        (source.address.data() - fixture.address.data()) / MMArch::PAGE_SIZE;
                    fixture
                        .source_page(index)
                        .is_some_and(|page| page.phys_address() != source.physical)
                })
                .count();
            let _ = writeln!(detail, "cleanup_quiet={quiescent} foreign_pins={} buckets={buckets} round_bound={round_bound} fixture_moved={fixture_moved} requests={} rounds={} moved={} stops={stops:?}",
                foreign_pins.len(), state.requests-initial.requests, state.rounds-initial.rounds, state.moved-initial.moved);
        }
        drop(foreign);
        drop(prefix);
        drop(foreign_pins);
        let passed =
            fixture.finish(quiescent && exercise.as_ref().is_ok_and(|(_, valid, _)| *valid));
        if !quiescent {
            phase = "cleanup_idle";
            return Err(cleanup.err().unwrap_or(SystemError::ETIMEDOUT));
        }
        let (triggered, _, idle) = exercise?;
        phase = "complete";
        Ok((triggered, passed, idle))
    })();
    let _ = writeln!(
        detail,
        "phase={phase} exit_reason={exit_reason} error={:?} legal_requests={rounds}",
        result.as_ref().err()
    );
    (result.unwrap_or((false, false, false)), detail)
}

pub(crate) fn report() -> alloc::string::String {
    let _serial = TEST_LOCK.lock();
    let mut deferred_detail = alloc::string::String::new();
    let (capacity_recovery, capacity_detail) = dma_capacity_recovery(0);
    let (capacity_occupied, capacity_occupied_detail) = dma_capacity_occupied_prefix();
    let mut checks = alloc::vec![
        (
            "real_fragmented_capture",
            real_fragmented_capture().unwrap_or(false),
        ),
        (
            "partial_pin_cleanup",
            partial_failure(true).unwrap_or(false),
        ),
        (
            "migration_budget_cleanup",
            partial_failure(false).unwrap_or(false),
        ),
        ("unknown_blocker", blocked_unknown().unwrap_or(false)),
        ("rmap_budget", rmap_limit().unwrap_or(false)),
        (
            "deferred_free_fastpath",
            match deferred_allocation_fastpath() {
                Ok((passed, detail)) => {
                    deferred_detail = detail;
                    passed
                }
                Err(error) => {
                    deferred_detail = alloc::format!("phase=setup error={error:?}");
                    false
                }
            },
        ),
        ("scan_budget_no_mutation", small_budget().unwrap_or(false)),
        (
            "isolation_allocation_free_window",
            isolation_allocation_free_window().unwrap_or(false),
        ),
        ("atomic_rejected", atomic_rejected()),
        ("deadline_rollback", deadline_rollback().unwrap_or(false)),
        ("dma_recovery_reserve_guard", dma_recovery_reserve_guard()),
        ("dma_capacity_recovery", capacity_recovery,),
        ("dma_capacity_occupied_prefix", capacity_occupied),
        ("seed_boundaries_no_mutation", invalid_seeds()),
        (
            "repeated_capture",
            real_fragmented_capture().unwrap_or(false),
        ),
    ];
    let (cold_workspace, cold_detail) = match cold_workspace_outside_target() {
        Ok(result) => result,
        Err(error) => (false, alloc::format!("setup_error={error:?}")),
    };
    checks.push(("cold_workspace_outside_target", cold_workspace));
    let ((baseline, atomic, noio, recovered), dma_detail) = dma_automatic_recovery(None);
    checks.extend([
        (
            "dma_discovery_deferral_progress",
            dma_discovery_deferral_progress().unwrap_or(false),
        ),
        ("dma_same_mask_fragmentation", baseline),
        ("dma_atomic_no_recovery", atomic),
        ("dma_noio_no_recovery", noio),
        ("dma_automatic_recovery", recovered),
    ]);
    let ((layout, layout_baseline, layout_atomic, layout_noio, layout_recovered), layout_detail) =
        dma_occupied_layout();
    checks.extend([
        ("dma_occupied_range_unavailable", layout),
        ("dma_occupied_same_mask_fragmentation", layout_baseline),
        ("dma_occupied_atomic_no_recovery", layout_atomic),
        ("dma_occupied_noio_no_recovery", layout_noio),
        ("dma_occupied_automatic_recovery", layout_recovered),
    ]);
    checks.extend(crate::mm::allocator::buddy::compaction::selftest_checks());
    let (discovery_buckets, zero_absent) = {
        let mut manager = page_manager_lock();
        (
            manager.phys2page.raw_table().buckets(),
            !manager.contains(&PhysAddr::new(0)),
        )
    };
    checks.extend(crate::mm::compaction::background::selftest::checks(
        discovery_buckets,
    ));
    let discovery_shape_stable = {
        let mut manager = page_manager_lock();
        zero_absent
            && !manager.contains(&PhysAddr::new(0))
            && manager.phys2page.raw_table().buckets() == discovery_buckets
    };
    checks.push((
        "background_discovery_registry_stable",
        discovery_shape_stable,
    ));
    let ((background_triggered, background_migrated, background_idle), background_detail) =
        dma_background_recovery();
    checks.extend([
        ("dma_background_real_request", background_triggered),
        ("dma_background_migration_retry", background_migrated),
        ("dma_background_idle", background_idle),
    ]);
    let failed = checks.iter().filter(|(_, passed)| !passed).count();
    let mut output = alloc::format!("status={}\n", if failed == 0 { "ok" } else { "fail" });
    for (name, passed) in checks {
        let _ = writeln!(output, "{name}={}", if passed { "ok" } else { "fail" });
    }
    let _ = writeln!(output, "deferred_free_detail={deferred_detail}");
    let _ = writeln!(output, "dma_capacity_recovery_detail=[{capacity_detail}]");
    let _ = writeln!(
        output,
        "dma_capacity_occupied_prefix_detail=[{capacity_occupied_detail}]"
    );
    let _ = writeln!(output, "cold_workspace_detail={cold_detail}");
    let _ = writeln!(output, "dma_recovery_detail={dma_detail}");
    let _ = writeln!(output, "dma_occupied_layout_detail={layout_detail}");
    let _ = writeln!(output, "dma_background_detail=[{background_detail}]");
    let _ = writeln!(output, "summary_fail={failed}");
    output
}
