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

use alloc::{boxed::Box, collections::VecDeque, rc::Rc, string::ToString, sync::Arc};
use core::{
    marker::PhantomData,
    ptr::{self, NonNull},
    sync::atomic::{fence, AtomicBool, AtomicPtr, Ordering},
};

use log::{error, warn};

use crate::{
    libs::{cpumask::CpuMask, spinlock::SpinLock, wait_queue::WaitQueue},
    mm::percpu::PerCpu,
    process::{kthread::KernelThreadClosure, kthread::KernelThreadMechanism, ProcessManager},
    sched::{sched_yield, SchedPolicy},
    smp::{
        core::smp_get_processor_id,
        cpu::{smp_cpu_manager, smp_cpu_manager_initialized, ProcessorId},
    },
};

mod gp;
mod selftest;
use gp::{CallbackTracker, GracePeriodState, RcuSequence};
pub use selftest::run_debug_selftests;

pub(crate) type RcuRawCallback = unsafe fn(NonNull<RcuHead>);

#[derive(Clone, Copy)]
struct QueuedRcuHead(NonNull<RcuHead>);

// SAFETY: the wrapped head is an opaque token that may be transferred to the
// RCU worker thread after `call_rcu_raw()` publishes it. The caller must keep
// the underlying allocation alive until the callback runs, and the token is not
// dereferenced except when the worker invokes that callback after a grace
// period.
unsafe impl Send for QueuedRcuHead {}

#[derive(Debug)]
pub struct RcuHead {
    queued: AtomicBool,
}

impl RcuHead {
    pub const fn new() -> Self {
        Self {
            queued: AtomicBool::new(false),
        }
    }
}

pub struct RcuReadGuard {
    active: bool,
    _not_send: PhantomData<Rc<()>>,
}

impl Drop for RcuReadGuard {
    fn drop(&mut self) {
        if self.active {
            rcu_read_unlock();
        }
    }
}

trait DeferredCall: Send {
    fn invoke(self: Box<Self>);
}

impl<F> DeferredCall for F
where
    F: FnOnce() + Send,
{
    fn invoke(self: Box<Self>) {
        (*self)();
    }
}

#[derive(Debug)]
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

enum CallbackKind {
    RawHead {
        head: QueuedRcuHead,
        func: RcuRawCallback,
    },
    Deferred(Box<dyn DeferredCall>),
}

struct CallbackItem {
    target_gp: RcuSequence,
    seq: RcuSequence,
    kind: CallbackKind,
}

#[derive(Clone, Copy, Debug, Default)]
struct RcuCpuState {
    in_idle_eqs: bool,
    irq_nesting: usize,
    irq_from_idle_eqs: bool,
}

struct RcuStateInner {
    gp: GracePeriodState,
    callbacks: CallbackTracker,
    cpu_states: [RcuCpuState; PerCpu::MAX_CPU_NUM as usize],
    pending_callbacks: VecDeque<CallbackItem>,
    ready_callbacks: VecDeque<CallbackItem>,
}

impl RcuStateInner {
    fn new() -> Self {
        Self {
            gp: GracePeriodState::new(),
            callbacks: CallbackTracker::new(),
            cpu_states: [RcuCpuState::default(); PerCpu::MAX_CPU_NUM as usize],
            pending_callbacks: VecDeque::new(),
            ready_callbacks: VecDeque::new(),
        }
    }

    fn has_ready_work(&self) -> bool {
        !self.ready_callbacks.is_empty()
    }

    fn has_drainable_work(&self) -> bool {
        self.has_ready_work() && self.callbacks.drainer_available()
    }
}

struct RcuState {
    initialized: AtomicBool,
    worker_started: AtomicBool,
    worker_should_stop: AtomicBool,
    inner: SpinLock<RcuStateInner>,
    state_wait: WaitQueue,
    worker_wait: WaitQueue,
}

impl RcuState {
    fn new() -> Self {
        Self {
            initialized: AtomicBool::new(false),
            worker_started: AtomicBool::new(false),
            worker_should_stop: AtomicBool::new(false),
            inner: SpinLock::new(RcuStateInner::new()),
            state_wait: WaitQueue::default(),
            worker_wait: WaitQueue::default(),
        }
    }

    fn is_initialized(&self) -> bool {
        self.initialized.load(Ordering::Acquire)
    }

