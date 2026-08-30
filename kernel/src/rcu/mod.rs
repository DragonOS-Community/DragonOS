#![allow(dead_code)]

//! DragonOS ordinary RCU.
//!
//! This is a non-preemptible, non-sleepable RCU flavor. Read-side critical
//! sections may nest, but must remain on the creating task and must not block.
//! A completed grace period covers every read-side critical section that
//! existed before that GP began. It need not wait for readers that started
//! afterward.
//!
//! Pointer publication uses Release operations and subscription uses Acquire
//! operations. The global RCU state lock plus full barriers at real GP start,
//! real quiescent-state reporting, GP completion, and synchronous return form
//! the happens-before chain from callback admission to callback invocation or
//! `synchronize_rcu()` return.

use alloc::{
    boxed::Box,
    rc::Rc,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use core::{
    fmt::Write,
    marker::PhantomData,
    ptr::{self, NonNull},
    sync::atomic::{fence, AtomicBool, AtomicPtr, AtomicUsize, Ordering},
};

use log::warn;

use crate::{
    libs::{cpumask::CpuMask, mutex::Mutex, spinlock::SpinLock, wait_queue::WaitQueue},
    mm::percpu::PerCpu,
    process::{
        kthread::KernelThreadClosure, kthread::KernelThreadMechanism, preempt::PreemptGuard,
        ProcessManager,
    },
    sched::{cond_resched, sched_yield, SchedPolicy},
    smp::{
        core::smp_get_processor_id,
        cpu::{smp_cpu_manager, smp_cpu_manager_initialized, ProcessorId},
    },
};

mod callback;
mod context;
mod gp;
mod selftest;
pub use callback::RcuHead;
use callback::{RcuCallbackQueueDepth, RcuSegmentedCallbacks};
use context::{
    BaseContext, ContextTransition, ContextTransitionError, IdleTransition, IrqDisposition,
    IrqEntry, RcuContextTracker,
};
use gp::{GracePeriodState, RcuSequence};
pub use selftest::run_debug_selftests;

pub(crate) type RcuRawCallback = unsafe fn(NonNull<RcuHead>);

pub struct RcuReadGuard {
    active: bool,
    _not_send: PhantomData<Rc<()>>,
}

#[must_use = "RCU idle entry must be paired with idle_exit(token)"]
pub struct RcuIdleToken {
    cpu: ProcessorId,
    _not_send: PhantomData<Rc<()>>,
}

#[must_use = "RCU IRQ entry must be paired with irq_exit(token, disposition)"]
pub struct RcuIrqToken {
    cpu: ProcessorId,
    entry: IrqEntry,
    _not_send: PhantomData<Rc<()>>,
}

impl RcuIrqToken {
    #[inline]
    pub fn is_outermost(&self) -> bool {
        self.entry.outermost()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RcuIrqDisposition {
    ResumeInterrupted,
    ToKernel,
}

impl Drop for RcuReadGuard {
    fn drop(&mut self) {
        if self.active {
            rcu_read_unlock();
        }
    }
}

#[derive(Debug)]
/// An RCU-published non-null `Arc` slot.
///
/// Deferred updates and destruction allocate callback storage, so this type
/// must be updated and dropped only from a task context where allocation is
/// permitted. Its read-side operations do not inherit that restriction.
pub struct RcuArcSlot<T>
where
    T: Send + Sync + 'static,
{
    ptr: AtomicPtr<T>,
}

unsafe fn defer_drop_slot_arc_raw<T>(raw: *mut T)
where
    T: Send + Sync + 'static,
{
    if raw.is_null() {
        return;
    }

    // SAFETY: callers pass a non-null pointer previously created by
    // Arc::into_raw() for the slot-owned reference. Removing that pointer from
    // an RCU-visible slot is a publication removal, so the reference must be
    // released only after a grace period.
    let old = unsafe { Arc::from_raw(raw) };
    rcu_defer_drop(old);
}

impl<T> RcuArcSlot<T>
where
    T: Send + Sync + 'static,
{
    pub fn new(initial: Arc<T>) -> Self {
        Self {
            ptr: AtomicPtr::new(Arc::into_raw(initial) as *mut T),
        }
    }

    pub fn load(&self) -> Arc<T> {
        let _guard = rcu_read_lock();
        let raw = rcu_dereference(&self.ptr);
        assert!(!raw.is_null(), "RcuArcSlot::load saw a null pointer");

        // SAFETY: the slot stores a valid Arc allocation. RCU prevents the
        // backing allocation from being reclaimed until after the current read
        // section, which gives us a stable window to acquire a strong count.
        unsafe {
            Arc::increment_strong_count(raw);
            Arc::from_raw(raw)
        }
    }

    pub fn with_read<R>(&self, f: impl FnOnce(&T) -> R) -> R {
        if !rcu_enabled() {
            let pinned = self.load();
            return f(&pinned);
        }

        let _guard = rcu_read_lock();
        let raw = rcu_dereference(&self.ptr);
        assert!(!raw.is_null(), "RcuArcSlot::with_read saw a null pointer");

        // SAFETY: RCU keeps the slot-owned Arc allocation alive for the whole
        // read-side section. The closure receives only a shared reference and
        // cannot outlive the guard held in this function.
        f(unsafe { &*raw })
    }

    /// Replaces the slot-owned reference without arranging its deferred drop.
    ///
    /// # Safety
    ///
    /// The caller must either keep the returned slot-owned `Arc` (or an
    /// equivalent strong reference) alive until a strong reference is
    /// submitted to `rcu_defer_drop()`, or prove that `new` points to the same
    /// allocation and remains continuously published by the slot. Any later
    /// removal of that allocation must still use deferred drop.
    pub(crate) unsafe fn swap(&self, new: Arc<T>) -> Arc<T> {
        let new_raw = Arc::into_raw(new) as *mut T;
        let old_raw = self.ptr.swap(new_raw, Ordering::AcqRel);
        assert!(
            !old_raw.is_null(),
            "RcuArcSlot::swap replaced a null pointer"
        );

        // SAFETY: the slot always contains an Arc-owned allocation. `swap`
        // transfers the single slot ownership from `old_raw` to `new_raw`,
        // so reconstructing the previous Arc is correct.
        unsafe { Arc::from_raw(old_raw) }
    }

    pub fn store_deferred(&self, new: Arc<T>) {
        // SAFETY: the removed slot reference is immediately transferred to
        // the RCU deferred-drop queue.
        let old = unsafe { self.swap(new) };
        rcu_defer_drop(old);
    }

    pub fn swap_deferred(&self, new: Arc<T>) -> Arc<T> {
        // SAFETY: the clone submitted below keeps the removed allocation alive
        // through a grace period even if the caller drops the returned Arc.
        let old = unsafe { self.swap(new) };
        rcu_defer_drop(old.clone());
        old
    }
}

impl<T> Drop for RcuArcSlot<T>
where
    T: Send + Sync + 'static,
{
    fn drop(&mut self) {
        let raw = self.ptr.swap(ptr::null_mut(), Ordering::AcqRel);
        // SAFETY: `raw` was removed from this slot exactly once. Even though
        // `Drop` has exclusive access to the slot, an RCU reader may have
        // already dereferenced the old pointer and not yet pinned its own Arc.
        unsafe { defer_drop_slot_arc_raw(raw) };
    }
}

#[derive(Debug)]
/// An RCU-published optional `Arc` slot.
///
/// Deferred updates and destruction allocate callback storage, so this type
/// must be updated and dropped only from a task context where allocation is
/// permitted. Its read-side operations do not inherit that restriction.
pub struct RcuOptionArcSlot<T>
where
    T: Send + Sync + 'static,
{
    ptr: AtomicPtr<T>,
}

impl<T> RcuOptionArcSlot<T>
where
    T: Send + Sync + 'static,
{
    pub const fn new_none() -> Self {
        Self {
            ptr: AtomicPtr::new(ptr::null_mut()),
        }
    }

    pub fn new_some(initial: Arc<T>) -> Self {
        Self {
            ptr: AtomicPtr::new(Arc::into_raw(initial) as *mut T),
        }
    }

    /// Pins a snapshot of the currently published value.
    ///
    /// A null precheck is a valid `None` snapshot and keeps the common empty
    /// path out of RCU. A non-null precheck is only a hint: the pointer is
    /// loaded again inside RCU before its strong count is incremented. Writers
    /// must defer release of a removed slot-owned reference until after an RCU
    /// grace period. Before RCU is initialized, callers rely on the existing
    /// boot-time invariant that no concurrent writer can mutate the slot.
    #[inline]
    pub fn load(&self) -> Option<Arc<T>> {
        if self.ptr.load(Ordering::Acquire).is_null() {
            return None;
        }

        let _guard = rcu_read_lock();
        let raw = rcu_dereference(&self.ptr);
        if raw.is_null() {
            return None;
        }

        // SAFETY: the non-null pointer is owned by the slot as an Arc raw
        // pointer. RCU keeps the allocation alive while the read section is
        // active, so it is safe to acquire a strong reference before leaving.
        unsafe {
            Arc::increment_strong_count(raw);
            Some(Arc::from_raw(raw))
        }
    }

    /// Replaces the slot-owned reference without arranging its deferred drop.
    ///
    /// # Safety
    ///
    /// The caller must either keep the returned slot-owned `Arc` (or an
    /// equivalent strong reference) alive until a strong reference is
    /// submitted to `rcu_defer_drop()`, or prove that the replacement points
    /// to the same allocation and remains continuously published by the slot.
    /// Any later removal of that allocation must still use deferred drop.
    pub(crate) unsafe fn swap(&self, new: Option<Arc<T>>) -> Option<Arc<T>> {
        let new_raw = new
            .map(|value| Arc::into_raw(value) as *mut T)
            .unwrap_or_default();
        let old_raw = self.ptr.swap(new_raw, Ordering::AcqRel);

        NonNull::new(old_raw).map(|old| {
            // SAFETY: a non-null pointer in the slot was previously created by
            // Arc::into_raw. The swap removes the slot-owned reference exactly
            // once, so reconstructing the Arc transfers that ownership back.
            unsafe { Arc::from_raw(old.as_ptr()) }
        })
    }

    pub fn store_deferred(&self, new: Option<Arc<T>>) {
        // SAFETY: every removed slot reference is immediately transferred to
        // the RCU deferred-drop queue.
        if let Some(old) = unsafe { self.swap(new) } {
            rcu_defer_drop(old);
        }
    }

    pub fn clear_if_deferred<F>(&self, mut pred: F) -> bool
    where
        F: FnMut(&T) -> bool,
    {
        loop {
            let _guard = rcu_read_lock();
            let raw = rcu_dereference(&self.ptr);
            let Some(current) = NonNull::new(raw) else {
                return false;
            };

            // SAFETY: the pointer is protected by the RCU read-side critical
            // section. Writers that remove it must defer the final slot-owned
            // Arc drop until after this read-side section completes.
            let should_clear = pred(unsafe { current.as_ref() });
            if !should_clear {
                return false;
            }

            if self
                .ptr
                .compare_exchange(raw, ptr::null_mut(), Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                // SAFETY: the successful compare_exchange removed this exact
                // slot-owned Arc reference, so reconstructing it transfers
                // ownership for deferred drop exactly once.
                let old = unsafe { Arc::from_raw(raw) };
                drop(_guard);
                rcu_defer_drop(old);
                return true;
            }
        }
    }
}

impl<T> Drop for RcuOptionArcSlot<T>
where
    T: Send + Sync + 'static,
{
    fn drop(&mut self) {
        let raw = self.ptr.swap(ptr::null_mut(), Ordering::AcqRel);
        // SAFETY: `raw` was removed from this slot exactly once. Dropping a
        // slot is still a removal from an RCU-visible publication point, so the
        // slot-owned Arc reference must be released after a grace period.
        unsafe { defer_drop_slot_arc_raw(raw) };
    }
}

struct RcuStateInner {
    gp: GracePeriodState,
    /// CPUs that have executed the RCU starting hook and are eligible for
    /// future GP snapshots. The active GP keeps its own immutable snapshot.
    participating_cpus: CpuMask,
}

impl RcuStateInner {
    fn new() -> Self {
        Self {
            gp: GracePeriodState::new(),
            participating_cpus: CpuMask::new(),
        }
    }
}

struct RcuCpuCallbackState {
    segments: RcuSegmentedCallbacks,
    executing: bool,
}

impl RcuCpuCallbackState {
    const fn new() -> Self {
        Self {
            segments: RcuSegmentedCallbacks::new(),
            executing: false,
        }
    }

    fn depth(&self) -> RcuCallbackQueueDepth {
        let mut depth = self.segments.depth();
        depth.executing = self.executing;
        depth
    }
}

#[repr(align(64))]
struct RcuCpuCallbacks {
    state: SpinLock<RcuCpuCallbackState>,
    needs_scan: AtomicBool,
    barrier_head: RcuHead,
}

impl RcuCpuCallbacks {
    const fn new() -> Self {
        Self {
            state: SpinLock::new(RcuCpuCallbackState::new()),
            needs_scan: AtomicBool::new(false),
            barrier_head: RcuHead::new(),
        }
    }

    fn publish_work(&self) -> bool {
        !self.needs_scan.swap(true, Ordering::AcqRel)
    }
}

const RCU_CALLBACK_BATCH_LIMIT: usize = 64;
const RCU_CALLBACK_CPU_QUANTUM: usize = 8;

struct RcuState {
    initialized: AtomicBool,
    worker_starting: AtomicBool,
    worker_started: AtomicBool,
    worker_should_stop: AtomicBool,
    gp_active: AtomicBool,
    contexts: Box<[RcuContextTracker]>,
    cpu_callbacks: Box<[RcuCpuCallbacks]>,
    inner: SpinLock<RcuStateInner>,
    callback_ownership: SpinLock<()>,
    executor_claimed: AtomicBool,
    worker_kick_pending: AtomicBool,
    next_scan_cpu: AtomicUsize,
    callbacks_invoked: AtomicUsize,
    barrier_mutex: Mutex<()>,
    barrier_remaining: AtomicUsize,
    barrier_wait: WaitQueue,
    state_wait: WaitQueue,
    worker_wait: WaitQueue,
}

impl RcuState {
    fn new() -> Self {
        // Construct both per-CPU arrays directly in heap storage. Embedding
        // contexts in RcuState makes lazy initialization reserve a large stack
        // frame even on the already-initialized fast path.
        let mut contexts = Vec::with_capacity(PerCpu::MAX_CPU_NUM as usize);
        contexts.resize_with(PerCpu::MAX_CPU_NUM as usize, RcuContextTracker::new);
        let mut cpu_callbacks = Vec::with_capacity(PerCpu::MAX_CPU_NUM as usize);
        cpu_callbacks.resize_with(PerCpu::MAX_CPU_NUM as usize, RcuCpuCallbacks::new);

        Self {
            initialized: AtomicBool::new(false),
            worker_starting: AtomicBool::new(false),
            worker_started: AtomicBool::new(false),
            worker_should_stop: AtomicBool::new(false),
            gp_active: AtomicBool::new(false),
            contexts: contexts.into_boxed_slice(),
            cpu_callbacks: cpu_callbacks.into_boxed_slice(),
            inner: SpinLock::new(RcuStateInner::new()),
            callback_ownership: SpinLock::new(()),
            executor_claimed: AtomicBool::new(false),
            worker_kick_pending: AtomicBool::new(false),
            next_scan_cpu: AtomicUsize::new(0),
            callbacks_invoked: AtomicUsize::new(0),
            barrier_mutex: Mutex::new(()),
            barrier_remaining: AtomicUsize::new(0),
            barrier_wait: WaitQueue::default(),
            state_wait: WaitQueue::default(),
            worker_wait: WaitQueue::default(),
        }
    }

    fn is_initialized(&self) -> bool {
        self.initialized.load(Ordering::Acquire)
    }

    fn pump_grace_periods(inner: &mut RcuStateInner) -> bool {
        Self::pump_grace_periods_with(
            inner,
            participating_context_snapshot,
            credit_context_progress_locked,
            publish_gp_active,
            |completed| RCU_STATE.complete_callback_gp(completed),
            |starting| RCU_STATE.prepare_callback_gp_start(starting),
        )
    }

    fn pump_grace_periods_with(
        inner: &mut RcuStateInner,
        mut waiting_snapshot: impl FnMut(&CpuMask) -> (CpuMask, [u64; PerCpu::MAX_CPU_NUM as usize]),
        mut credit_context: impl FnMut(&mut GracePeriodState),
        mut publish_active: impl FnMut(bool),
        mut complete_callbacks: impl FnMut(RcuSequence) -> bool,
        mut prepare_callbacks: impl FnMut(RcuSequence),
    ) -> bool {
        let mut ready_changed = false;
        loop {
            credit_context(&mut inner.gp);

            if inner.gp.ready_to_complete() {
                // Pair all real quiescent-state reports with GP completion.
                // This is a GP slow path and does not affect RCU readers.
                fence(Ordering::SeqCst);
                let completed = inner.gp.complete_ready();
                ready_changed |= complete_callbacks(completed);
                continue;
            }

            if inner.gp.is_active() {
                break;
            }

            if !inner.gp.has_request() {
                break;
            }

            // Admission/request operations preceding this point must be
            // ordered before the fresh CPU snapshot that defines GP start.
            let starting = inner.gp.current().next();
            prepare_callbacks(starting);
            publish_active(true);
            fence(Ordering::SeqCst);
            let (waiting_cpus, context_generations) = waiting_snapshot(&inner.participating_cpus);
            let started = inner.gp.start_requested(waiting_cpus, context_generations);
            debug_assert_eq!(started, starting);
        }

        publish_active(inner.gp.is_active());

        debug_assert!(inner.gp.is_active() || !inner.gp.has_request());
        ready_changed
    }

    fn complete_callback_gp(&self, completed: RcuSequence) -> bool {
        let _ownership = self.callback_ownership.lock_irqsave();
        let mut became_ready = false;
        for callbacks in &self.cpu_callbacks {
            let ready = callbacks
                .state
                .lock_irqsave()
                .segments
                .complete_gp(completed);
            if ready {
                callbacks.publish_work();
                became_ready = true;
            }
        }
        became_ready
    }

    fn prepare_callback_gp_start(&self, starting: RcuSequence) {
        let _ownership = self.callback_ownership.lock_irqsave();
        for callbacks in &self.cpu_callbacks {
            let mut state = callbacks.state.lock_irqsave();
            state.segments.prepare_gp_start(starting);
            let runnable = state.segments.has_ready() || state.segments.has_unclassified();
            callbacks.needs_scan.store(runnable, Ordering::Release);
        }
    }

    fn classify_new_callbacks(&self, inner: &mut RcuStateInner) -> bool {
        let _ownership = self.callback_ownership.lock_irqsave();
        let mut classified = false;
        for callbacks in &self.cpu_callbacks {
            // Only `next` admissions require classification, and enqueue
            // publishes them through this persistent per-CPU predicate.
            // Ready-only queues may still pass the filter, but idle and
            // GP-blocked possible CPUs avoid an irqsave lock entirely.
            if !callbacks.needs_scan.load(Ordering::Acquire) {
                continue;
            }
            let mut state = callbacks.state.lock_irqsave();
            if state.segments.has_unclassified() {
                let target = inner.gp.request_future();
                let active = inner.gp.is_active();
                classified |= state.segments.classify_next(target, active);
            }
            let runnable = state.segments.has_ready() || state.segments.has_unclassified();
            callbacks.needs_scan.store(runnable, Ordering::Release);
        }
        classified
    }

    /// Moves all queued callback segments while the caller holds the GP lock.
    fn migrate_callback_segments(&self, source: usize, destination: usize) {
        if source == destination {
            return;
        }

        let _ownership = self.callback_ownership.lock_irqsave();
        let (low, high) = if source < destination {
            (source, destination)
        } else {
            (destination, source)
        };
        let mut low_state = self.cpu_callbacks[low].state.lock_irqsave();
        let mut high_state = self.cpu_callbacks[high].state.lock_irqsave();

        if source < destination {
            high_state.segments.merge_from(&mut low_state.segments);
        } else {
            low_state.segments.merge_from(&mut high_state.segments);
        }

        let destination_state = if destination == low {
            &*low_state
        } else {
            &*high_state
        };
        let destination_runnable =
            destination_state.segments.has_ready() || destination_state.segments.has_unclassified();
        self.cpu_callbacks[destination]
            .needs_scan
            .store(destination_runnable, Ordering::Release);

        let source_state = if source == low {
            &*low_state
        } else {
            &*high_state
        };
        let source_runnable =
            source_state.segments.has_ready() || source_state.segments.has_unclassified();
        self.cpu_callbacks[source]
            .needs_scan
            .store(source_runnable, Ordering::Release);
    }

    fn progress_callbacks_and_gps(&self) -> bool {
        let mut inner = self.inner.lock_irqsave();
        let mut ready_changed = Self::pump_grace_periods(&mut inner);
        let classified = self.classify_new_callbacks(&mut inner);
        if classified {
            ready_changed |= Self::pump_grace_periods(&mut inner);
        }
        ready_changed
    }

    fn has_worker_work(&self) -> bool {
        if self
            .cpu_callbacks
            .iter()
            .any(|callbacks| callbacks.needs_scan.load(Ordering::Acquire))
        {
            return true;
        }
        let inner = self.inner.lock_irqsave();
        inner.gp.ready_to_complete() || (!inner.gp.is_active() && inner.gp.has_request())
    }

    fn wake_state_waiters(&self) {
        self.state_wait.wake_all();
    }

    fn wake_worker(&self) {
        // A single worker only needs one outstanding kick. Coalescing here
        // keeps simultaneous per-CPU admissions from all serializing on the
        // waitqueue's internal lock.
        if !self.worker_kick_pending.swap(true, Ordering::AcqRel) {
            self.worker_wait.wake_all();
        }
    }

    fn wake_barrier_waiter_if_pending(&self) {
        if self.barrier_remaining.load(Ordering::Acquire) != 0 {
            self.barrier_wait.wake_all();
        }
    }

    fn progress_and_drain_inline_if_no_worker(&self) {
        if self.worker_started.load(Ordering::Acquire) {
            return;
        }
        self.process_callback_batch();
    }

    fn try_claim_executor(&self) -> Option<RcuExecutorGuard<'_>> {
        self.executor_claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| RcuExecutorGuard {
                claimed: &self.executor_claimed,
            })
    }

    fn pop_ready_from_cpu(&self, cpu: usize) -> Option<callback::ReadyRcuCallback> {
        let callbacks = &self.cpu_callbacks[cpu];
        // Most possible CPUs have no runnable callback. Avoid taking their
        // queue locks during a scan; publishers store queue state before
        // making this persistent predicate visible.
        if !callbacks.needs_scan.load(Ordering::Acquire) {
            return None;
        }

        let mut state = callbacks.state.lock_irqsave();
        let callback = state.segments.pop_ready()?;
        debug_assert!(!state.executing);
        state.executing = true;
        let more = state.segments.has_ready() || state.segments.has_unclassified();
        callbacks.needs_scan.store(more, Ordering::Release);
        Some(callback)
    }

    fn pop_ready_round_robin(
        &self,
        preferred_cpu: Option<usize>,
    ) -> Option<(usize, callback::ReadyRcuCallback)> {
        let cpu_count = PerCpu::MAX_CPU_NUM as usize;
        if let Some(cpu) = preferred_cpu {
            if let Some(callback) = self.pop_ready_from_cpu(cpu) {
                return Some((cpu, callback));
            }
        }

        let start = self.next_scan_cpu.load(Ordering::Relaxed) % cpu_count;
        for offset in 0..cpu_count {
            let cpu = (start + offset) % cpu_count;
            if let Some(callback) = self.pop_ready_from_cpu(cpu) {
                self.next_scan_cpu
                    .store((cpu + 1) % cpu_count, Ordering::Relaxed);
                return Some((cpu, callback));
            }
        }
        None
    }

    fn finish_callback(&self, cpu: usize) {
        let callbacks = &self.cpu_callbacks[cpu];
        let mut state = callbacks.state.lock_irqsave();
        debug_assert!(state.executing);
        state.executing = false;
        let more = state.segments.has_ready() || state.segments.has_unclassified();
        callbacks.needs_scan.store(more, Ordering::Release);
    }

    fn process_callback_batch(&self) -> usize {
        let Some(_executor) = self.try_claim_executor() else {
            self.wake_worker();
            return 0;
        };

        self.progress_callbacks_and_gps();
        // GP completion, including a completion credited from a context
        // snapshot, may satisfy synchronize_rcu() even when no callback is
        // ready. One wake per batch is sufficient.
        self.wake_state_waiters();
        let mut count = 0;
        let mut preferred_cpu = None;
        let mut cpu_quantum = 0;
        while count < RCU_CALLBACK_BATCH_LIMIT {
            let Some((cpu, callback)) = self.pop_ready_round_robin(preferred_cpu) else {
                break;
            };
            // SAFETY: `pop_ready()` detached the head, copied all state needed
            // after invocation, and released duplicate ownership. The unsafe
            // admission contract keeps the head valid until this call starts.
            unsafe { (callback.func)(callback.head) };
            self.callbacks_invoked.fetch_add(1, Ordering::Relaxed);
            self.finish_callback(cpu);
            count += 1;

            if preferred_cpu == Some(cpu) {
                cpu_quantum += 1;
            } else {
                preferred_cpu = Some(cpu);
                cpu_quantum = 1;
            }
            if cpu_quantum == RCU_CALLBACK_CPU_QUANTUM {
                preferred_cpu = None;
                cpu_quantum = 0;
            }
        }

        let more = self.has_worker_work();
        drop(_executor);
        if more {
            self.wake_worker();
        }
        // In the pre-worker boot window, the barrier waiter is itself the
        // bounded inline executor. Wake it once per batch so markers behind
        // more than one batch cannot stall indefinitely.
        self.wake_barrier_waiter_if_pending();
        if count != 0 {
            cond_resched();
        }
        count
    }
}

struct RcuExecutorGuard<'a> {
    claimed: &'a AtomicBool,
}

