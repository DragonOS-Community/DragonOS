//! Sleepable RCU (SRCU).
//!
//! SRCU uses two per-CPU reader-counter slots. Readers may sleep, be
//! preempted, and migrate between lock and unlock. Each domain owns its grace
//! periods and callback FIFO; the only global component is a fair executor.

use alloc::{
    boxed::Box,
    rc::Rc,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    cell::UnsafeCell,
    fmt::Write,
    marker::PhantomData,
    ptr::NonNull,
    sync::atomic::{fence, AtomicBool, AtomicPtr, AtomicU64, AtomicU8, Ordering},
};

use log::warn;
use system_error::SystemError;

use crate::{
    exception::{
        in_interrupt,
        softirq::{softirq_vectors, SoftirqNumber, SoftirqVec},
    },
    libs::{mutex::Mutex, spinlock::SpinLock, wait_queue::WaitQueue},
    mm::percpu::PerCpu,
    process::{
        kthread::{KernelThreadClosure, KernelThreadMechanism},
        ProcessControlBlock, ProcessManager,
    },
    sched::cond_resched,
    smp::core::smp_get_processor_id,
};

const HALF_RANGE: u64 = 1 << 63;
const CALLBACK_QUOTA: usize = 32;
const SLOT_0: u8 = 1;
const SLOT_1: u8 = 2;
const HEAD_IDLE: u8 = 0;
const HEAD_QUEUED: u8 = 1;
const HEAD_INVOKING: u8 = 2;

