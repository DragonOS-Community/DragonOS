//! Real mapped-page ownership tests; failed fixtures are rescued only after
//! recording failure. Rescue is never evidence of successful retirement.
use crate::{
    arch::MMArch,
    cgroup::memory::{MemoryAccount, MemoryKind, MemoryStat},
    filesystem::page_cache::PageCache,
    libs::{
        mutex::Mutex,
        resource_wait::{self, ResourceMutexGuard},
        rwsem::RwSem,
    },
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
    process::{
        kthread::{KernelThreadClosure, KernelThreadMechanism},
        namespace::user_namespace::INIT_USER_NAMESPACE,
        preempt::PreemptGuard,
        ProcessManager,
    },
    sched::completion::Completion,
    smp::cpu::{smp_cpu_manager, ProcessorId},
    time::clocksource::HZ,
};
use alloc::{
    boxed::Box,
    format,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    fmt::Write,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
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

fn multibatch_teardown(mode: usize) -> Result<bool, SystemError> {
    // Two complete 32-page gathers and a trailing page. Both tracked active
    // teardown and final-Arc inactive teardown must retire every real owner.
    let count = 65;
    let mut f = Fixture::new(count, true)?;
    let generation = f.mm.tlb_gen.load(Ordering::SeqCst);
    match mode {
        0 => {
            if !f.unmap(0, count) {
                return Ok(false);
            }
        }
        1 => unsafe { f.mm.write().unmap_all() },
        _ => {
            let empty = AddressSpace::new(false, INIT_USER_NAMESPACE.clone(), 1)
                .map_err(|_| SystemError::ENOMEM)?;
            drop(core::mem::replace(&mut f.mm, empty));
            return Ok(f.freed());
        }
    }
    Ok(f.freed() && f.mm.tlb_gen.load(Ordering::SeqCst).wrapping_sub(generation) >= 3)
}

/// Exercise natural MM teardown itself with the ordinary heap denied. The
/// fixture was never selected/killed, so no reaper job can do this work first.
fn heap_denied_teardown(inactive: bool) -> Result<bool, SystemError> {
    use crate::mm::oom::denial;

    let mut f = Fixture::new(65, true)?;
    let generation = f.mm.tlb_gen.load(Ordering::SeqCst);
    // Every fallible fixture allocation and replacement happens before denial.
    let old_mm = if inactive {
        let empty = AddressSpace::new(false, INIT_USER_NAMESPACE.clone(), 1)
            .map_err(|_| SystemError::ENOMEM)?;
        Some(core::mem::replace(&mut f.mm, empty))
    } else {
        None
    };
    let originally_denied = denial::is_current_denied();
    let probes_denied = {
        let _heap_scope = denial::Scope::enter(true);
        let probes_denied = denial::probe_denied();
        if let Some(mm) = old_mm {
            // This is the actual final strong reference, not a synthetic
            // leaf-zap or an OOM worker operation on a still-owned MM.
            drop(mm);
        } else {
            unsafe { f.mm.write().unmap_all() };
        }
        // Re-probe after the actual retirement, while the scope is still
        // active: teardown must not silently disable the denial mid-operation.
        let still_denied = denial::probe_denied();
        probes_denied && still_denied
    };
    // Both result inspection and any fixture failure cleanup run after the
    // test-local scope restores the caller's original allocator state.
    Ok(probes_denied
        && denial::is_current_denied() == originally_denied
        && f.freed()
        && (inactive || f.mm.tlb_gen.load(Ordering::SeqCst).wrapping_sub(generation) >= 3))
}

fn bounded_poll(mut ready: impl FnMut() -> bool) -> bool {
    let began = crate::time::timer::clock();
    // Also bounded if timer progress is disrupted by a broken lock protocol.
    for _ in 0..20_000_000 {
        if ready() {
            return true;
        }
        if crate::time::timer::clock().wrapping_sub(began) >= 5 * HZ {
            break;
        }
        core::hint::spin_loop();
    }
    false
}

fn contended_memory_resource<T: Send + Sync + 'static>(
    lock: fn() -> ResourceMutexGuard<'static, T>,
    holder_cpu: ProcessorId,
) -> bool {
    use crate::mm::oom::denial;

    let initiator = ProcessManager::current_pcb();
    let worker_initiator = initiator.clone();
    let ready = Arc::new(AtomicBool::new(false));
    let observed = Arc::new(AtomicBool::new(false));
    let acquired = Arc::new(AtomicBool::new(false));
    let rescued = Arc::new(AtomicBool::new(false));
    let complete = Arc::new(Completion::new());
    let worker_ready = ready.clone();
    let worker_observed = observed.clone();
    let worker_acquired = acquired.clone();
    let worker_rescued = rescued.clone();
    let worker_complete = complete.clone();
    let closure = KernelThreadClosure::EmptyClosure((
        Box::new(move || {
            let held = lock();
            let key = held.resource_key_for_test();
            // Only the holder's finite atomic/queue observation window is
            // nonpreemptible. The real contender must remain able to sleep.
            let preempt = PreemptGuard::new();
            worker_ready.store(true, Ordering::Release);
            let registered = bounded_poll(|| {
                resource_wait::is_waiting_for_resource_for_test(&worker_initiator, key)
            });
            worker_observed.store(registered, Ordering::Release);
            drop(held);
            drop(preempt);
            // Failure rescue is outside both the resource and queue locks.
            // It cannot turn a missing production notification into success.
            if !bounded_poll(|| worker_acquired.load(Ordering::Acquire)) {
                worker_rescued.store(true, Ordering::Release);
                let _ = ProcessManager::wakeup(&worker_initiator);
            }
            worker_complete.complete();
            0
        }),
        (),
    ));
    let Some(holder) =
        KernelThreadMechanism::create_on_cpu(closure, "memory-lock-holder".to_string(), holder_cpu)
    else {
        return false;
    };
    if ProcessManager::wakeup(&holder).is_err() {
        let _ = KernelThreadMechanism::stop(&holder);
        return false;
    }
    // Atomic readiness avoids allocating a completion waiter while the holder
    // owns a global page registry lock. All fixtures already exist before deny.
    let ready_ok = bounded_poll(|| ready.load(Ordering::Acquire));
    let originally_denied = denial::is_current_denied();
    let denied_ok = if ready_ok {
        let _scope = denial::Scope::enter(true);
        let before = denial::probe_denied();
        let held = lock();
        acquired.store(true, Ordering::Release);
        drop(held);
        let after = denial::probe_denied();
        before && after
    } else {
        false
    };
    let restored = denial::is_current_denied() == originally_denied;
    let completed = complete
        .wait_for_completion_timeout((10 * HZ) as i64)
        .is_ok_and(|remaining| remaining > 0);
    if !completed {
        let _ = KernelThreadMechanism::request_stop(&holder);
        return false;
    }
    let stopped = matches!(KernelThreadMechanism::stop(&holder), Ok(0));
    ready_ok
        && denied_ok
        && restored
        && stopped
        && observed.load(Ordering::Acquire)
        && acquired.load(Ordering::Acquire)
        && !rescued.load(Ordering::Acquire)
}