impl Drop for RcuExecutorGuard<'_> {
    fn drop(&mut self) {
        self.claimed.store(false, Ordering::Release);
    }
}

lazy_static! {
    static ref RCU_STATE: RcuState = RcuState::new();
}

#[inline]
fn rcu_enabled() -> bool {
    RCU_STATE.is_initialized()
}

fn publish_gp_active(active: bool) {
    // Production callers hold `RCU_STATE.inner`, so GP-active publishers are
    // serialized. Avoid a write RMW on the common inactive -> inactive path:
    // context-transition readers would otherwise contend on this cache line.
    let was_active = RCU_STATE.gp_active.load(Ordering::SeqCst);
    if was_active == active {
        return;
    }

    RCU_STATE.gp_active.store(active, Ordering::SeqCst);
    if !active {
        for context in &RCU_STATE.contexts {
            context.clear_gp_report();
        }
    }
}

fn participating_context_snapshot(
    participating_cpus: &CpuMask,
) -> (CpuMask, [u64; PerCpu::MAX_CPU_NUM as usize]) {
    let mut waiting = CpuMask::new();
    let mut generations = [0; PerCpu::MAX_CPU_NUM as usize];

    for cpu in participating_cpus.iter_cpu() {
        let context = &RCU_STATE.contexts[cpu.data() as usize];
        context.prepare_gp_report();
        let snapshot = context.snapshot();
        generations[cpu.data() as usize] = snapshot.generation();
        if snapshot.is_watching() {
            waiting.set(cpu, true);
        } else {
            context.clear_gp_report();
        }
    }

    (waiting, generations)
}