#[inline]
fn seq_reached(current: u64, target: u64) -> bool {
    current.wrapping_sub(target) < HALF_RANGE
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GpPhase {
    Idle,
    ScanReusable(u8),
    ScanPreexisting(u8),
}

#[derive(Debug)]
struct GpState {
    completed: u64,
    requested: u64,
    phase: GpPhase,
    waiters: usize,
}

impl GpState {
    const fn new() -> Self {
        Self {
            completed: 0,
            requested: 0,
            phase: GpPhase::Idle,
            waiters: 0,
        }
    }

    fn future(&self) -> u64 {
        match self.phase {
            GpPhase::Idle => self.completed.wrapping_add(1),
            _ => self.completed.wrapping_add(2),
        }
    }

    fn request_future(&mut self) -> u64 {
        let target = self.future();
        if !seq_reached(self.requested, target) {
            self.requested = target;
        }
        target
    }

    fn request(&mut self, target: u64) {
        if !seq_reached(self.requested, target) {
            self.requested = target;
        }
    }
}

#[repr(align(64))]
struct SrcuCpuCounters {
    locks: [AtomicU64; 2],
    unlocks: [AtomicU64; 2],
}

impl SrcuCpuCounters {
    const fn new() -> Self {
        Self {
            locks: [AtomicU64::new(0), AtomicU64::new(0)],
            unlocks: [AtomicU64::new(0), AtomicU64::new(0)],
        }
    }
}

type SrcuRawCallback = unsafe fn(NonNull<SrcuHead>);

struct SrcuHeadNode {
    next: Option<NonNull<SrcuHead>>,
    func: Option<SrcuRawCallback>,
    target: u64,
    domain: Option<Arc<SrcuInner>>,
}

/// Intrusive storage for one SRCU callback.
pub struct SrcuHead {
    state: AtomicU8,
    node: UnsafeCell<SrcuHeadNode>,
}

impl SrcuHead {
    pub const fn new() -> Self {
        Self {
            state: AtomicU8::new(HEAD_IDLE),
            node: UnsafeCell::new(SrcuHeadNode {
                next: None,
                func: None,
                target: 0,
                domain: None,
            }),
        }
    }

    pub fn is_queued(&self) -> bool {
        self.state.load(Ordering::Acquire) != HEAD_IDLE
    }

    /// Marks the exact callback-entry ownership-transfer point.
    ///
    /// # Safety
    /// An intrusive callback must call this as its first operation, before
    /// recovering or freeing its containing allocation.
    pub unsafe fn begin_callback(&self) {
        assert_eq!(
            self.state.compare_exchange(
                HEAD_INVOKING,
                HEAD_IDLE,
                Ordering::AcqRel,
                Ordering::Acquire,
            ),
            Ok(HEAD_INVOKING),
            "invalid SRCU callback entry state"
        );
    }
}

impl Default for SrcuHead {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: node access is serialized by its domain callback lock. Admission
// requires the containing allocation to remain stable until callback start.
unsafe impl Send for SrcuHead {}
unsafe impl Sync for SrcuHead {}

struct CallbackQueue {
    head: Option<NonNull<SrcuHead>>,
    tail: Option<NonNull<SrcuHead>>,
    len: usize,
    submitted: u64,
    completed: u64,
}

impl CallbackQueue {
    const fn new() -> Self {
        Self {
            head: None,
            tail: None,
            len: 0,
            submitted: 0,
            completed: 0,
        }
    }

    fn push(
        &mut self,
        head: NonNull<SrcuHead>,
        func: SrcuRawCallback,
        target: u64,
        domain: Arc<SrcuInner>,
    ) {
        // SAFETY: the head was exclusively claimed and the callback lock is held.
        let node = unsafe { &mut *head.as_ref().node.get() };
        debug_assert!(node.next.is_none() && node.func.is_none());
        node.func = Some(func);
        node.target = target;
        node.domain = Some(domain);
        if let Some(tail) = self.tail {
            // SAFETY: every linked node is stable while queued.
            unsafe { (*tail.as_ref().node.get()).next = Some(head) };
        } else {
            self.head = Some(head);
        }
        self.tail = Some(head);
        self.len = self
            .len
            .checked_add(1)
            .expect("SRCU callback count overflow");
        self.submitted = self.submitted.wrapping_add(1);
    }

    fn front_target(&self) -> Option<u64> {
        let head = self.head?;
        // SAFETY: inspection is serialized by the callback lock.
        Some(unsafe { (*head.as_ref().node.get()).target })
    }

    fn pop_if_target(
        &mut self,
        expected: u64,
    ) -> Option<(NonNull<SrcuHead>, SrcuRawCallback, Arc<SrcuInner>)> {
        let head = self.head?;
        // SAFETY: mutation is serialized by the callback lock.
        let node = unsafe { &mut *head.as_ref().node.get() };
        if node.target != expected {
            return None;
        }
        self.head = node.next.take();
        if self.head.is_none() {
            self.tail = None;
        }
        self.len -= 1;
        let func = node.func.take().expect("SRCU callback missing function");
        let domain = node
            .domain
            .take()
            .expect("SRCU callback missing domain pin");
        // SAFETY: the linked head remains valid until callback start.
        unsafe { head.as_ref() }
            .state
            .store(HEAD_INVOKING, Ordering::Release);
        Some((head, func, domain))
    }
}

// SAFETY: raw links are only dereferenced under the callback lock.
unsafe impl Send for CallbackQueue {}

struct SrcuInner {
    id: u64,
    name: &'static str,
    active: AtomicBool,
    current_idx: AtomicU8,
    counters: [SrcuCpuCounters; PerCpu::MAX_CPU_NUM as usize],
    gp: SpinLock<GpState>,
    gp_wait: WaitQueue,
    waiting_slots: AtomicU8,
    callbacks: SpinLock<CallbackQueue>,
    callback_executing: AtomicBool,
    callback_wait: WaitQueue,
    barrier_mutex: Mutex<()>,
}

struct SrcuCallbackTaskGuard(Arc<ProcessControlBlock>);

impl SrcuCallbackTaskGuard {
    fn enter(domain: u64) -> Self {
        let pcb = ProcessManager::current_pcb();
        let previous = pcb.srcu_callback_domain.swap(domain, Ordering::AcqRel);
        assert_eq!(previous, 0, "nested SRCU callback executor state");
        Self(pcb)
    }
}

impl Drop for SrcuCallbackTaskGuard {
    fn drop(&mut self) {
        self.0.srcu_callback_domain.store(0, Ordering::Release);
    }
}

fn in_srcu_callback_context() -> bool {
    ProcessManager::initialized()
        && ProcessManager::current_pcb()
            .srcu_callback_domain
            .load(Ordering::Acquire)
            != 0
}

impl SrcuInner {
    fn new(id: u64, name: &'static str) -> Self {
        Self {
            id,
            name,
            active: AtomicBool::new(true),
            current_idx: AtomicU8::new(0),
            counters: [const { SrcuCpuCounters::new() }; PerCpu::MAX_CPU_NUM as usize],
            gp: SpinLock::new(GpState::new()),
            gp_wait: WaitQueue::default(),
            waiting_slots: AtomicU8::new(0),
            callbacks: SpinLock::new(CallbackQueue::new()),
            callback_executing: AtomicBool::new(false),
            callback_wait: WaitQueue::default(),
            barrier_mutex: Mutex::new(()),
        }
    }

    fn slot_balanced(&self, idx: u8) -> bool {
        let idx = idx as usize;
        let mut unlocks = 0_u64;
        for cpu in &self.counters {
            unlocks = unlocks.wrapping_add(cpu.unlocks[idx].load(Ordering::Acquire));
        }
        fence(Ordering::SeqCst); // Linux SRCU barrier A.
        let mut locks = 0_u64;
        for cpu in &self.counters {
            locks = locks.wrapping_add(cpu.locks[idx].load(Ordering::Acquire));
        }
        locks == unlocks
    }

    fn arm_and_scan(&self, idx: u8) -> bool {
        let bit = if idx == 0 { SLOT_0 } else { SLOT_1 };
        self.waiting_slots.fetch_or(bit, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        let balanced = self.slot_balanced(idx);
        if balanced {
            self.waiting_slots.fetch_and(!bit, Ordering::SeqCst);
        }
        balanced
    }

    /// Makes all immediately possible GP transitions. Never sleeps.
    fn pump_gp(&self) -> bool {
        let mut completed_any = false;
        loop {
            let phase = self.gp.lock_irqsave().phase;
            match phase {
                GpPhase::Idle => {
                    let mut gp = self.gp.lock_irqsave();
                    if seq_reached(gp.completed, gp.requested) {
                        break;
                    }
                    let reusable = (self.current_idx.load(Ordering::Acquire) ^ 1) & 1;
                    gp.phase = GpPhase::ScanReusable(reusable);
                }
                GpPhase::ScanReusable(idx) => {
                    if !self.arm_and_scan(idx) {
                        break;
                    }
                    let mut gp = self.gp.lock_irqsave();
                    if gp.phase != phase {
                        continue;
                    }
                    fence(Ordering::SeqCst); // Linux SRCU barrier E.
                    let old = self.current_idx.load(Ordering::Relaxed) & 1;
                    self.current_idx.store(old ^ 1, Ordering::SeqCst);
                    fence(Ordering::SeqCst); // Linux SRCU barrier D.
                    gp.phase = GpPhase::ScanPreexisting(old);
                }
                GpPhase::ScanPreexisting(idx) => {
                    if !self.arm_and_scan(idx) {
                        break;
                    }
                    let mut gp = self.gp.lock_irqsave();
                    if gp.phase != phase {
                        continue;
                    }
                    fence(Ordering::SeqCst);
                    gp.completed = gp.completed.wrapping_add(1);
                    gp.phase = GpPhase::Idle;
                    completed_any = true;
                }
            }
        }
        if completed_any {
            self.gp_wait.wake_all();
        }
        completed_any
    }

    fn has_completed(&self, target: u64) -> bool {
        seq_reached(self.gp.lock_irqsave().completed, target)
    }

    fn has_pending_work(&self) -> bool {
        let pending_gp = {
            let gp = self.gp.lock_irqsave();
            !seq_reached(gp.completed, gp.requested)
        };
        pending_gp || self.callbacks.lock_irqsave().len != 0
    }

    fn process_callbacks(&self) -> bool {
        self.pump_gp();
        let mut did_work = false;
        for _ in 0..CALLBACK_QUOTA {
            let target = match self.callbacks.lock_irqsave().front_target() {
                Some(target) => target,
                None => break,
            };
            if !self.has_completed(target) {
                break;
            }
            let ready = self.callbacks.lock_irqsave().pop_if_target(target);
            let Some((head, func, _domain_pin)) = ready else {
                continue;
            };
            self.callback_executing.store(true, Ordering::Release);
            let _callback_task = SrcuCallbackTaskGuard::enter(self.id);
            fence(Ordering::SeqCst);
            // SAFETY: call_srcu_raw's admission contract keeps head stable.
            unsafe { func(head) };
            {
                let mut callbacks = self.callbacks.lock_irqsave();
                callbacks.completed = callbacks.completed.wrapping_add(1);
            }
            self.callback_executing.store(false, Ordering::Release);
            self.callback_wait.wake_all();
            did_work = true;
        }
        if self
            .callbacks
            .lock_irqsave()
            .front_target()
            .is_some_and(|target| self.has_completed(target))
        {
            SRCU_RUNTIME.kick();
        }
        did_work
    }
}

/// One independent SRCU protection domain.
pub struct SrcuDomain {
    inner: Arc<SrcuInner>,
}

impl core::fmt::Debug for SrcuDomain {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SrcuDomain")
            .field("id", &self.inner.id)
            .field("name", &self.inner.name)
            .finish_non_exhaustive()
    }
}

impl SrcuDomain {
    pub fn try_new(name: &'static str) -> Result<Self, SystemError> {
        if !SRCU_RUNTIME.initialized.load(Ordering::Acquire) {
            return Err(SystemError::ENODEV);
        }
        let id = SRCU_RUNTIME.next_id.fetch_add(1, Ordering::Relaxed);
        let inner = Arc::try_new(SrcuInner::new(id, name)).map_err(|_| SystemError::ENOMEM)?;
        SRCU_RUNTIME.register(&inner)?;
        Ok(Self { inner })
    }

    #[inline]
    pub fn read_lock(&self) -> SrcuReadCookie<'_> {
        self.read_lock_impl(true)
    }

    #[inline]
    pub(crate) fn read_lock_notrace(&self) -> SrcuReadCookie<'_> {
        self.read_lock_impl(false)
    }

    #[inline]
    fn read_lock_impl(&self, track: bool) -> SrcuReadCookie<'_> {
        debug_assert!(self.inner.active.load(Ordering::Acquire));
        let idx = self.inner.current_idx.load(Ordering::Acquire) & 1;
        let cpu = smp_get_processor_id().data() as usize;
        self.inner.counters[cpu].locks[idx as usize].fetch_add(1, Ordering::Relaxed);
        fence(Ordering::SeqCst); // Linux SRCU barrier B.
        let tracked = if track && ProcessManager::initialized() {
            Some(
                ProcessManager::current_pcb()
                    .srcu_task_state
                    .lock_irqsave()
                    .enter(self.inner.id),
            )
        } else {
            None
        };
        SrcuReadCookie {
            domain: self,
            idx,
            active: true,
            tracked,
            _not_send: PhantomData,
        }
    }

    pub fn synchronize(&self) -> Result<(), SystemError> {
        if in_interrupt() {
            return Err(SystemError::EDEADLK_OR_EDEADLOCK);
        }
        if self.read_held_by_current() || in_srcu_callback_context() {
            return Err(SystemError::EDEADLK_OR_EDEADLOCK);
        }
        fence(Ordering::SeqCst);
        let target = self.inner.gp.lock_irqsave().request_future();
        SRCU_RUNTIME.kick();
        if !SRCU_RUNTIME.worker_ready.load(Ordering::Acquire) {
            self.inner.pump_gp();
            if self.inner.has_completed(target) {
                fence(Ordering::SeqCst);
                return Ok(());
            }
            return Err(SystemError::EBUSY);
        }
        self.inner.gp.lock_irqsave().waiters += 1;
        self.inner.gp_wait.wait_until(|| {
            if self.inner.has_completed(target) {
                Some(())
            } else {
                None
            }
        });
        self.inner.gp.lock_irqsave().waiters -= 1;
        fence(Ordering::SeqCst);
        Ok(())
    }

    pub(crate) fn validate_update_context(&self) -> Result<(), SystemError> {
        self.validate_deferred_update_context()?;
        if self.read_held_by_current() || in_srcu_callback_context() {
            return Err(SystemError::EDEADLK_OR_EDEADLOCK);
        }
        Ok(())
    }

    pub(crate) fn validate_deferred_update_context(&self) -> Result<(), SystemError> {
        if in_interrupt() || !SRCU_RUNTIME.worker_ready.load(Ordering::Acquire) {
            return Err(SystemError::EDEADLK_OR_EDEADLOCK);
        }
        Ok(())
    }

    pub(crate) fn synchronize_after_publication(&self) {
        self.validate_update_context()
            .expect("SRCU update context changed after publication");
        self.synchronize()
            .expect("prevalidated SRCU synchronize failed after publication");
    }

    pub(crate) fn read_held_by_current(&self) -> bool {
        ProcessManager::initialized()
            && ProcessManager::current_pcb()
                .srcu_task_state
                .lock_irqsave()
                .may_wait(self.inner.id)
    }

    pub fn get_state_synchronize(&self) -> SrcuPollCookie {
        fence(Ordering::SeqCst);
        let target = self.inner.gp.lock_irqsave().future();
        fence(Ordering::SeqCst);
        SrcuPollCookie(target)
    }

    pub fn start_poll_synchronize(&self) -> SrcuPollCookie {
        fence(Ordering::SeqCst);
        let target = self.inner.gp.lock_irqsave().request_future();
        fence(Ordering::SeqCst);
        SRCU_RUNTIME.kick();
        SrcuPollCookie(target)
    }

    pub fn poll_state_synchronize(&self, cookie: SrcuPollCookie) -> bool {
        if !self.inner.has_completed(cookie.0) {
            return false;
        }
        fence(Ordering::SeqCst);
        true
    }

    /// Queues one intrusive callback after a future grace period.
    ///
    /// # Safety
    /// `head` must remain at a stable address until `func` starts and must not
    /// already be queued in any SRCU domain. `func` must call
    /// [`SrcuHead::begin_callback`] as its first operation before requeueing or
    /// freeing the containing allocation.
    pub unsafe fn call_raw(
        &self,
        head: NonNull<SrcuHead>,
        func: SrcuRawCallback,
    ) -> Result<(), SystemError> {
        if head
            .as_ref()
            .state
            .compare_exchange(HEAD_IDLE, HEAD_QUEUED, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(SystemError::EBUSY);
        }
        fence(Ordering::SeqCst);
        {
            // Required lock order: callbacks -> gp.
            let mut callbacks = self.inner.callbacks.lock_irqsave();
            let mut gp = self.inner.gp.lock_irqsave();
            let target = gp.request_future();
            callbacks.push(head, func, target, self.inner.clone());
        }
        SRCU_RUNTIME.kick();
        Ok(())
    }

    pub fn barrier(&self) -> Result<(), SystemError> {
        if in_interrupt() {
            return Err(SystemError::EDEADLK_OR_EDEADLOCK);
        }
        if self.read_held_by_current() || in_srcu_callback_context() {
            return Err(SystemError::EDEADLK_OR_EDEADLOCK);
        }
        if !SRCU_RUNTIME.worker_ready.load(Ordering::Acquire) {
            let callbacks = self.inner.callbacks.lock_irqsave();
            if callbacks.submitted == callbacks.completed
                && !self.inner.callback_executing.load(Ordering::Acquire)
            {
                return Ok(());
            }
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }
        let _barrier = self.inner.barrier_mutex.lock();
        let target = self.inner.callbacks.lock_irqsave().submitted;
        self.inner.callback_wait.wait_until(|| {
            let completed = self.inner.callbacks.lock_irqsave().completed;
            seq_reached(completed, target).then_some(())
        });
        fence(Ordering::SeqCst);
        Ok(())
    }

    pub(crate) fn barrier_after_publication(&self) {
        self.validate_update_context()
            .expect("SRCU update context changed after publication");
        self.barrier()
            .expect("prevalidated SRCU barrier failed after publication");
    }

    pub fn try_cleanup(self) -> Result<(), (Self, SystemError)> {
        if in_interrupt() || in_srcu_callback_context() {
            return Err((self, SystemError::EDEADLK_OR_EDEADLOCK));
        }
        for idx in 0..2 {
            if !self.inner.slot_balanced(idx) {
                return Err((self, SystemError::EBUSY));
            }
        }
        let gp_busy = {
            let gp = self.inner.gp.lock_irqsave();
            gp.phase != GpPhase::Idle || !seq_reached(gp.completed, gp.requested) || gp.waiters != 0
        };
        if gp_busy {
            return Err((self, SystemError::EBUSY));
        }
        if self.inner.callbacks.lock_irqsave().len != 0
            || self.inner.callback_executing.load(Ordering::Acquire)
        {
            return Err((self, SystemError::EBUSY));
        }
        SRCU_RUNTIME.unregister_and_wait(&self.inner);
        self.inner.active.store(false, Ordering::Release);
        fence(Ordering::SeqCst);
        Ok(())
    }
}

