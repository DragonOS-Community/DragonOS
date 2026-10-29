//! Reproducible SMP stress coverage for ordinary RCU.
//!
//! This module is debugfs-driven test code. It deliberately keeps every test
//! object allocated until the final barrier, so a broken early callback can be
//! diagnosed without turning the oracle itself into a use-after-free.

use alloc::{
    boxed::Box,
    format,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use core::{
    ptr::{self, NonNull},
    sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicU8, AtomicUsize, Ordering},
};

use log::info;
use system_error::SystemError;

use crate::{
    libs::spinlock::SpinLock,
    process::{
        kthread::{KernelThreadClosure, KernelThreadMechanism},
        ProcessControlBlock, ProcessManager,
    },
    sched::{completion::Completion, cond_resched, sched_yield},
    smp::cpu::{smp_cpu_manager, ProcessorId},
};

use super::*;

pub(crate) const MAX_TORTURE_ROUNDS: usize = 4096;
const PUBLISHER_COUNT: usize = 2;
const MAX_READER_COUNT: usize = 4;
const RECLAIM_NONE: u8 = 0;
const RECLAIM_ASYNC: u8 = 1;
const RECLAIM_SYNC: u8 = 2;

const ERR_DUPLICATE_RECLAIM: usize = 1;
const ERR_PREMATURE_RECLAIM: usize = 2;
const ERR_CORRUPT_OBJECT: usize = 3;
const ERR_WRONG_RECLAIM_MODE: usize = 4;
const ERR_BARRIER_PREFIX: usize = 5;
const ERR_OWNERSHIP_MISMATCH: usize = 6;
const ERR_COMPLETION_WAIT: usize = 7;

const REGISTRY_PREPARED: u8 = 0;
const REGISTRY_EXPOSED: u8 = 1;
const REGISTRY_DRAINED: u8 = 2;
const REGISTRY_QUARANTINED: u8 = 3;

#[derive(Clone, Copy, Debug)]
pub(crate) struct RcuTortureConfig {
    pub seed: u64,
    pub rounds: usize,
}

impl RcuTortureConfig {
    pub(crate) fn validate(self) -> Result<Self, SystemError> {
        if self.rounds == 0 || self.rounds > MAX_TORTURE_ROUNDS {
            return Err(SystemError::EINVAL);
        }
        self.rounds.checked_add(1).ok_or(SystemError::E2BIG)?;
        Ok(self)
    }
}

pub(crate) struct RcuTortureResult {
    pub report: String,
    pub passed: bool,
    pub reboot_required: bool,
}

struct TortureShared {
    seed: u64,
    published: AtomicPtr<TortureObject>,
    admission_lock: SpinLock<()>,
    callback_completed: Vec<AtomicBool>,
    start: Arc<Completion>,
    ready: Arc<Completion>,
    done: Arc<Completion>,
    abort: AtomicBool,
    readers_started: AtomicUsize,
    expected_readers: usize,
    next_work: AtomicUsize,
    active_publishers: AtomicUsize,
    reads: AtomicU64,
    publishes: AtomicU64,
    callbacks_admitted: AtomicU64,
    callbacks_invoked: AtomicU64,
    sync_reclaims: AtomicU64,
    synchronize_calls: AtomicU64,
    gp_total_ns: AtomicU64,
    gp_max_ns: AtomicU64,
    barrier_calls: AtomicU64,
    max_observed_queue_depth: AtomicUsize,
    publication_cas_retries: AtomicU64,
    premature_reclaims: AtomicU64,
    duplicate_reclaims: AtomicU64,
    corrupt_reads: AtomicU64,
    first_error: AtomicUsize,
    first_error_worker: AtomicUsize,
    first_error_iteration: AtomicUsize,
}

