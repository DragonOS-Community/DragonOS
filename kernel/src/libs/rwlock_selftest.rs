use alloc::{boxed::Box, format, string::String, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use crate::{
    arch::CurrentIrqArch,
    exception::{enter_hardirq, in_hardirq, in_interrupt, InterruptArch},
    process::{
        kthread::{KernelThreadClosure, KernelThreadMechanism},
        ProcessManager,
    },
    sched::{completion::Completion, schedule, SchedMode},
};

use super::*;

struct Report {
    text: String,
    passed: usize,
    failed: usize,
}

impl Report {
    fn new() -> Self {
        Self {
            text: String::new(),
            passed: 0,
            failed: 0,
        }
    }

    fn case(&mut self, name: &str, ok: bool) {
        if ok {
            self.passed += 1;
            self.text.push_str(&format!("rwlock.{name}=ok\n"));
        } else {
            self.failed += 1;
            self.text.push_str(&format!("rwlock.{name}=fail\n"));
        }
    }
}

/// Acquire a writer that has already registered its ticket.
///
/// This mirrors the preemption ownership contract of the public try-lock
/// helpers: a successful guard owns the disable and restores it on drop,
/// while a failed attempt must restore preemption before returning.
fn try_write_registered<T>(lock: &RwLock<T>) -> Option<RwLockWriteGuard<'_, T>> {
    ProcessManager::preempt_disable();
    let writer = lock.inner_try_write_registered();
    if writer.is_none() {
        ProcessManager::preempt_enable();
    }
    writer
}

fn concurrent_reader_flood_writer_progress() -> bool {
    const READER_THREADS: usize = 2;
    const READS_PER_THREAD: usize = 16_384;

    let lock = Arc::new(RwLock::new(0usize));
    let ready = Arc::new(Completion::new());
    let start = Arc::new(Completion::new());
    let writer_acquired = Arc::new(AtomicBool::new(false));
    let invalid_read = Arc::new(AtomicBool::new(false));
    let read_count = Arc::new(AtomicUsize::new(0));
    let mut readers = Vec::new();

    for _ in 0..READER_THREADS {
        let lock = lock.clone();
        let ready = ready.clone();
        let reader_start = start.clone();
        let writer_acquired = writer_acquired.clone();
        let invalid_read = invalid_read.clone();
        let read_count = read_count.clone();
        let closure = KernelThreadClosure::EmptyClosure((
            Box::new(move || {
                ready.complete();
                if reader_start.wait_for_completion().is_err() {
                    return 1;
                }
                for iteration in 0..READS_PER_THREAD {
                    let value = *lock.read();
                    if value > 1 {
                        invalid_read.store(true, Ordering::Release);
                    }
                    read_count.fetch_add(1, Ordering::Relaxed);
                    if writer_acquired.load(Ordering::Acquire) {
                        break;
                    }
                    if iteration % 64 == 63 {
                        schedule(SchedMode::SM_NONE);
                    }
                }
                0
            }),
            (),
        ));
        let Some(reader) =
            KernelThreadMechanism::create_and_run(closure, "rwlock-reader-flood-selftest".into())
        else {
            start.complete_all();
            for reader in &readers {
                let _ = KernelThreadMechanism::stop(reader);
            }
            return false;
        };
        readers.push(reader);
    }

    let writer_lock = lock.clone();
    let writer_ready = ready.clone();
    let writer_start = start.clone();
    let writer_done = writer_acquired.clone();
    let writer_closure = KernelThreadClosure::EmptyClosure((
        Box::new(move || {
            writer_ready.complete();
            if writer_start.wait_for_completion().is_err() {
                return 1;
            }
            let mut guard = writer_lock.write();
            *guard = 1;
            writer_done.store(true, Ordering::Release);
            0
        }),
        (),
    ));
    let Some(writer) = KernelThreadMechanism::create_and_run(
        writer_closure,
        "rwlock-writer-progress-selftest".into(),
    ) else {
        start.complete_all();
        for reader in &readers {
            let _ = KernelThreadMechanism::stop(reader);
        }
        return false;
    };

    let mut all_ready = true;
    for _ in 0..=READER_THREADS {
        all_ready &= ready.wait_for_completion().is_ok();
    }
    start.complete_all();

    let writer_stopped = KernelThreadMechanism::stop(&writer).is_ok();
    let readers_stopped = readers
        .iter()
        .all(|reader| KernelThreadMechanism::stop(reader).is_ok());
    let final_value = *lock.read();

    all_ready
        && writer_stopped
        && readers_stopped
        && writer_acquired.load(Ordering::Acquire)
        && read_count.load(Ordering::Acquire) != 0
        && read_count.load(Ordering::Acquire) < READER_THREADS * READS_PER_THREAD
        && !invalid_read.load(Ordering::Acquire)
        && final_value == 1
}