#[inline]
fn current_task_is_idle() -> bool {
    ProcessManager::current_pcb().sched_info().policy() == SchedPolicy::IDLE
}

#[inline]
fn credit_context_progress_locked(gp: &mut GracePeriodState) {
    for cpu_idx in 0..PerCpu::MAX_CPU_NUM as usize {
        let cpu = ProcessorId::new(cpu_idx as u32);
        if !gp.is_waiting_for(cpu) {
            continue;
        }
        let context = &RCU_STATE.contexts[cpu_idx];
        let snapshot = context.snapshot();
        if gp.report_context_progress(cpu, snapshot.generation(), snapshot.in_eqs()) {
            context.clear_gp_report();
        }
    }
}

/// Reports a real quiescent state while `RcuState::inner` is held.
///
/// The full barrier is paid only when this CPU is actually a holdout for an
/// active GP. Context switches, user returns, and duplicate reports that do
/// not advance a GP stay free of this extra barrier.
fn report_quiescent_state_locked(inner: &mut RcuStateInner, cpu: ProcessorId) -> bool {
    if !inner.gp.is_waiting_for(cpu) {
        return false;
    }

    fence(Ordering::SeqCst);
    let cleared = inner.gp.report_quiescent_state(cpu);
    debug_assert!(cleared, "RCU holdout disappeared before QS reporting");
    if cleared {
        RCU_STATE.contexts[cpu.data() as usize].clear_gp_report();
    }
    cleared
}

