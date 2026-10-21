//! Real allocation rejection at the unpublished request and publication boundaries.
use super::*;
use crate::{
    libs::resource_wait,
    mm::oom::denial,
    process::kthread::{KernelThreadClosure, KernelThreadMechanism},
    smp::cpu::smp_cpu_manager,
    time::{clocksource::HZ, timer::clock},
};
use alloc::{boxed::Box, string::ToString};
use core::sync::atomic::AtomicBool;

fn connection() -> Arc<FuseConn> {
    let conn = FuseConn::new_for_virtiofs(8192, 8192);
    {
        let mut inner = conn.lock_inner();
        inner.mounted = true;
        inner.initialized = true;
    }
    conn
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

fn unpublished(conn: &FuseConn) -> bool {
    let inner = conn.lock_inner();
    inner.pending.is_empty() && inner.hiprio_pending.is_empty() && inner.processing.is_empty()
}

fn rejected_birth() -> Result<bool, SystemError> {
    let conn = connection();
    let credit = conn.background.acquire(false)?;
    let credit_owner = conn.background.clone();
    let passed = {
        let _deny = denial::Scope::enter(true);
        let probe = denial::probe_denied();
        let bytes = conn.build_request(
            2,
            FUSE_LOOKUP,
            1,
            b"name\0",
            FuseRequestCred::nocreds(),
            FuseReplyRouting::Normal,
        );
        let pending = conn.prepare_request(FUSE_LOOKUP, 1, b"name\0", FuseRequestCred::nocreds());
        let read_pending = Arc::try_new(FusePendingState::new_with_credit(
            4, FUSE_READ, credit, None,
        ));
        let background = conn.request_nocreds_background(FUSE_RELEASE, 1, &[]);
        probe
            && matches!(bytes, Err(SystemError::ENOMEM))
            && matches!(pending, Err(SystemError::ENOMEM))
            && read_pending.is_err()
            && background == Err(SystemError::ENOMEM)
            && credit_owner.inner.lock_irqsave().inflight == 0
            && unpublished(&conn)
            && denial::probe_denied()
    };
    conn.disconnect_requests();
    Ok(passed)
}

fn rejected_publication(reserve_queue: bool) -> Result<bool, SystemError> {
    let conn = connection();
    let (req, pending) =
        conn.prepare_request(FUSE_LOOKUP, 1, b"name\0", FuseRequestCred::nocreds())?;
    let unique = pending.unique();
    let request_owner = Arc::downgrade(&req);
    let pending_owner = Arc::downgrade(&pending);
    if reserve_queue {
        conn.lock_inner()
            .pending
            .try_reserve(1)
            .map_err(|_| SystemError::ENOMEM)?;
    }
    let rejected = {
        let _deny = denial::Scope::enter(true);
        let probe = denial::probe_denied();
        let result = conn.push_request(req, Some(pending.clone()), unique);
        probe
            && result == Err(SystemError::ENOMEM)
            && unpublished(&conn)
            && request_owner.upgrade().is_none()
            && !pending.processing_owner.load(Ordering::Acquire)
            && denial::probe_denied()
    };
    drop(pending);
    let retired = pending_owner.upgrade().is_none();
    // The same owner is usable after rejection; no half-published unique survives.
    let (req, pending) =
        conn.prepare_request(FUSE_LOOKUP, 1, b"later\0", FuseRequestCred::nocreds())?;
    conn.push_request(req, Some(pending.clone()), pending.unique())?;
    conn.disconnect_requests();
    let terminal = matches!(
        pending.wait_result_with_kind_uninterruptible(),
        Ok((Err(SystemError::ENOTCONN), FuseCompletionKind::Disconnected))
    );
    Ok(rejected && retired && terminal && unpublished(&conn))
}

fn prepared_publication_without_heap() -> Result<bool, SystemError> {
    let conn = connection();
    let (req, pending) =
        conn.prepare_request(FUSE_LOOKUP, 1, b"name\0", FuseRequestCred::nocreds())?;
    {
        let mut inner = conn.lock_inner();
        inner
            .pending
            .try_reserve(1)
            .map_err(|_| SystemError::ENOMEM)?;
        inner
            .processing
            .try_reserve(1)
            .map_err(|_| SystemError::ENOMEM)?;
    }
    let passed = {
        let _deny = denial::Scope::enter(true);
        let probe = denial::probe_denied();
        let published = conn
            .push_request(req, Some(pending.clone()), pending.unique())
            .is_ok();
        let visible = {
            let inner = conn.lock_inner();
            inner.pending.len() == 1 && inner.processing.contains_key(&pending.unique())
        };
        conn.disconnect_requests();
        probe
            && published
            && visible
            && unpublished(&conn)
            && matches!(
                pending.wait_result_with_kind_uninterruptible(),
                Ok((Err(SystemError::ENOTCONN), FuseCompletionKind::Disconnected))
            )
            && denial::probe_denied()
    };
    Ok(passed)
}

fn interrupt_failure_keeps_original() -> Result<bool, SystemError> {
    let conn = connection();
    let (req, pending) =
        conn.prepare_request(FUSE_LOOKUP, 1, b"name\0", FuseRequestCred::nocreds())?;
    let unique = pending.unique();
    conn.push_request(req, Some(pending.clone()), unique)?;
    let transport = conn.dequeue_virtiofs_ordinary_request(8192)?;
    conn.mark_request_sent(unique);
    let interrupted = pending.mark_interrupted();
    let rejected = {
        let _deny = denial::Scope::enter(true);
        let probe = denial::probe_denied();
        let result = conn.queue_interrupt(unique);
        probe
            && result == Err(SystemError::ENOMEM)
            && conn.has_processing_request(unique)
            && !conn.has_pending_high_priority_requests()
            && pending.processing_owner.load(Ordering::Acquire)
            && denial::probe_denied()
    };
    // A real original reply can still retire the request after failed cancellation.
    let hdr = FuseOutHeader {
        len: size_of::<FuseOutHeader>() as u32,
        error: SystemError::EIO.to_posix_errno(),
        unique,
    };
    conn.write_reply(fuse_pack_struct(&hdr))?;
    let terminal = matches!(
        pending.wait_result_with_kind_uninterruptible(),
        Ok((Err(SystemError::EIO), FuseCompletionKind::DaemonError))
    );
    conn.disconnect_requests();
    drop(transport);
    Ok(interrupted && rejected && terminal && !pending.processing_owner.load(Ordering::Acquire))
}

fn denied_disconnect() -> Result<bool, SystemError> {
    let conn = connection();
    let (req, pending) =
        conn.prepare_request(FUSE_LOOKUP, 1, b"name\0", FuseRequestCred::nocreds())?;
    conn.push_request(req, Some(pending.clone()), pending.unique())?;
    conn.queue_forget(1, 1)?;
    let passed = {
        let _deny = denial::Scope::enter(true);
        let probe = denial::probe_denied();
        conn.disconnect_requests();
        probe
            && unpublished(&conn)
            && matches!(
                pending.wait_result_with_kind_uninterruptible(),
                Ok((Err(SystemError::ENOTCONN), FuseCompletionKind::Disconnected))
            )
            && !pending.processing_owner.load(Ordering::Acquire)
            && denial::probe_denied()
    };
    Ok(passed)
}

fn contended_connection() -> Result<bool, SystemError> {
    let Some(cpu) = smp_cpu_manager()
        .present_cpus()
        .iter_cpu()
        .find(|cpu| smp_cpu_manager().is_online_cpu(*cpu))
    else {
        return Ok(false);
    };
    let conn = connection();
    let passed = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let worker_conn = conn.clone();
    let worker_passed = passed.clone();
    let worker_done = done.clone();
    let worker = KernelThreadMechanism::create_on_cpu(
        KernelThreadClosure::EmptyClosure((
            Box::new(move || {
                let ok = {
                    let _deny = denial::Scope::enter(true);
                    let before = denial::probe_denied();
                    let lock = worker_conn.lock_inner();
                    let connected = lock.connected;
                    drop(lock);
                    before && connected && denial::probe_denied()
                };
                worker_passed.store(ok, Ordering::Release);
                worker_done.store(true, Ordering::Release);
                0
            }),
            (),
        )),
        "fuse-publish-lock".to_string(),
        cpu,
    )
    .ok_or(SystemError::ENOMEM)?;
    let lock = conn.lock_inner();
    let key = lock.resource_key_for_test();
    let observed = ProcessManager::wakeup(&worker).is_ok()
        && poll(|| {
            resource_wait::is_waiting_for_resource_for_test(&worker, key)
                && worker.sched_info().state().is_blocked()
                && !worker.sched_info().is_running()
        });
    drop(lock);
    let completed = observed && poll(|| done.load(Ordering::Acquire));
    if !completed {
        // Cleanup wake is deliberately excluded from the passing observation.
        let _ = ProcessManager::wakeup(&worker);
    }
    if !poll(|| done.load(Ordering::Acquire)) {
        let _ = KernelThreadMechanism::request_stop(&worker);
        return Ok(false);
    }
    let joined = matches!(KernelThreadMechanism::stop(&worker), Ok(0));
    conn.disconnect_requests();
    Ok(completed && joined && passed.load(Ordering::Acquire))
}

fn rejected_dax_abort_preflight() -> Result<bool, SystemError> {
    let conn = connection();
    let owners = Arc::strong_count(&conn);
    let input = super::super::super::protocol::FuseSetupMappingIn {
        fh: 0,
        foffset: 0,
        len: super::super::super::virtiofs::dax::DAX_RANGE_SIZE as u64,
        flags: 1,
        moffset: 0,
    };
    let passed = {
        let _deny = denial::Scope::enter(true);
        let probe = denial::probe_denied();
        let result = conn.request_dax_mapping(FUSE_SETUPMAPPING, 1, fuse_pack_struct(&input));
        probe
            && matches!(
                result,
                FuseDaxRequestOutcome::NeverSubmitted(SystemError::ENOMEM)
            )
            && unpublished(&conn)
            && Arc::strong_count(&conn) == owners
            && denial::probe_denied()
    };
    conn.disconnect_requests();
    Ok(passed)
}

pub(super) fn run() -> Result<bool, SystemError> {
    Ok(rejected_dax_abort_preflight()?
        && rejected_birth()?
        && rejected_publication(false)?
        && rejected_publication(true)?
        && prepared_publication_without_heap()?
        && interrupt_failure_keeps_original()?
        && denied_disconnect()?
        && contended_connection()?)
}
