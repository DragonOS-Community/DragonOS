//! CPU deadline domains shared by sleeps and POSIX CPU timers.
//!
//! Producers only publish counters and schedule one preborn tasklet. The lock
//! order is domain queues -> registration state. Callbacks and final drops are
//! outside both guards; registrations do not retain their domain.
mod queue;

use alloc::sync::Arc;
use core::sync::atomic::{fence, AtomicU64, Ordering};
use system_error::SystemError;

use crate::{
    cgroup::memory::{
        accounting::KernelByteKind, object::ObjectAccount, owned::OwnedBox,
        runtime::ChargeRecoveryState,
    },
    exception::tasklet::{tasklet_schedule, Tasklet},
    libs::{
        spinlock::SpinLock,
        wait_queue::{Waiter, Waker},
    },
    mm::allocation::MemoryAllocationError,
    process::ProcessManager,
    time::cpu_clock::CpuClockKind,
};
use queue::{CpuDeadlineQueue, Node};

type Key = (u64, usize);
const MAX_DUE_EVENTS: usize = 32;

fn index(kind: CpuClockKind) -> usize {
    match kind {
        CpuClockKind::Prof => 0,
        CpuClockKind::Virt => 1,
        CpuClockKind::Sched => 2,
    }
}

pub(crate) trait CpuEventCallback: Send + Sync {
    /// The captured arm generation, not the registration's possibly newer one.
    fn expired(&self, generation: u64);
}

struct RegistrationState {
    domain: Option<usize>,
    queued: Option<(usize, Key)>,
    spare: Option<OwnedBox<Node>>,
    generation: u64,
}

struct RegistrationData {
    state: SpinLock<RegistrationState>,
    callback: Arc<dyn CpuEventCallback>,
}

pub(crate) struct CpuEventRegistration {
    body: OwnedBox<RegistrationData>,
}

struct Queues {
    trees: [CpuDeadlineQueue; 3],
    retired: bool,
    next_kind: usize,
}

struct DomainData {
    counters: [AtomicU64; 3],
    earliest: [AtomicU64; 3],
    queues: SpinLock<Queues>,
    tasklet: SpinLock<Option<Arc<Tasklet>>>,
}

pub(crate) struct CpuEventDomain {
    body: OwnedBox<DomainData>,
}

impl core::fmt::Debug for CpuEventDomain {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CpuEventDomain").finish_non_exhaustive()
    }
}

impl CpuEventDomain {
    pub(crate) fn try_new_for(
        creator: &Arc<ObjectAccount>,
    ) -> Result<Arc<Self>, MemoryAllocationError> {
        let body = OwnedBox::prepare(creator, KernelByteKind::SlabUnreclaimable)?
            .try_init(DomainData {
                counters: core::array::from_fn(|_| AtomicU64::new(0)),
                earliest: core::array::from_fn(|_| AtomicU64::new(u64::MAX)),
                queues: SpinLock::new(Queues {
                    trees: core::array::from_fn(|_| CpuDeadlineQueue::new()),
                    retired: false,
                    next_kind: 0,
                }),
                tasklet: SpinLock::new(None),
            })
            .map_err(|_| SystemError::ENOMEM)?;
        let target = Arc::try_new(Self { body }).map_err(|_| SystemError::ENOMEM)?;
        let weak = Arc::downgrade(&target);
        let tasklet = Tasklet::try_new_for(
            move |_, _| {
                if let Some(target) = weak.upgrade() {
                    fence(Ordering::SeqCst);
                    target.fire_due();
                }
            },
            0,
            None,
            creator,
        )?;
        *target.body.tasklet.lock_irqsave() = Some(tasklet);
        Ok(target)
    }

    pub(crate) fn sample(&self, kind: CpuClockKind) -> u64 {
        self.body.counters[index(kind)].load(Ordering::Acquire)
    }

    fn due(&self) -> bool {
        (0..3).any(|i| {
            self.body.counters[i].load(Ordering::Acquire)
                >= self.body.earliest[i].load(Ordering::Acquire)
        })
    }

    fn schedule(&self) {
        let tasklet = self
            .body
            .tasklet
            .lock_irqsave()
            .as_ref()
            .expect("published CPU domain tasklet")
            .clone();
        tasklet_schedule(&tasklet);
    }

    pub(crate) fn account(&self, kind: CpuClockKind, delta: u64) {
        if delta == 0 {
            return;
        }
        let i = index(kind);
        let now = self.body.counters[i]
            .fetch_add(delta, Ordering::Release)
            .wrapping_add(delta);
        fence(Ordering::SeqCst);
        if now >= self.body.earliest[i].load(Ordering::Acquire) {
            self.schedule();
        }
    }

