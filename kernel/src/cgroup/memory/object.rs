//! Shared byte budgets for kernel objects, with objcg-style ancestor reparent.
//! Physical owners release their ObjectCharge only after returning memory.

use super::accounting::{
    ChargeFailure, ChargeFailureReason, KernelByteKind, MemoryAccount, MemoryCharge, MemoryKind,
    KERNEL_BYTE_KIND_COUNT,
};
use crate::{arch::MMArch, libs::spinlock::SpinLock, mm::MemoryManagementArch};
use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
};
use system_error::SystemError;

const PAGE_BYTES: u64 = MMArch::PAGE_SIZE as u64;
const KINDS: [KernelByteKind; KERNEL_BYTE_KIND_COUNT] = [
    KernelByteKind::Stack,
    KernelByteKind::PageTables,
    KernelByteKind::SlabReclaimable,
    KernelByteKind::SlabUnreclaimable,
    KernelByteKind::KernelOther,
    KernelByteKind::Vmalloc,
];

#[derive(Debug)]
struct ObjectState {
    budget: MemoryCharge,
    bytes: u64,
    kinds: [u64; KERNEL_BYTE_KIND_COUNT],
}

#[derive(Debug)]
pub(crate) struct ObjectAccount {
    state: SpinLock<ObjectState>,
}

#[derive(Debug)]
#[must_use]
pub(crate) struct ObjectCharge {
    owner: Arc<ObjectAccount>,
    kind: KernelByteKind,
    bytes: u64,
}

impl ObjectCharge {
    /// Borrow the stable owner without creating a separate objcg budget.
    pub(crate) fn owner(&self) -> &Arc<ObjectAccount> {
        &self.owner
    }
}

struct ObjectLink {
    owner: Weak<ObjectAccount>,
    next: Option<Box<Self>>,
}

struct ObjectRegistry {
    head: SpinLock<Option<Box<ObjectLink>>>,
}

/// Fallible topology preparation is separate from registry publication. A
/// cancelled controller transaction leaves no weak node in the registry.
pub(crate) struct PreparedObjectAccount {
    owner: Arc<ObjectAccount>,
    link: Box<ObjectLink>,
}

impl PreparedObjectAccount {
    pub fn object_account(&self) -> &Arc<ObjectAccount> {
        &self.owner
    }

    /// Consumes preallocated metadata; cannot fail or allocate.
    pub fn publish(self) -> Arc<ObjectAccount> {
        self.publish_in(&OBJECTS)
    }

    fn publish_in(self, registry: &ObjectRegistry) -> Arc<ObjectAccount> {
        let Self { owner, mut link } = self;
        {
            let mut head = registry.head.lock_irqsave();
            // Serializes registration against the offline mark/list detach.
            owner.state.lock_irqsave().route_online();
            link.next = head.take();
            *head = Some(link);
        }
        owner
    }
}

static OBJECTS: ObjectRegistry = ObjectRegistry::new();

fn pages(bytes: u64) -> u64 {
    bytes / PAGE_BYTES + u64::from(!bytes.is_multiple_of(PAGE_BYTES))
}

impl ObjectAccount {
    /// Topology setup is fallible; neither charge nor free allocates metadata.
    pub fn new(account: &Arc<MemoryAccount>) -> Result<Arc<Self>, SystemError> {
        Ok(Self::prepare(account)?.publish())
    }

    fn new_in(
        account: &Arc<MemoryAccount>,
        registry: &ObjectRegistry,
    ) -> Result<Arc<Self>, SystemError> {
        Ok(Self::prepare(account)?.publish_in(registry))
    }

    pub fn prepare(account: &Arc<MemoryAccount>) -> Result<PreparedObjectAccount, SystemError> {
        let budget = account
            .try_charge(MemoryKind::KernelObject, 0)
            .map_err(|_| SystemError::ENOMEM)?;
        let owner = Arc::try_new(Self {
            state: SpinLock::new(ObjectState {
                budget,
                bytes: 0,
                kinds: [0; KERNEL_BYTE_KIND_COUNT],
            }),
        })
        .map_err(|_| SystemError::ENOMEM)?;
        let link = Box::try_new(ObjectLink {
            owner: Arc::downgrade(&owner),
            next: None,
        })
        .map_err(|_| SystemError::ENOMEM)?;
        Ok(PreparedObjectAccount { owner, link })
    }

