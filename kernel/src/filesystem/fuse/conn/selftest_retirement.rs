//! Real page-cache owners through domain cancellation and abnormal reply order.
use super::super::protocol::{fuse_pack_struct, FuseOutHeader, FuseReadIn, FUSE_READ};
use super::*;
use crate::{
    filesystem::{
        page_cache::PageCache,
        ramfs::RamFS,
        vfs::{
            mount::{MountFlags, SuperBlockState},
            FileSystem,
        },
    },
    mm::oom::denial,
    process::kthread::{KernelThreadClosure, KernelThreadMechanism},
    smp::cpu::smp_cpu_manager,
    time::{clocksource::HZ, timer::clock},
};
use alloc::{boxed::Box, string::ToString};

fn connection() -> Arc<FuseConn> {
    let conn = FuseConn::new_for_virtiofs(8192, 8192);
    {
        let mut inner = conn.lock_inner();
        inner.mounted = true;
        inner.initialized = true;
    }
    conn
}

fn domain(owner: &Arc<dyn FileSystem>) -> Result<Arc<PageCacheWritebackDomain>, SystemError> {
    // The test exercises admission/retirement, not writeback worker creation.
    let domain = PageCacheWritebackDomain::new_shmem();
    let superblock = Arc::new(SuperBlockState::new(MountFlags::empty()));
    domain.bind(owner, &superblock)?;
    Ok(domain)
}

type ReadOwners = (
    Arc<PageCache>,
    Arc<FusePendingState>,
    Weak<PageCacheReadDmaReservation>,
);

fn read(
    conn: &FuseConn,
    domain: &Arc<PageCacheWritebackDomain>,
) -> Result<ReadOwners, SystemError> {
    let cache = PageCache::new_unowned(None, None);
    let destination = Arc::new(cache.manager().reserve_read_dma(0, 1, false)?);
    let weak = Arc::downgrade(&destination);
    let unique = conn.next_unique.fetch_add(2, Ordering::Relaxed);
    let pending = Arc::new(FusePendingState::new_with_credit(
        unique,
        FUSE_READ,
        conn.background.acquire(false)?,
        Some(FuseReadCompletion {
            target: destination.clone(),
            node: Weak::new(),
            start_page: 0,
            requested: MMArch::PAGE_SIZE,
            observed_size: MMArch::PAGE_SIZE,
            observed_attr_version: 0,
            open_pin: SpinLock::new(None),
            domain_io: SpinLock::new(Some(domain.try_acquire_io()?)),
        }),
    ));
    let input = FuseReadIn {
        fh: 0,
        offset: 0,
        size: MMArch::PAGE_SIZE as u32,
        read_flags: 0,
        lock_owner: 0,
        flags: 0,
        padding: 0,
    };
    let request = conn.build_request(
        unique,
        FUSE_READ,
        1,
        fuse_pack_struct(&input),
        FuseRequestCred::nocreds(),
        request::FuseReplyRouting::ReadPages(destination),
    )?;
    conn.push_request(request, Some(pending.clone()), unique)?;
    Ok((cache, pending, weak))
}

fn cancel_domains() -> Result<bool, SystemError> {
    let conn = connection();
    let owner: Arc<dyn FileSystem> = RamFS::new();
    let first = domain(&owner)?;
    let other = domain(&owner)?;
    let (queued_cache, queued, queued_owner) = read(&conn, &first)?;
    let (sent_cache, sent, sent_owner) = read(&conn, &first)?;
    let (other_cache, other_read, other_owner) = read(&conn, &other)?;
    // Actual dequeue moves one queue owner to the transport side. Leave both
    // that owner and another matching request pending during cancellation.
    let transport = conn.dequeue_virtiofs_ordinary_request(8192)?;
    let capacity = conn.lock_inner().pending.capacity();
    first.close_background_admission();
    let passed = {
        let _deny = denial::Scope::enter(true);
        let probe = denial::probe_denied();
        conn.cancel_page_cache_reads(&first);
        let isolated = {
            let inner = conn.lock_inner();
            inner.pending.capacity() == capacity
                && inner.pending.len() == 1
                && inner
                    .pending
                    .front()
                    .is_some_and(|r| r.unique == other_read.unique())
                && inner.processing.len() == 1
                && inner.processing.contains_key(&other_read.unique())
        };
        probe
            && isolated
            && !queued_cache.fault_page_state(0).cached
            && !sent_cache.fault_page_state(0).cached
            && other_cache.fault_page_state(0).cached
            && matches!(
                queued.wait_result_with_kind_uninterruptible(),
                Ok((Err(SystemError::ENOTCONN), FuseCompletionKind::Disconnected))
            )
            && matches!(
                sent.wait_result_with_kind_uninterruptible(),
                Ok((Err(SystemError::ENOTCONN), FuseCompletionKind::Disconnected))
            )
            && queued
                .read_completion
                .as_ref()
                .unwrap()
                .domain_io
                .lock()
                .is_none()
            && sent
                .read_completion
                .as_ref()
                .unwrap()
                .domain_io
                .lock()
                .is_none()
            && other_read
                .read_completion
                .as_ref()
                .unwrap()
                .domain_io
                .lock()
                .is_some()
            && conn.background.inner.lock_irqsave().inflight == 1
            && denial::probe_denied()
    };
    drop((queued, sent, transport));
    let retired = queued_owner.upgrade().is_none() && sent_owner.upgrade().is_none();
    conn.disconnect_requests();
    drop(other_read);
    Ok(passed && retired && other_owner.upgrade().is_none())
}