    fn refresh(&self, queues: &Queues, i: usize) {
        self.body.earliest[i].store(
            queues.trees[i].first_key().map_or(u64::MAX, |key| key.0),
            Ordering::Release,
        );
    }

    pub(crate) fn prepare_registration(
        creator: &Arc<ObjectAccount>,
        callback: Arc<dyn CpuEventCallback>,
    ) -> Result<Arc<CpuEventRegistration>, MemoryAllocationError> {
        let spare = Node::try_new(creator)?;
        let body = OwnedBox::prepare(creator, KernelByteKind::SlabUnreclaimable)?
            .try_init(RegistrationData {
                state: SpinLock::new(RegistrationState {
                    domain: None,
                    queued: None,
                    spare: Some(spare),
                    generation: 0,
                }),
                callback,
            })
            .map_err(|_| SystemError::ENOMEM)?;
        Arc::try_new(CpuEventRegistration { body }).map_err(|_| SystemError::ENOMEM.into())
    }

    /// One registration belongs to one domain for its entire lifetime.
    /// Arm may change kind/deadline and reuses the same accounted allocation.
    pub(crate) fn arm(
        &self,
        entry: &Arc<CpuEventRegistration>,
        kind: CpuClockKind,
        deadline: u64,
        generation: u64,
    ) -> Result<(), SystemError> {
        let mut queues = self.body.queues.lock_irqsave();
        if queues.retired {
            return Err(SystemError::ESRCH);
        }
        let mut state = entry.body.state.lock_irqsave();
        let domain = self as *const Self as usize;
        if state.domain.is_some_and(|owner| owner != domain) {
            return Err(SystemError::EINVAL);
        }
        state.domain = Some(domain);
        let retired = if let Some((old_i, key)) = state.queued.take() {
            let mut old = queues.trees[old_i]
                .remove(&key)
                .expect("registered CPU node exists");
            let retired = old.pending.take();
            assert!(state.spare.is_none());
            state.spare = Some(old);
            self.refresh(&queues, old_i);
            retired
        } else {
            None
        };
        let i = index(kind);
        let key = (deadline, Arc::as_ptr(entry) as usize);
        let mut node = state.spare.take().expect("idle registration owns node");
        node.key = key;
        node.pending = Some(entry.clone());
        state.generation = generation;
        state.queued = Some((i, key));
        queues.trees[i].insert(node);
        self.refresh(&queues, i);
        drop(state);
        drop(queues);
        drop(retired);
        fence(Ordering::SeqCst);
        if self.due() {
            self.schedule();
        }
        Ok(())
    }

    pub(crate) fn cancel(&self, entry: &Arc<CpuEventRegistration>) {
        let mut queues = self.body.queues.lock_irqsave();
        let mut state = entry.body.state.lock_irqsave();
        if state
            .domain
            .is_some_and(|owner| owner != self as *const Self as usize)
        {
            return;
        }
        let retired = if let Some((i, key)) = state.queued.take() {
            let mut node = queues.trees[i]
                .remove(&key)
                .expect("registered CPU node exists");
            let retired = node.pending.take();
            assert!(state.spare.is_none());
            state.spare = Some(node);
            self.refresh(&queues, i);
            retired
        } else {
            None
        };
        drop(state);
        drop(queues);
        drop(retired);
    }

    fn pop(&self, due_only: bool) -> Option<(Arc<CpuEventRegistration>, u64)> {
        let mut queues = self.body.queues.lock_irqsave();
        for offset in 0..3 {
            let i = (queues.next_kind + offset) % 3;
            let Some(key) = queues.trees[i].first_key() else {
                continue;
            };
            if due_only && (queues.retired || key.0 > self.body.counters[i].load(Ordering::Acquire))
            {
                continue;
            }
            let mut node = queues.trees[i].pop_first().expect("first CPU node exists");
            let entry = node.pending.take().expect("queued node pins registration");
            let generation;
            {
                let mut state = entry.body.state.lock_irqsave();
                assert_eq!(state.queued.take(), Some((i, key)));
                generation = state.generation;
                assert!(state.spare.is_none());
                state.spare = Some(node);
            }
            self.refresh(&queues, i);
            queues.next_kind = (i + 1) % 3;
            drop(queues);
            return Some((entry, generation));
        }
        None
    }

