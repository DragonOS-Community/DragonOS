#![allow(dead_code)]

use alloc::{boxed::Box, collections::VecDeque, string::ToString, sync::Arc};
use core::{
    ptr::{self, NonNull},
    sync::atomic::{fence, AtomicBool, AtomicPtr, Ordering},
};

use log::{error, warn};

use crate::{
    libs::{cpumask::CpuMask, spinlock::SpinLock, wait_queue::WaitQueue},
    mm::percpu::PerCpu,
    process::{kthread::KernelThreadClosure, kthread::KernelThreadMechanism, ProcessManager},
    sched::SchedPolicy,
    smp::{
        core::smp_get_processor_id,
        cpu::{smp_cpu_manager, smp_cpu_manager_initialized, ProcessorId},
    },
};

mod selftest;
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

    pub fn swap(&self, new: Arc<T>) -> Arc<T> {
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
        let old = self.swap(new);
        rcu_defer_drop(old);
    }

    pub fn swap_deferred(&self, new: Arc<T>) -> Arc<T> {
        let old = self.swap(new);
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
        if raw.is_null() {
            return;
        }

        // SAFETY: dropping the slot consumes the final slot-owned reference to
        // the published Arc. Any reader that needed the object must already
        // have pinned its own strong reference through `load()`.
        unsafe {
            drop(Arc::from_raw(raw));
        }
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
    target_gp: u64,
    seq: u64,
    kind: CallbackKind,
}

#[derive(Clone, Copy, Debug, Default)]
struct RcuCpuState {
    in_idle_eqs: bool,
    irq_nesting: usize,
    irq_from_idle_eqs: bool,
}

struct RcuStateInner {
    gp_seq: u64,
    completed_gp_seq: u64,
    requested_gp_seq: u64,
    next_callback_seq: u64,
    completed_callback_seq: u64,
    gp_active: bool,
    waiting_cpus: CpuMask,
    cpu_states: [RcuCpuState; PerCpu::MAX_CPU_NUM as usize],
    pending_callbacks: VecDeque<CallbackItem>,
    ready_callbacks: VecDeque<CallbackItem>,
}

impl RcuStateInner {
    fn new() -> Self {
        Self {
            gp_seq: 0,
            completed_gp_seq: 0,
            requested_gp_seq: 0,
            next_callback_seq: 1,
            completed_callback_seq: 0,
            gp_active: false,
            waiting_cpus: CpuMask::new(),
            cpu_states: [RcuCpuState::default(); PerCpu::MAX_CPU_NUM as usize],
            pending_callbacks: VecDeque::new(),
            ready_callbacks: VecDeque::new(),
        }
    }

    fn allocate_callback_seq(&mut self) -> u64 {
        let seq = self.next_callback_seq;
        self.next_callback_seq += 1;
        seq
    }

    fn request_future_gp(&mut self) -> u64 {
        let target_gp = self.gp_seq + 1;
        if self.requested_gp_seq < target_gp {
            self.requested_gp_seq = target_gp;
        }
        target_gp
    }