impl TortureShared {
    fn new(seed: u64, callback_completed: Vec<AtomicBool>, expected_readers: usize) -> Self {
        Self {
            seed,
            published: AtomicPtr::new(ptr::null_mut()),
            admission_lock: SpinLock::new(()),
            callback_completed,
            readers_started: AtomicUsize::new(0),
            expected_readers,
            start: Arc::new(Completion::new()),
            ready: Arc::new(Completion::new()),
            done: Arc::new(Completion::new()),
            abort: AtomicBool::new(false),
            next_work: AtomicUsize::new(0),
            active_publishers: AtomicUsize::new(PUBLISHER_COUNT),
            reads: AtomicU64::new(0),
            publishes: AtomicU64::new(0),
            callbacks_admitted: AtomicU64::new(0),
            callbacks_invoked: AtomicU64::new(0),
            sync_reclaims: AtomicU64::new(0),
            synchronize_calls: AtomicU64::new(0),
            gp_total_ns: AtomicU64::new(0),
            gp_max_ns: AtomicU64::new(0),
            barrier_calls: AtomicU64::new(0),
            max_observed_queue_depth: AtomicUsize::new(0),
            publication_cas_retries: AtomicU64::new(0),
            premature_reclaims: AtomicU64::new(0),
            duplicate_reclaims: AtomicU64::new(0),
            corrupt_reads: AtomicU64::new(0),
            first_error: AtomicUsize::new(0),
            first_error_worker: AtomicUsize::new(0),
            first_error_iteration: AtomicUsize::new(0),
        }
    }

    fn record_error(&self, error: usize, worker: usize, iteration: usize) {
        if self
            .first_error
            .compare_exchange(0, error, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            self.first_error_worker.store(worker, Ordering::SeqCst);
            self.first_error_iteration
                .store(iteration, Ordering::SeqCst);
        }
    }

    fn sample_queue_depth(&self) {
        let (depth, _) = callback_queue_depth_snapshot();
        self.max_observed_queue_depth
            .fetch_max(depth.total(), Ordering::Relaxed);
    }
}

#[repr(C)]
struct TortureObject {
    head: RcuHead,
    generation: usize,
    checksum: u64,
    active_readers: AtomicUsize,
    reclaim_started: AtomicBool,
    async_admitted: AtomicBool,
    reclaim_mode: AtomicU8,
    shared: Arc<TortureShared>,
}

impl TortureObject {
    fn new(generation: usize, seed: u64, shared: Arc<TortureShared>) -> Self {
        Self {
            head: RcuHead::new(),
            generation,
            checksum: object_checksum(seed, generation),
            active_readers: AtomicUsize::new(0),
            reclaim_started: AtomicBool::new(false),
            async_admitted: AtomicBool::new(false),
            reclaim_mode: AtomicU8::new(RECLAIM_NONE),
            shared,
        }
    }
}

/// Owns every intrusive object and makes the fail-safe release policy part of
/// the type. The boxed slice provides stable callback addresses without a
/// raw-owner list or a separate allocation for every object.
struct TortureRegistry {
    objects: Box<[TortureObject]>,
    state: AtomicU8,
}

impl TortureRegistry {
    fn new(objects: Box<[TortureObject]>) -> Self {
        Self {
            objects,
            state: AtomicU8::new(REGISTRY_PREPARED),
        }
    }

    fn mark_exposed(&self) {
        assert_eq!(
            self.state.compare_exchange(
                REGISTRY_PREPARED,
                REGISTRY_EXPOSED,
                Ordering::AcqRel,
                Ordering::Acquire,
            ),
            Ok(REGISTRY_PREPARED),
            "RCU torture registry exposed from an invalid phase"
        );
    }

    fn mark_drained(&self) {
        assert_eq!(
            self.state.compare_exchange(
                REGISTRY_EXPOSED,
                REGISTRY_DRAINED,
                Ordering::AcqRel,
                Ordering::Acquire,
            ),
            Ok(REGISTRY_EXPOSED),
            "RCU torture registry drained from an invalid phase"
        );
    }

    fn quarantine(&self) {
        self.state.store(REGISTRY_QUARANTINED, Ordering::Release);
    }
}