fn prepare_cpu_starting_locked(inner: &RcuStateInner, cpu: ProcessorId) -> bool {
    !inner.participating_cpus.get(cpu).unwrap_or(false) && !inner.gp.is_waiting_for(cpu)
}

fn cpu_starting_locked(inner: &mut RcuStateInner, cpu: ProcessorId) {
    inner.participating_cpus.set(cpu, true);
}

fn cpu_dying_locked(inner: &mut RcuStateInner, cpu: ProcessorId) {
    inner.participating_cpus.set(cpu, false);
    report_quiescent_state_locked(inner, cpu);
}

fn report_quiescent_state(cpu: ProcessorId) {
    if !rcu_enabled() {
        return;
    }

    let (wake_worker, wake_waiters) = {
        let mut inner = RCU_STATE.inner.lock_irqsave();
        report_quiescent_state_locked(&mut inner, cpu);
        let ready_changed = RcuState::pump_grace_periods(&mut inner);
        (ready_changed, true)
    };

    if wake_waiters {
        RCU_STATE.wake_state_waiters();
    }
    if wake_worker {
        RCU_STATE.wake_worker();
    }
    RCU_STATE.wake_barrier_waiter_if_pending();
}

fn queue_raw_callback(head: NonNull<RcuHead>, func: RcuRawCallback) {
    // Pin before selecting the queue. `lock_irqsave()` only disables
    // preemption after a particular lock has already been selected.
    let pin = PreemptGuard::new();
    let cpu = smp_get_processor_id().data() as usize;
    let callbacks = &RCU_STATE.cpu_callbacks[cpu];
    callbacks.state.lock_irqsave().segments.enqueue(head, func);
    let wake_worker = callbacks.publish_work();
    drop(pin);

    if wake_worker {
        RCU_STATE.wake_worker();
    }
}