    /// Classify the actual allocation using the allocator's own route. Large
    /// kmalloc-equivalent storage is kernel memory, but not slab memory.
    pub fn try_charge_allocation(
        self: &Arc<Self>,
        kind: KernelByteKind,
        layout: core::alloc::Layout,
    ) -> Result<ObjectCharge, ChargeFailure> {
        let kind = kind.for_allocation(layout);
        self.try_charge(
            kind,
            crate::mm::allocator::kernel_allocator::allocation_footprint(layout),
        )
    }

    /// Nonblocking admission only. Reclaim/OOM retry belongs outside this lock.
    pub fn try_charge(
        self: &Arc<Self>,
        kind: KernelByteKind,
        bytes: u64,
    ) -> Result<ObjectCharge, ChargeFailure> {
        let mut state = self.state.lock_irqsave();
        state.route_online();
        let total = state
            .bytes
            .checked_add(bytes)
            .ok_or_else(|| ChargeFailure {
                account: state.budget.account().clone(),
                reason: ChargeFailureReason::CounterOverflow,
                request_kind: MemoryKind::KernelObject,
                requested_pages: pages(bytes),
            })?;
        let extra = pages(total) - state.budget.pages();
        let reservation = state
            .budget
            .account()
            .try_charge(MemoryKind::KernelObject, extra)?;
        // Same owner/kind is established under this unique state lock. If this
        // invariant ever changes, the returned reservation safely rolls back.
        state
            .budget
            .merge(reservation)
            .expect("object budget owner changed under lock");
        state
            .budget
            .account()
            .adjust_kernel_bytes(kind, bytes, true);
        state.bytes = total;
        state.kinds[kind as usize] += bytes;
        Ok(ObjectCharge {
            owner: self.clone(),
            kind,
            bytes,
        })
    }
}

impl ObjectState {
    fn route_online(&mut self) {
        if self.budget.account().is_online() {
            return;
        }
        let mut target = self.budget.account().clone();
        while !target.is_online() {
            target = target
                .parent()
                .expect("root memory account remains online")
                .clone();
        }
        self.reparent(&target);
    }

    fn reparent(&mut self, target: &Arc<MemoryAccount>) {
        let old = self.budget.account().clone();
        self.budget
            .reparent_to_ancestor(target)
            .expect("object target must be an ancestor");
        for kind in KINDS {
            old.reparent_kernel_bytes(target, kind, self.kinds[kind as usize]);
        }
    }
}

impl ObjectRegistry {
    const fn new() -> Self {
        Self {
            head: SpinLock::new(None),
        }
    }

    /// Caller holds cgroup UPDATE_LOCK, serializing topology offline operations.
    /// Detach under the registry lock, then upgrade/destroy metadata outside it.
    fn offline(&self, account: &Arc<MemoryAccount>) -> Result<(), SystemError> {
        if account.parent().is_none() {
            return Err(SystemError::EINVAL);
        }
        let mut pending = {
            let mut head = self.head.lock_irqsave();
            account.offline();
            head.take()
        };
        let mut live = None;
        while let Some(mut link) = pending {
            pending = link.next.take();
            if let Some(owner) = link.owner.upgrade() {
                {
                    let mut state = owner.state.lock_irqsave();
                    if Arc::ptr_eq(state.budget.account(), account) {
                        state.route_online();
                    }
                }
                // Last strong object owner may destroy an account chain; do
                // not do that under the registry IRQ lock.
                drop(owner);
                link.next = live;
                live = Some(link);
            }
            // A dead weak link is likewise destroyed outside registry locks.
        }
        if let Some(mut live) = live {
            let mut tail = &mut live;
            while tail.next.is_some() {
                tail = tail.next.as_mut().unwrap();
            }
            let mut head = self.head.lock_irqsave();
            tail.next = head.take();
            *head = Some(live);
        }
        Ok(())
    }
}

/// Must be called from the serialized cgroup topology transaction, not IRQ/free.
pub(crate) fn offline_memory_account(account: &Arc<MemoryAccount>) -> Result<(), SystemError> {
    OBJECTS.offline(account)
}