impl Drop for SrcuDomain {
    fn drop(&mut self) {
        if self.inner.active.load(Ordering::Acquire) {
            warn!(
                "SRCU domain '{}' dropped without successful cleanup",
                self.inner.name
            );
            // Ensure an abandoned domain is either drained (callback pins may
            // still keep it alive) or its dead registry entry is reclaimed.
            SRCU_RUNTIME.kick();
        }
    }
}

/// A task-bound reader cookie. Dropping it performs the matching unlock.
#[must_use]
pub struct SrcuReadCookie<'a> {
    domain: &'a SrcuDomain,
    idx: u8,
    active: bool,
    tracked: Option<bool>,
    _not_send: PhantomData<Rc<()>>,
}

impl SrcuReadCookie<'_> {
    pub fn unlock(mut self) {
        self.release();
    }

    fn release(&mut self) {
        if !self.active {
            return;
        }
        fence(Ordering::SeqCst); // Linux SRCU barrier C.
        let cpu = smp_get_processor_id().data() as usize;
        self.domain.inner.counters[cpu].unlocks[self.idx as usize].fetch_add(1, Ordering::Release);

        // Store->load full barrier paired with worker arm->scan.
        fence(Ordering::SeqCst);
        let bit = if self.idx == 0 { SLOT_0 } else { SLOT_1 };
        if self.domain.inner.waiting_slots.load(Ordering::SeqCst) & bit != 0
            && self
                .domain
                .inner
                .waiting_slots
                .fetch_and(!bit, Ordering::SeqCst)
                & bit
                != 0
        {
            SRCU_RUNTIME.kick();
        }
        if let Some(tracked) = self.tracked.take() {
            ProcessManager::current_pcb()
                .srcu_task_state
                .lock_irqsave()
                .leave(self.domain.inner.id, tracked);
        }
        self.active = false;
    }
}