    fn fire_due(&self) {
        for _ in 0..MAX_DUE_EVENTS {
            let Some((entry, generation)) = self.pop(true) else {
                return;
            };
            entry.body.callback.expired(generation);
        }
        if self.due() {
            self.schedule();
        }
    }

    /// Task reaping removes measurement registrations, without fabricating a
    /// timer expiration or completing a CPU sleep successfully.
    pub(crate) fn retire(&self) {
        self.body.queues.lock_irqsave().retired = true;
        while let Some(entry) = self.pop(false) {
            drop(entry);
        }
    }

    fn wait_once<F: FnMut() -> bool>(
        &self,
        kind: CpuClockKind,
        deadline: u64,
        mut condition: F,
        creator: &Arc<ObjectAccount>,
    ) -> Result<(), MemoryAllocationError> {
        if condition() {
            return Ok(());
        }
        let (waiter, waker) = Waiter::try_new_pair_for(creator)?;
        let callback = Arc::try_new(WakeCallback(waker)).map_err(|_| SystemError::ENOMEM)?;
        let entry = Self::prepare_registration(creator, callback)?;
        struct Active<'a>(&'a CpuEventDomain, Arc<CpuEventRegistration>);
        impl Drop for Active<'_> {
            fn drop(&mut self) {
                self.0.cancel(&self.1);
            }
        }
        let active = Active(self, entry);
        loop {
            // An exited target has stopped its clock. Like Linux's removed
            // temporary timer, remain interruptible rather than fake expiry.
            match self.arm(&active.1, kind, deadline, 0) {
                Ok(()) | Err(SystemError::ESRCH) => {}
                Err(error) => return Err(error.into()),
            }
            if condition() {
                return Ok(());
            }
            match waiter.wait(true) {
                Ok(()) => {}
                Err(SystemError::ERESTARTSYS) if condition() => return Ok(()),
                Err(error) => return Err(error.into()),
            }
        }
    }

    pub(crate) fn wait<F: FnMut() -> bool>(
        &self,
        kind: CpuClockKind,
        deadline: u64,
        mut condition: F,
    ) -> Result<(), SystemError> {
        if condition() {
            return Ok(());
        }
        let creator = crate::cgroup::memory::object_for_task(&ProcessManager::current_pcb());
        let mut recovery = ChargeRecoveryState::new();
        loop {
            match self.wait_once(kind, deadline, &mut condition, &creator) {
                Ok(()) => return Ok(()),
                Err(error) => error.recover(&mut recovery)?,
            }
        }
    }
}