impl Drop for ObjectCharge {
    fn drop(&mut self) {
        let mut state = self.owner.state.lock_irqsave();
        state
            .budget
            .account()
            .adjust_kernel_bytes(self.kind, self.bytes, false);
        state.bytes -= self.bytes;
        state.kinds[self.kind as usize] -= self.bytes;
        let excess = state.budget.pages() - pages(state.bytes);
        drop(
            state
                .budget
                .split_off(excess)
                .expect("object budget covers live bytes"),
        );
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::super::accounting::MemoryPolicy;
    use super::*;
    use std::{
        sync::Barrier,
        thread,
        time::{Duration, Instant},
    };

    fn wait_for_offline(account: &MemoryAccount) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while account.is_online() {
            if Instant::now() >= deadline {
                return false;
            }
            thread::yield_now();
        }
        true
    }

    fn assert_empty(account: &MemoryAccount) {
        let snapshot = account.snapshot();
        assert_eq!(snapshot.current, 0);
        assert_eq!(snapshot.local_current, 0);
        assert_eq!(
            snapshot.kernel_bytes,
            [0; crate::cgroup::memory::accounting::KERNEL_BYTE_KIND_COUNT]
        );
    }

    #[test]
    fn concurrent_mixed_byte_charges_share_and_release_the_same_budget() {
        let registry = ObjectRegistry::new();
        let root = MemoryAccount::new(1, None).unwrap();
        let parent = MemoryAccount::new(2, Some(root.clone())).unwrap();
        let leaf = MemoryAccount::new(3, Some(parent.clone())).unwrap();
        let object = ObjectAccount::new_in(&leaf, &registry).unwrap();
        let barrier = Barrier::new(8);
        thread::scope(|scope| {
            for worker in 0..8_u64 {
                let object = &object;
                let barrier = &barrier;
                scope.spawn(move || {
                    let mut random = worker + 1;
                    barrier.wait();
                    for _ in 0..2000 {
                        let mut tokens: [Option<ObjectCharge>; 4] = core::array::from_fn(|_| None);
                        for slot in &mut tokens {
                            random ^= random << 13;
                            random ^= random >> 7;
                            random ^= random << 17;
                            let size = [0, 1, PAGE_BYTES - 1, PAGE_BYTES, PAGE_BYTES + 1]
                                [(random % 5) as usize];
                            *slot = Some(
                                object
                                    .try_charge(KINDS[(random % 4) as usize], size)
                                    .unwrap(),
                            );
                        }
                        // Keep several differently sized live tokens while other threads
                        // merge/split the one rounded budget; vary the free order too.
                        if random & 7 == 0 {
                            thread::yield_now();
                        }
                        for index in 0..4 {
                            drop(tokens[((random as usize & 3) + index) % 4].take());
                        }
                    }
                });
            }
        });
        // Snapshots are checked only after joining: their separate atomic fields
        // deliberately do not promise a consistent snapshot during mutations.
        for account in [&root, &parent, &leaf] {
            assert_empty(account);
        }
        let state = object.state.lock_irqsave();
        assert_eq!(state.bytes, 0);
        assert_eq!(
            state.kinds,
            [0; crate::cgroup::memory::accounting::KERNEL_BYTE_KIND_COUNT]
        );
        assert_eq!(state.budget.pages(), 0);
    }

