//! BIO terminal protocol tests behind the existing mm_selftest feature.
use super::*;
use crate::{
    mm::oom::denial,
    process::{
        kthread::{KernelThreadClosure, KernelThreadMechanism},
        ProcessManager,
    },
    smp::cpu::smp_cpu_manager,
    time::{clocksource::HZ, timer::clock},
};
use alloc::string::ToString;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

pub(crate) fn heap_denied_bio_birth() -> Result<bool, SystemError> {
    let buffer = DmaBuffer::try_alloc_bytes(LBA_SIZE, Default::default())?;
    let rejected = {
        let _denied = denial::Scope::enter(true);
        let before = denial::rejected_count();
        let owner = matches!(
            BioRequest::try_from_buffer(BioType::Read, 0, 1, buffer),
            Err(SystemError::ENOMEM)
        );
        let read = matches!(BioRequest::try_new_read(0, 1), Err(SystemError::ENOMEM));
        let write = matches!(
            BioRequest::try_new_write(0, 1, &[0; LBA_SIZE]),
            Err(SystemError::ENOMEM)
        );
        let flush = matches!(BioRequest::try_new_flush(), Err(SystemError::ENOMEM));
        owner && read && write && flush && denial::rejected_count() > before
    };
    let bio = BioRequest::try_new_flush()?;
    bio.complete(Ok(0));
    Ok(rejected && bio.wait_status()? == 0 && bio.wait_status()? == 0)
}

pub(crate) fn bio_terminal_protocol() -> Result<bool, SystemError> {
    let bio = BioRequest::try_new_read(0, 1)?;
    bio.write_buffer(&[0x5a; LBA_SIZE])?;
    let calls = Arc::new(AtomicUsize::new(0));
    let weak = Arc::downgrade(&bio);
    let count = calls.clone();
    bio.try_on_complete(move |result| {
        let request = weak.upgrade().unwrap();
        if result == Ok(LBA_SIZE) && request.wait_status() == Ok(LBA_SIZE) {
            count.fetch_add(1, Ordering::Relaxed);
        }
    })?;
    bio.complete(Ok(LBA_SIZE));
    bio.complete(Err(SystemError::EIO));
    let count = calls.clone();
    bio.try_on_complete(move |result| {
        if result == Ok(LBA_SIZE) {
            count.fetch_add(1, Ordering::Relaxed);
        }
    })?;
    let mut output = [0; LBA_SIZE];
    let copied = bio.wait_read_into(&mut output)? == LBA_SIZE && output == [0x5a; LBA_SIZE];
    let duplicate = bio.wait_status()? == LBA_SIZE && bio.wait()?.as_slice() == output;
    let invalid = bio.wait_read_into(&mut []) == Err(SystemError::EINVAL)
        && bio.write_buffer(&output) == Err(SystemError::EINVAL);
    let short = BioRequest::try_new_read(0, 1)?;
    short.complete(Ok(LBA_SIZE - 1));
    let failed = BioRequest::try_new_flush()?;
    failed.complete(Err(SystemError::ESHUTDOWN));
    let errors = short.wait_status() == Err(SystemError::EIO)
        && short.wait_read_into(&mut output) == Err(SystemError::EIO)
        && failed.wait_status() == Err(SystemError::ESHUTDOWN);
    let pending = BioRequest::try_new_flush()?;
    let rejected_calls = Arc::new(AtomicUsize::new(0));
    let count = rejected_calls.clone();
    // Prepare a non-ZST box separately to exercise the vector admission edge.
    let prepared: BioCompleteCallback = Box::new(move |_| {
        count.fetch_add(1, Ordering::Relaxed);
    });
    let rejected = {
        let _denied = denial::Scope::enter(true);
        let count = rejected_calls.clone();
        let boxed = pending.try_on_complete(move |_| {
            count.fetch_add(1, Ordering::Relaxed);
        }) == Err(SystemError::ENOMEM);
        let listed = pending.register_callback(prepared) == Err(SystemError::ENOMEM);
        let copy = bio.wait() == Err(SystemError::ENOMEM);
        boxed && listed && copy && bio.wait_read_into(&mut output) == Ok(LBA_SIZE)
    };
    pending.complete(Ok(0));
    Ok(copied
        && duplicate
        && invalid
        && errors
        && rejected
        && calls.load(Ordering::Relaxed) == 2
        && rejected_calls.load(Ordering::Relaxed) == 0
        && bio.wait()?.as_slice() == output
        && callback_registration_race()?)
}

fn bounded_poll(mut condition: impl FnMut() -> bool) -> bool {
    let start = clock();
    for _ in 0..20_000_000 {
        if condition() {
            return true;
        }
        if clock().wrapping_sub(start) >= 5 * HZ {
            break;
        }
        core::hint::spin_loop();
    }
    false
}