impl core::ops::Deref for TortureRegistry {
    type Target = [TortureObject];

    fn deref(&self) -> &Self::Target {
        &self.objects
    }
}

impl Drop for TortureRegistry {
    fn drop(&mut self) {
        let state = self.state.load(Ordering::Acquire);
        if state == REGISTRY_EXPOSED || state == REGISTRY_QUARANTINED {
            // Fail-safe quarantine: leaking is required because an RCU bug may
            // have left an intrusive callback with this stable address.
            core::mem::forget(core::mem::take(&mut self.objects));
        }
        // PREPARED and DRAINED use ordinary boxed-slice destruction.
    }
}

struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self {
            state: seed ^ 0x9e37_79b9_7f4a_7c15,
        }
    }

    fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.state;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }
}

fn object_checksum(seed: u64, generation: usize) -> u64 {
    let mut random = SplitMix64::new(seed ^ generation as u64);
    random.next() ^ 0xd4c3_b2a1_5a5a_c3c3
}

fn error_name(error: usize) -> &'static str {
    match error {
        ERR_DUPLICATE_RECLAIM => "duplicate_reclaim",
        ERR_PREMATURE_RECLAIM => "premature_reclaim",
        ERR_CORRUPT_OBJECT => "corrupt_object",
        ERR_WRONG_RECLAIM_MODE => "wrong_reclaim_mode",
        ERR_BARRIER_PREFIX => "broken_barrier_prefix",
        ERR_OWNERSHIP_MISMATCH => "ownership_mismatch",
        ERR_COMPLETION_WAIT => "completion_wait_error",
        _ => "none",
    }
}

unsafe fn mark_reclaimed(
    object: NonNull<TortureObject>,
    expected_mode: u8,
    worker: usize,
    iteration: usize,
) {
    // SAFETY: the coordinator-owned registry keeps every object alive until
    // after the final barrier and worker join.
    let object = unsafe { object.as_ref() };
    let shared = &object.shared;
    if object
        .reclaim_started
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        shared.duplicate_reclaims.fetch_add(1, Ordering::Relaxed);
        shared.record_error(ERR_DUPLICATE_RECLAIM, worker, iteration);
        return;
    }

    if object.reclaim_mode.load(Ordering::SeqCst) != expected_mode {
        shared.record_error(ERR_WRONG_RECLAIM_MODE, worker, iteration);
    }
    if object.checksum != object_checksum(shared.seed, object.generation) {
        shared.corrupt_reads.fetch_add(1, Ordering::Relaxed);
        shared.record_error(ERR_CORRUPT_OBJECT, worker, iteration);
    }
    if object.active_readers.load(Ordering::SeqCst) != 0 {
        shared.premature_reclaims.fetch_add(1, Ordering::Relaxed);
        shared.record_error(ERR_PREMATURE_RECLAIM, worker, iteration);
    }
}

unsafe fn torture_callback(head: NonNull<RcuHead>) {
    // SAFETY: TortureObject is repr(C) and RcuHead is its first field. The
    // coordinator registry keeps the allocation stable through this callback.
    let object = head.cast::<TortureObject>();
    let (shared, generation) = {
        // SAFETY: callback admission keeps the embedded head stable, and a
        // failed final ownership check quarantines rather than frees it.
        let object_ref = unsafe { object.as_ref() };
        let generation = object_ref.generation;
        let shared = object_ref.shared.clone();
        unsafe { mark_reclaimed(object, RECLAIM_ASYNC, usize::MAX, generation) };
        (shared, generation)
    };
    // Publish completion only after the callback's last access through the
    // object/head. A broken early barrier can then be diagnosed without the
    // coordinator freeing storage still in use by this callback.
    shared.callbacks_invoked.fetch_add(1, Ordering::Release);
    shared.callback_completed[generation].store(true, Ordering::Release);
}