#[repr(C)]
struct DeferredRcuCall<F> {
    head: RcuHead,
    call: Option<F>,
}

impl<F> DeferredRcuCall<F> {
    fn new(call: F) -> Self {
        Self {
            head: RcuHead::new(),
            call: Some(call),
        }
    }
}

unsafe fn invoke_deferred_rcu_call<F>(head: NonNull<RcuHead>)
where
    F: FnOnce() + Send + 'static,
{
    // SAFETY: `DeferredRcuCall` is repr(C) with `head` as its first field, and
    // `submit_deferred_rcu_call()` transferred this Box to the callback.
    let mut deferred = unsafe { Box::from_raw(head.as_ptr().cast::<DeferredRcuCall<F>>()) };
    let call = deferred
        .call
        .take()
        .expect("deferred RCU callback invoked more than once");
    call();
}

fn submit_deferred_rcu_call<F>(deferred: Box<DeferredRcuCall<F>>)
where
    F: FnOnce() + Send + 'static,
{
    let deferred = Box::into_raw(deferred);
    // SAFETY: the Box has been transferred to `invoke_deferred_rcu_call`, so
    // it remains at this address until callback invocation starts.
    unsafe {
        call_rcu_raw(
            NonNull::new_unchecked(core::ptr::addr_of_mut!((*deferred).head)),
            invoke_deferred_rcu_call::<F>,
        );
    }
}

fn try_queue_deferred_callback_with<F, A>(call: F, allocate: A) -> Result<(), ()>
where
    F: FnOnce() + Send + 'static,
    A: FnOnce(DeferredRcuCall<F>) -> Result<Box<DeferredRcuCall<F>>, ()>,
{
    let deferred = allocate(DeferredRcuCall::new(call))?;
    submit_deferred_rcu_call(deferred);
    Ok(())
}

fn worker_main() -> i32 {
    {
        let _inner = RCU_STATE.inner.lock_irqsave();
        RCU_STATE.worker_started.store(true, Ordering::Release);
        RCU_STATE.worker_starting.store(false, Ordering::Release);
    }
    RCU_STATE.wake_state_waiters();

    loop {
        RCU_STATE.worker_wait.wait_until(|| {
            if RCU_STATE.worker_should_stop.load(Ordering::Acquire) {
                return Some(());
            }

            if RCU_STATE.has_worker_work() {
                return Some(());
            }

            // Retire the coalesced kick only after observing no work, then
            // recheck. A publisher racing either side of this store will
            // therefore be observed by the predicate or issue a fresh wake.
            RCU_STATE
                .worker_kick_pending
                .store(false, Ordering::Release);
            if RCU_STATE.worker_should_stop.load(Ordering::Acquire) {
                return Some(());
            }
            if RCU_STATE.has_worker_work() {
                return Some(());
            }

            None
        });

        if RCU_STATE.worker_should_stop.load(Ordering::Acquire) {
            break;
        }

        RCU_STATE.process_callback_batch();
    }

    {
        let _inner = RCU_STATE.inner.lock_irqsave();
        RCU_STATE.worker_started.store(false, Ordering::Release);
    }
    RCU_STATE.wake_state_waiters();
    // Publish the executor handoff to a barrier that observed the worker
    // before shutdown. This wake must be unconditional: the exit path has no
    // synchronization that requires it to observe a concurrent barrier's
    // counter publication before deciding whether to wake.
    RCU_STATE.barrier_wait.wake_all();
    0
}

pub fn init() {
    if RCU_STATE.initialized.load(Ordering::Acquire) {
        return;
    }

    // The BSP is already executing and may have used its persistent context
    // before the SMP manager exists. Admit it without applying the AP restart
    // reset protocol.
    let boot_cpu = smp_get_processor_id();
    let mut inner = RCU_STATE.inner.lock_irqsave();
    let previous = inner.participating_cpus.set(boot_cpu, true);
    debug_assert_eq!(previous, Some(false));
    drop(inner);

    RCU_STATE.initialized.store(true, Ordering::Release);
}