pub(crate) fn heap_denied_bio_wait() -> Result<bool, SystemError> {
    let current_cpu = ProcessManager::current_pcb().sched_info().on_cpu();
    let Some(worker_cpu) = smp_cpu_manager()
        .present_cpus()
        .iter_cpu()
        .find(|cpu| Some(*cpu) != current_cpu && smp_cpu_manager().is_online_cpu(*cpu))
    else {
        return Ok(false);
    };
    let bio = BioRequest::try_new_read(0, 1)?;
    bio.write_buffer(&[0x6b; LBA_SIZE])?;
    bio.mark_submitted(0)?;
    let mut tasks = Vec::new();
    let mut outcomes = Vec::new();
    // Both request observers and their result owners exist before denial.
    for _ in 0..2 {
        let request = bio.clone();
        let passed = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let worker_passed = passed.clone();
        let worker_done = done.clone();
        let closure = KernelThreadClosure::EmptyClosure((
            Box::new(move || {
                let success = {
                    let _denied = denial::Scope::enter(true);
                    let mut output = [0; LBA_SIZE];
                    let before = denial::probe_denied();
                    let read = request.wait_read_into(&mut output) == Ok(LBA_SIZE);
                    let repeated = request.wait_status() == Ok(LBA_SIZE);
                    before
                        && read
                        && repeated
                        && output == [0x6b; LBA_SIZE]
                        && denial::probe_denied()
                };
                worker_passed.store(success, Ordering::Release);
                worker_done.store(true, Ordering::Release);
                0
            }),
            (),
        ));
        let Some(task) =
            KernelThreadMechanism::create_on_cpu(closure, "bio-observer".to_string(), worker_cpu)
        else {
            bio.complete(Err(SystemError::EIO));
            for task in tasks {
                let _ = ProcessManager::wakeup(&task);
                let _ = KernelThreadMechanism::stop(&task);
            }
            return Err(SystemError::ENOMEM);
        };
        tasks.push(task);
        outcomes.push((passed, done));
    }
    let mut all_registered = true;
    for task in &tasks {
        all_registered &= ProcessManager::wakeup(task).is_ok()
            && bounded_poll(|| {
                resource_wait::is_waiting_for_resource_for_test(task, bio.wait_key())
            });
    }
    // Both observers are sleeping with heap denial when terminal publication wakes them.
    bio.complete(Ok(LBA_SIZE));
    let completed = all_registered
        && bounded_poll(|| {
            outcomes
                .iter()
                .all(|(_, done)| done.load(Ordering::Acquire))
        });
    if !completed {
        for task in &tasks {
            let _ = ProcessManager::wakeup(task);
        }
    }
    if !bounded_poll(|| {
        outcomes
            .iter()
            .all(|(_, done)| done.load(Ordering::Acquire))
    }) {
        return Ok(false);
    }
    let mut joined = true;
    for task in tasks {
        joined &= matches!(KernelThreadMechanism::stop(&task), Ok(0));
    }
    Ok(completed
        && joined
        && outcomes
            .iter()
            .all(|(passed, _)| passed.load(Ordering::Acquire)))
}

fn callback_registration_race() -> Result<bool, SystemError> {
    let cpu = ProcessManager::current_pcb().sched_info().on_cpu();
    let Some(worker_cpu) = smp_cpu_manager()
        .present_cpus()
        .iter_cpu()
        .find(|candidate| Some(*candidate) != cpu && smp_cpu_manager().is_online_cpu(*candidate))
    else {
        return Ok(false);
    };
    let requests = Arc::new(
        (0..16)
            .map(|_| BioRequest::try_new_flush())
            .collect::<Result<Vec<_>, _>>()?,
    );
    let count = Arc::new(AtomicUsize::new(0));
    let finished = Arc::new(AtomicBool::new(false));
    let passed = Arc::new(AtomicBool::new(false));
    let worker_requests = requests.clone();
    let worker_count = count.clone();
    let worker_finished = finished.clone();
    let worker_passed = passed.clone();
    let task = KernelThreadMechanism::create_on_cpu(
        KernelThreadClosure::EmptyClosure((
            Box::new(move || {
                let mut success = true;
                for request in worker_requests.iter() {
                    let weak = Arc::downgrade(request);
                    let count = worker_count.clone();
                    success &= request
                        .try_on_complete(move |result| {
                            if result == Ok(0) && weak.upgrade().unwrap().wait_status() == Ok(0) {
                                count.fetch_add(1, Ordering::Relaxed);
                            }
                        })
                        .is_ok();
                }
                worker_passed.store(success, Ordering::Release);
                worker_finished.store(true, Ordering::Release);
                0
            }),
            (),
        )),
        "bio-callback".to_string(),
        worker_cpu,
    )
    .ok_or(SystemError::ENOMEM)?;
    let started = ProcessManager::wakeup(&task).is_ok();
    for request in requests.iter() {
        request.complete(Ok(0));
    }
    if !bounded_poll(|| finished.load(Ordering::Acquire)) {
        let _ = ProcessManager::wakeup(&task);
        return Ok(false);
    }
    let joined = matches!(KernelThreadMechanism::stop(&task), Ok(0));
    Ok(started
        && joined
        && passed.load(Ordering::Acquire)
        && count.load(Ordering::Relaxed) == requests.len())
}
