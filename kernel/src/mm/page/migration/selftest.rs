//! Private migration fixtures and per-invocation diagnostic controls.
//! Default kernels contain neither this command interface nor fault observation.
use super::{migrate_with_control, MigrationError, MigrationResult, MigrationStage};
use crate::{
    arch::MMArch,
    cgroup::memory::{accounting::MemoryPolicy, MemoryAccount, MemoryKind},
    libs::{mutex::Mutex, spinlock::SpinLock},
    mm::{
        allocator::page_frame::{PageFrameCount, VirtPageFrame},
        page::{
            allocate_registered_data_page, page_manager_lock, EntryFlags, Page, PageFlags, PageType,
        },
        syscall::{MapFlags, ProtFlags},
        ucontext::AddressSpace,
        MemoryManagementArch, PhysAddr, VirtAddr,
    },
    process::{namespace::user_namespace::INIT_USER_NAMESPACE, ProcessManager},
    time::clocksource::HZ,
};
use alloc::{format, string::String, sync::Arc};
use core::{
    fmt::Write,
    sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};
use system_error::SystemError;

static TEST_LOCK: Mutex<()> = Mutex::new(());
static NEXT_ACCOUNT: AtomicU64 = AtomicU64::new(0x0050_0000_0000_0000);
static FAULT_OBSERVER: SpinLock<Option<Arc<FaultObserver>>> = SpinLock::new(None);

#[derive(Clone, Copy, Debug)]
pub(crate) enum Mode {
    Normal,
    Fail(MigrationStage),
    WaitFault,
}

#[derive(Debug)]
struct FaultObserver {
    mm: AtomicUsize,
    address: AtomicUsize,
    generation: AtomicUsize,
    armed: AtomicUsize,
    observed: AtomicUsize,
}

#[derive(Debug)]
pub(crate) struct Control {
    mode: Mode,
    fault: Arc<FaultObserver>,
    committed: AtomicBool,
    extra_owner: AtomicBool,
    old: AtomicUsize,
    new: AtomicUsize,
}

impl Control {
    pub(crate) fn new(mode: Mode) -> Self {
        Self {
            mode,
            fault: Arc::new(FaultObserver {
                mm: AtomicUsize::new(0),
                address: AtomicUsize::new(0),
                generation: AtomicUsize::new(0),
                armed: AtomicUsize::new(0),
                observed: AtomicUsize::new(0),
            }),
            committed: AtomicBool::new(false),
            extra_owner: AtomicBool::new(false),
            old: AtomicUsize::new(0),
            new: AtomicUsize::new(0),
        }
    }

    pub(crate) fn at(
        &self,
        stage: MigrationStage,
        mm: &Arc<AddressSpace>,
        address: VirtAddr,
    ) -> Result<(), MigrationError> {
        if let Mode::Fail(selected) = self.mode {
            if selected == stage {
                return Err(MigrationError::Injected);
            }
        }
        if matches!(self.mode, Mode::WaitFault) && stage == MigrationStage::Prepared {
            self.fault.armed.store(0, Ordering::Release);
            self.fault.generation.fetch_add(1, Ordering::Relaxed);
            self.fault
                .mm
                .store(Arc::as_ptr(mm) as usize, Ordering::Relaxed);
            self.fault.address.store(address.data(), Ordering::Relaxed);
            {
                let mut slot = FAULT_OBSERVER.lock_irqsave();
                // Production migration serialization permits one invocation.
                if slot
                    .as_ref()
                    .is_some_and(|other| !Arc::ptr_eq(other, &self.fault))
                {
                    return Err(MigrationError::Busy);
                }
                *slot = Some(self.fault.clone());
            }
        }
        if matches!(self.mode, Mode::WaitFault) && stage == MigrationStage::Flushed {
            let generation = self.fault.generation.load(Ordering::Relaxed);
            let began = crate::time::timer::clock();
            while self.fault.observed.load(Ordering::Acquire) != generation
                && crate::time::timer::clock().wrapping_sub(began) < 5 * HZ
            {
                crate::sched::sched_yield();
            }
            let observed = self.fault.observed.load(Ordering::Acquire) == generation;
            if !observed {
                return Err(MigrationError::Injected);
            }
        }
        Ok(())
    }