pub fn start_worker() {
    if !rcu_enabled() {
        return;
    }

    {
        let _inner = RCU_STATE.inner.lock_irqsave();
        if RCU_STATE.worker_should_stop.load(Ordering::Acquire)
            || RCU_STATE.worker_started.load(Ordering::Acquire)
            || RCU_STATE.worker_starting.load(Ordering::Acquire)
        {
            return;
        }
        RCU_STATE.worker_starting.store(true, Ordering::Release);
    }

    let worker_cpu = smp_get_processor_id();
    let closure = KernelThreadClosure::EmptyClosure((Box::new(worker_main), ()));
    if KernelThreadMechanism::create_and_run_on_cpu(closure, "rcu_gp".to_string(), worker_cpu)
        .is_none()
    {
        let _inner = RCU_STATE.inner.lock_irqsave();
        RCU_STATE.worker_starting.store(false, Ordering::Release);
        panic!("failed to create the bound RCU callback worker");
    }

    RCU_STATE.wake_worker();
}

/// Requests terminal asynchronous shutdown of the RCU worker.
///
/// This is a teardown operation: the global RCU worker is initialized once
/// and must not be restarted after shutdown has been requested.
pub fn shutdown_worker() {
    if !rcu_enabled() {
        return;
    }

    let wake_worker = {
        let _inner = RCU_STATE.inner.lock_irqsave();
        let wake_worker = RCU_STATE.worker_started.load(Ordering::Acquire)
            || RCU_STATE.worker_starting.load(Ordering::Acquire);
        RCU_STATE.worker_should_stop.store(true, Ordering::Release);
        wake_worker
    };
    if wake_worker {
        RCU_STATE.wake_worker();
    }
}

/// Enters a non-preemptible, non-sleepable ordinary-RCU read-side section.
///
/// Sections may nest. The returned guard is task-bound and must be dropped on
/// the task that created it. Do not block or call an RCU synchronization API
/// while the guard is alive.
pub fn rcu_read_lock() -> RcuReadGuard {
    if !rcu_enabled() {
        return RcuReadGuard {
            active: false,
            _not_send: PhantomData,
        };
    }

    ProcessManager::preempt_disable();
    #[cfg(debug_assertions)]
    {
        let cpu = smp_get_processor_id();
        let watching = RCU_STATE.contexts[cpu.data() as usize]
            .snapshot()
            .is_watching();
        if !watching {
            warn!("rcu_read_lock() called while RCU is not watching CPU {cpu:?}");
            debug_assert!(watching, "ordinary RCU reader entered from an EQS");
        }
    }
    ProcessManager::current_pcb().rcu_read_lock();
    RcuReadGuard {
        active: true,
        _not_send: PhantomData,
    }
}

pub fn rcu_read_unlock() {
    if !rcu_enabled() {
        return;
    }

    let pcb = ProcessManager::current_pcb();
    pcb.rcu_read_unlock();
    ProcessManager::preempt_enable();
}

pub fn rcu_read_lock_held() -> bool {
    if !rcu_enabled() || !ProcessManager::initialized() {
        return false;
    }

    ProcessManager::current_pcb().rcu_read_depth() > 0
}

#[inline]
/// Subscribes to a pointer published with `rcu_assign_pointer()`.
///
/// The returned raw pointer must only be dereferenced while protected by an
/// ordinary-RCU read-side critical section or another proven lifetime pin.
pub fn rcu_dereference<T>(ptr: &AtomicPtr<T>) -> *mut T {
    ptr.load(Ordering::Acquire)
}

#[inline]
/// Publishes a fully initialized RCU-protected pointer with Release ordering.
pub fn rcu_assign_pointer<T>(ptr: &AtomicPtr<T>, value: *mut T) {
    fence(Ordering::Release);
    ptr.store(value, Ordering::Release);
}

/// Queues `func` for exactly-once invocation after a GP that starts after
/// admission. Before RCU initialization, boot-time no-reader rules make the
/// callback execute synchronously instead.
///
/// # Safety
///
/// `head` must remain initialized at the same address until `func` begins. It
/// must not be queued again before that point. Clearing the duplicate state
/// does not transfer ownership to an unsynchronized third party.
pub(crate) unsafe fn call_rcu_raw(head: NonNull<RcuHead>, func: RcuRawCallback) {
    if !rcu_enabled() {
        // SAFETY: before RCU init there is no concurrent reader relying on
        // grace-period semantics, so direct invocation is safe.
        unsafe { func(head) };
        return;
    }

    // SAFETY: the caller guarantees that `head` remains initialized at this
    // address until callback invocation starts.
    if !unsafe { head.as_ref() }.try_claim() {
        panic!("call_rcu_raw received a duplicated rcu_head enqueue");
    }

    queue_raw_callback(head, func);
}

/// Defers a closure until after a future grace period.
///
/// This convenience API allocates before raw callback admission. It must not
/// be called from IRQ or other contexts where allocation is forbidden; use an
/// object-embedded [`RcuHead`] with `call_rcu_raw()` there.
pub fn rcu_defer<F>(f: F)
where
    F: FnOnce() + Send + 'static,
{
    if !rcu_enabled() {
        f();
        return;
    }

    submit_deferred_rcu_call(Box::new(DeferredRcuCall::new(f)));
}

/// Defers destruction using the allocating [`rcu_defer`] helper.
///
/// This must not be called from IRQ or another non-allocating context.
pub fn rcu_defer_drop<T>(value: T)
where
    T: Send + 'static,
{
    rcu_defer(move || {
        drop(value);
    });
}

/// Tries to defer the drop of an `Arc` without invoking the allocator's
/// infallible OOM path.
///
/// On failure, the original reference is returned and has not been published
/// to the RCU callback queue. The caller must keep it alive through a grace
/// period before dropping it.
///
/// This helper still invokes the allocator and is not an IRQ-context API.
pub(crate) fn try_rcu_defer_drop_arc<T>(value: Arc<T>) -> Result<(), Arc<T>>
where
    T: Send + Sync + 'static,
{
    if !rcu_enabled() {
        drop(value);
        return Ok(());
    }

    let queued_value = value.clone();
    if try_queue_deferred_callback_with(
        move || drop(queued_value),
        |deferred| Box::try_new(deferred).map_err(|_| ()),
    )
    .is_err()
    {
        return Err(value);
    }

    Ok(())
}

/// Waits for a GP that starts after this call.
///
/// This function may sleep and must not be called from an RCU read-side
/// critical section.
pub fn synchronize_rcu() {
    if !rcu_enabled() {
        return;
    }

    if rcu_read_lock_held() {
        warn!("synchronize_rcu() called inside rcu_read_lock() region");
        debug_assert!(!rcu_read_lock_held());
    }

    let target_gp = {
        let mut inner = RCU_STATE.inner.lock_irqsave();
        let target_gp = inner.gp.request_future();
        RcuState::pump_grace_periods(&mut inner);
        target_gp
    };

    RCU_STATE.wake_state_waiters();
    RCU_STATE.wake_worker();

    RCU_STATE.state_wait.wait_until(|| {
        let completed = RCU_STATE.inner.lock_irqsave().gp.has_completed(target_gp);
        if completed {
            Some(())
        } else {
            None
        }
    });
    fence(Ordering::SeqCst);
}