    #[test]
    fn registration_while_offline_list_is_detached_survives_ancestor_offline() {
        let registry = ObjectRegistry::new();
        let root = MemoryAccount::new(1, None).unwrap();
        let parent = MemoryAccount::new(2, Some(root.clone())).unwrap();
        let leaf = MemoryAccount::new(3, Some(parent.clone())).unwrap();
        let old = ObjectAccount::new_in(&leaf, &registry).unwrap();
        let old_charge = old
            .try_charge(KernelByteKind::Stack, PAGE_BYTES + 1)
            .unwrap();
        let held = old.state.lock_irqsave();
        let fresh = thread::scope(|scope| {
            let offliner = scope.spawn(|| registry.offline(&leaf).unwrap());
            let detached = wait_for_offline(&leaf);
            let (sender, receiver) = std::sync::mpsc::channel();
            let registry_ref = &registry;
            let leaf_ref = &leaf;
            let registrar = scope.spawn(move || {
                let object = ObjectAccount::new_in(leaf_ref, registry_ref).unwrap();
                sender.send(object).unwrap();
            });
            let result = receiver.recv_timeout(Duration::from_secs(5));
            // Release the state lock before reporting timeout/joining, so a
            // registry-lock regression cannot leave this test's cleanup blocked.
            drop(held);
            offliner.join().unwrap();
            registrar.join().unwrap();
            assert!(detached, "offline did not reach its detached-list phase");
            result.expect("publication must not wait for an unrelated object's state lock")
        });
        let fresh_charge = fresh
            .try_charge(KernelByteKind::PageTables, PAGE_BYTES - 1)
            .unwrap();
        assert!(Arc::ptr_eq(
            fresh.state.lock_irqsave().budget.account(),
            &parent
        ));
        registry.offline(&parent).unwrap();
        // Do not charge again before checking: lazy rerouting would conceal a
        // newly registered link lost by the detached-list splice.
        assert!(Arc::ptr_eq(
            fresh.state.lock_irqsave().budget.account(),
            &root
        ));
        assert!(Arc::ptr_eq(
            old.state.lock_irqsave().budget.account(),
            &root
        ));
        assert_empty(&leaf);
        assert_empty(&parent);
        assert_eq!(root.snapshot().current, 3);
        assert_eq!(
            root.snapshot().kernel_bytes,
            [PAGE_BYTES + 1, PAGE_BYTES - 1, 0, 0, 0]
        );
        drop(old_charge);
        drop(fresh_charge);
        assert_empty(&root);
    }

    #[test]
    fn buddy_bytes_reparent_without_becoming_slab() {
        let registry = ObjectRegistry::new();
        let root = MemoryAccount::new(1, None).unwrap();
        let parent = MemoryAccount::new(2, Some(root.clone())).unwrap();
        let leaf = MemoryAccount::new(3, Some(parent.clone())).unwrap();
        let object = ObjectAccount::new_in(&leaf, &registry).unwrap();
        let charge = object
            .try_charge_allocation(
                KernelByteKind::SlabUnreclaimable,
                core::alloc::Layout::new::<[u8; 2064]>(),
            )
            .unwrap();
        let other = KernelByteKind::KernelOther as usize;
        assert_eq!(leaf.snapshot().kernel_bytes[other], PAGE_BYTES);
        registry.offline(&leaf).unwrap();
        assert_empty(&leaf);
        assert_eq!(parent.snapshot().kernel_bytes[other], PAGE_BYTES);
        registry.offline(&parent).unwrap();
        assert_empty(&parent);
        assert_eq!(root.snapshot().kernel_bytes[other], PAGE_BYTES);
        assert_eq!(
            root.snapshot().kernel_bytes[KernelByteKind::SlabUnreclaimable as usize],
            0
        );
        assert_eq!(root.current_pages(), 1);
        drop(charge);
        assert_empty(&root);
    }

    #[test]
    fn last_object_owner_dropped_during_detached_walk_is_pruned() {
        let registry = ObjectRegistry::new();
        let root = MemoryAccount::new(1, None).unwrap();
        let parent = MemoryAccount::new(2, Some(root.clone())).unwrap();
        let leaf = MemoryAccount::new(3, Some(parent.clone())).unwrap();
        // Registry prepends: the blocked live object is visited before doomed.
        let doomed = ObjectAccount::new_in(&leaf, &registry).unwrap();
        let weak = Arc::downgrade(&doomed);
        let doomed_charge = doomed
            .try_charge(KernelByteKind::SlabUnreclaimable, 1)
            .unwrap();
        let blocker = ObjectAccount::new_in(&leaf, &registry).unwrap();
        let charge = blocker
            .try_charge(KernelByteKind::PageTables, PAGE_BYTES)
            .unwrap();
        let held = blocker.state.lock_irqsave();
        thread::scope(|scope| {
            let offliner = scope.spawn(|| registry.offline(&leaf).unwrap());
            let detached = wait_for_offline(&leaf);
            drop(doomed_charge);
            drop(doomed);
            let destroyed = weak.upgrade().is_none();
            drop(held);
            offliner.join().unwrap();
            assert!(detached);
            assert!(destroyed);
        });
        {
            let head = registry.head.lock_irqsave();
            assert!(head.is_some());
            assert!(head.as_ref().unwrap().next.is_none());
        }
        registry.offline(&parent).unwrap();
        assert!(Arc::ptr_eq(
            blocker.state.lock_irqsave().budget.account(),
            &root
        ));
        assert_empty(&leaf);
        assert_empty(&parent);
        drop(charge);
        assert_empty(&root);
    }