struct MmContentionState {
    ready: AtomicBool,
    attempting: AtomicBool,
    observed: AtomicBool,
    done: AtomicBool,
    rescued: AtomicBool,
}

fn hold_for_mm_contention<G>(
    held: G,
    initiator: &Arc<crate::process::ProcessControlBlock>,
    state: &MmContentionState,
    resource_key: Option<usize>,
) {
    let preempt = PreemptGuard::new();
    state.ready.store(true, Ordering::Release);
    // This private initiator has no other sleep after publishing attempting:
    // its real mmput must first contend on the precise guard held here.
    let blocked = bounded_poll(|| {
        state.attempting.load(Ordering::Acquire)
            && resource_key.map_or_else(
                || initiator.sched_info().state().is_blocked(),
                |key| resource_wait::is_waiting_for_resource_for_test(initiator, key),
            )
    });
    state.observed.store(blocked, Ordering::Release);
    drop(held);
    drop(preempt);
}

fn contended_mm_resource(holder_cpu: ProcessorId, resource: ContendedResource) -> bool {
    use crate::mm::oom::denial;

    let Ok(fixture) = Fixture::new(65, true) else {
        return false;
    };
    let initiator = ProcessManager::current_pcb();
    let worker_initiator = initiator.clone();
    let worker_mm = fixture.mm.clone();
    // Only the holder owns these additional pins. Join it before checking Weak
    // disappearance: publishing its completion precedes closure destruction.
    let worker_page = if matches!(resource, ContendedResource::PageWrite) {
        fixture.pages[0].upgrade()
    } else {
        None
    };
    let worker_vma = if matches!(resource, ContendedResource::Vma) {
        fixture.mm.read().mappings.contains(fixture.address)
    } else {
        None
    };
    let state = Arc::new(MmContentionState {
        ready: AtomicBool::new(false),
        attempting: AtomicBool::new(false),
        observed: AtomicBool::new(false),
        done: AtomicBool::new(false),
        rescued: AtomicBool::new(false),
    });
    let worker_state = state.clone();
    let complete = Arc::new(Completion::new());
    let worker_complete = complete.clone();
    let closure = KernelThreadClosure::EmptyClosure((
        Box::new(move || {
            match resource {
                ContendedResource::PageTableEdit => hold_for_mm_contention(
                    worker_mm.page_table_edit(),
                    &worker_initiator,
                    &worker_state,
                    None,
                ),
                ContendedResource::MmWrite => hold_for_mm_contention(
                    worker_mm.write(),
                    &worker_initiator,
                    &worker_state,
                    None,
                ),
                ContendedResource::PageWrite => {
                    let Some(page) = worker_page.as_ref() else {
                        return 1;
                    };
                    let key = page.inner.resource_key_for_test();
                    hold_for_mm_contention(
                        page.write(),
                        &worker_initiator,
                        &worker_state,
                        Some(key),
                    );
                }
                ContendedResource::Vma => {
                    let Some(vma) = worker_vma.as_ref() else {
                        return 1;
                    };
                    let held = vma.lock();
                    let key = held.resource_key_for_test();
                    hold_for_mm_contention(held, &worker_initiator, &worker_state, Some(key));
                }
                _ => return 1,
            }
            // No MM lock is held across the bounded failure rescue.
            if !bounded_poll(|| worker_state.done.load(Ordering::Acquire)) {
                worker_state.rescued.store(true, Ordering::Release);
                let _ = ProcessManager::wakeup(&worker_initiator);
            }
            worker_complete.complete();
            0
        }),
        (),
    ));
    let Some(holder) =
        KernelThreadMechanism::create_on_cpu(closure, "mm-lock-holder".to_string(), holder_cpu)
    else {
        return false;
    };
    if ProcessManager::wakeup(&holder).is_err() {
        let _ = KernelThreadMechanism::stop(&holder);
        return false;
    }
    let ready = bounded_poll(|| state.ready.load(Ordering::Acquire));
    // Arc clones only pin lock lifetime; they do not add installed MM users.
    let last_user = fixture.mm.user_count() == 1;
    let originally_denied = denial::is_current_denied();
    let probes = if ready && last_user {
        let _scope = denial::Scope::enter(true);
        let before = denial::probe_denied();
        state.attempting.store(true, Ordering::Release);
        fixture.mm.mmput();
        let after = denial::probe_denied();
        before && after
    } else {
        false
    };
    state.done.store(true, Ordering::Release);
    let restored = denial::is_current_denied() == originally_denied;
    let completed = complete
        .wait_for_completion_timeout((10 * HZ) as i64)
        .is_ok_and(|remaining| remaining > 0);
    if !completed {
        let _ = KernelThreadMechanism::request_stop(&holder);
        return false;
    }
    let stopped = matches!(KernelThreadMechanism::stop(&holder), Ok(0));
    ready
        && last_user
        && probes
        && restored
        && stopped
        && state.observed.load(Ordering::Acquire)
        && !state.rescued.load(Ordering::Acquire)
        && fixture.mm.user_count() == 0
        && fixture.mm.is_torn_down()
        && fixture.freed()
}