fn run_reader(shared: &Arc<TortureShared>, seed: u64, worker: usize) -> i32 {
    let mut random = SplitMix64::new(seed ^ 0x1000_0000_0000_0000 ^ worker as u64);
    let mut iteration = 0usize;
    let mut announced = false;
    while shared.active_publishers.load(Ordering::Acquire) != 0 {
        let guard = rcu_read_lock();
        let raw = rcu_dereference(&shared.published);
        if !raw.is_null() {
            // SAFETY: the read-side guard prevents legitimate reclamation and
            // the registry keeps even a broken early-reclaimed node allocated.
            let object = unsafe { &*raw };
            object.active_readers.fetch_add(1, Ordering::SeqCst);
            if object.reclaim_started.load(Ordering::SeqCst) {
                shared.premature_reclaims.fetch_add(1, Ordering::Relaxed);
                shared.record_error(ERR_PREMATURE_RECLAIM, worker, iteration);
            }
            if object.checksum != object_checksum(seed, object.generation) {
                shared.corrupt_reads.fetch_add(1, Ordering::Relaxed);
                shared.record_error(ERR_CORRUPT_OBJECT, worker, iteration);
            }
            for _ in 0..(random.next() as usize & 0x1f) {
                core::hint::spin_loop();
            }
            if object.reclaim_started.load(Ordering::SeqCst) {
                shared.premature_reclaims.fetch_add(1, Ordering::Relaxed);
                shared.record_error(ERR_PREMATURE_RECLAIM, worker, iteration);
            }
            object.active_readers.fetch_sub(1, Ordering::SeqCst);
            shared.reads.fetch_add(1, Ordering::Relaxed);
            if !announced {
                shared.readers_started.fetch_add(1, Ordering::Release);
                announced = true;
            }
        }
        drop(guard);
        iteration = iteration.wrapping_add(1);
        if iteration & 0x3f == 0 {
            cond_resched();
        }
    }
    0
}

fn run_publisher(
    shared: &Arc<TortureShared>,
    registry: &Arc<TortureRegistry>,
    config: RcuTortureConfig,
    worker: usize,
) -> i32 {
    let mut random = SplitMix64::new(config.seed ^ 0x2000_0000_0000_0000 ^ worker as u64);
    while shared.readers_started.load(Ordering::Acquire) != shared.expected_readers {
        sched_yield();
    }
    loop {
        let iteration = shared.next_work.fetch_add(1, Ordering::Relaxed);
        if iteration >= config.rounds {
            break;
        }

        let new = NonNull::from(&registry[iteration + 1]).as_ptr();
        let old = loop {
            let current = shared.published.load(Ordering::Acquire);
            match shared.published.compare_exchange_weak(
                current,
                new,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(old) => break old,
                Err(_) => {
                    shared
                        .publication_cas_retries
                        .fetch_add(1, Ordering::Relaxed);
                    core::hint::spin_loop();
                }
            }
        };
        if old.is_null() {
            shared.record_error(ERR_OWNERSHIP_MISMATCH, worker, iteration);
            continue;
        }
        let old = NonNull::new(old).unwrap();
        // Roughly one quarter of updates use synchronous reclamation. The
        // choice is deterministic per logical iteration, while the SMP
        // interleaving is intentionally left to the scheduler.
        let reclaim_choice =
            SplitMix64::new(config.seed ^ 0x3000_0000_0000_0000 ^ iteration as u64).next();
        if iteration < PUBLISHER_COUNT || reclaim_choice & 3 == 0 {
            // SAFETY: this publisher uniquely removed old from the slot.
            unsafe { old.as_ref() }
                .reclaim_mode
                .store(RECLAIM_SYNC, Ordering::SeqCst);
            let started = rcu_now();
            synchronize_rcu();
            let elapsed = rcu_now().wrapping_sub(started);
            shared.synchronize_calls.fetch_add(1, Ordering::Relaxed);
            shared.gp_total_ns.fetch_add(elapsed, Ordering::Relaxed);
            shared.gp_max_ns.fetch_max(elapsed, Ordering::Relaxed);
            // SAFETY: synchronize_rcu() covered the removed object.
            unsafe { mark_reclaimed(old, RECLAIM_SYNC, worker, iteration) };
            shared.sync_reclaims.fetch_add(1, Ordering::Release);
        } else {
            // SAFETY: this publisher uniquely removed old and the registry
            // keeps its embedded head stable through callback start.
            let object = unsafe { old.as_ref() };
            object.reclaim_mode.store(RECLAIM_ASYNC, Ordering::SeqCst);
            let _admission_guard = shared.admission_lock.lock_irqsave();
            unsafe { call_rcu_raw(NonNull::from(&object.head), torture_callback) };
            object.async_admitted.store(true, Ordering::Release);
            shared.callbacks_admitted.fetch_add(1, Ordering::Relaxed);
            drop(_admission_guard);
            shared.sample_queue_depth();
        }
        shared.publishes.fetch_add(1, Ordering::Relaxed);
        for _ in 0..(random.next() as usize & 0xf) {
            core::hint::spin_loop();
        }
        if iteration & 0x1f == 0 {
            sched_yield();
        }
    }

    shared.active_publishers.fetch_sub(1, Ordering::Release);
    0
}