struct WakeCallback(Arc<Waker>);
impl CpuEventCallback for WakeCallback {
    fn expired(&self, _generation: u64) {
        self.0.wake();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cgroup::memory::accounting::{MemoryAccount, MemoryPolicy, KERNEL_BYTE_KIND_COUNT};

    fn actor(id: u64) -> (Arc<MemoryAccount>, Arc<ObjectAccount>) {
        let root = MemoryAccount::new(id, None).unwrap();
        let leaf = MemoryAccount::new(id + 1, Some(root)).unwrap();
        let actor = ObjectAccount::new(&leaf).unwrap();
        (leaf, actor)
    }

    struct Capture(AtomicU64);
    impl CpuEventCallback for Capture {
        fn expired(&self, generation: u64) {
            self.0.store(generation, Ordering::Release);
        }
    }

    #[test]
    fn cpu_wait_ready_condition_needs_no_metadata_or_quota() {
        let (leaf, actor) = actor(77100);
        let domain = CpuEventDomain::try_new_for(&actor).unwrap();
        let baseline = leaf.snapshot();
        leaf.set_policy(MemoryPolicy {
            max: Some(0),
            ..Default::default()
        });
        domain
            .wait_once(CpuClockKind::Sched, 0, || true, &actor)
            .unwrap();
        assert_eq!(leaf.snapshot().current, baseline.current);
        assert_eq!(leaf.snapshot().kernel_bytes, baseline.kernel_bytes);
        drop(domain);
        assert_eq!(leaf.current_pages(), 0);
    }

    #[test]
    fn cpu_wait_due_and_cancel_return_same_nodes_before_last_weak_drop() {
        let (leaf, actor) = actor(77102);
        let domain = CpuEventDomain::try_new_for(&actor).unwrap();
        let baseline = leaf.current_pages();
        let mut registrations = alloc::vec::Vec::new();
        for id in 0..24 {
            let callback = Arc::new(Capture(AtomicU64::new(0)));
            let entry = CpuEventDomain::prepare_registration(&actor, callback.clone()).unwrap();
            let identity = {
                let state = entry.body.state.lock_irqsave();
                &**state.spare.as_ref().unwrap() as *const Node as usize
            };
            domain.arm(&entry, CpuClockKind::Sched, 1, id + 1).unwrap();
            registrations.push((entry, callback, identity));
        }
        domain.body.counters[2].store(1, Ordering::Release);
        domain.fire_due();
        assert!(domain.body.queues.lock_irqsave().trees[2]
            .first_key()
            .is_none());
        for (i, (entry, callback, identity)) in registrations.iter().enumerate() {
            let state = entry.body.state.lock_irqsave();
            assert!(state.queued.is_none());
            assert_eq!(
                &**state.spare.as_ref().unwrap() as *const Node as usize,
                *identity
            );
            assert!(state.spare.as_ref().unwrap().pending.is_none());
            assert_eq!(callback.0.load(Ordering::Acquire), i as u64 + 1);
        }
        for (entry, _, _) in &registrations {
            domain.arm(entry, CpuClockKind::Virt, 2, 33).unwrap();
            domain.cancel(entry);
        }
        assert!(domain.body.queues.lock_irqsave().trees[1]
            .first_key()
            .is_none());
        let weak = Arc::downgrade(&registrations[0].0);
        drop(registrations);
        assert!(weak.upgrade().is_none());
        assert_eq!(leaf.current_pages(), baseline);
        drop(weak);
        let weak_domain = Arc::downgrade(&domain);
        drop(domain);
        assert!(weak_domain.upgrade().is_none());
        assert_eq!(leaf.current_pages(), 0);
        assert_eq!(leaf.snapshot().kernel_bytes, [0; KERNEL_BYTE_KIND_COUNT]);
    }

    #[test]
    fn cpu_wait_ordered_tree_preserves_identity_and_balance_randomly() {
        let (leaf, actor) = actor(77104);
        let mut tree = CpuDeadlineQueue::new();
        let mut nodes = alloc::vec::Vec::new();
        let mut identities = [0usize; 64];
        for (id, identity) in identities.iter_mut().enumerate() {
            let mut node = Node::try_new(&actor).unwrap();
            node.key = ((id % 7) as u64, id);
            *identity = &*node as *const Node as usize;
            nodes.push(Some(node));
        }
        let mut random = 7u64;
        for _ in 0..10000 {
            random ^= random << 13;
            random ^= random >> 7;
            random ^= random << 17;
            let id = random as usize % 64;
            if let Some(node) = nodes[id].take() {
                tree.insert(node);
            } else {
                let node = tree.remove(&((id % 7) as u64, id)).unwrap();
                assert_eq!(&*node as *const Node as usize, identities[id]);
                nodes[id] = Some(node);
            }
            tree.validate();
        }
        let mut previous = None;
        while let Some(node) = tree.pop_first() {
            assert!(previous.is_none_or(|key| key < node.key));
            previous = Some(node.key);
            assert_eq!(&*node as *const Node as usize, identities[node.key.1]);
            drop(node);
        }
        drop(tree);
        drop(nodes);
        assert_eq!(leaf.current_pages(), 0);
    }

    #[test]
    fn cpu_kinds_rearm_generation_and_retirement_are_independent() {
        let (_leaf, actor) = actor(77106);
        let domain = CpuEventDomain::try_new_for(&actor).unwrap();
        let callback = Arc::new(Capture(AtomicU64::new(0)));
        let entry = CpuEventDomain::prepare_registration(&actor, callback.clone()).unwrap();
        domain.arm(&entry, CpuClockKind::Virt, 3, 1).unwrap();
        domain.body.counters[0].store(20, Ordering::Release);
        domain.fire_due();
        assert_eq!(callback.0.load(Ordering::Acquire), 0);
        // Move an armed node to another kind; the old arm cannot fire it.
        domain.arm(&entry, CpuClockKind::Prof, 10, 2).unwrap();
        domain.fire_due();
        assert_eq!(callback.0.load(Ordering::Acquire), 2);
        domain.arm(&entry, CpuClockKind::Sched, 1, 3).unwrap();
        domain.retire();
        domain.body.counters[2].store(20, Ordering::Release);
        domain.fire_due();
        assert_eq!(callback.0.load(Ordering::Acquire), 2);
        assert_eq!(
            domain.arm(&entry, CpuClockKind::Sched, 1, 4),
            Err(SystemError::ESRCH)
        );
        domain.cancel(&entry);
        assert!(entry.body.state.lock_irqsave().spare.is_some());
    }
}