pub(crate) fn run_rwlock_selftests() -> (usize, usize, String) {
    let mut report = Report::new();

    let fifo = WriterTickets::new();
    let first = fifo.issue();
    let second = fifo.issue();
    let first_turn = fifo.is_turn(first) && !fifo.is_turn(second);
    fifo.finish(first);
    let second_turn = fifo.is_turn(second);
    fifo.finish(second);
    report.case(
        "ticket_fifo",
        first == 0 && second == 1 && first_turn && second_turn,
    );

    let wrapping = WriterTickets {
        next: AtomicU32::new(u32::MAX),
        serving: AtomicU32::new(u32::MAX),
    };
    let last = wrapping.issue();
    let wrap_pending = wrapping.is_turn(last);
    wrapping.finish(last);
    let zero = wrapping.issue();
    let zero_turn = wrapping.is_turn(zero);
    wrapping.finish(zero);
    report.case(
        "ticket_wrap",
        last == u32::MAX
            && zero == 0
            && wrap_pending
            && zero_turn
            && wrapping.next.load(Ordering::SeqCst) == 1
            && wrapping.serving.load(Ordering::SeqCst) == 1,
    );

    let overflow = RwLock::new(());
    overflow.lock.store(PENDING_WRITER_MASK, Ordering::Relaxed);
    let next_before = overflow.writer_tickets.next.load(Ordering::Relaxed);
    let pending_overflow_rejected = overflow.register_waiting_writer().is_err()
        && overflow.lock.load(Ordering::Relaxed) == PENDING_WRITER_MASK
        && overflow.writer_tickets.next.load(Ordering::Relaxed) == next_before;
    overflow.lock.store(0, Ordering::Relaxed);
    report.case("pending_overflow", pending_overflow_rejected);

    let reader_limit = RwLock::new(());
    let full_reader_state = MAX_READERS * READER;
    reader_limit
        .lock
        .store(full_reader_state, Ordering::Relaxed);
    let upgrader_at_limit_rejected = reader_limit.try_upgradeable_read().is_none()
        && reader_limit.lock.load(Ordering::Relaxed) == full_reader_state;
    reader_limit.lock.store(0, Ordering::Relaxed);
    report.case("upgrader_reader_limit", upgrader_at_limit_rejected);

    let outside_interrupt = !in_hardirq() && !in_interrupt();
    ProcessManager::preempt_disable();
    let hardirq_outer = enter_hardirq();
    let first_depth = in_hardirq() && in_interrupt();
    let hardirq_inner = enter_hardirq();
    let nested_depth = in_hardirq();
    drop(hardirq_inner);
    let outer_restored = in_hardirq();
    drop(hardirq_outer);
    ProcessManager::preempt_enable();
    report.case(
        "hardirq_context_depth",
        outside_interrupt && first_depth && nested_depth && outer_restored && !in_hardirq(),
    );

    let lock = RwLock::new(7u32);
    let ticket = lock.register_waiting_writer().unwrap();
    let preempt_before = ProcessManager::current_pcb().preempt_count();
    let irq_before = CurrentIrqArch::is_irq_enabled();
    let reader_blocked = lock.try_read().is_none();
    let upgradeable_blocked = lock.try_upgradeable_read().is_none();
    let writer_barge_blocked = lock.try_write().is_none();
    let irq_reader_blocked = lock.try_read_irqsave().is_none();
    let state_restored = ProcessManager::current_pcb().preempt_count() == preempt_before
        && CurrentIrqArch::is_irq_enabled() == irq_before;

    let no_pending_writer = RwLock::new(());
    let failed_registered_writer = try_write_registered(&no_pending_writer);
    let registered_failure_restored = failed_registered_writer.is_none()
        && ProcessManager::current_pcb().preempt_count() == preempt_before;
    drop(failed_registered_writer);
    report.case(
        "registered_writer_failure_restores_preempt",
        registered_failure_restored,
    );

    // Merely disabling preemption must not masquerade as interrupt context.
    ProcessManager::preempt_disable();
    let nested_preempt = ProcessManager::current_pcb().preempt_count();
    let nested_reader = lock.try_read();
    let nested_reader_blocked = nested_reader.is_none();
    drop(nested_reader);
    let nested_state_restored = ProcessManager::current_pcb().preempt_count() == nested_preempt;
    ProcessManager::preempt_enable();

    // A real interrupt reader bypasses pending writers to avoid deadlocking
    // against an interrupted outer reader.
    ProcessManager::preempt_disable();
    let hardirq = enter_hardirq();
    let interrupt_reader = lock.try_read();
    let interrupt_reader_ok = interrupt_reader.as_ref().is_some_and(|guard| **guard == 7);
    drop(interrupt_reader);
    drop(hardirq);
    ProcessManager::preempt_enable();

    let writer = try_write_registered(&lock);
    let writer_acquired = writer.is_some();
    lock.finish_writer_turn(ticket);
    drop(writer);
    report.case(
        "pending_blocks_new_owners",
        reader_blocked
            && upgradeable_blocked
            && writer_barge_blocked
            && irq_reader_blocked
            && state_restored
            && nested_reader_blocked
            && nested_state_restored
            && interrupt_reader_ok
            && writer_acquired
            && ProcessManager::current_pcb().preempt_count() == preempt_before
            && !lock.has_waiting_writer(),
    );

    let transitions = RwLock::new(1u32);
    let read_ok = transitions.try_read().is_some_and(|guard| *guard == 1);
    let (upgrade_ok, downgrade_ok, pending_preserved) = match transitions.try_upgradeable_read() {
        Some(upgradeable) => {
            let queued = transitions.register_waiting_writer().unwrap();
            match upgradeable.try_upgrade() {
                Ok(mut writer) => {
                    *writer = 2;
                    let reader = writer.downgrade();
                    let preserved = transitions.has_waiting_writer();
                    let ok = *reader == 2 && transitions.writer_count() == 0;
                    drop(reader);
                    let queued_writer = try_write_registered(&transitions);
                    let acquired = queued_writer.is_some();
                    transitions.finish_writer_turn(queued);
                    drop(queued_writer);
                    (true, ok, preserved && acquired)
                }
                Err(upgradeable) => {
                    drop(upgradeable);
                    (false, false, false)
                }
            }
        }
        None => (false, false, false),
    };
    let downgrade_upgradeable_ok = match transitions.try_write() {
        Some(writer) => {
            let upgradeable = writer.downgrade_to_upgradeable();
            let ok = *upgradeable == 2;
            drop(upgradeable);
            ok
        }
        None => false,
    };
    let downgrade_with_pending = RwLock::new(());
    let downgrade_pending_ok = match downgrade_with_pending.try_write() {
        Some(writer) => {
            let queued = downgrade_with_pending.register_waiting_writer().unwrap();
            let upgradeable = writer.downgrade_to_upgradeable();
            let preserved = downgrade_with_pending.has_waiting_writer();
            drop(upgradeable);
            let queued_writer = try_write_registered(&downgrade_with_pending);
            let acquired = queued_writer.is_some();
            downgrade_with_pending.finish_writer_turn(queued);
            drop(queued_writer);
            preserved && acquired && !downgrade_with_pending.has_waiting_writer()
        }
        None => false,
    };
    report.case(
        "guard_transitions",
        read_ok
            && upgrade_ok
            && downgrade_ok
            && pending_preserved
            && downgrade_upgradeable_ok
            && downgrade_pending_ok
            && transitions.reader_count() == 0
            && transitions.writer_count() == 0,
    );

    report.case(
        "concurrent_reader_flood_writer_progress",
        concurrent_reader_flood_writer_progress(),
    );

    (report.passed, report.failed, report.text)
}
