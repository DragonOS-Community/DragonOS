//! Real sleeping entry predicates; every fixture is built before heap denial.
use super::*;
use crate::{
    libs::resource_wait,
    mm::oom::denial,
    process::{
        kthread::{KernelThreadClosure, KernelThreadMechanism},
        ProcessManager,
    },
    sched::completion::Completion,
    smp::cpu::{smp_cpu_manager, ProcessorId},
    time::clocksource::HZ,
};
use alloc::string::ToString;

fn poll(mut condition: impl FnMut() -> bool) -> bool {
    let began = crate::time::timer::clock();
    for _ in 0..20_000_000 {
        if condition() {
            return true;
        }
        if crate::time::timer::clock().wrapping_sub(began) >= 5 * HZ {
            break;
        }
        core::hint::spin_loop();
    }
    false
}

fn case(
    kind: usize,
    waiter_cpu: ProcessorId,
    publisher_cpu: ProcessorId,
) -> Result<bool, SystemError> {
    let cache = PageCache::new_shmem(None, None);
    let page = cache.get_or_create_page_zero(0)?;
    let entry = Arc::new(PageEntry::new(
        page,
        if kind == 3 {
            PageState::Writeback
        } else {
            PageState::Loading
        },
    ));
    let pin = SpinLock::new((kind == 2).then(|| entry.pin()));
    let result = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let observed = Arc::new(AtomicBool::new(false));
    let rescued = Arc::new(AtomicBool::new(false));
    let completion = Arc::new(Completion::new());
    let waiter_entry = entry.clone();
    let waiter_result = result.clone();
    let waiter_done = done.clone();
    let waiter_completion = completion.clone();
    let waiter = KernelThreadMechanism::create_on_cpu(
        KernelThreadClosure::EmptyClosure((
            Box::new(move || {
                let passed = {
                    let _scope = denial::Scope::enter(true);
                    let before = denial::probe_denied();
                    let predicate_ok = match kind {
                        0 => waiter_entry
                            .wait_ready()
                            .is_ok_and(|page| Arc::ptr_eq(&page, &waiter_entry.page)),
                        1 => matches!(waiter_entry.wait_ready(), Err(SystemError::EIO)),
                        2 => {
                            waiter_entry.wait_inactive();
                            waiter_entry.active_users() == 0
                        }
                        _ => {
                            waiter_entry
                                .wait_until(|| {
                                    PageCacheManager::writeback_incarnation_result(&waiter_entry, 0)
                                })
                                .is_ok()
                                && waiter_entry.state() == PageState::Writeback
                        }
                    };
                    let after = denial::probe_denied();
                    before && predicate_ok && after
                };
                waiter_result.store(passed, Ordering::Release);
                waiter_done.store(true, Ordering::Release);
                waiter_completion.complete();
                0
            }),
            (),
        )),
        "entry-waiter".to_string(),
        waiter_cpu,
    )
    .ok_or(SystemError::ENOMEM)?;
    let publisher_entry = entry.clone();
    let publisher_waiter = waiter.clone();
    let publisher_done = done.clone();
    let publisher_observed = observed.clone();
    let publisher_rescued = rescued.clone();
    let publisher = KernelThreadMechanism::create_on_cpu(
        KernelThreadClosure::EmptyClosure((
            Box::new(move || {
                let slept = poll(|| {
                    resource_wait::is_waiting_for_resource_for_test(
                        &publisher_waiter,
                        publisher_entry.wait_key(),
                    ) && publisher_waiter.sched_info().state().is_blocked()
                        && !publisher_waiter.sched_info().is_running()
                });
                publisher_observed.store(slept, Ordering::Release);
                match kind {
                    0 => publisher_entry.set_state(PageState::UpToDate),
                    1 => publisher_entry.set_state(PageState::Error),
                    2 => {
                        let released = pin.lock().take();
                        drop(released);
                    }
                    _ => publisher_entry
                        .writeback_incarnation
                        .store(1, Ordering::Release),
                }
                if kind != 2 {
                    publisher_entry.notify_waiters();
                }
                if !poll(|| publisher_done.load(Ordering::Acquire)) {
                    publisher_rescued.store(true, Ordering::Release);
                    let _ = ProcessManager::wakeup(&publisher_waiter);
                }
                0
            }),
            (),
        )),
        "entry-publisher".to_string(),
        publisher_cpu,
    );
    let Some(publisher) = publisher else {
        let _ = KernelThreadMechanism::stop(&waiter);
        return Err(SystemError::ENOMEM);
    };
    if ProcessManager::wakeup(&publisher).is_err() || ProcessManager::wakeup(&waiter).is_err() {
        let _ = KernelThreadMechanism::stop(&publisher);
        let _ = KernelThreadMechanism::stop(&waiter);
        return Ok(false);
    }
    let completed = completion
        .wait_for_completion_timeout((15 * HZ) as i64)
        .is_ok_and(|remaining| remaining > 0);
    if !completed {
        let _ = KernelThreadMechanism::request_stop(&waiter);
        let _ = KernelThreadMechanism::request_stop(&publisher);
        return Ok(false);
    }
    let publisher_joined = matches!(KernelThreadMechanism::stop(&publisher), Ok(0));
    let waiter_joined = matches!(KernelThreadMechanism::stop(&waiter), Ok(0));
    Ok(publisher_joined
        && waiter_joined
        && result.load(Ordering::Acquire)
        && observed.load(Ordering::Acquire)
        && !rescued.load(Ordering::Acquire))
}

pub(super) fn run() -> Result<bool, SystemError> {
    let mut cpus = smp_cpu_manager()
        .present_cpus()
        .iter_cpu()
        .filter(|cpu| smp_cpu_manager().is_online_cpu(*cpu));
    let Some(waiter_cpu) = cpus.next() else {
        return Ok(false);
    };
    let Some(publisher_cpu) = cpus.next() else {
        return Ok(false);
    };
    for kind in 0..4 {
        if !case(kind, waiter_cpu, publisher_cpu)? {
            return Ok(false);
        }
    }
    Ok(true)
}