/// Waits for a grace period without registering a waiter or allocating.
///
/// This is reserved for recovery after a fallible callback admission has
/// already failed. Normal callers should use `synchronize_rcu()`, which sleeps
/// efficiently on the RCU state wait queue.
pub(crate) fn synchronize_rcu_noalloc() {
    if !rcu_enabled() {
        return;
    }

    if rcu_read_lock_held() {
        warn!("synchronize_rcu_noalloc() called inside rcu_read_lock() region");
        debug_assert!(!rcu_read_lock_held());
    }

    let target_gp = {
        let mut inner = RCU_STATE.inner.lock_irqsave();
        let target_gp = inner.gp.request_future();
        RcuState::pump_grace_periods(&mut inner);
        target_gp
    };

    RCU_STATE.wake_state_waiters();
    RCU_STATE.wake_worker();

    loop {
        let completed = RCU_STATE.inner.lock_irqsave().gp.has_completed(target_gp);
        if completed {
            break;
        }
        sched_yield();
    }
    fence(Ordering::SeqCst);
}

/// Waits until every callback admitted before this call has finished.
///
/// This does not necessarily start a GP when no callbacks are pending. It must
/// not be called from an RCU read-side critical section or from an RCU callback
/// that would be included in its own barrier snapshot.
pub fn rcu_barrier() {
    if !rcu_enabled() {
        return;
    }

    if rcu_read_lock_held() {
        warn!("rcu_barrier() called inside rcu_read_lock() region");
        debug_assert!(!rcu_read_lock_held());
    }

    let _barrier = RCU_STATE.barrier_mutex.lock();
    debug_assert_eq!(RCU_STATE.barrier_remaining.load(Ordering::Acquire), 0);
    RCU_STATE.barrier_remaining.store(1, Ordering::Release);

    {
        // Keep ownership stable across the complete scan. Otherwise a CPU
        // migration could move callbacks from an unscanned source into an
        // already-scanned destination.
        let _ownership = RCU_STATE.callback_ownership.lock_irqsave();
        for callbacks in &RCU_STATE.cpu_callbacks {
            let mut state = callbacks.state.lock_irqsave();
            if state.segments.is_empty() && !state.executing {
                continue;
            }

            if !callbacks.barrier_head.try_claim() {
                panic!("RCU barrier head was already queued");
            }
            RCU_STATE.barrier_remaining.fetch_add(1, Ordering::AcqRel);
            let marker = NonNull::from(&callbacks.barrier_head);
            if !state.segments.entrain(marker, rcu_barrier_callback) {
                debug_assert!(state.executing);
                state.segments.push_done(marker, rcu_barrier_callback);
            }
            if state.segments.has_ready() || state.segments.has_unclassified() {
                callbacks.publish_work();
            }
        }
    }

    // Drop the setup sentinel only after every queue has been inspected.
    if RCU_STATE.barrier_remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
        fence(Ordering::SeqCst);
        return;
    }

    RCU_STATE.wake_worker();
    RCU_STATE.barrier_wait.wait_until(|| {
        RCU_STATE.progress_and_drain_inline_if_no_worker();
        (RCU_STATE.barrier_remaining.load(Ordering::Acquire) == 0).then_some(())
    });
    fence(Ordering::SeqCst);
}

unsafe fn rcu_barrier_callback(_head: NonNull<RcuHead>) {
    let previous = RCU_STATE.barrier_remaining.fetch_sub(1, Ordering::AcqRel);
    debug_assert!(previous > 0, "RCU barrier callback count underflow");
    if previous == 1 {
        RCU_STATE.barrier_wait.wake_all();
    }
}

pub fn note_context_switch() {
    if !rcu_enabled() {
        return;
    }

    let current = ProcessManager::current_pcb();
    if current.rcu_read_depth() != 0 {
        warn!("context switch observed while still inside rcu_read_lock()");
        debug_assert_eq!(current.rcu_read_depth(), 0);
        return;
    }

    report_quiescent_state(smp_get_processor_id());
}

fn context_transition_or_panic<T>(operation: &str, result: Result<T, ContextTransitionError>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => {
            warn!("invalid RCU context transition {operation}: {error:?}");
            panic!("invalid RCU context transition");
        }
    }
}

/// Credits an EQS boundary without executing callbacks inline. After the
/// transition, this path must remain free of ordinary RCU read-side use.
fn note_context_eqs_transition(transition: ContextTransition) {
    if !rcu_enabled() || !transition.entered_eqs() {
        return;
    }

    // This SeqCst load participates in the GP-start/context-transition
    // handshake documented in docs/kernel/libs/rcu-context-tracking.md.
    let cpu = smp_get_processor_id();
    let context = &RCU_STATE.contexts[cpu.data() as usize];
    if !RCU_STATE.gp_active.load(Ordering::SeqCst) || !context.gp_report_needed() {
        return;
    }

    let (wake_waiters, wake_worker) = {
        let mut inner = RCU_STATE.inner.lock_irqsave();
        if !inner.gp.is_waiting_for(cpu) {
            context.clear_gp_report();
            (false, false)
        } else {
            let ready_changed = RcuState::pump_grace_periods(&mut inner);
            (true, ready_changed)
        }
    };

    if wake_waiters {
        RCU_STATE.wake_state_waiters();
        // Before the callback worker starts, rcu_barrier() is the bounded
        // inline executor. An EQS transition may be the final GP holdout, so
        // it must wake that executor just like report_qs() and cpu_dying().
        RCU_STATE.wake_barrier_waiter_if_pending();
    }
    if wake_worker {
        RCU_STATE.wake_worker();
    }
}

pub fn user_enter() {
    let cpu = smp_get_processor_id();
    let transition = context_transition_or_panic(
        "user_enter",
        RCU_STATE.contexts[cpu.data() as usize].user_enter(),
    );
    note_context_eqs_transition(transition);
}

pub fn user_exit() {
    let cpu = smp_get_processor_id();
    context_transition_or_panic(
        "user_exit",
        RCU_STATE.contexts[cpu.data() as usize].user_exit(),
    );
}

pub fn idle_enter() -> RcuIdleToken {
    if !current_task_is_idle() {
        warn!("rcu::idle_enter() must only be called from the idle task");
        debug_assert!(current_task_is_idle());
        panic!("RCU idle entry outside the idle task");
    }

    let cpu = smp_get_processor_id();
    let transition: IdleTransition = context_transition_or_panic(
        "idle_enter",
        RCU_STATE.contexts[cpu.data() as usize].idle_enter(),
    );
    note_context_eqs_transition(transition.transition());
    RcuIdleToken {
        cpu,
        _not_send: PhantomData,
    }
}