    #[test]
    fn cancelled_preparation_does_not_publish_a_weak_registry_node() {
        let registry = ObjectRegistry::new();
        let root = MemoryAccount::new(1, None).unwrap();
        let prepared = ObjectAccount::prepare(&root).unwrap();
        let weak = Arc::downgrade(prepared.object_account());
        drop(prepared);
        assert!(weak.upgrade().is_none());
        assert!(registry.head.lock_irqsave().is_none());
        assert_empty(&root);
    }

    #[test]
    fn prepared_object_published_after_offline_routes_to_live_ancestor() {
        let registry = ObjectRegistry::new();
        let root = MemoryAccount::new(1, None).unwrap();
        let parent = MemoryAccount::new(2, Some(root.clone())).unwrap();
        let leaf = MemoryAccount::new(3, Some(parent.clone())).unwrap();
        let prepared = ObjectAccount::prepare(&leaf).unwrap();
        registry.offline(&leaf).unwrap();
        registry.offline(&parent).unwrap();
        let object = prepared.publish_in(&registry);
        assert!(Arc::ptr_eq(
            object.state.lock_irqsave().budget.account(),
            &root
        ));
        let charge = object
            .try_charge(KernelByteKind::SlabReclaimable, PAGE_BYTES + 1)
            .unwrap();
        assert_empty(&leaf);
        assert_empty(&parent);
        assert_eq!(root.snapshot().current, 2);
        assert_eq!(root.snapshot().kernel_bytes, [0, 0, PAGE_BYTES + 1, 0, 0]);
        drop(charge);
        assert_empty(&root);
    }

    #[test]
    fn aggregate_byte_statistics_are_bounded_by_root_budget() {
        use super::super::accounting::PAGE_COUNTER_MAX;
        let registry = ObjectRegistry::new();
        let root = MemoryAccount::new(1, None).unwrap();
        let first = ObjectAccount::new_in(&root, &registry).unwrap();
        let second = ObjectAccount::new_in(&root, &registry).unwrap();
        let failure = first
            .try_charge(KernelByteKind::SlabUnreclaimable, u64::MAX)
            .unwrap_err();
        assert_eq!(failure.reason, ChargeFailureReason::CounterOverflow);
        assert_eq!(root.snapshot().current, 0);
        assert_eq!(
            root.snapshot().kernel_bytes,
            [0; crate::cgroup::memory::accounting::KERNEL_BYTE_KIND_COUNT]
        );

        let a_bytes = (PAGE_COUNTER_MAX - 1) * PAGE_BYTES;
        let a = first
            .try_charge(KernelByteKind::SlabUnreclaimable, a_bytes)
            .unwrap();
        let b = second
            .try_charge(KernelByteKind::PageTables, PAGE_BYTES)
            .unwrap();
        assert_eq!(root.snapshot().current, PAGE_COUNTER_MAX);
        assert_eq!(root.snapshot().kernel_bytes, [0, PAGE_BYTES, 0, a_bytes, 0]);
        assert_eq!(
            second
                .try_charge(KernelByteKind::Stack, 1)
                .unwrap_err()
                .reason,
            ChargeFailureReason::CounterOverflow
        );
        assert_eq!(root.snapshot().kernel_bytes, [0, PAGE_BYTES, 0, a_bytes, 0]);
        assert_eq!(second.state.lock_irqsave().bytes, PAGE_BYTES);
        drop(b);
        let replacement = second
            .try_charge(KernelByteKind::Stack, PAGE_BYTES)
            .unwrap();
        drop(a);
        drop(replacement);
        assert_eq!(root.snapshot().current, 0);
        assert_eq!(
            root.snapshot().kernel_bytes,
            [0; crate::cgroup::memory::accounting::KERNEL_BYTE_KIND_COUNT]
        );
    }