    /// Arm before the atomic revoke, closing the marker-to-observer window.
    pub(crate) fn before_revoke(&self) {
        self.fault.armed.store(
            self.fault.generation.load(Ordering::Relaxed),
            Ordering::Release,
        );
    }

    /// Record the real source-consumption refusal, not earlier contention.
    pub(crate) fn extra_owner_rejected(&self) {
        self.extra_owner.store(true, Ordering::Release);
    }

    /// Called by the real transaction only after old Page/frame destruction.
    pub(crate) fn committed(&self, old: PhysAddr, new: PhysAddr) {
        self.old.store(old.data(), Ordering::Relaxed);
        self.new.store(new.data(), Ordering::Relaxed);
        self.committed.store(true, Ordering::Release);
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        let retired = {
            let mut slot = FAULT_OBSERVER.lock_irqsave();
            if slot
                .as_ref()
                .is_some_and(|active| Arc::ptr_eq(active, &self.fault))
            {
                slot.take()
            } else {
                None
            }
        };
        drop(retired);
    }
}

/// Observe real fault lock contention; do not replace the normal fault path.
pub(crate) fn fault_enter(mm: &Arc<AddressSpace>, address: VirtAddr) {
    let observer = FAULT_OBSERVER.lock_irqsave().clone();
    let Some(observer) = observer else {
        return;
    };
    let generation = observer.armed.load(Ordering::Acquire);
    if generation != 0
        && observer.mm.load(Ordering::Relaxed) == Arc::as_ptr(mm) as usize
        && observer.address.load(Ordering::Relaxed) == address.data() & !MMArch::PAGE_OFFSET_MASK
        && mm.try_write().is_none()
    {
        observer.observed.fetch_max(generation, Ordering::Release);
    }
}

fn migrate(
    mm: &Arc<AddressSpace>,
    address: VirtAddr,
    control: &Control,
) -> Result<MigrationResult, MigrationError> {
    // Retry only unrelated lock contention, outside every resource guard.
    for _ in 0..128 {
        match migrate_with_control(mm, address, control) {
            Err(MigrationError::Busy) => crate::sched::sched_yield(),
            result => return result,
        }
    }
    Err(MigrationError::Busy)
}

/// Fixed 16-byte little-endian command: own VA, mode, zero reserved word.
/// The callback stores this returned report in that open file's snapshot.
pub(crate) fn command(bytes: &[u8]) -> Result<String, SystemError> {
    if bytes.len() != 16 {
        return Err(SystemError::EINVAL);
    }
    let address = u64::from_le_bytes(bytes[..8].try_into().unwrap()) as usize;
    let mode_number = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    if bytes[12..] != [0, 0, 0, 0]
        || address & MMArch::PAGE_OFFSET_MASK != 0
        || address >= MMArch::USER_END_VADDR.data()
    {
        return Err(SystemError::EINVAL);
    }
    let mode = match mode_number {
        0 => Mode::Normal,
        1 => Mode::Fail(MigrationStage::Prepared),
        2 => Mode::Fail(MigrationStage::Revoked),
        3 => Mode::Fail(MigrationStage::Flushed),
        4 => Mode::Fail(MigrationStage::Copied),
        5 => Mode::Fail(MigrationStage::BeforeCommit),
        6 => Mode::WaitFault,
        _ => return Err(SystemError::EINVAL),
    };
    let mm = ProcessManager::current_pcb()
        .basic()
        .user_vm()
        .ok_or(SystemError::EINVAL)?;
    let control = Control::new(mode);
    let result = migrate(&mm, VirtAddr::new(address), &control);
    let (status, error) = match result {
        Ok(_) => ("ok", "none"),
        Err(MigrationError::Busy) => ("fail", "busy"),
        Err(MigrationError::Unsupported(_)) => ("fail", "unsupported"),
        Err(MigrationError::InvalidMapping) => ("fail", "invalid_mapping"),
        Err(MigrationError::NoMemory) => ("fail", "no_memory"),
        Err(MigrationError::Injected) => ("fail", "injected"),
    };
    Ok(format!(
        "status={status}\nerror={error}\nold_phys={:#x}\nnew_phys={:#x}\nfault_wait={}\n",
        control.old.load(Ordering::Relaxed),
        control.new.load(Ordering::Relaxed),
        if control.fault.generation.load(Ordering::Relaxed) != 0
            && control.fault.observed.load(Ordering::Acquire)
                == control.fault.generation.load(Ordering::Relaxed)
        {
            "ok"
        } else {
            "absent"
        }
    ))
}