// Queue the reader first, then a writer. Releasing the initial writer must
// notify both: waking only the reader deadlocks behind writer preference.
fn mixed_rwsem_broadcast(worker_cpu: ProcessorId) -> bool {
    use crate::mm::oom::denial;

    let lock = Arc::new(RwSem::new_resource(0usize));
    let reader_passed = Arc::new(AtomicBool::new(false));
    let writer_passed = Arc::new(AtomicBool::new(false));
    let reader_done = Arc::new(AtomicBool::new(false));
    let writer_done = Arc::new(AtomicBool::new(false));
    let reader_lock = lock.clone();
    let reader_result = reader_passed.clone();
    let reader_complete = reader_done.clone();
    let reader_closure = KernelThreadClosure::EmptyClosure((
        Box::new(move || {
            let originally_denied = denial::is_current_denied();
            let passed = {
                let _scope = denial::Scope::enter(true);
                let before = denial::probe_denied();
                let observed = *reader_lock.read();
                let after = denial::probe_denied();
                before && after && observed == 1
            };
            reader_result.store(
                passed && denial::is_current_denied() == originally_denied,
                Ordering::Release,
            );
            reader_complete.store(true, Ordering::Release);
            0
        }),
        (),
    ));
    let Some(reader) = KernelThreadMechanism::create_on_cpu(
        reader_closure,
        "resource-reader".to_string(),
        worker_cpu,
    ) else {
        return false;
    };
    let writer_lock = lock.clone();
    let writer_result = writer_passed.clone();
    let writer_complete = writer_done.clone();
    let writer_closure = KernelThreadClosure::EmptyClosure((
        Box::new(move || {
            let originally_denied = denial::is_current_denied();
            let passed = {
                let _scope = denial::Scope::enter(true);
                let before = denial::probe_denied();
                *writer_lock.write() = 1;
                let after = denial::probe_denied();
                before && after
            };
            writer_result.store(
                passed && denial::is_current_denied() == originally_denied,
                Ordering::Release,
            );
            writer_complete.store(true, Ordering::Release);
            0
        }),
        (),
    ));
    let Some(writer) = KernelThreadMechanism::create_on_cpu(
        writer_closure,
        "resource-writer".to_string(),
        worker_cpu,
    ) else {
        let _ = KernelThreadMechanism::stop(&reader);
        return false;
    };
    // All allocations and both task births precede either worker's deny scope.
    let held = lock.write();
    let key = lock.resource_key_for_test();
    let reader_registered = ProcessManager::wakeup(&reader).is_ok()
        && bounded_poll(|| resource_wait::is_waiting_for_resource_for_test(&reader, key));
    let writer_registered = reader_registered
        && ProcessManager::wakeup(&writer).is_ok()
        && bounded_poll(|| resource_wait::is_waiting_for_resource_for_test(&writer, key));
    drop(held);
    let both_done = || reader_done.load(Ordering::Acquire) && writer_done.load(Ordering::Acquire);
    let completed = reader_registered && writer_registered && bounded_poll(both_done);
    if !completed {
        // Finite failure cleanup only, after releasing the actual resource.
        // This wake cannot make the assertion pass.
        let _ = ProcessManager::wakeup(&reader);
        let _ = ProcessManager::wakeup(&writer);
    }
    if !bounded_poll(both_done) {
        let _ = KernelThreadMechanism::request_stop(&reader);
        let _ = KernelThreadMechanism::request_stop(&writer);
        return false;
    }
    let reader_joined = matches!(KernelThreadMechanism::stop(&reader), Ok(0));
    let writer_joined = matches!(KernelThreadMechanism::stop(&writer), Ok(0));
    completed
        && reader_joined
        && writer_joined
        && reader_passed.load(Ordering::Acquire)
        && writer_passed.load(Ordering::Acquire)
}

