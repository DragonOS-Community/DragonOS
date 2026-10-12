//! rq producers only update counters and schedule a preborn tasklet.
//! One prepared ordered node owns both the CPU deadline and waiter.
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
};
use alloc::sync::Arc;
use core::sync::atomic::{fence, AtomicU64, Ordering};
use system_error::SystemError;
#[path = "cpu_deadline_queue.rs"]
mod queue;
use queue::{CpuDeadlineQueue, Node};
type Key = (u64, usize);
const MAX_DUE_WAKEUPS: usize = 32;
struct RegistrationState {
    queued: Option<Key>,
    spare: Option<OwnedBox<Node>>,
}
struct RegistrationData {
    state: SpinLock<RegistrationState>,
    waker: Arc<Waker>,
}
struct Registration {
    body: OwnedBox<RegistrationData>,
}
pub(super) struct CpuTimeWait {
    body: OwnedBox<CpuTimeWaitData>,
}
struct CpuTimeWaitData {
    runtime: AtomicU64,
    deadlines: SpinLock<CpuDeadlineQueue>,
    earliest: AtomicU64,
    // Filled once while unpublished, after the true weak target exists.
    tasklet: SpinLock<Option<Arc<Tasklet>>>,
}
impl CpuTimeWait {
    pub(super) fn try_new_for(
        creator: &Arc<ObjectAccount>,
    ) -> Result<Arc<Self>, MemoryAllocationError> {
        let body = OwnedBox::prepare(creator, KernelByteKind::SlabUnreclaimable)?
            .try_init(CpuTimeWaitData {
                runtime: AtomicU64::new(0),
                deadlines: SpinLock::new(CpuDeadlineQueue::new()),
                earliest: AtomicU64::new(u64::MAX),
                tasklet: SpinLock::new(None),
            })
            .map_err(|_| SystemError::ENOMEM)?;
        let target = Arc::try_new(Self { body }).map_err(|_| SystemError::ENOMEM)?;
        let weak = Arc::downgrade(&target);
        let tasklet = Tasklet::try_new_for(
            move |_, _| {
                if let Some(target) = weak.upgrade() {
                    fence(Ordering::SeqCst);
                    target.wake_due();
                }
            },
            0,
            None,
            creator,
        )?;
        *target.body.tasklet.lock_irqsave() = Some(tasklet);
        Ok(target)
    }
    fn due(&self) -> bool {
        self.runtime() >= self.body.earliest.load(Ordering::Acquire)
    }
    pub(super) fn runtime(&self) -> u64 {
        self.body.runtime.load(Ordering::Acquire)
    }
    fn schedule(&self) {
        let tasklet = self
            .body
            .tasklet
            .lock_irqsave()
            .as_ref()
            .expect("published CPU wait has a tasklet")
            .clone();
        tasklet_schedule(&tasklet);
    }
    pub(super) fn account(&self, delta: u64) {
        self.body.runtime.fetch_add(delta, Ordering::Release);
        fence(Ordering::SeqCst);
        if self.due() {
            self.schedule();
        }
    }
    fn refresh_earliest(&self, queue: &CpuDeadlineQueue) {
        self.body.earliest.store(
            queue.first_key().map_or(u64::MAX, |key| key.0),
            Ordering::Release,
        );
    }
    fn register(&self, entry: &Arc<Registration>, deadline: u64) {
        let mut queue = self.body.deadlines.lock_irqsave();
        let mut state = entry.body.state.lock_irqsave();
        if entry.body.waker.is_closed() || state.queued.is_some() {
            return;
        }
        let mut node = state.spare.take().expect("idle CPU waiter owns its node");
        let key = (deadline, Arc::as_ptr(entry) as usize);
        node.key = key;
        assert!(node.pending.is_none());
        node.pending = Some(entry.clone());
        queue.insert(node);
        state.queued = Some(key);
        self.refresh_earliest(&queue);
        drop(state);
        drop(queue);
        fence(Ordering::SeqCst);
        // The last rq update may have happened before registration.
        if self.due() {
            self.schedule();
        }
    }
    fn unregister(&self, entry: &Arc<Registration>) {
        entry.body.waker.close();
        let mut queue = self.body.deadlines.lock_irqsave();
        let mut state = entry.body.state.lock_irqsave();
        let retired = if let Some(key) = state.queued.take() {
            let mut node = queue.remove(&key).expect("registered CPU node exists");
            let retired = node.pending.take();
            assert!(state.spare.is_none());
            state.spare = Some(node);
            retired
        } else {
            None
        };
        self.refresh_earliest(&queue);
        drop(state);
        drop(queue);
        drop(retired);
    }
    fn wake_due(&self) {
        for _ in 0..MAX_DUE_WAKEUPS {
            let entry = {
                let mut queue = self.body.deadlines.lock_irqsave();
                let Some(key) = queue.first_key() else { return };
                if key.0 > self.runtime() {
                    return;
                }
                let mut node = queue.pop_first().expect("first CPU node exists");
                let entry = node.pending.take().expect("queued node pins registration");
                {
                    let mut state = entry.body.state.lock_irqsave();
                    assert_eq!(state.queued.take(), Some(key));
                    assert!(state.spare.is_none());
                    state.spare = Some(node);
                }
                self.refresh_earliest(&queue);
                drop(queue);
                entry
            };
            entry.body.waker.wake();
            // Final node/registration/body references retire outside guards.
        }
        if self.due() {
            self.schedule();
        }
    }
    fn prepare_registration(
        creator: &Arc<ObjectAccount>,
    ) -> Result<(Waiter, Arc<Registration>), MemoryAllocationError> {
        let spare = Node::try_new(creator)?;
        let (waiter, waker) = Waiter::try_new_pair_for(creator)?;
        let body = OwnedBox::prepare(creator, KernelByteKind::SlabUnreclaimable)?
            .try_init(RegistrationData {
                state: SpinLock::new(RegistrationState {
                    queued: None,
                    spare: Some(spare),
                }),
                waker,
            })
            .map_err(|_| SystemError::ENOMEM)?;
        let entry = Arc::try_new(Registration { body }).map_err(|_| SystemError::ENOMEM)?;
        Ok((waiter, entry))
    }
    fn wait_once<F: FnMut() -> bool>(
        &self,
        deadline: u64,
        mut condition: F,
        creator: &Arc<ObjectAccount>,
    ) -> Result<(), MemoryAllocationError> {
        if condition() {
            return Ok(());
        }
        let (waiter, entry) = Self::prepare_registration(creator)?;
        struct Active<'a>(&'a CpuTimeWait, Arc<Registration>);
        impl Drop for Active<'_> {
            fn drop(&mut self) {
                self.0.unregister(&self.1);
            }
        }
        let active = Active(self, entry);
        loop {
            self.register(&active.1, deadline);
            if condition() {
                return Ok(());
            }
            waiter.wait(true)?;
        }
    }
}
impl super::ProcessSignalState {
    pub(crate) fn account_cpu_runtime(&self, delta: u64) {
        self.body.cpu_time_wait.account(delta);
    }
    pub(crate) fn cpu_runtime(&self) -> u64 {
        self.body.cpu_time_wait.runtime()
    }
    /// CPU nanosleep and its restart are the only callers and hold no resource
    /// guards. Keep the exact group and allocating caller identity on retries.
    pub(crate) fn wait_cpu_time<F: FnMut() -> bool>(
        &self,
        deadline: u64,
        mut condition: F,
    ) -> Result<(), SystemError> {
        if condition() {
            return Ok(());
        }
        let creator = crate::cgroup::memory::object_for_task(&ProcessManager::current_pcb());
        let mut recovery = ChargeRecoveryState::new();
        loop {
            match self
                .body
                .cpu_time_wait
                .wait_once(deadline, &mut condition, &creator)
            {
                Ok(()) => return Ok(()),
                Err(error) => error.recover(&mut recovery)?,
            }
        }
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

    #[test]
    fn cpu_wait_ready_condition_needs_no_metadata_or_quota() {
        let (leaf, actor) = actor(77100);
        let wait = CpuTimeWait::try_new_for(&actor).unwrap();
        let baseline = leaf.snapshot();
        leaf.set_policy(MemoryPolicy {
            max: Some(0),
            ..Default::default()
        });
        wait.wait_once(0, || true, &actor).unwrap();
        assert_eq!(leaf.snapshot().current, baseline.current);
        assert_eq!(leaf.snapshot().kernel_bytes, baseline.kernel_bytes);
        drop(wait);
        assert_eq!(leaf.current_pages(), 0);
    }

    #[test]
    fn cpu_wait_due_and_cancel_return_same_nodes_before_last_weak_drop() {
        let (leaf, actor) = actor(77102);
        let wait = CpuTimeWait::try_new_for(&actor).unwrap();
        let baseline = leaf.current_pages();
        let mut registrations = alloc::vec::Vec::new();
        for _ in 0..24 {
            let (waiter, entry) = CpuTimeWait::prepare_registration(&actor).unwrap();
            let identity = {
                let state = entry.body.state.lock_irqsave();
                &**state.spare.as_ref().unwrap() as *const Node as usize
            };
            wait.register(&entry, 1);
            registrations.push((waiter, entry, identity));
        }
        wait.body.runtime.store(1, Ordering::Release);
        wait.wake_due();
        assert!(wait.body.deadlines.lock_irqsave().first_key().is_none());
        for (_, entry, identity) in &registrations {
            let state = entry.body.state.lock_irqsave();
            assert!(state.queued.is_none());
            assert_eq!(
                &**state.spare.as_ref().unwrap() as *const Node as usize,
                *identity
            );
            assert!(state.spare.as_ref().unwrap().pending.is_none());
        }
        // Cancel closed registrations after pop: no stale re-publication.
        for (_, entry, _) in &registrations {
            wait.unregister(entry);
            wait.register(entry, 2);
        }
        assert!(wait.body.deadlines.lock_irqsave().first_key().is_none());
        let weak = Arc::downgrade(&registrations[0].1);
        drop(registrations);
        assert!(weak.upgrade().is_none());
        assert_eq!(leaf.current_pages(), baseline);
        drop(weak);
        let weak_wait = Arc::downgrade(&wait);
        drop(wait);
        assert!(weak_wait.upgrade().is_none());
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
            let id = (random as usize) % 64;
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
}