fn check_barrier_prefix(
    shared: &Arc<TortureShared>,
    registry: &Arc<TortureRegistry>,
    scratch: &mut Vec<usize>,
    iteration: usize,
) {
    scratch.clear();
    {
        // Linearize the oracle snapshot against callback admission. This lock
        // is dropped before rcu_barrier(), so callbacks never acquire it and
        // it cannot enter production RCU lock ordering.
        let _admission_guard = shared.admission_lock.lock_irqsave();
        for object in registry.iter() {
            if object.async_admitted.load(Ordering::Acquire) {
                scratch.push(object.generation);
            }
        }
    }
    rcu_barrier();
    shared.barrier_calls.fetch_add(1, Ordering::Relaxed);
    for generation in scratch.iter().copied() {
        if !shared.callback_completed[generation].load(Ordering::Acquire) {
            shared.record_error(ERR_BARRIER_PREFIX, PUBLISHER_COUNT, iteration);
            break;
        }
    }
    shared.sample_queue_depth();
}

fn run_barrier(
    shared: &Arc<TortureShared>,
    registry: &Arc<TortureRegistry>,
    mut scratch: Vec<usize>,
) -> i32 {
    let mut iteration = 0usize;
    let mut last_admitted = 0;
    while shared.active_publishers.load(Ordering::Acquire) != 0 {
        let admitted = shared.callbacks_admitted.load(Ordering::Acquire);
        if admitted != last_admitted {
            check_barrier_prefix(shared, registry, &mut scratch, iteration);
            last_admitted = admitted;
            iteration = iteration.wrapping_add(1);
        }
        sched_yield();
    }
    check_barrier_prefix(shared, registry, &mut scratch, iteration);
    0
}

fn online_cpus() -> Vec<ProcessorId> {
    smp_cpu_manager()
        .present_cpus()
        .iter_cpu()
        .filter(|cpu| smp_cpu_manager().is_online_cpu(*cpu))
        .collect()
}

fn make_worker(
    body: impl Fn() -> i32 + Send + Sync + 'static,
    shared: Arc<TortureShared>,
) -> KernelThreadClosure {
    let ready = shared.ready.clone();
    let start = shared.start.clone();
    let done = shared.done.clone();
    KernelThreadClosure::EmptyClosure((
        Box::new(move || {
            ready.complete();
            let wait_error = wait_for_completions(&start, 1);
            if wait_error.is_some() {
                shared.record_error(ERR_COMPLETION_WAIT, usize::MAX, 0);
            }
            let result = if shared.abort.load(Ordering::Acquire) {
                1
            } else {
                body()
            };
            done.complete();
            result
        }),
        (),
    ))
}

/// Owns only kthread lifecycle mechanics. RCU phase ordering, barriers and
/// object recovery remain explicit in the coordinator below.
struct TortureWorkers {
    pcbs: Vec<Arc<ProcessControlBlock>>,
    shared: Arc<TortureShared>,
}