#[derive(Clone, Copy)]
enum ContendedResource {
    Manager,
    Reclaimer,
    MmWrite,
    PageTableEdit,
    PageWrite,
    Vma,
    MixedBroadcast,
}

fn heap_denied_resource_contention(resource: ContendedResource) -> Result<bool, SystemError> {
    let mut online = smp_cpu_manager()
        .present_cpus()
        .iter_cpu()
        .filter(|cpu| smp_cpu_manager().is_online_cpu(*cpu));
    let Some(initiator_cpu) = online.next() else {
        return Ok(false);
    };
    let Some(holder_cpu) = online.next() else {
        return Ok(false);
    };
    let result = Arc::new(AtomicBool::new(false));
    let complete = Arc::new(Completion::new());
    let worker_result = result.clone();
    let worker_complete = complete.clone();
    let closure = KernelThreadClosure::EmptyClosure((
        Box::new(move || {
            let passed = match resource {
                ContendedResource::Manager => {
                    contended_memory_resource(page_manager_lock, holder_cpu)
                }
                ContendedResource::Reclaimer => {
                    contended_memory_resource(page_reclaimer_lock, holder_cpu)
                }
                ContendedResource::MixedBroadcast => mixed_rwsem_broadcast(holder_cpu),
                _ => contended_mm_resource(holder_cpu, resource),
            };
            worker_result.store(passed, Ordering::Release);
            worker_complete.complete();
            0
        }),
        (),
    ));
    let Some(initiator) = KernelThreadMechanism::create_on_cpu(
        closure,
        "memory-lock-initiator".to_string(),
        initiator_cpu,
    ) else {
        return Ok(false);
    };
    if ProcessManager::wakeup(&initiator).is_err() {
        let _ = KernelThreadMechanism::stop(&initiator);
        return Ok(false);
    }
    let completed = complete
        .wait_for_completion_timeout((20 * HZ) as i64)
        .is_ok_and(|remaining| remaining > 0);
    if !completed {
        let _ = KernelThreadMechanism::request_stop(&initiator);
        return Ok(false);
    }
    Ok(matches!(KernelThreadMechanism::stop(&initiator), Ok(0)) && result.load(Ordering::Acquire))
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

fn heap_denied_device_output_buffer() -> Result<bool, SystemError> {
    use crate::mm::oom::denial;
    use device_output_buffer::{BufferError, DeviceOutputBuffer};

    let translated = core::cell::Cell::new(false);
    let previously_denied = denial::is_current_denied();
    let passed = {
        let _scope = denial::Scope::enter(true);
        let before = denial::probe_denied();
        let result = DeviceOutputBuffer::new(4112, |address| {
            translated.set(true);
            Some(address)
        });
        let rejected = matches!(result, Err(BufferError::OutOfMemory));
        let after = denial::probe_denied();
        before && after && rejected && !translated.get()
    };
    Ok(passed && denial::is_current_denied() == previously_denied)
}

pub(crate) fn report() -> String {
    let _serial = TEST_LOCK.lock();
    type Check = fn() -> Result<bool, SystemError>;
    let tests: &[(&str, Check)] = &[
        #[cfg(target_arch = "x86_64")]
        ("socket_scratch_birth", || {
            Ok(crate::net::socket::scratch_birth_selftest())
        }),
        #[cfg(target_arch = "x86_64")]
        ("elf_scratch_birth", || {
            Ok(crate::libs::elf::scratch_birth_selftest())
        }),
        (
            "heap_denied_bio_birth",
            crate::driver::base::block::bio::heap_denied_bio_birth,
        ),
        #[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
        (
            "heap_denied_virtio_share",
            crate::driver::virtio::virtio_impl::heap_denied_virtio_share,
        ),
        (
            "heap_denied_rcu_slot_drop",
            crate::rcu::heap_denied_slot_drop_selftest,
        ),
        (
            "heap_denied_fuse_birth",
            crate::filesystem::fuse::conn::heap_denied_fuse_birth_selftest,
        ),
        (
            "heap_denied_fuse_wait",
            crate::filesystem::fuse::conn::heap_denied_fuse_wait_selftest,
        ),
        (
            "heap_denied_clocksource_watchdog",
            crate::time::clocksource::heap_denied_watchdog,
        ),
        (
            "heap_denied_irq_dispatch",
            crate::exception::handle::heap_denied_irq_dispatch_selftest,
        ),
        (
            "heap_denied_workqueue",
            crate::exception::workqueue::heap_denied_workqueue_selftest,
        ),
        (
            "heap_denied_device_output_buffer",
            heap_denied_device_output_buffer,
        ),
        ("last_unmap", last_unmap),
        ("partial_unmap", partial_unmap),
        ("madvise_range", madvise_range),
        ("teardown", || teardown(false)),
        ("inactive_teardown", || teardown(true)),
        ("multibatch_munmap", || multibatch_teardown(0)),
        ("multibatch_teardown", || multibatch_teardown(1)),
        ("multibatch_inactive_teardown", || multibatch_teardown(2)),
        ("heap_denied_teardown", || heap_denied_teardown(false)),
        ("heap_denied_inactive_teardown", || {
            heap_denied_teardown(true)
        }),
        ("heap_denied_manager_contention", || {
            heap_denied_resource_contention(ContendedResource::Manager)
        }),
        ("heap_denied_reclaimer_contention", || {
            heap_denied_resource_contention(ContendedResource::Reclaimer)
        }),
        ("heap_denied_mm_write_contention", || {
            heap_denied_resource_contention(ContendedResource::MmWrite)
        }),
        ("heap_denied_pt_edit_contention", || {
            heap_denied_resource_contention(ContendedResource::PageTableEdit)
        }),
        ("heap_denied_page_write_contention", || {
            heap_denied_resource_contention(ContendedResource::PageWrite)
        }),
        ("heap_denied_vma_contention", || {
            heap_denied_resource_contention(ContendedResource::Vma)
        }),
        ("heap_denied_rwsem_mixed_broadcast", || {
            heap_denied_resource_contention(ContendedResource::MixedBroadcast)
        }),
        (
            "heap_denied_page_entry_wait",
            crate::filesystem::page_cache::heap_denied_entry_wait_selftest,
        ),
        (
            "heap_denied_futex_registration",
            crate::libs::futex::futex::Futex::failed_registration_selftest,
        ),
        (
            "heap_denied_futex_wake",
            crate::libs::futex::futex::Futex::heap_denied_wake_selftest,
        ),
        (
            "heap_denied_lazy_mm_queue",
            crate::mm::lazy_mm_drop::selftest_report_check,
        ),
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