    #[test]
    fn kinds_share_one_tail_and_roll_back_failed_growth() {
        let registry = ObjectRegistry::new();
        let root = MemoryAccount::new(1, None).unwrap();
        let leaf = MemoryAccount::new(2, Some(root.clone())).unwrap();
        leaf.set_policy(MemoryPolicy {
            max: Some(1),
            ..Default::default()
        });
        let object = ObjectAccount::new_in(&leaf, &registry).unwrap();
        let a = object
            .try_charge(KernelByteKind::SlabUnreclaimable, 64)
            .unwrap();
        let b = object
            .try_charge(KernelByteKind::SlabReclaimable, PAGE_BYTES - 64)
            .unwrap();
        assert_eq!(leaf.snapshot().current, 1);
        assert!(object.try_charge(KernelByteKind::Stack, 1).is_err());
        assert_eq!(leaf.snapshot().kernel_bytes, [0, 0, PAGE_BYTES - 64, 64, 0]);
        drop(a);
        assert_eq!(leaf.snapshot().current, 1);
        drop(b);
        assert_eq!(root.snapshot().current, 0);
    }

    #[test]
    fn whole_pages_and_multigeneration_reparent() {
        let registry = ObjectRegistry::new();
        let root = MemoryAccount::new(1, None).unwrap();
        let parent = MemoryAccount::new(2, Some(root.clone())).unwrap();
        let leaf = MemoryAccount::new(3, Some(parent.clone())).unwrap();
        let object = ObjectAccount::new_in(&leaf, &registry).unwrap();
        let stack = object
            .try_charge(KernelByteKind::Stack, 2 * PAGE_BYTES)
            .unwrap();
        let table = object
            .try_charge(KernelByteKind::PageTables, PAGE_BYTES)
            .unwrap();
        registry.offline(&leaf).unwrap();
        assert_eq!(leaf.snapshot().current, 0);
        assert_eq!(
            leaf.snapshot().kernel_bytes,
            [0; crate::cgroup::memory::accounting::KERNEL_BYTE_KIND_COUNT]
        );
        assert_eq!(parent.snapshot().local_current, 3);
        registry.offline(&parent).unwrap();
        assert_eq!(parent.snapshot().current, 0);
        assert_eq!(root.snapshot().current, 3);
        assert_eq!(
            root.snapshot().kernel_bytes,
            [2 * PAGE_BYTES, PAGE_BYTES, 0, 0, 0]
        );
        drop(stack);
        drop(table);
        assert_eq!(root.snapshot().current, 0);
        assert_eq!(
            root.snapshot().kernel_bytes,
            [0; crate::cgroup::memory::accounting::KERNEL_BYTE_KIND_COUNT]
        );
    }

    #[test]
    fn newly_registered_account_routes_past_offline_owner() {
        let registry = ObjectRegistry::new();
        let root = MemoryAccount::new(1, None).unwrap();
        let leaf = MemoryAccount::new(2, Some(root.clone())).unwrap();
        registry.offline(&leaf).unwrap();
        let object = ObjectAccount::new_in(&leaf, &registry).unwrap();
        let token = object
            .try_charge(KernelByteKind::SlabUnreclaimable, 1)
            .unwrap();
        assert_eq!(leaf.snapshot().current, 0);
        assert_eq!(root.snapshot().local_current, 1);
        drop(token);
        assert_eq!(root.snapshot().current, 0);
    }

    #[test]
    fn physical_owner_returns_storage_before_uncharge() {
        struct Owner {
            storage: Option<Box<[u8; 4096]>>,
            account: Arc<MemoryAccount>,
            charge: Option<ObjectCharge>,
        }
        impl Drop for Owner {
            fn drop(&mut self) {
                assert_eq!(self.account.snapshot().current, 1);
                drop(self.storage.take());
                assert_eq!(self.account.snapshot().current, 1);
                drop(self.charge.take());
                assert_eq!(self.account.snapshot().current, 0);
            }
        }
        let registry = ObjectRegistry::new();
        let root = MemoryAccount::new(1, None).unwrap();
        let object = ObjectAccount::new_in(&root, &registry).unwrap();
        let owner = Owner {
            storage: Some(Box::try_new([0; 4096]).unwrap()),
            account: root.clone(),
            charge: Some(
                object
                    .try_charge(KernelByteKind::PageTables, PAGE_BYTES)
                    .unwrap(),
            ),
        };
        assert_eq!(root.snapshot().current, 1);
        drop(owner);
        assert_eq!(root.snapshot().current, 0);
    }
}