impl Drop for SrcuReadCookie<'_> {
    fn drop(&mut self) {
        self.release();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SrcuPollCookie(u64);

struct SrcuRuntime {
    initialized: AtomicBool,
    worker_ready: AtomicBool,
    pending: AtomicBool,
    next_id: AtomicU64,
    domains: SpinLock<Vec<Weak<SrcuInner>>>,
    worker_wait: WaitQueue,
    epoch: AtomicU64,
    epoch_wait: WaitQueue,
}

impl SrcuRuntime {
    fn new() -> Self {
        Self {
            initialized: AtomicBool::new(false),
            worker_ready: AtomicBool::new(false),
            pending: AtomicBool::new(false),
            next_id: AtomicU64::new(1),
            domains: SpinLock::new(Vec::new()),
            worker_wait: WaitQueue::default(),
            epoch: AtomicU64::new(0),
            epoch_wait: WaitQueue::default(),
        }
    }

    fn register(&self, domain: &Arc<SrcuInner>) -> Result<(), SystemError> {
        let mut domains = self.domains.lock_irqsave();
        if let Some(slot) = domains
            .iter_mut()
            .find(|registered| registered.strong_count() == 0)
        {
            *slot = Arc::downgrade(domain);
            return Ok(());
        }
        domains.try_reserve(1).map_err(|_| SystemError::ENOMEM)?;
        domains.push(Arc::downgrade(domain));
        Ok(())
    }

    fn unregister_and_wait(&self, domain: &Arc<SrcuInner>) {
        {
            let mut domains = self.domains.lock_irqsave();
            if let Some(registered) = domains
                .iter()
                .position(|registered| registered.as_ptr() == Arc::as_ptr(domain))
                .and_then(|index| domains.get_mut(index))
            {
                *registered = Weak::new();
            }
        }
        if !self.worker_ready.load(Ordering::Acquire) {
            return;
        }
        let target = self.epoch.load(Ordering::Acquire).wrapping_add(1);
        self.kick();
        self.epoch_wait
            .wait_until(|| seq_reached(self.epoch.load(Ordering::Acquire), target).then_some(()));
    }

    fn kick(&self) {
        if self.pending.swap(true, Ordering::AcqRel) {
            return;
        }
        if self.worker_ready.load(Ordering::Acquire) {
            if in_interrupt() {
                softirq_vectors().raise_softirq(SoftirqNumber::SRCU);
            } else {
                self.worker_wait.wake_all();
            }
        }
    }

    fn domain_at(&self, index: usize) -> Option<Arc<SrcuInner>> {
        self.domains
            .lock_irqsave()
            .get(index)
            .and_then(Weak::upgrade)
    }

    fn process_round(&self) {
        let count = self.domains.lock_irqsave().len();
        for index in 0..count {
            if let Some(domain) = self.domain_at(index) {
                domain.process_callbacks();
            }
            cond_resched();
        }
        self.epoch.fetch_add(1, Ordering::Release);
        self.epoch_wait.wake_all();
    }
}

lazy_static! {
    static ref SRCU_RUNTIME: SrcuRuntime = SrcuRuntime::new();
}

#[derive(Debug)]
struct SrcuSoftirq;

impl SoftirqVec for SrcuSoftirq {
    fn run(&self) {
        if SRCU_RUNTIME.pending.load(Ordering::Acquire) {
            SRCU_RUNTIME.worker_wait.wake_all();
        }
    }
}

fn worker_main() -> i32 {
    loop {
        SRCU_RUNTIME.worker_wait.wait_until(|| {
            SRCU_RUNTIME
                .pending
                .swap(false, Ordering::AcqRel)
                .then_some(())
        });
        SRCU_RUNTIME.process_round();
    }
}

pub(super) fn init() {
    let _ = &*SRCU_RUNTIME;
    SRCU_RUNTIME.initialized.store(true, Ordering::Release);
}

pub(super) fn start_worker() {
    if !SRCU_RUNTIME.initialized.load(Ordering::Acquire)
        || SRCU_RUNTIME.worker_ready.load(Ordering::Acquire)
    {
        return;
    }
    softirq_vectors()
        .register_softirq(SoftirqNumber::SRCU, Arc::new(SrcuSoftirq))
        .expect("failed to register SRCU softirq");
    let closure = KernelThreadClosure::EmptyClosure((Box::new(worker_main), ()));
    if KernelThreadMechanism::create_and_run(closure, "srcu_gp".to_string()).is_none() {
        panic!("failed to create SRCU worker");
    }
    SRCU_RUNTIME.worker_ready.store(true, Ordering::Release);
    SRCU_RUNTIME.pending.store(true, Ordering::Release);
    SRCU_RUNTIME.worker_wait.wake_all();
}

/// A sized, single-owner `Arc` pointer slot protected by one SRCU domain.
pub struct SrcuArcSlot<T: Send + Sync + 'static> {
    ptr: AtomicPtr<T>,
}