impl TortureWorkers {
    fn new(shared: Arc<TortureShared>, count: usize) -> Result<Self, SystemError> {
        let mut pcbs = Vec::new();
        pcbs.try_reserve_exact(count)
            .map_err(|_| SystemError::ENOMEM)?;
        Ok(Self { pcbs, shared })
    }

    fn push(&mut self, pcb: Arc<ProcessControlBlock>) {
        self.pcbs.push(pcb);
    }

    fn abort_before_start(&self) {
        self.shared.abort.store(true, Ordering::Release);
        self.shared.start.complete_all();
        for worker in &self.pcbs {
            let _ = ProcessManager::wakeup(worker);
            Self::stop_confirmed(worker);
        }
    }

    fn wake_all(&self) -> Result<(), SystemError> {
        for worker in &self.pcbs {
            if ProcessManager::wakeup(worker).is_err() {
                self.abort_before_start();
                return Err(SystemError::EIO);
            }
        }
        Ok(())
    }

    fn wait_ready(&self) -> Option<SystemError> {
        wait_for_completions(&self.shared.ready, self.pcbs.len())
    }

    fn wait_done(&self) -> Option<SystemError> {
        wait_for_completions(&self.shared.done, self.pcbs.len())
    }

    fn stop_all(&self) {
        for worker in &self.pcbs {
            if Self::stop_confirmed(worker) {
                self.shared.record_error(ERR_COMPLETION_WAIT, usize::MAX, 0);
            }
        }
    }

    /// Returns whether a transient wait error occurred. Retrying is safe:
    /// stop() first publishes SHOULD_STOP, and a subsequent call observes an
    /// already exited task or waits on the same exit completion.
    fn stop_confirmed(worker: &Arc<ProcessControlBlock>) -> bool {
        let mut saw_error = false;
        loop {
            match KernelThreadMechanism::stop(worker) {
                Ok(_) => return saw_error,
                Err(_) => {
                    saw_error = true;
                    sched_yield();
                }
            }
        }
    }
}

fn wait_for_completions(completion: &Completion, count: usize) -> Option<SystemError> {
    let mut completed = 0;
    let mut first_error = None;
    while completed < count {
        match completion.wait_for_completion() {
            Ok(_) => completed += 1,
            Err(error) => {
                first_error.get_or_insert(error);
                sched_yield();
            }
        }
    }
    first_error
}