pub fn idle_exit(token: RcuIdleToken) {
    if !current_task_is_idle() {
        warn!("rcu::idle_exit() must only be called from the idle task");
        debug_assert!(current_task_is_idle());
        panic!("RCU idle exit outside the idle task");
    }

    let cpu = smp_get_processor_id();
    assert_eq!(token.cpu, cpu, "RCU idle token migrated across CPUs");
    context_transition_or_panic(
        "idle_exit",
        RCU_STATE.contexts[cpu.data() as usize].idle_exit(),
    );
}

pub fn irq_enter() -> RcuIrqToken {
    let cpu = smp_get_processor_id();
    let entry = context_transition_or_panic(
        "irq_enter",
        RCU_STATE.contexts[cpu.data() as usize].irq_enter(),
    );
    RcuIrqToken {
        cpu,
        entry,
        _not_send: PhantomData,
    }
}

pub fn irq_exit(token: RcuIrqToken, disposition: RcuIrqDisposition) {
    let cpu = smp_get_processor_id();
    assert_eq!(token.cpu, cpu, "RCU IRQ token migrated across CPUs");
    let disposition = match disposition {
        RcuIrqDisposition::ResumeInterrupted => IrqDisposition::ResumeInterrupted,
        RcuIrqDisposition::ToKernel => IrqDisposition::ToKernel,
    };
    let transition = context_transition_or_panic(
        "irq_exit",
        RCU_STATE.contexts[cpu.data() as usize].irq_exit(&token.entry, disposition),
    );
    note_context_eqs_transition(transition);
}

/// Prepares an AP's context before the SMP coordinator publishes Starting.
///
/// Returns false if a previous lifecycle still owns RCU participation or GP
/// responsibility. The caller must not start the AP in that case.
pub fn prepare_cpu_starting(cpu: ProcessorId) -> bool {
    if !rcu_enabled() {
        return true;
    }

    let inner = RCU_STATE.inner.lock_irqsave();
    if !prepare_cpu_starting_locked(&inner, cpu) {
        return false;
    }
    RCU_STATE.contexts[cpu.data() as usize].reset_for_cpu_starting();
    true
}

/// Admits the current AP to future GP snapshots.
///
/// This must run on the incoming CPU with interrupts disabled, before its
/// startup path first uses ordinary RCU. It deliberately does not modify the
/// immutable waiting snapshot of an already-active GP.
pub fn cpu_starting(cpu: ProcessorId) {
    if !rcu_enabled() {
        return;
    }

    let mut inner = RCU_STATE.inner.lock_irqsave();
    cpu_starting_locked(&mut inner, cpu);
    drop(inner);

    // Match Linux rcu_cpu_starting(): no ordinary RCU read-side operation on
    // the incoming AP may become visible before its context initialization
    // and future-GP admission are globally published.
    fence(Ordering::SeqCst);
}

/// Removes a CPU from future GP admission and transfers its active-GP
/// responsibility before the CPU becomes unable to report.
pub fn cpu_dying(cpu: ProcessorId) {
    if !rcu_enabled() {
        return;
    }

    let wake_worker = {
        let mut inner = RCU_STATE.inner.lock_irqsave();
        cpu_dying_locked(&mut inner, cpu);
        RCU_STATE.contexts[cpu.data() as usize].clear_gp_report();
        let mut ready_changed = RcuState::pump_grace_periods(&mut inner);
        let classified = RCU_STATE.classify_new_callbacks(&mut inner);
        if classified {
            ready_changed |= RcuState::pump_grace_periods(&mut inner);
        }

        // `cpu_dying_locked()` removed the source from the authoritative RCU
        // admission set. Select the destination from that same set while the
        // GP lock is held, so lifecycle state and queue ownership cannot
        // disagree. If this is the final participant, the global executor can
        // still drain the stable source record without migrating it.
        if let Some(destination) = inner.participating_cpus.iter_cpu().next() {
            RCU_STATE.migrate_callback_segments(cpu.data() as usize, destination.data() as usize);
        }

        ready_changed
            || RCU_STATE
                .cpu_callbacks
                .iter()
                .any(|callbacks| callbacks.needs_scan.load(Ordering::Acquire))
    };

    RCU_STATE.wake_state_waiters();
    if wake_worker {
        RCU_STATE.wake_worker();
    }
    RCU_STATE.wake_barrier_waiter_if_pending();
}

#[allow(dead_code)]
pub fn debug_snapshot() -> (u64, u64, u64, usize, bool) {
    let inner = RCU_STATE.inner.lock_irqsave();
    let (aggregate, _) = callback_queue_depth_snapshot();
    (
        inner.gp.current().raw(),
        inner.gp.completed().raw(),
        RCU_STATE.callbacks_invoked.load(Ordering::Relaxed) as u64,
        aggregate.total(),
        aggregate.done != 0,
    )
}

pub(crate) fn callback_queue_depth_snapshot() -> (
    RcuCallbackQueueDepth,
    [RcuCallbackQueueDepth; PerCpu::MAX_CPU_NUM as usize],
) {
    let mut aggregate = RcuCallbackQueueDepth::default();
    let mut per_cpu = [RcuCallbackQueueDepth::default(); PerCpu::MAX_CPU_NUM as usize];
    for (cpu, callbacks) in RCU_STATE.cpu_callbacks.iter().enumerate() {
        let depth = callbacks.state.lock_irqsave().depth();
        per_cpu[cpu] = depth;
        aggregate.add_assign(depth);
    }
    (aggregate, per_cpu)
}

pub(crate) fn callback_queue_debug_report() -> String {
    let (aggregate, per_cpu) = callback_queue_depth_snapshot();
    let mut report = String::new();
    writeln!(
        report,
        "aggregate total={} done={} wait={} next_ready={} next={} executing={}",
        aggregate.total(),
        aggregate.done,
        aggregate.wait,
        aggregate.next_ready,
        aggregate.next,
        usize::from(aggregate.executing),
    )
    .expect("writing RCU callback snapshot to String failed");

    for (cpu, depth) in per_cpu.iter().copied().enumerate() {
        let present = !smp_cpu_manager_initialized()
            || smp_cpu_manager()
                .present_cpus()
                .get(ProcessorId::new(cpu as u32))
                .unwrap_or(false);
        if !present && depth.total() == 0 && !depth.executing {
            continue;
        }
        writeln!(
            report,
            "cpu={} total={} done={} wait={} next_ready={} next={} executing={}",
            cpu,
            depth.total(),
            depth.done,
            depth.wait,
            depth.next_ready,
            depth.next,
            usize::from(depth.executing),
        )
        .expect("writing RCU per-CPU callback snapshot to String failed");
    }
    report
}

#[allow(dead_code)]
pub fn debug_force_quiescent_state() {
    report_quiescent_state(smp_get_processor_id());
}

#[allow(dead_code)]
pub fn debug_current_cpu_in_idle_eqs() -> bool {
    let cpu = smp_get_processor_id();
    let snapshot = RCU_STATE.contexts[cpu.data() as usize].snapshot();
    snapshot.base() == BaseContext::Idle && snapshot.in_eqs()
}