impl<T: Send + Sync + 'static> SrcuArcSlot<T> {
    pub fn new(initial: Arc<T>) -> Self {
        Self {
            ptr: AtomicPtr::new(Arc::into_raw(initial) as *mut T),
        }
    }

    pub fn with_read<R>(&self, domain: &SrcuDomain, f: impl FnOnce(&T) -> R) -> R {
        let _guard = domain.read_lock();
        let ptr = self.ptr.load(Ordering::Acquire);
        assert!(!ptr.is_null(), "SRCU slot is empty");
        // SAFETY: the slot owns an Arc and the guard delays reclamation.
        f(unsafe { &*ptr })
    }

    /// Replaces the slot-owned Arc. The caller must synchronize the domain
    /// before dropping the returned Arc.
    pub unsafe fn swap(&self, new: Arc<T>) -> Arc<T> {
        let old = self
            .ptr
            .swap(Arc::into_raw(new) as *mut T, Ordering::AcqRel);
        assert!(!old.is_null(), "SRCU slot is empty");
        Arc::from_raw(old)
    }
}

impl<T: Send + Sync + 'static> Drop for SrcuArcSlot<T> {
    fn drop(&mut self) {
        let ptr = self.ptr.swap(core::ptr::null_mut(), Ordering::AcqRel);
        if !ptr.is_null() {
            // SAFETY: Drop has exclusive access to the slot. Safe readers borrow
            // the containing owner, so none can outlive this exclusive drop.
            unsafe { drop(Arc::from_raw(ptr)) };
        }
    }
}