    fn has_ready_work(&self) -> bool {
        !self.ready_callbacks.is_empty()
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
        let mut ready_changed = false;
        loop {
            if inner.gp_active {
                if !inner.waiting_cpus.is_empty() {
                    break;
                }
                inner.completed_gp_seq = inner.gp_seq;
                inner.gp_active = false;

                while inner
                    .pending_callbacks
                    .front()
                    .is_some_and(|cb| cb.target_gp <= inner.completed_gp_seq)
                {
                    if let Some(cb) = inner.pending_callbacks.pop_front() {
                        inner.ready_callbacks.push_back(cb);
                        ready_changed = true;
                    }
                }
                continue;
            }

            let need_gp = inner.requested_gp_seq > inner.completed_gp_seq
                || inner
                    .pending_callbacks
                    .front()
                    .is_some_and(|cb| cb.target_gp > inner.completed_gp_seq);
            if !need_gp {
                break;
            }

            inner.gp_seq += 1;
            inner.gp_active = true;
            inner.waiting_cpus = online_non_idle_cpus(&inner.cpu_states);
        }

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
        loop {
            let next = {
                let mut inner = self.inner.lock_irqsave();
                inner.ready_callbacks.pop_front()
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
                inner.completed_callback_seq = callback.seq;
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
    let ready_changed = if inner.gp_active && inner.waiting_cpus.get(cpu).unwrap_or(false) {
        inner.waiting_cpus.set(cpu, false);
        RcuState::pump_grace_periods(inner)
    } else {
        false
    };
    ready_changed || inner.has_ready_work()
}

fn exit_cpu_idle_eqs(inner: &mut RcuStateInner, cpu: ProcessorId) {
    inner.cpu_states[cpu.data() as usize].in_idle_eqs = false;
}

fn report_quiescent_state(cpu: ProcessorId) {
    if !rcu_enabled() {
        return;
    }

    let (wake_worker, wake_waiters) = {
        let mut inner = RCU_STATE.inner.lock_irqsave();
        if inner.gp_active && inner.waiting_cpus.get(cpu).unwrap_or(false) {
            inner.waiting_cpus.set(cpu, false);
        }
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

fn queue_callback(kind: CallbackKind) {
    let wake_worker = {
        let mut inner = RCU_STATE.inner.lock_irqsave();
        let target_gp = inner.request_future_gp();
        let seq = inner.allocate_callback_seq();
        inner.pending_callbacks.push_back(CallbackItem {
            target_gp,
            seq,
            kind,
        });
        let ready_changed = RcuState::pump_grace_periods(&mut inner);
        ready_changed || inner.has_ready_work()
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

fn worker_main() -> i32 {
    loop {
        RCU_STATE.worker_wait.wait_until(|| {
            if RCU_STATE.worker_should_stop.load(Ordering::Acquire) {
                return Some(());
            }

            if RCU_STATE.inner.lock_irqsave().has_ready_work() {
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

pub fn rcu_read_lock() -> RcuReadGuard {
    if !rcu_enabled() {
        return RcuReadGuard { active: false };
    }

    ProcessManager::preempt_disable();
    ProcessManager::current_pcb().rcu_read_lock();
    RcuReadGuard { active: true }
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
pub fn rcu_dereference<T>(ptr: &AtomicPtr<T>) -> *mut T {
    ptr.load(Ordering::Acquire)
}

#[inline]
pub fn rcu_assign_pointer<T>(ptr: &AtomicPtr<T>, value: *mut T) {
    fence(Ordering::Release);
    ptr.store(value, Ordering::Release);
}

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
        let target_gp = inner.request_future_gp();
        RcuState::pump_grace_periods(&mut inner);
        target_gp
    };

    RCU_STATE.wake_state_waiters();
    RCU_STATE.wake_worker();

    RCU_STATE.state_wait.wait_until(|| {
        let completed = RCU_STATE.inner.lock_irqsave().completed_gp_seq;
        if completed >= target_gp {
            Some(())
        } else {
            None
        }
    });
}

pub fn rcu_barrier() {
    if !rcu_enabled() {
        return;
    }

    let target_cb = {
        let inner = RCU_STATE.inner.lock_irqsave();
        inner.next_callback_seq.saturating_sub(1)
    };

    if target_cb == 0 {
        return;
    }

    loop {
        if !RCU_STATE.worker_started.load(Ordering::Acquire) {
            RCU_STATE.maybe_process_ready_callbacks_inline();
        }

        let done = {
            let inner = RCU_STATE.inner.lock_irqsave();
            inner.completed_callback_seq >= target_cb
        };
        if done {
            return;
        }

        RCU_STATE.state_wait.wait_until(|| {
            let completed = RCU_STATE.inner.lock_irqsave().completed_callback_seq;
            if completed >= target_cb {
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

pub fn irq_exit(resume_idle_eqs: bool) {
    if !rcu_enabled() {
        return;
    }

    let cpu = smp_get_processor_id();
    let wake_worker = {
        let mut inner = RCU_STATE.inner.lock_irqsave();
        let cpu_idx = cpu.data() as usize;
        assert!(
            inner.cpu_states[cpu_idx].irq_nesting > 0,
            "rcu::irq_exit without irq_enter"
        );
        inner.cpu_states[cpu_idx].irq_nesting -= 1;
        if inner.cpu_states[cpu_idx].irq_nesting != 0 {
            false
        } else {
            let resume_idle_eqs = inner.cpu_states[cpu_idx].irq_from_idle_eqs && resume_idle_eqs;
            inner.cpu_states[cpu_idx].irq_from_idle_eqs = false;
            if resume_idle_eqs {
                enter_cpu_idle_eqs(&mut inner, cpu)
            } else {
                inner.cpu_states[cpu_idx].in_idle_eqs = false;
                false
            }
        }
    };

    RCU_STATE.wake_state_waiters();
    if wake_worker {
        RCU_STATE.wake_worker();
        RCU_STATE.maybe_process_ready_callbacks_inline();
    }
}

pub fn cpu_offline(cpu: ProcessorId) {
    if !rcu_enabled() {
        return;
    }

    let wake_worker = {
        let mut inner = RCU_STATE.inner.lock_irqsave();
        if inner.gp_active && inner.waiting_cpus.get(cpu).unwrap_or(false) {
            inner.waiting_cpus.set(cpu, false);
        }
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
        inner.gp_seq,
        inner.completed_gp_seq,
        inner.completed_callback_seq,
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