struct Fixture {
    mm: Arc<AddressSpace>,
    account: Arc<MemoryAccount>,
    address: VirtAddr,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Retire the private mapping even if an assertion below returns early.
        let _ = self
            .mm
            .munmap_wait(VirtPageFrame::new(self.address), PageFrameCount::ONE);
    }
}

impl Fixture {
    fn new(lru: bool) -> Result<Self, SystemError> {
        let mm = AddressSpace::new(false, INIT_USER_NAMESPACE.clone(), 1)
            .map_err(|_| SystemError::ENOMEM)?;
        let account = MemoryAccount::new(NEXT_ACCOUNT.fetch_add(1, Ordering::Relaxed), None)?;
        account.set_policy(MemoryPolicy {
            max: Some(1),
            ..MemoryPolicy::default()
        });
        let address = mm
            .map_anonymous_wait(
                VirtAddr::new(0),
                MMArch::PAGE_SIZE,
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS,
                true,
                false,
            )?
            .virt_address();
        let charge = account
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
        let published = (|| {
            let mut space = mm.write();
            let vma = space
                .mappings
                .contains(address)
                .ok_or(SystemError::EFAULT)?;
            let _pt = mm.page_table_edit();
            let flags = EntryFlags::<MMArch>::from_prot_flags(
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                true,
            )
            // Private VMA protection templates start read-only for COW.
            // This fixture owns a fresh exclusive frame and fills it below:
            // model the completed first write, as do_wp_page's one-source
            // reuse path sets writable/dirty (Linux do_anonymous_page does
            // this directly for a writable private anonymous allocation).
            .set_write(true)
            .set_access(true)
            .set_dirty(true);
            let flush = unsafe {
                space
                    .user_mapper
                    .utable
                    .map_phys(address, page.phys_address(), flags)
            }
            .ok_or(SystemError::ENOMEM)?;
            page.write().insert_vma(vma, false);
            mm.account_present_page_add();
            flush.flush();
            Ok::<(), SystemError>(())
        })();
        if let Err(error) = published {
            crate::mm::page::retire_unmapped_normal_page(&page);
            return Err(error);
        }
        // Distinct bytes across the complete page catch partial copies.
        {
            let mut data = page.write();
            for (index, byte) in unsafe { data.as_slice_mut() }.iter_mut().enumerate() {
                *byte = (index.wrapping_mul(37) ^ (index >> 5)) as u8;
            }
        }
        Ok(Self {
            mm,
            account,
            address,
        })
    }

    fn page(&self) -> Option<Arc<Page>> {
        let physical = self.mm.read().user_mapper.utable.translate(self.address)?.0;
        page_manager_lock().get_registered(&physical)
    }
    fn mapping(&self) -> Option<(PhysAddr, usize)> {
        self.mm
            .read()
            .user_mapper
            .utable
            .translate(self.address)
            .map(|(p, f)| (p, f.data()))
    }
    fn valid(&self) -> bool {
        let Some(page) = self.page() else {
            return false;
        };
        if page
            .memory_account()
            .is_none_or(|a| !Arc::ptr_eq(a, &self.account))
        {
            return false;
        }
        let guard = page.read();
        guard.map_count() == 1
            && unsafe { guard.as_slice() }
                .iter()
                .enumerate()
                .all(|(i, b)| *b == (i.wrapping_mul(37) ^ (i >> 5)) as u8)
            && self.account.snapshot().anon == 1
            && self.account.current_pages() == 1
            && self.mm.resident_pages() == 1
    }
    fn unmap(&self) -> bool {
        self.mm
            .munmap_wait(VirtPageFrame::new(self.address), PageFrameCount::ONE)
            .is_ok()
            && self.account.current_pages() == 0
    }
}