/// Runs deterministic state-machine checks without scheduling dependencies.
pub fn run_state_machine_selftests() -> Result<(), &'static str> {
    let mut gp = GpState::new();
    let first = gp.request_future();
    if first != 1 || gp.request_future() != first {
        return Err("idle SRCU requests did not coalesce");
    }
    gp.phase = GpPhase::ScanReusable(1);
    let second = gp.request_future();
    if second != 2 || gp.request_future() != second {
        return Err("active SRCU requests did not target the following GP");
    }
    if !seq_reached(0, u64::MAX) || seq_reached(u64::MAX, 0) {
        return Err("SRCU wrapping comparison is incorrect");
    }
    Ok(())
}

#[repr(C)]
struct SelftestCallback {
    head: SrcuHead,
    calls: Arc<AtomicU64>,
}

unsafe fn selftest_callback(head: NonNull<SrcuHead>) {
    // SAFETY: this is the callback's first operation and establishes ownership.
    unsafe { head.as_ref().begin_callback() };
    let callback = head.as_ptr().cast::<SelftestCallback>();
    // SAFETY: head is the first repr(C) field and admission owns this Box.
    let callback = unsafe { Box::from_raw(callback) };
    callback.calls.fetch_add(1, Ordering::Release);
}

pub(super) fn run_runtime_selftests() -> Result<(), &'static str> {
    if !SRCU_RUNTIME.worker_ready.load(Ordering::Acquire) {
        return Err("SRCU worker is not ready");
    }
    let first =
        SrcuDomain::try_new("srcu_selftest_a").map_err(|_| "failed to create first SRCU domain")?;
    let second = SrcuDomain::try_new("srcu_selftest_b")
        .map_err(|_| "failed to create second SRCU domain")?;

    {
        let current = ProcessManager::current_pcb();
        let mut task_state = current.srcu_task_state.lock_irqsave();
        let first_test_domain = u64::MAX - 1024;
        let mut overflow_domain = first_test_domain;
        while task_state.enter(overflow_domain) {
            overflow_domain = overflow_domain
                .checked_add(1)
                .ok_or("SRCU task tracking did not overflow")?;
        }
        let overflow_is_conservative = task_state.may_wait(overflow_domain);
        task_state.leave(overflow_domain, false);
        while overflow_domain != first_test_domain {
            overflow_domain -= 1;
            task_state.leave(overflow_domain, true);
        }
        if !overflow_is_conservative {
            return Err("SRCU task tracking overflow allowed a synchronous wait");
        }
    }

    let outer = first.read_lock();
    let inner = first.read_lock();
    if first.synchronize() != Err(SystemError::EDEADLK_OR_EDEADLOCK) {
        return Err("same-domain SRCU synchronize was not rejected");
    }
    second
        .synchronize()
        .map_err(|_| "independent SRCU domain was blocked")?;
    inner.unlock();
    outer.unlock();
    first
        .synchronize()
        .map_err(|_| "SRCU synchronize failed after nested readers")?;

    let cleanup_target = SrcuDomain::try_new("srcu_selftest_cleanup")
        .map_err(|_| "failed to create SRCU cleanup test domain")?;
    let cleanup_target = {
        let _callback_task = SrcuCallbackTaskGuard::enter(first.inner.id);
        if second.synchronize() != Err(SystemError::EDEADLK_OR_EDEADLOCK)
            || second.barrier() != Err(SystemError::EDEADLK_OR_EDEADLOCK)
            || second.validate_update_context() != Err(SystemError::EDEADLK_OR_EDEADLOCK)
        {
            return Err("cross-domain wait from an SRCU callback was not rejected");
        }
        match cleanup_target.try_cleanup() {
            Err((domain, SystemError::EDEADLK_OR_EDEADLOCK)) => domain,
            _ => return Err("cross-domain cleanup from an SRCU callback was not rejected"),
        }
    };

    let poll = first.start_poll_synchronize();
    while !first.poll_state_synchronize(poll) {
        cond_resched();
    }

    let calls = Arc::new(AtomicU64::new(0));
    let callback = Box::try_new(SelftestCallback {
        head: SrcuHead::new(),
        calls: calls.clone(),
    })
    .map_err(|_| "failed to allocate SRCU selftest callback")?;
    let callback = Box::into_raw(callback);
    // SAFETY: the callback Box remains stable and is consumed by its callback.
    unsafe {
        first
            .call_raw(NonNull::from(&(*callback).head), selftest_callback)
            .map_err(|_| "failed to queue SRCU selftest callback")?;
    }
    first.barrier().map_err(|_| "SRCU barrier failed")?;
    if calls.load(Ordering::Acquire) != 1 {
        return Err("SRCU callback was not invoked exactly once");
    }

    first
        .try_cleanup()
        .map_err(|_| "first SRCU domain cleanup failed")?;
    second
        .try_cleanup()
        .map_err(|_| "second SRCU domain cleanup failed")?;
    cleanup_target
        .try_cleanup()
        .map_err(|_| "SRCU cleanup test domain cleanup failed")?;
    Ok(())
}