fn early_reply_last_owner() -> Result<bool, SystemError> {
    let conn = connection();
    let owner: Arc<dyn FileSystem> = RamFS::new();
    let domain = domain(&owner)?;
    let (cache, pending, reservation) = read(&conn, &domain)?;
    let unique = pending.unique();
    // A real daemon error may claim processing before reading its queued
    // request. Completion releases its permit but leaves the queued Arc alive.
    let reply = FuseOutHeader {
        len: size_of::<FuseOutHeader>() as u32,
        error: SystemError::EIO.to_posix_errno(),
        unique,
    };
    conn.write_reply(fuse_pack_struct(&reply))?;
    drop(pending);
    let sole_owner = reservation
        .upgrade()
        .is_some_and(|owner| Arc::strong_count(&owner) == 2);
    conn.queue_forget(11, 1)?;
    conn.queue_forget(22, 1)?;
    let passed = {
        let _deny = denial::Scope::enter(true);
        let probe = denial::probe_denied();
        // DESTROY preparation is correctly fallible. Under denied heap the
        // original abort fallback retires the preserved FORGET queue as well.
        conn.on_umount();
        conn.on_umount();
        probe
            && reservation.upgrade().is_none()
            && !cache.fault_page_state(0).cached
            && conn.lock_inner().teardown_started
            && denial::probe_denied()
    };
    // Exercise preserved FORGET order/capacity with successful DESTROY as well.
    let ordered = connection();
    ordered.queue_forget(11, 1)?;
    ordered.queue_forget(22, 1)?;
    let before = ordered.lock_inner().hiprio_pending.capacity();
    ordered.on_umount();
    let ordered_ok = {
        let inner = ordered.lock_inner();
        inner.hiprio_pending.capacity() == before
            && inner.hiprio_pending.len() == 2
            && inner.hiprio_pending[0].opcode == FUSE_FORGET
            && inner.hiprio_pending[1].opcode == FUSE_FORGET
            && inner.hiprio_pending[0].unique < inner.hiprio_pending[1].unique
            && inner
                .pending
                .iter()
                .filter(|r| r.opcode == FUSE_DESTROY)
                .count()
                == 1
    };
    ordered.disconnect_requests();
    Ok(sole_owner && passed && ordered_ok)
}

fn poll(mut condition: impl FnMut() -> bool) -> bool {
    let began = clock();
    for step in 0..20_000_000 {
        if condition() {
            return true;
        }
        if clock().wrapping_sub(began) >= 5 * HZ {
            break;
        }
        if step % 256 == 0 {
            crate::sched::cond_resched();
        }
        core::hint::spin_loop();
    }
    false
}

fn last_owner_cache_contention() -> Result<bool, SystemError> {
    let conn = connection();
    let owner: Arc<dyn FileSystem> = RamFS::new();
    let domain = domain(&owner)?;
    let (cache, pending, reservation) = read(&conn, &domain)?;
    let reply = FuseOutHeader {
        len: size_of::<FuseOutHeader>() as u32,
        error: SystemError::EIO.to_posix_errno(),
        unique: pending.unique(),
    };
    conn.write_reply(fuse_pack_struct(&reply))?;
    drop(pending);
    let sole_owner = reservation
        .upgrade()
        .is_some_and(|owner| Arc::strong_count(&owner) == 2);
    let began = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let worker_began = began.clone();
    let worker_done = done.clone();
    let worker_conn = conn.clone();
    let cpu = smp_cpu_manager()
        .present_cpus()
        .iter_cpu()
        .find(|cpu| smp_cpu_manager().is_online_cpu(*cpu))
        .ok_or(SystemError::ENODEV)?;
    let worker = KernelThreadMechanism::create_on_cpu(
        KernelThreadClosure::EmptyClosure((
            Box::new(move || {
                worker_began.store(true, Ordering::Release);
                worker_conn.on_umount();
                worker_done.store(true, Ordering::Release);
                0
            }),
            (),
        )),
        "fuse-retire-cache".to_string(),
        cpu,
    )
    .ok_or(SystemError::ENOMEM)?;
    // The real reservation destructor always acquires cache.inner, even after
    // its earlier reply cleared the Loading marker. Hold that mutex to pause
    // the final owner Drop and inspect the connection independently.
    let cache_lock = cache.lock();
    let blocked = ProcessManager::wakeup(&worker).is_ok()
        && poll(|| {
            began.load(Ordering::Acquire)
                && worker.sched_info().state().is_blocked()
                && !worker.sched_info().is_running()
        });
    let connection_free = blocked && conn.inner.try_lock().is_ok();
    drop(cache_lock);
    let finished = poll(|| done.load(Ordering::Acquire));
    if !finished {
        let _ = ProcessManager::wakeup(&worker);
    }
    if !poll(|| done.load(Ordering::Acquire)) {
        let _ = KernelThreadMechanism::request_stop(&worker);
        return Ok(false);
    }
    let joined = matches!(KernelThreadMechanism::stop(&worker), Ok(0));
    conn.disconnect_requests();
    Ok(sole_owner
        && blocked
        && connection_free
        && finished
        && joined
        && reservation.upgrade().is_none())
}

fn long_retirement_batch() -> bool {
    let mut batch = Retired::new();
    let mut last = Weak::new();
    for unique in 1..=2048 {
        let pending = Arc::new(FusePendingState::new(unique, FUSE_READ));
        last = Arc::downgrade(&pending);
        batch.push(pending);
    }
    let _deny = denial::Scope::enter(true);
    let probe = denial::probe_denied();
    drop(batch);
    probe && last.upgrade().is_none() && denial::probe_denied()
}

pub(super) fn run() -> Result<bool, SystemError> {
    Ok(long_retirement_batch()
        && cancel_domains()?
        && early_reply_last_owner()?
        && last_owner_cache_contention()?)
}