    fn pump_grace_periods(inner: &mut RcuStateInner) -> bool {
        Self::pump_grace_periods_with(inner, online_non_idle_cpus)
    }

    fn pump_grace_periods_with(
        inner: &mut RcuStateInner,
        mut waiting_snapshot: impl FnMut(&[RcuCpuState; PerCpu::MAX_CPU_NUM as usize]) -> CpuMask,
    ) -> bool {
        let mut ready_changed = false;
        loop {
            if inner.gp.ready_to_complete() {
                // Pair all real quiescent-state reports with GP completion.
                // This is a GP slow path and does not affect RCU readers.
                fence(Ordering::SeqCst);
                let completed = inner.gp.complete_ready();

                while inner
                    .pending_callbacks
                    .front()
                    .is_some_and(|cb| cb.target_gp == completed)
                {
                    if let Some(cb) = inner.pending_callbacks.pop_front() {
                        inner.ready_callbacks.push_back(cb);
                        ready_changed = true;
                    }
                }
                debug_assert!(
                    inner
                        .pending_callbacks
                        .front()
                        .is_none_or(|cb| !inner.gp.has_completed(cb.target_gp)),
                    "pending RCU callback targets an already completed GP"
                );
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
            fence(Ordering::SeqCst);
            let waiting_cpus = waiting_snapshot(&inner.cpu_states);
            inner.gp.start_requested(waiting_cpus);
        }

        debug_assert!(inner.gp.is_active() || !inner.gp.has_request());
        debug_assert!(
            inner.pending_callbacks.is_empty() || inner.gp.is_active() || inner.gp.has_request(),
            "pending RCU callbacks exist without active or requested GP work"
        );
        ready_changed
    }

    fn wake_state_waiters(&self) {
        self.state_wait.wake_all();
    }

    fn wake_worker(&self) {
        self.worker_wait.wake_all();
    }

    fn maybe_process_ready_callbacks_inline(&self) {
        if self.worker_started.load(Ordering::Acquire) {
            return;
        }

        self.process_ready_callbacks();
    }

    fn process_ready_callbacks(&self) {
        {
            let mut inner = self.inner.lock_irqsave();
            if !inner.callbacks.try_claim_drainer() {
                return;
            }
        }

        loop {
            let next = {
                let mut inner = self.inner.lock_irqsave();
                match inner.ready_callbacks.pop_front() {
                    Some(callback) => Some(callback),
                    None => {
                        inner.callbacks.release_drainer();
                        None
                    }
                }
            };

            let Some(callback) = next else {
                break;
            };

            match callback.kind {
                CallbackKind::RawHead { head, func } => {
                    let head = head.0;
                    // SAFETY: `head` is queued only once and the callback owns
                    // the right to recycle or requeue it after execution.
                    unsafe {
                        head.as_ref().queued.store(false, Ordering::Release);
                        func(head);
                    }
                }
                CallbackKind::Deferred(call) => call.invoke(),
            }

            {
                let mut inner = self.inner.lock_irqsave();
                inner.callbacks.complete(callback.seq);
            }

            self.wake_state_waiters();
        }
    }
}

lazy_static! {
    static ref RCU_STATE: RcuState = RcuState::new();
}

#[inline]
fn rcu_enabled() -> bool {
    RCU_STATE.is_initialized()
}

fn online_non_idle_cpus(cpu_states: &[RcuCpuState; PerCpu::MAX_CPU_NUM as usize]) -> CpuMask {
    let mut waiting = CpuMask::new();

    if smp_cpu_manager_initialized() {
        let cpu_manager = smp_cpu_manager();
        for cpu in cpu_manager.present_cpus().iter_cpu() {
            if !cpu_manager.is_online_cpu(cpu) {
                continue;
            }

            if cpu_in_idle_eqs(&cpu_states[cpu.data() as usize]) {
                continue;
            }

            waiting.set(cpu, true);
        }
    } else {
        waiting.set(smp_get_processor_id(), true);
    }

    waiting
}

#[inline]
fn current_task_is_idle() -> bool {
    ProcessManager::current_pcb().sched_info().policy() == SchedPolicy::IDLE
}

#[inline]
fn cpu_in_idle_eqs(cpu_state: &RcuCpuState) -> bool {
    cpu_state.in_idle_eqs && cpu_state.irq_nesting == 0
}

fn enter_cpu_idle_eqs(inner: &mut RcuStateInner, cpu: ProcessorId) -> bool {
    let cpu_idx = cpu.data() as usize;
    debug_assert_eq!(inner.cpu_states[cpu_idx].irq_nesting, 0);

    inner.cpu_states[cpu_idx].in_idle_eqs = true;
    let ready_changed =
        report_quiescent_state_locked(inner, cpu) && RcuState::pump_grace_periods(inner);
    ready_changed || inner.has_ready_work()
}

fn exit_cpu_idle_eqs(inner: &mut RcuStateInner, cpu: ProcessorId) {
    inner.cpu_states[cpu.data() as usize].in_idle_eqs = false;
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
    cleared
}

fn report_quiescent_state(cpu: ProcessorId) {
    if !rcu_enabled() {
        return;
    }

    let (wake_worker, wake_waiters) = {
        let mut inner = RCU_STATE.inner.lock_irqsave();
        report_quiescent_state_locked(&mut inner, cpu);
        let ready_changed = RcuState::pump_grace_periods(&mut inner);
        (ready_changed || inner.has_ready_work(), true)
    };

    if wake_waiters {
        RCU_STATE.wake_state_waiters();
    }
    if wake_worker {
        RCU_STATE.wake_worker();
        RCU_STATE.maybe_process_ready_callbacks_inline();
    }
}

fn reserve_callback_capacity(inner: &mut RcuStateInner) {
    let ready_additional = inner
        .pending_callbacks
        .len()
        .checked_add(1)
        .expect("RCU callback count overflow");
    inner.pending_callbacks.reserve(1);
    inner.ready_callbacks.reserve(ready_additional);
}

fn try_reserve_callback_capacity(inner: &mut RcuStateInner) -> Result<(), ()> {
    let ready_additional = inner.pending_callbacks.len().checked_add(1).ok_or(())?;
    inner.pending_callbacks.try_reserve(1).map_err(|_| ())?;
    inner
        .ready_callbacks
        .try_reserve(ready_additional)
        .map_err(|_| ())?;
    Ok(())
}

fn enqueue_callback_locked(inner: &mut RcuStateInner, kind: CallbackKind) -> bool {
    let target_gp = inner.gp.request_future();
    let seq = inner.callbacks.admit();
    inner.pending_callbacks.push_back(CallbackItem {
        target_gp,
        seq,
        kind,
    });
    let ready_changed = RcuState::pump_grace_periods(inner);
    ready_changed || inner.has_ready_work()
}

fn queue_callback(kind: CallbackKind) {
    let wake_worker = {
        let mut inner = RCU_STATE.inner.lock_irqsave();
        // Admission reserves the destination capacity as well, so grace-period
        // completion can move every pending callback to the ready queue without
        // allocating in IRQ or other non-fallible progress paths.
        reserve_callback_capacity(&mut inner);
        enqueue_callback_locked(&mut inner, kind)
    };

    RCU_STATE.wake_state_waiters();
    if wake_worker {
        RCU_STATE.wake_worker();
        RCU_STATE.maybe_process_ready_callbacks_inline();
    }
}

fn queue_raw_callback(head: NonNull<RcuHead>, func: RcuRawCallback) {
    queue_callback(CallbackKind::RawHead {
        head: QueuedRcuHead(head),
        func,
    });
}

fn queue_deferred_callback(call: Box<dyn DeferredCall>) {
    queue_callback(CallbackKind::Deferred(call));
}

fn try_queue_deferred_callback(call: Box<dyn DeferredCall>) -> Result<(), Box<dyn DeferredCall>> {
    let mut call = Some(call);
    let wake_worker = {
        let mut inner = RCU_STATE.inner.lock_irqsave();
        if try_reserve_callback_capacity(&mut inner).is_err() {
            return Err(call.take().unwrap());
        }
        enqueue_callback_locked(&mut inner, CallbackKind::Deferred(call.take().unwrap()))
    };

    RCU_STATE.wake_state_waiters();
    if wake_worker {
        RCU_STATE.wake_worker();
        RCU_STATE.maybe_process_ready_callbacks_inline();
    }
    Ok(())
}

fn worker_main() -> i32 {
    loop {
        RCU_STATE.worker_wait.wait_until(|| {
            if RCU_STATE.worker_should_stop.load(Ordering::Acquire) {
                return Some(());
            }

            if RCU_STATE.inner.lock_irqsave().has_drainable_work() {
                return Some(());
            }

            None
        });

        if RCU_STATE.worker_should_stop.load(Ordering::Acquire) {
            break;
        }

        RCU_STATE.process_ready_callbacks();
    }

    0
}

pub fn init() {
    let already = RCU_STATE.initialized.swap(true, Ordering::AcqRel);
    if already {
        return;
    }

    {
        let mut inner = RCU_STATE.inner.lock_irqsave();
        let cpu = smp_get_processor_id();
        inner.cpu_states[cpu.data() as usize].in_idle_eqs = false;
    }
}

pub fn start_worker() {
    if !rcu_enabled() {
        return;
    }

    let already = RCU_STATE.worker_started.swap(true, Ordering::AcqRel);
    if already {
        return;
    }

    let closure = KernelThreadClosure::EmptyClosure((Box::new(worker_main), ()));
    if KernelThreadMechanism::create_and_run(closure, "rcu_gp".to_string()).is_none() {
        RCU_STATE.worker_started.store(false, Ordering::Release);
        error!("failed to create RCU callback worker");
        return;
    }

    RCU_STATE.wake_worker();
}

pub fn shutdown_worker() {
    if !rcu_enabled() || !RCU_STATE.worker_started.load(Ordering::Acquire) {
        return;
    }

    RCU_STATE.worker_should_stop.store(true, Ordering::Release);
    RCU_STATE.wake_worker();
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
/// `head` must remain valid until `func` runs and must not be queued again
/// before that invocation begins.
pub(crate) unsafe fn call_rcu_raw(head: NonNull<RcuHead>, func: RcuRawCallback) {
    if !rcu_enabled() {
        // SAFETY: before RCU init there is no concurrent reader relying on
        // grace-period semantics, so direct invocation is safe.
        unsafe { func(head) };
        return;
    }

    // SAFETY: the caller guarantees that `head` is valid until callback
    // completion and not queued twice concurrently.
    let already = unsafe { head.as_ref().queued.swap(true, Ordering::AcqRel) };
    if already {
        panic!("call_rcu_raw received a duplicated rcu_head enqueue");
    }

    queue_raw_callback(head, func);
}

pub fn rcu_defer<F>(f: F)
where
    F: FnOnce() + Send + 'static,
{
    if !rcu_enabled() {
        f();
        return;
    }

    queue_deferred_callback(Box::new(f));
}

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
pub(crate) fn try_rcu_defer_drop_arc<T>(value: Arc<T>) -> Result<(), Arc<T>>
where
    T: Send + Sync + 'static,
{
    if !rcu_enabled() {
        drop(value);
        return Ok(());
    }

    let queued_value = value.clone();
    let call: Box<dyn DeferredCall> = match Box::try_new(move || drop(queued_value)) {
        Ok(call) => call,
        Err(_) => return Err(value),
    };

    if let Err(call) = try_queue_deferred_callback(call) {
        drop(call);
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

    let target_cb = {
        let inner = RCU_STATE.inner.lock_irqsave();
        inner.callbacks.barrier_target()
    };

    let Some(target_cb) = target_cb else {
        return;
    };

    loop {
        if !RCU_STATE.worker_started.load(Ordering::Acquire) {
            RCU_STATE.maybe_process_ready_callbacks_inline();
        }

        let done = {
            let inner = RCU_STATE.inner.lock_irqsave();
            inner.callbacks.has_completed(target_cb)
        };
        if done {
            return;
        }

        RCU_STATE.state_wait.wait_until(|| {
            let completed = RCU_STATE
                .inner
                .lock_irqsave()
                .callbacks
                .has_completed(target_cb);
            if completed {
                Some(())
            } else {
                None
            }
        });
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

pub fn note_exit_to_user_mode() {
    if !rcu_enabled() {
        return;
    }

    report_quiescent_state(smp_get_processor_id());
}

pub fn enter_idle() {
    if !rcu_enabled() {
        return;
    }

    if !current_task_is_idle() {
        warn!("rcu::enter_idle() must only be called from the idle task");
        debug_assert!(current_task_is_idle());
        return;
    }

    let cpu = smp_get_processor_id();
    let wake_worker = {
        let mut inner = RCU_STATE.inner.lock_irqsave();
        enter_cpu_idle_eqs(&mut inner, cpu)
    };

    RCU_STATE.wake_state_waiters();
    if wake_worker {
        RCU_STATE.wake_worker();
        RCU_STATE.maybe_process_ready_callbacks_inline();
    }
}

pub fn exit_idle() {
    if !rcu_enabled() {
        return;
    }

    if !current_task_is_idle() {
        warn!("rcu::exit_idle() must only be called from the idle task");
        debug_assert!(current_task_is_idle());
        return;
    }

    let cpu = smp_get_processor_id();
    let mut inner = RCU_STATE.inner.lock_irqsave();
    exit_cpu_idle_eqs(&mut inner, cpu);
}

pub fn irq_enter() {
    if !rcu_enabled() {
        return;
    }

    let cpu = smp_get_processor_id();
    let mut inner = RCU_STATE.inner.lock_irqsave();
    let cpu_state = &mut inner.cpu_states[cpu.data() as usize];
    if cpu_state.irq_nesting == 0 {
        cpu_state.irq_from_idle_eqs = cpu_in_idle_eqs(cpu_state);
    }
    cpu_state.irq_nesting += 1;
}

/// Returns true when this call exits the outermost IRQ nesting level.
pub fn irq_is_outermost() -> bool {
    if !rcu_enabled() {
        return true;
    }

    let cpu = smp_get_processor_id();
    let inner = RCU_STATE.inner.lock_irqsave();
    inner.cpu_states[cpu.data() as usize].irq_nesting == 1
}

/// Returns true when this call exits the outermost IRQ nesting level.
pub fn irq_exit(resume_idle_eqs: bool) -> bool {
    if !rcu_enabled() {
        return true;
    }

    let cpu = smp_get_processor_id();
    let (outermost, wake_worker) = {
        let mut inner = RCU_STATE.inner.lock_irqsave();
        let cpu_idx = cpu.data() as usize;
        assert!(
            inner.cpu_states[cpu_idx].irq_nesting > 0,
            "rcu::irq_exit without irq_enter"
        );
        inner.cpu_states[cpu_idx].irq_nesting -= 1;
        if inner.cpu_states[cpu_idx].irq_nesting != 0 {
            (false, false)
        } else {
            let resume_idle_eqs = inner.cpu_states[cpu_idx].irq_from_idle_eqs && resume_idle_eqs;
            inner.cpu_states[cpu_idx].irq_from_idle_eqs = false;
            if resume_idle_eqs {
                (true, enter_cpu_idle_eqs(&mut inner, cpu))
            } else {
                inner.cpu_states[cpu_idx].in_idle_eqs = false;
                (true, false)
            }
        }
    };

    RCU_STATE.wake_state_waiters();
    if wake_worker {
        RCU_STATE.wake_worker();
        RCU_STATE.maybe_process_ready_callbacks_inline();
    }

    outermost
}

pub fn cpu_offline(cpu: ProcessorId) {
    if !rcu_enabled() {
        return;
    }

    let wake_worker = {
        let mut inner = RCU_STATE.inner.lock_irqsave();
        report_quiescent_state_locked(&mut inner, cpu);
        let ready_changed = RcuState::pump_grace_periods(&mut inner);
        ready_changed || inner.has_ready_work()
    };

    RCU_STATE.wake_state_waiters();
    if wake_worker {
        RCU_STATE.wake_worker();
        RCU_STATE.maybe_process_ready_callbacks_inline();
    }
}

#[allow(dead_code)]
pub fn debug_snapshot() -> (u64, u64, u64, usize, usize) {
    let inner = RCU_STATE.inner.lock_irqsave();
    (
        inner.gp.current().raw(),
        inner.gp.completed().raw(),
        inner.callbacks.completed_raw(),
        inner.pending_callbacks.len(),
        inner.ready_callbacks.len(),
    )
}

#[allow(dead_code)]
pub fn debug_force_quiescent_state() {
    report_quiescent_state(smp_get_processor_id());
}

#[allow(dead_code)]
pub fn debug_current_cpu_in_idle_eqs() -> bool {
    let cpu = smp_get_processor_id();
    let inner = RCU_STATE.inner.lock_irqsave();
    cpu_in_idle_eqs(&inner.cpu_states[cpu.data() as usize])
}