pub fn state_debug_report() -> String {
    let mut report = String::new();
    let count = SRCU_RUNTIME.domains.lock_irqsave().len();
    for index in 0..count {
        let Some(domain) = SRCU_RUNTIME.domain_at(index) else {
            continue;
        };
        let (completed, requested, phase) = {
            let gp = domain.gp.lock_irqsave();
            (gp.completed, gp.requested, gp.phase)
        };
        let (callback_len, callback_submitted, callback_completed) = {
            let callbacks = domain.callbacks.lock_irqsave();
            (callbacks.len, callbacks.submitted, callbacks.completed)
        };
        let mut slots = [(0_u64, 0_u64); 2];
        for cpu in &domain.counters {
            for (idx, slot) in slots.iter_mut().enumerate() {
                slot.0 = slot.0.wrapping_add(cpu.locks[idx].load(Ordering::Acquire));
                slot.1 = slot
                    .1
                    .wrapping_add(cpu.unlocks[idx].load(Ordering::Acquire));
            }
        }
        let _ = writeln!(
            report,
            "id={} name={} active={} idx={} completed={} requested={} phase={:?} slot0={}/{} slot1={}/{} callbacks={} callback_submitted={} callback_completed={} executing={}",
            domain.id,
            domain.name,
            domain.active.load(Ordering::Acquire),
            domain.current_idx.load(Ordering::Acquire) & 1,
            completed,
            requested,
            phase,
            slots[0].0,
            slots[0].1,
            slots[1].0,
            slots[1].1,
            callback_len,
            callback_submitted,
            callback_completed,
            domain.callback_executing.load(Ordering::Acquire),
        );
    }
    if report.is_empty() {
        report.push_str("no-domains\n");
    }
    report
}
