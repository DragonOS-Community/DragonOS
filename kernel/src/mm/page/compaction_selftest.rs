//! Deterministic compaction inputs built from real global Buddy frames.
//! Only fixture setup chooses PFNs. Allocation, isolation, migration and final
//! frame ownership use the production paths, with ordinary PTEs and charges.
use super::*;
use crate::{
    arch::CurrentIrqArch,
    cgroup::memory::accounting::MemoryPolicy,
    exception::InterruptArch,
    mm::{
        allocator::buddy::compaction::IsolationScratch,
        allocator::page_frame::VirtPageFrame,
        compaction::{try_allocate_in_range, Compactor, Isolation, Limits, Request, Stop},
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
        let mm = AddressSpace::new(false, INIT_USER_NAMESPACE.clone(), 1)
            .map_err(|_| SystemError::ENOMEM)?;
        let account =
            MemoryAccount::new(NEXT_ACCOUNT.fetch_add(1, Ordering::Relaxed) as u64, None)?;
        account.set_policy(MemoryPolicy {
            max: Some(SOURCES as u64),
            ..MemoryPolicy::default()
        });
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
        fixture
            .sources
            .try_reserve_exact(SOURCES)
            .map_err(|_| SystemError::ENOMEM)?;
        let (base, count) = unsafe {
            LockedFrameAllocator
                .allocate_below(PageFrameCount::new(BACKING), PhysAddr::new(usize::MAX))
        }
        .ok_or(SystemError::ENOMEM)?;
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
            let charge = fixture
                .account
                .try_charge(MemoryKind::Anon, 1)
                .map_err(|_| SystemError::ENOMEM)?;
            // The Page constructor consumes its frame even if Arc allocation fails.
            fixture.held[2 * index] = false;
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
            register(&page)?;
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

    // Call only after all fixture/test metadata and allocator scratch exist.
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

fn prepared_compactor() -> Result<Compactor, SystemError> {
    let mut compactor = Compactor::new();
    compactor.prepare(ORDER).map_err(|_| SystemError::ENOMEM)?;
    Ok(compactor)
}

fn real_fragmented_capture() -> Result<bool, SystemError> {
    let mut fixture = Fixture::new(false, false)?;
    let mut compactor = prepared_compactor()?;
    let request = fixture.request();
    let mut evidence = IsolationScratch::try_new(ORDER + 1).map_err(|_| SystemError::ENOMEM)?;
    let mut entries = request.limits.entries;
    fixture.release_holes();
    // Measure actual eligible free ownership in the entire 2N domain, not
    // guest-wide counters or merely the number of free calls made by setup.
    let domain = Isolation::begin(request.start, ORDER + 1, &mut evidence, &mut entries)
        .map_err(|_| SystemError::EBUSY)?;
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
    let mut compactor = prepared_compactor()?;
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
    let mut compactor = prepared_compactor()?;
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
    let mut compactor = prepared_compactor()?;
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

fn deferred_allocation_fastpath() -> Result<bool, SystemError> {
    let mut fixture = Fixture::new(false, true)?;
    let mut compactor = prepared_compactor()?;
    let mut request = fixture.request();
    request.limits.targets = 1;
    request.limits.no_progress = 1;
    fixture.release_holes();
    let first = compactor.allocate(request);
    if !matches!(first.outcome, Err(Stop::Blocked)) || first.stats.moved != 0 {
        return Ok(false);
    }
    let deferred = compactor.allocate(request);
    let deferred_ok = matches!(deferred.outcome, Err(Stop::Deferred)) && deferred.stats.moved == 0;
    if !fixture.retire_first_target() {
        return Ok(false);
    }
    let ready = compactor.allocate(request);
    let passed =
        deferred_ok && ready.outcome.is_ok() && ready.stats.migrations == 0 && fixture.valid();
    drop(ready);
    Ok(fixture.finish(passed))
}

fn small_budget() -> Result<bool, SystemError> {
    let mut fixture = Fixture::new(false, false)?;
    let mut compactor = prepared_compactor()?;
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
    let mut scratch = IsolationScratch::try_new(ORDER).map_err(|_| SystemError::ENOMEM)?;
    let request = fixture.request();
    let mut entries = request.limits.entries;
    fixture.release_holes();
    let mut lease = Isolation::begin(request.start, ORDER, &mut scratch, &mut entries)
        .map_err(|_| SystemError::EBUSY)?;
    let initial_free = lease.free_pages();
    let second_busy = matches!(
        Isolation::begin(request.start, ORDER, &mut scratch, &mut entries),
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

pub(crate) fn report() -> alloc::string::String {
    let _serial = TEST_LOCK.lock();
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
            deferred_allocation_fastpath().unwrap_or(false),
        ),
        ("scan_budget_no_mutation", small_budget().unwrap_or(false)),
        (
            "isolation_allocation_free_window",
            isolation_allocation_free_window().unwrap_or(false),
        ),
        ("atomic_rejected", atomic_rejected()),
        (
            "repeated_capture",
            real_fragmented_capture().unwrap_or(false),
        ),
    ];
    checks.extend(crate::mm::allocator::buddy::compaction::selftest_checks());
    let failed = checks.iter().filter(|(_, passed)| !passed).count();
    let mut output = alloc::format!("status={}\n", if failed == 0 { "ok" } else { "fail" });
    for (name, passed) in checks {
        let _ = writeln!(output, "{name}={}", if passed { "ok" } else { "fail" });
    }
    let _ = writeln!(output, "summary_fail={failed}");
    output
}