pub(crate) fn run_torture(config: RcuTortureConfig) -> Result<RcuTortureResult, SystemError> {
    let config = config.validate()?;
    let cpus = online_cpus();
    if cpus.is_empty() {
        return Err(SystemError::ENODEV);
    }
    let reader_count = cpus.len().min(MAX_READER_COUNT);
    let worker_count = reader_count
        .checked_add(PUBLISHER_COUNT)
        .and_then(|count| count.checked_add(1))
        .ok_or(SystemError::E2BIG)?;
    let node_count = config.rounds.checked_add(1).ok_or(SystemError::E2BIG)?;
    let mut callback_completed = Vec::new();
    callback_completed
        .try_reserve_exact(node_count)
        .map_err(|_| SystemError::ENOMEM)?;
    callback_completed.resize_with(node_count, || AtomicBool::new(false));
    let shared = Arc::new(TortureShared::new(
        config.seed,
        callback_completed,
        reader_count,
    ));

    let mut objects = Vec::new();
    objects
        .try_reserve_exact(node_count)
        .map_err(|_| SystemError::ENOMEM)?;
    for generation in 0..node_count {
        objects.push(TortureObject::new(generation, config.seed, shared.clone()));
    }
    let registry = Arc::new(TortureRegistry::new(objects.into_boxed_slice()));
    let mut barrier_scratch = Vec::new();
    if barrier_scratch.try_reserve_exact(registry.len()).is_err() {
        return Err(SystemError::ENOMEM);
    }

    info!(
        "RCU torture start: seed={:#018x} rounds={} readers={} publishers={} cpus={}",
        config.seed,
        config.rounds,
        reader_count,
        PUBLISHER_COUNT,
        cpus.len(),
    );

    let mut workers = TortureWorkers::new(shared.clone(), worker_count)?;

    for worker in 0..reader_count {
        let worker_shared = shared.clone();
        let body_shared = shared.clone();
        let body = move || run_reader(&body_shared, config.seed, worker);
        let closure = make_worker(body, worker_shared);
        let Some(pcb) = KernelThreadMechanism::create_on_cpu(
            closure,
            format!("rcu-torture-reader-{worker}"),
            cpus[worker % cpus.len()],
        ) else {
            workers.abort_before_start();
            return Err(SystemError::ENOMEM);
        };
        workers.push(pcb);
    }

    for publisher in 0..PUBLISHER_COUNT {
        let worker_shared = shared.clone();
        let body_shared = shared.clone();
        let body_registry = registry.clone();
        let body = move || run_publisher(&body_shared, &body_registry, config, publisher);
        let closure = make_worker(body, worker_shared);
        let Some(pcb) = KernelThreadMechanism::create_on_cpu(
            closure,
            format!("rcu-torture-publisher-{publisher}"),
            cpus[(reader_count + publisher) % cpus.len()],
        ) else {
            workers.abort_before_start();
            return Err(SystemError::ENOMEM);
        };
        workers.push(pcb);
    }

    let worker_shared = shared.clone();
    let body_shared = shared.clone();
    let body_registry = registry.clone();
    let body_scratch = Arc::new(SpinLock::new(Some(barrier_scratch)));
    let body = move || {
        let scratch = body_scratch
            .lock_irqsave()
            .take()
            .expect("RCU torture barrier worker invoked more than once");
        run_barrier(&body_shared, &body_registry, scratch)
    };
    let closure = make_worker(body, worker_shared);
    let Some(barrier) = KernelThreadMechanism::create_on_cpu(
        closure,
        "rcu-torture-barrier".to_string(),
        cpus[(reader_count + PUBLISHER_COUNT) % cpus.len()],
    ) else {
        workers.abort_before_start();
        return Err(SystemError::ENOMEM);
    };
    workers.push(barrier);

    workers.wake_all()?;
    let mut completion_error = workers.wait_ready();

    registry.mark_exposed();
    shared
        .published
        .store(NonNull::from(&registry[0]).as_ptr(), Ordering::Release);
    let started_at = rcu_now();
    shared.start.complete_all();

    if let Some(error) = workers.wait_done() {
        completion_error.get_or_insert(error);
    }
    if completion_error.is_some() {
        shared.record_error(ERR_COMPLETION_WAIT, usize::MAX, 0);
    }
    workers.stop_all();

    let final_object = shared.published.swap(ptr::null_mut(), Ordering::AcqRel);
    if let Some(final_object) = NonNull::new(final_object) {
        // SAFETY: all publishers have exited, so the coordinator uniquely
        // removed the final published object.
        let object = unsafe { final_object.as_ref() };
        object.reclaim_mode.store(RECLAIM_ASYNC, Ordering::SeqCst);
        let _admission_guard = shared.admission_lock.lock_irqsave();
        unsafe { call_rcu_raw(NonNull::from(&object.head), torture_callback) };
        object.async_admitted.store(true, Ordering::Release);
        shared.callbacks_admitted.fetch_add(1, Ordering::Relaxed);
    } else {
        shared.record_error(ERR_OWNERSHIP_MISMATCH, usize::MAX, 0);
    }
    rcu_barrier();
    shared.sample_queue_depth();
    let elapsed_ns = rcu_now().wrapping_sub(started_at);

    let mut every_object_reclaimed = true;
    let mut every_async_callback_completed = true;
    for object in registry.iter() {
        if !object.reclaim_started.load(Ordering::Acquire) {
            every_object_reclaimed = false;
            shared.record_error(ERR_OWNERSHIP_MISMATCH, usize::MAX, object.generation);
        }
        if object.async_admitted.load(Ordering::Acquire)
            && !shared.callback_completed[object.generation].load(Ordering::Acquire)
        {
            every_async_callback_completed = false;
            shared.record_error(ERR_OWNERSHIP_MISMATCH, usize::MAX, object.generation);
        }
    }
    let admitted = shared.callbacks_admitted.load(Ordering::Acquire);
    let invoked = shared.callbacks_invoked.load(Ordering::Acquire);
    let sync_reclaims = shared.sync_reclaims.load(Ordering::Acquire);
    let ownership_matches = admitted == invoked
        && invoked.checked_add(sync_reclaims) == Some(node_count as u64)
        && every_object_reclaimed
        && every_async_callback_completed;
    if !ownership_matches {
        shared.record_error(ERR_OWNERSHIP_MISMATCH, usize::MAX, 0);
    }

    let first_error = shared.first_error.load(Ordering::SeqCst);
    let passed = first_error == 0;
    let synchronize_calls = shared.synchronize_calls.load(Ordering::Relaxed);
    let gp_total_ns = shared.gp_total_ns.load(Ordering::Relaxed);
    let callback_throughput = invoked
        .saturating_mul(1_000_000_000)
        .checked_div(elapsed_ns)
        .unwrap_or(0);
    let report = format!(
        concat!(
            "status={}\n",
            "seed={:#018x}\n",
            "rounds={}\n",
            "cpus={}\n",
            "readers={}\n",
            "reads={}\n",
            "publishes={}\n",
            "callbacks_admitted={}\n",
            "callbacks_invoked={}\n",
            "sync_reclaims={}\n",
            "synchronize_calls={}\n",
            "gp_average_ns={}\n",
            "gp_max_ns={}\n",
            "barrier_calls={}\n",
            "callback_throughput_per_sec={}\n",
            "max_observed_callback_queue_depth={}\n",
            "publication_cas_retries={}\n",
            "premature_reclaims={}\n",
            "duplicate_reclaims={}\n",
            "corrupt_reads={}\n",
            "registry_quarantined={}\n",
            "elapsed_ns={}\n",
            "first_error={}\n",
            "first_error_worker={}\n",
            "first_error_iteration={}\n",
        ),
        if passed { "ok" } else { "fail" },
        config.seed,
        config.rounds,
        cpus.len(),
        reader_count,
        shared.reads.load(Ordering::Relaxed),
        shared.publishes.load(Ordering::Relaxed),
        admitted,
        invoked,
        sync_reclaims,
        synchronize_calls,
        gp_total_ns.checked_div(synchronize_calls).unwrap_or(0),
        shared.gp_max_ns.load(Ordering::Relaxed),
        shared.barrier_calls.load(Ordering::Relaxed),
        callback_throughput,
        shared.max_observed_queue_depth.load(Ordering::Relaxed),
        shared.publication_cas_retries.load(Ordering::Relaxed),
        shared.premature_reclaims.load(Ordering::Relaxed),
        shared.duplicate_reclaims.load(Ordering::Relaxed),
        shared.corrupt_reads.load(Ordering::Relaxed),
        usize::from(!ownership_matches),
        elapsed_ns,
        error_name(first_error),
        shared.first_error_worker.load(Ordering::SeqCst),
        shared.first_error_iteration.load(Ordering::SeqCst),
    );

    info!(
        "RCU torture finish: seed={:#018x} status={} reads={} publishes={} callbacks={}/{} sync={}",
        config.seed,
        if passed { "ok" } else { "fail" },
        shared.reads.load(Ordering::Relaxed),
        shared.publishes.load(Ordering::Relaxed),
        invoked,
        admitted,
        sync_reclaims,
    );

    if ownership_matches {
        registry.mark_drained();
    } else {
        registry.quarantine();
    }
    Ok(RcuTortureResult {
        report,
        passed,
        reboot_required: !ownership_matches,
    })
}