fn identity_and_data() -> bool {
    let Ok(f) = Fixture::new(false) else {
        return false;
    };
    let before = f.mapping().unwrap();
    let weak = Arc::downgrade(&f.page().unwrap());
    let control = Control::new(Mode::Normal);
    let result = migrate(&f.mm, f.address, &control);
    let after = f.mapping().unwrap();
    let ok = result.is_ok()
        && before.0 != after.0
        && before.1 == after.1
        && f.valid()
        && control.committed.load(Ordering::Acquire)
        && weak.upgrade().is_none()
        && page_manager_lock().get_registered(&before.0).is_none();
    ok && f.unmap()
}

fn charge_at_limit(offline: bool) -> bool {
    let Ok(f) = Fixture::new(false) else {
        return false;
    };
    if offline {
        f.account.offline();
    }
    let before = f.account.snapshot();
    if Arc::ptr_eq(&f.account, &f.mm.memory_account()) {
        return false;
    }
    let ok = migrate(&f.mm, f.address, &Control::new(Mode::Normal)).is_ok() && f.valid();
    let after = f.account.snapshot();
    ok && before.current == after.current
        && before.peak == after.peak
        && before.events == after.events
        && f.unmap()
}

fn pin_and_arc() -> bool {
    let Ok(f) = Fixture::new(false) else {
        return false;
    };
    let old = f.mapping();
    let page = f.page().unwrap();
    let pin = page.try_pin_access().unwrap();
    let pin_busy = matches!(
        migrate_with_control(&f.mm, f.address, &Control::new(Mode::Normal)),
        Err(MigrationError::Busy)
    );
    drop(pin);
    let control = Control::new(Mode::Normal);
    let mut arc_busy = false;
    for _ in 0..128 {
        let result = migrate_with_control(&f.mm, f.address, &control);
        if control.extra_owner.load(Ordering::Acquire) {
            arc_busy = matches!(result, Err(MigrationError::Busy));
            break;
        }
        if !matches!(result, Err(MigrationError::Busy)) {
            return false;
        }
        crate::sched::sched_yield();
    }
    let restored = old == f.mapping() && f.valid();
    drop(page);
    pin_busy
        && arc_busy
        && restored
        && migrate(&f.mm, f.address, &Control::new(Mode::Normal)).is_ok()
        && f.valid()
        && f.unmap()
}

fn rollback() -> bool {
    let Ok(f) = Fixture::new(false) else {
        return false;
    };
    let before = f.mapping();
    let current = f.account.current_pages();
    for stage in [
        MigrationStage::Prepared,
        MigrationStage::Revoked,
        MigrationStage::Flushed,
        MigrationStage::Copied,
        MigrationStage::BeforeCommit,
    ] {
        if !matches!(
            migrate(&f.mm, f.address, &Control::new(Mode::Fail(stage))),
            Err(MigrationError::Injected)
        ) || before != f.mapping()
            || !f.valid()
            || current != f.account.current_pages()
        {
            return false;
        }
    }
    f.unmap()
}

fn unsupported_lru() -> bool {
    let Ok(f) = Fixture::new(true) else {
        return false;
    };
    let before = f.mapping();
    let unsupported = matches!(
        migrate(&f.mm, f.address, &Control::new(Mode::Normal)),
        Err(MigrationError::Unsupported(_))
    );
    let unchanged = before == f.mapping()
        && f.valid()
        && f.page()
            .is_some_and(|page| page.read().flags().contains(PageFlags::PG_LRU));
    // This fixture explicitly created an anonymous LRU owner. Production
    // anonymous munmap does not yet retire that owner (tracked separately as
    // MM-139), outside migration's supported range. First prove rejection
    // preserved all state, then clean up only this private fixture's owner
    // after real munmap has cleared PTEs and synchronously invalidated TLBs.
    let page = f.page().unwrap();
    let unmapped =
        f.mm.munmap_wait(VirtPageFrame::new(f.address), PageFrameCount::ONE)
            .is_ok();
    if unmapped {
        crate::mm::page::detach_pages_from_reclaimer_batched(core::iter::once(&page));
    }
    drop(page);
    unsupported && unchanged && unmapped && f.account.current_pages() == 0
}

fn repeated() -> bool {
    let Ok(f) = Fixture::new(false) else {
        return false;
    };
    for _ in 0..32 {
        let old = f.mapping().unwrap().0;
        let control = Control::new(Mode::Normal);
        if migrate(&f.mm, f.address, &control).is_err()
            || !f.valid()
            || f.mapping().unwrap().0 == old
            || !control.committed.load(Ordering::Acquire)
            || page_manager_lock().get_registered(&old).is_some()
        {
            return false;
        }
    }
    f.unmap()
}

fn pin_hva(
    mm: &Arc<AddressSpace>,
    address: VirtAddr,
    write: bool,
) -> Result<(crate::mm::page::access::PageAccessPin, bool), SystemError> {
    for _ in 0..128 {
        match crate::virt::vm::kvm_host::mem::try_pin_hva(mm, address, write) {
            Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => crate::sched::sched_yield(),
            result => return result,
        }
    }
    Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
}

fn atomic_hva(details: &mut String) -> bool {
    let Ok(f) = Fixture::new(false) else {
        let _ = writeln!(details, "hva_stage=fixture");
        return false;
    };
    let before = f.mapping();
    let (pin, writable) = match pin_hva(&f.mm, f.address, true) {
        Ok(value) => value,
        Err(error) => {
            let _ = writeln!(
                details,
                "hva_stage=resident_write_pin error={error:?} mapping={before:?}"
            );
            return false;
        }
    };
    let pinned = writable
        && Some(pin.phys_address()) == before.map(|(p, _)| p)
        && matches!(
            migrate_with_control(&f.mm, f.address, &Control::new(Mode::Normal)),
            Err(MigrationError::Busy)
        );
    drop(pin);
    let lazy = match f.mm.map_anonymous_wait(
        VirtAddr::new(0),
        MMArch::PAGE_SIZE,
        ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
        MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS,
        true,
        false,
    ) {
        Ok(mapping) => mapping.virt_address(),
        Err(error) => {
            let _ = writeln!(details, "hva_stage=lazy_map error={error:?}");
            return false;
        }
    };
    let missing_result = pin_hva(&f.mm, lazy, false);
    let missing = matches!(&missing_result, Err(SystemError::EFAULT))
        && f.mm.read().user_mapper.utable.translate(lazy).is_none();
    drop(missing_result);
    if let Err(error) = f.mm.mprotect_wait(
        VirtPageFrame::new(f.address),
        PageFrameCount::ONE,
        ProtFlags::PROT_READ,
    ) {
        let _ = writeln!(details, "hva_stage=mprotect error={error:?}");
        return false;
    }
    let readonly = matches!(pin_hva(&f.mm, f.address, true), Err(SystemError::EFAULT));
    let readable_result = pin_hva(&f.mm, f.address, false);
    let readable = match readable_result {
        Ok((pin, writable)) => {
            drop(pin);
            !writable
        }
        Err(_) => false,
    };
    let migration_result = migrate(&f.mm, f.address, &Control::new(Mode::Normal));
    let valid = f.valid();
    let _ =
        f.mm.munmap_wait(VirtPageFrame::new(lazy), PageFrameCount::ONE);
    let unmapped = f.unmap();
    let ok =
        pinned && missing && readonly && readable && migration_result.is_ok() && valid && unmapped;
    if !ok {
        let _ = writeln!(details,
        "hva_stage=checks pinned={pinned} missing={missing} readonly={readonly} readable={readable} migration={migration_result:?} valid={valid} unmap={unmapped}");
    }
    ok
}

pub(crate) fn report() -> String {
    let _serial = TEST_LOCK.lock();
    let mut details = String::new();
    let checks = [
        ("identity_data_permissions", identity_and_data()),
        ("unique_charge_full_limit", charge_at_limit(false)),
        ("offline_owner", charge_at_limit(true)),
        ("pin_and_extra_arc", pin_and_arc()),
        ("rollback_stages", rollback()),
        ("unsupported_lru", unsupported_lru()),
        ("repeated_migration", repeated()),
        ("atomic_hva_pin", atomic_hva(&mut details)),
    ];
    let failed = checks.iter().filter(|(_, ok)| !ok).count();
    let mut output = format!("status={}\n", if failed == 0 { "ok" } else { "fail" });
    for (key, ok) in checks {
        let _ = writeln!(output, "{key}={}", if ok { "ok" } else { "fail" });
    }
    output.push_str(&details);
    let _ = writeln!(output, "summary_fail={failed}");
    output
}
