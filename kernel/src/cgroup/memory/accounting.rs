//! Allocation-free, non-sleeping hierarchical page accounting.
//!
//! Like Linux's page_counter, reservations visit leaf to root and undo the
//! visited prefix on failure. No ancestor locks are nested. Each policy lock
//! serializes a max update with reservations at that node; reclamation and OOM
//! must never run inside this module. A charge pins its original parent chain,
//! including offline accounts, until the resource is actually released.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use system_error::SystemError;

use crate::{arch::MMArch, libs::spinlock::SpinLock, mm::MemoryManagementArch};

/// Linux's page-counter representation bound, including the unlimited root.
/// This is an arithmetic bound, not a memory.max policy. It also bounds all
/// byte statistics backed by these page reservations below LONG_MAX.
pub(crate) const PAGE_COUNTER_MAX: u64 = (isize::MAX as u64) / MMArch::PAGE_SIZE as u64;

const KIND_COUNT: usize = 7;
const EVENT_COUNT: usize = 6;
pub(crate) const KERNEL_BYTE_KIND_COUNT: usize = 5;
const STAT_COUNT: usize = 15;

/// Page-state counters are updated at their unique Page/rmap transition; event
/// counters are updated by fault/reclaim, never synthesized by file reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub(crate) enum MemoryStat {
    FileMapped,
    FileDirty,
    FileWriteback,
    Unevictable,
    PgFault,
    PgMajFault,
    PgScan,
    PgSteal,
    PgRefill,
    InactiveAnon,
    ActiveAnon,
    InactiveFile,
    ActiveFile,
    PgActivate,
    PgDeactivate,
}

/// Actual allocated bytes, independent of the shared kernel page budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub(crate) enum KernelByteKind {
    Stack,
    PageTables,
    SlabReclaimable,
    SlabUnreclaimable,
    /// Buddy-backed kernel allocations, not slab and not socket data.
    KernelOther,
}

impl KernelByteKind {
    pub(crate) fn for_allocation(self, layout: core::alloc::Layout) -> Self {
        if matches!(self, Self::SlabReclaimable | Self::SlabUnreclaimable)
            && crate::mm::allocator::kernel_allocator::allocation_uses_buddy(layout)
        {
            Self::KernelOther
        } else {
            self
        }
    }
}

/// All sizes in this module are physical pages, not bytes or virtual mappings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct MemoryPolicy {
    pub min: u64,
    pub low: u64,
    pub high: Option<u64>,
    pub max: Option<u64>,
    pub oom_group: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub(crate) enum MemoryKind {
    Anon,
    File,
    Shmem,
    KernelStack,
    PageTables,
    KernelObject,
    Sock,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub(crate) enum MemoryEvent {
    Low,
    High,
    Max,
    Oom,
    OomKill,
    OomGroupKill,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct MemorySnapshot {
    pub current: u64,
    pub local_current: u64,
    pub peak: u64,
    pub anon: u64,
    /// Includes shmem, matching memory.stat. Never sum file + shmem for usage.
    pub file: u64,
    pub shmem: u64,
    pub kernel_stack: u64,
    pub pagetables: u64,
    pub kernel_object: u64,
    pub sock: u64,
    pub kernel_bytes: [u64; KERNEL_BYTE_KIND_COUNT],
    pub events: [u64; EVENT_COUNT],
    pub events_local: [u64; EVENT_COUNT],
    pub event_sequence: u64,
    pub local_event_sequence: u64,
    pub stats: [u64; STAT_COUNT],
}

#[derive(Debug)]
pub(crate) struct MemoryAccount {
    id: u64,
    parent: Option<Arc<Self>>,
    policy: SpinLock<MemoryPolicy>,
    online: AtomicBool,
    current: AtomicU64,
    local_current: AtomicU64,
    peak: AtomicU64,
    kinds: [AtomicU64; KIND_COUNT],
    kernel_bytes: [AtomicU64; KERNEL_BYTE_KIND_COUNT],
    events: [AtomicU64; EVENT_COUNT],
    events_local: [AtomicU64; EVENT_COUNT],
    event_sequence: AtomicU64,
    local_event_sequence: AtomicU64,
    // Admission-space progress, not memory.events or physical reclaim counts.
    release_epoch: AtomicU64,
    stats: [AtomicU64; STAT_COUNT],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChargeFailureReason {
    Max,
    CounterOverflow,
    /// Caller selected a controller instance that was retired before admission.
    /// Resolve the active domain again; never turn this into a limit/OOM event.
    Offline,
}

/// Pins the actual limiting ancestor so callers can act after dropping locks.
#[derive(Debug)]
pub(crate) struct ChargeFailure {
    pub account: Arc<MemoryAccount>,
    pub reason: ChargeFailureReason,
    pub request_kind: MemoryKind,
    pub requested_pages: u64,
}

/// Unique ownership of a reservation. Not Clone: shared pages share the Page,
/// not duplicated charges. Ordinary pages do not use objcg reparent semantics.
#[derive(Debug)]
#[must_use]
pub(crate) struct MemoryCharge {
    account: Arc<MemoryAccount>,
    kind: MemoryKind,
    pages: u64,
}

impl MemoryAccount {
    /// Construction is fallible and belongs to a sleepable topology transaction.
    pub fn new(id: u64, parent: Option<Arc<Self>>) -> Result<Arc<Self>, SystemError> {
        let account = Arc::try_new(Self {
            id,
            parent,
            policy: SpinLock::new(MemoryPolicy::default()),
            online: AtomicBool::new(true),
            current: AtomicU64::new(0),
            local_current: AtomicU64::new(0),
            peak: AtomicU64::new(0),
            kinds: [const { AtomicU64::new(0) }; KIND_COUNT],
            kernel_bytes: [const { AtomicU64::new(0) }; KERNEL_BYTE_KIND_COUNT],
            events: [const { AtomicU64::new(0) }; EVENT_COUNT],
            events_local: [const { AtomicU64::new(0) }; EVENT_COUNT],
            event_sequence: AtomicU64::new(0),
            local_event_sequence: AtomicU64::new(0),
            release_epoch: AtomicU64::new(0),
            stats: [const { AtomicU64::new(0) }; STAT_COUNT],
        })
        .map_err(|_| SystemError::ENOMEM)?;
        super::registry::register(&account)?;
        Ok(account)
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn parent(&self) -> Option<&Arc<Self>> {
        self.parent.as_ref()
    }

    pub fn is_online(&self) -> bool {
        self.online.load(Ordering::Acquire)
    }

    pub fn current_pages(&self) -> u64 {
        self.current.load(Ordering::Acquire)
    }

    pub fn release_epoch(&self) -> u64 {
        self.release_epoch.load(Ordering::Acquire)
    }

    /// Publish only after returning admission space. A reservation rollback
    /// also permits a retry, but does not prove that a physical frame is free.
    /// No task lookup, allocation, or scheduler callback belongs on this path.
    fn release_counter(&self, pages: u64) {
        if pages != 0 {
            subtract(&self.current, pages);
            self.release_epoch.fetch_add(1, Ordering::Release);
            crate::mm::memory_notify::request_oom_progress();
        }
    }

    /// Identity-based scope checks also work for accounts pinned after rmdir.
    pub fn is_descendant_of(&self, ancestor: &Self) -> bool {
        let mut node = self;
        loop {
            if core::ptr::eq(node, ancestor) {
                return true;
            }
            match node.parent.as_deref() {
                Some(parent) => node = parent,
                None => return false,
            }
        }
    }

    pub fn policy(&self) -> MemoryPolicy {
        *self.policy.lock_irqsave()
    }

    /// Publish only; reclaim/high handling occurs in the caller after unlocking.
    pub fn set_policy(&self, policy: MemoryPolicy) {
        *self.policy.lock_irqsave() = policy;
        crate::mm::memory_notify::request_oom_progress();
    }

    /// Existing page tokens retain this account; routing new allocations and
    /// reparenting kernel object accounts are the topology caller's responsibility.
    pub fn offline(&self) {
        let mut policy = self.policy.lock_irqsave();
        // css_offline removes protection, not the entire historical policy.
        // Hidden-but-still-live controller reset is a different operation.
        policy.min = 0;
        policy.low = 0;
        self.online.store(false, Ordering::Release);
        drop(policy);
        crate::mm::memory_notify::request_oom_progress();
    }

    pub fn try_charge(
        self: &Arc<Self>,
        kind: MemoryKind,
        pages: u64,
    ) -> Result<MemoryCharge, ChargeFailure> {
        self.try_charge_path(kind, pages, true)
    }

    /// Socket birth must be admitted while this generation is online, even
    /// though its initial stock contains no pages. Existing sockets retain
    /// this authorization when their original CSS subsequently goes offline.
    pub(super) fn try_new_socket_budget(self: &Arc<Self>) -> Result<MemoryCharge, ChargeFailure> {
        let _policy = self.policy.lock_irqsave();
        if !self.is_online() {
            return Err(ChargeFailure {
                account: self.clone(),
                reason: ChargeFailureReason::Offline,
                request_kind: MemoryKind::Sock,
                requested_pages: 0,
            });
        }
        Ok(MemoryCharge {
            account: self.clone(),
            kind: MemoryKind::Sock,
            pages: 0,
        })
    }

    /// Only a previously admitted socket may grow its stock on an offline
    /// account. Its historical limits and ancestor counters still apply.
    pub(super) fn try_charge_pinned_socket(
        self: &Arc<Self>,
        pages: u64,
    ) -> Result<MemoryCharge, ChargeFailure> {
        self.try_charge_path(MemoryKind::Sock, pages, false)
    }

    fn try_charge_path(
        self: &Arc<Self>,
        kind: MemoryKind,
        pages: u64,
        require_online: bool,
    ) -> Result<MemoryCharge, ChargeFailure> {
        if pages == 0 {
            return Ok(MemoryCharge {
                account: self.clone(),
                kind,
                pages,
            });
        }
        let mut node = self;
        loop {
            if let Err(reason) =
                node.reserve_local_counter(pages, require_online && Arc::ptr_eq(node, self))
            {
                // Only nodes before the failed node have been reserved. Walk
                // the immutable strong-parent chain again rather than storing
                // a temporary vector or recursing on an unbounded hierarchy.
                let mut reserved = self;
                while !Arc::ptr_eq(reserved, node) {
                    reserved.release_counter(pages);
                    reserved = reserved.parent.as_ref().expect("ancestor chain changed");
                }
                return Err(ChargeFailure {
                    account: node.clone(),
                    reason,
                    request_kind: kind,
                    requested_pages: pages,
                });
            }
            match node.parent.as_ref() {
                Some(parent) => node = parent,
                None => break,
            }
        }
        self.local_current.fetch_add(pages, Ordering::Relaxed);
        self.for_each(|node| node.adjust_kind(kind, pages, true));
        super::high::record_charge(self, pages);
        Ok(MemoryCharge {
            account: self.clone(),
            kind,
            pages,
        })
    }

    fn reserve_local_counter(&self, pages: u64, leaf: bool) -> Result<(), ChargeFailureReason> {
        let policy = self.policy.lock_irqsave();
        if leaf && !self.is_online() {
            return Err(ChargeFailureReason::Offline);
        }
        // Uncharge can run concurrently; CAS prevents a lost decrement.
        let result = self
            .current
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                let new = old.checked_add(pages)?;
                if new > PAGE_COUNTER_MAX
                    || (self.parent.is_some() && policy.max.is_some_and(|max| new > max))
                {
                    None
                } else {
                    Some(new)
                }
            });
        match result {
            Ok(old) => {
                // Linux page_counter watermark may include a speculative leaf
                // reservation subsequently rejected by a higher ancestor.
                self.peak.fetch_max(old + pages, Ordering::Relaxed);
                Ok(())
            }
            Err(old) => Err(
                if old
                    .checked_add(pages)
                    .is_none_or(|new| new > PAGE_COUNTER_MAX)
                {
                    ChargeFailureReason::CounterOverflow
                } else {
                    ChargeFailureReason::Max
                },
            ),
        }
    }

    fn for_each(&self, mut action: impl FnMut(&Self)) {
        let mut node = self;
        loop {
            action(node);
            match node.parent.as_ref() {
                Some(parent) => node = parent,
                None => break,
            }
        }
    }

    fn adjust_kind(&self, kind: MemoryKind, pages: u64, charge: bool) {
        let adjust = |counter: &AtomicU64| {
            if charge {
                counter.fetch_add(pages, Ordering::Relaxed);
            } else {
                subtract(counter, pages);
            }
        };
        adjust(&self.kinds[kind as usize]);
        if kind == MemoryKind::Shmem {
            adjust(&self.kinds[MemoryKind::File as usize]);
        }
    }

    pub(super) fn adjust_kernel_bytes(&self, kind: KernelByteKind, bytes: u64, add: bool) {
        // ObjectAccount reserves the shared page budget before adding bytes
        // and subtracts bytes before releasing it. The common root's bounded
        // reservation therefore prevents aggregate byte-counter overflow,
        // even across many independent object accounts and reparenting.
        self.for_each(|node| {
            let counter = &node.kernel_bytes[kind as usize];
            if add {
                counter.fetch_add(bytes, Ordering::Relaxed);
            } else {
                subtract(counter, bytes);
            }
        });
    }

    /// Common ancestors already include these bytes. Caller validated ancestry
    /// and holds the unique object-budget lock across both budget/stat moves.
    pub(super) fn reparent_kernel_bytes(
        self: &Arc<Self>,
        target: &Arc<Self>,
        kind: KernelByteKind,
        bytes: u64,
    ) {
        let mut node = self;
        while !Arc::ptr_eq(node, target) {
            subtract(&node.kernel_bytes[kind as usize], bytes);
            node = node.parent.as_ref().expect("validated ancestor chain");
        }
    }

    /// Events are recorded by policy slow paths, not every failed speculative
    /// reservation (a caller may retry with a smaller batch).
    pub fn record_event(&self, event: MemoryEvent) {
        self.events_local[event as usize].fetch_add(1, Ordering::Relaxed);
        self.local_event_sequence.fetch_add(1, Ordering::Release);
        self.for_each(|node| {
            node.events[event as usize].fetch_add(1, Ordering::Relaxed);
            node.event_sequence.fetch_add(1, Ordering::Release);
        });
        crate::mm::memory_notify::request_file_events();
    }

    pub(crate) fn event_sequence(&self, local: bool) -> u64 {
        if local {
            self.local_event_sequence.load(Ordering::Acquire)
        } else {
            self.event_sequence.load(Ordering::Acquire)
        }
    }

    pub fn adjust_stat(&self, stat: MemoryStat, amount: u64, add: bool) {
        self.for_each(|node| {
            let counter = &node.stats[stat as usize];
            if add {
                counter.fetch_add(amount, Ordering::Relaxed);
            } else {
                subtract(counter, amount);
            }
        });
    }

    /// Concurrent counter snapshots are approximate, as with Linux vmstats.
    /// Policy admission itself is serialized and does not use this snapshot.
    pub fn snapshot(&self) -> MemorySnapshot {
        // Capture the generation before the counters. A read may acknowledge
        // this generation, but must not consume an event arriving while its
        // snapshot is being built.
        let event_sequence = self.event_sequence.load(Ordering::Acquire);
        let local_event_sequence = self.local_event_sequence.load(Ordering::Acquire);
        MemorySnapshot {
            current: self.current.load(Ordering::Acquire),
            local_current: self.local_current.load(Ordering::Relaxed),
            peak: self.peak.load(Ordering::Relaxed),
            anon: self.kinds[MemoryKind::Anon as usize].load(Ordering::Relaxed),
            file: self.kinds[MemoryKind::File as usize].load(Ordering::Relaxed),
            shmem: self.kinds[MemoryKind::Shmem as usize].load(Ordering::Relaxed),
            kernel_stack: self.kinds[MemoryKind::KernelStack as usize].load(Ordering::Relaxed),
            pagetables: self.kinds[MemoryKind::PageTables as usize].load(Ordering::Relaxed),
            kernel_object: self.kinds[MemoryKind::KernelObject as usize].load(Ordering::Relaxed),
            sock: self.kinds[MemoryKind::Sock as usize].load(Ordering::Relaxed),
            kernel_bytes: core::array::from_fn(|i| self.kernel_bytes[i].load(Ordering::Relaxed)),
            events: core::array::from_fn(|index| self.events[index].load(Ordering::Relaxed)),
            events_local: core::array::from_fn(|index| {
                self.events_local[index].load(Ordering::Relaxed)
            }),
            event_sequence,
            local_event_sequence,
            stats: core::array::from_fn(|index| self.stats[index].load(Ordering::Relaxed)),
        }
    }
}

impl Drop for MemoryAccount {
    fn drop(&mut self) {
        // A final page may pin a long chain of deleted accounts. Tear down the
        // exclusively owned prefix iteratively instead of recursively dropping
        // each parent's Arc. Shared ancestors retain their other owners.
        let mut parent = self.parent.take();
        while let Some(account) = parent {
            // Unlike try_unwrap's discarded Err Arc, concurrent into_inner
            // calls cannot make the last failure recursively drop the body.
            let Some(mut account) = Arc::into_inner(account) else {
                break;
            };
            parent = account.parent.take();
        }
    }
}

impl MemoryCharge {
    /// Transfer admitted socket page stock during pending -> accepted birth.
    /// Both socket stock locks must be held by the caller. Only the net page
    /// difference is reserved on common ancestors; no physical allocation or
    /// ordinary task migration is represented by this operation.
    pub(super) fn transfer_socket_stock(
        source: &mut Self,
        target: &mut Self,
        released: u64,
        added: u64,
    ) -> Result<(), ChargeFailure> {
        assert!(source.kind == MemoryKind::Sock && target.kind == MemoryKind::Sock);
        assert!(released <= source.pages);
        let target_pages = target
            .pages
            .checked_add(added)
            .ok_or_else(|| ChargeFailure {
                account: target.account.clone(),
                reason: ChargeFailureReason::CounterOverflow,
                request_kind: MemoryKind::Sock,
                requested_pages: added,
            })?;
        fn depth(account: &Arc<MemoryAccount>) -> usize {
            let mut depth = 0;
            let mut node = account;
            while let Some(parent) = node.parent.as_ref() {
                depth += 1;
                node = parent;
            }
            depth
        }
        let (mut old, mut new) = (&source.account, &target.account);
        let (mut old_depth, mut new_depth) = (depth(old), depth(new));
        while old_depth > new_depth {
            old = old.parent.as_ref().unwrap();
            old_depth -= 1;
        }
        while new_depth > old_depth {
            new = new.parent.as_ref().unwrap();
            new_depth -= 1;
        }
        let common = loop {
            if Arc::ptr_eq(old, new) {
                break Some(old.clone());
            }
            match (old.parent.as_ref(), new.parent.as_ref()) {
                (Some(a), Some(b)) => {
                    old = a;
                    new = b;
                }
                _ => break None,
            }
        };
        // Walk target -> root once. At/above LCA, only added-released is new
        // stock; below it the target branch receives the complete added stock.
        let mut node = &target.account;
        let mut shared = false;
        loop {
            shared |= common
                .as_ref()
                .is_some_and(|common| Arc::ptr_eq(node, common));
            let amount = if shared {
                added.saturating_sub(released)
            } else {
                added
            };
            if amount != 0 {
                if let Err(reason) = node.reserve_local_counter(amount, false) {
                    let mut reserved = &target.account;
                    let mut rollback_shared = false;
                    while !Arc::ptr_eq(reserved, node) {
                        rollback_shared |= common
                            .as_ref()
                            .is_some_and(|common| Arc::ptr_eq(reserved, common));
                        let rollback = if rollback_shared {
                            added.saturating_sub(released)
                        } else {
                            added
                        };
                        if rollback != 0 {
                            reserved.release_counter(rollback);
                        }
                        reserved = reserved.parent.as_ref().expect("immutable target path");
                    }
                    return Err(ChargeFailure {
                        account: node.clone(),
                        reason,
                        request_kind: MemoryKind::Sock,
                        requested_pages: amount,
                    });
                }
            }
            match node.parent.as_ref() {
                Some(parent) => node = parent,
                None => break,
            }
        }
        let mut node = &source.account;
        let mut shared = false;
        loop {
            shared |= common
                .as_ref()
                .is_some_and(|common| Arc::ptr_eq(node, common));
            let amount = if shared {
                released.saturating_sub(added)
            } else {
                released
            };
            if amount != 0 {
                node.release_counter(amount);
            }
            match node.parent.as_ref() {
                Some(parent) => node = parent,
                None => break,
            }
        }
        // Statistics may be observed approximately during transfer, but the
        // page admission counters above never duplicate the common budget.
        source
            .account
            .for_each(|node| node.adjust_kind(MemoryKind::Sock, released, false));
        target
            .account
            .for_each(|node| node.adjust_kind(MemoryKind::Sock, added, true));
        subtract(&source.account.local_current, released);
        target
            .account
            .local_current
            .fetch_add(added, Ordering::Relaxed);
        source.pages -= released;
        target.pages = target_pages;
        super::high::record_charge_debt(Self::socket_transfer_high_debt(
            &target.account,
            common.as_ref(),
            released,
            added,
        ));
        Ok(())
    }

    fn socket_transfer_high_debt(
        target: &Arc<MemoryAccount>,
        common: Option<&Arc<MemoryAccount>>,
        released: u64,
        added: u64,
    ) -> u64 {
        let mut node = target;
        let mut shared = false;
        while let Some(parent) = node.parent.as_ref() {
            shared |= common.is_some_and(|common| Arc::ptr_eq(node, common));
            let delta = if shared {
                added.saturating_sub(released)
            } else {
                added
            };
            if delta != 0
                && node
                    .policy()
                    .high
                    .is_some_and(|high| node.current_pages() > high)
            {
                return delta;
            }
            node = parent;
        }
        0
    }

    /// Merge an already reserved budget without allocation or a second charge.
    /// An invalid reservation is returned intact for safe caller rollback.
    pub(super) fn merge(&mut self, mut other: Self) -> Result<(), Self> {
        if self.kind != other.kind || !Arc::ptr_eq(&self.account, &other.account) {
            return Err(other);
        }
        let Some(pages) = self.pages.checked_add(other.pages) else {
            return Err(other);
        };
        self.pages = pages;
        other.pages = 0;
        Ok(())
    }
    pub fn account(&self) -> &Arc<MemoryAccount> {
        &self.account
    }

    pub fn pages(&self) -> u64 {
        self.pages
    }

    pub fn kind(&self) -> MemoryKind {
        self.kind
    }

    /// Move a unique kernel budget to an ancestor during objcg offline.
    /// Common ancestors already include this reservation; charging them again
    /// would double-count or spuriously fail a newly lowered limit. Ordinary
    /// data pages and socket accounting retain their separate lifetime rules.
    pub fn reparent_to_ancestor(&mut self, target: &Arc<MemoryAccount>) -> Result<(), SystemError> {
        if !matches!(
            self.kind,
            MemoryKind::KernelStack | MemoryKind::PageTables | MemoryKind::KernelObject
        ) || !self.account.is_descendant_of(target)
        {
            return Err(SystemError::EINVAL);
        }
        if Arc::ptr_eq(&self.account, target) {
            return Ok(());
        }
        subtract(&self.account.local_current, self.pages);
        target
            .local_current
            .fetch_add(self.pages, Ordering::Relaxed);
        let mut node = &self.account;
        while !Arc::ptr_eq(node, target) {
            node.adjust_kind(self.kind, self.pages, false);
            node.release_counter(self.pages);
            node = node.parent.as_ref().expect("validated ancestor chain");
        }
        self.account = target.clone();
        Ok(())
    }

    /// Divide a batch reservation between resource owners without charging it
    /// again. Failure leaves the original token unchanged.
    pub fn split_off(&mut self, pages: u64) -> Result<Self, SystemError> {
        if pages > self.pages {
            return Err(SystemError::EINVAL);
        }
        self.pages -= pages;
        Ok(Self {
            account: self.account.clone(),
            kind: self.kind,
            pages,
        })
    }
}

impl Drop for MemoryCharge {
    fn drop(&mut self) {
        if self.pages == 0 {
            return;
        }
        self.account.for_each(|node| {
            node.adjust_kind(self.kind, self.pages, false);
            node.release_counter(self.pages);
        });
        subtract(&self.account.local_current, self.pages);
    }
}

fn subtract(counter: &AtomicU64, pages: u64) {
    counter
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |old| {
            old.checked_sub(pages)
        })
        .expect("memory charge accounting underflow");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_transfer_high_only_tracks_each_paths_real_increment() {
        let root = MemoryAccount::new(9001, None).unwrap();
        let parent = MemoryAccount::new(9002, Some(root.clone())).unwrap();
        let leaf = MemoryAccount::new(9003, Some(parent.clone())).unwrap();
        let _stock = leaf.try_charge(MemoryKind::Sock, 2).unwrap();
        root.set_policy(MemoryPolicy {
            high: Some(0),
            ..Default::default()
        });
        // Root high is ignored, as in the ordinary charge-side hook.
        assert_eq!(
            MemoryCharge::socket_transfer_high_debt(&leaf, Some(&parent), 1, 1),
            0
        );
        parent.set_policy(MemoryPolicy {
            high: Some(0),
            ..Default::default()
        });
        // A common ancestor's old excess did not gain physical stock.
        assert_eq!(
            MemoryCharge::socket_transfer_high_debt(&leaf, Some(&parent), 1, 1),
            0
        );
        assert_eq!(
            MemoryCharge::socket_transfer_high_debt(&leaf, Some(&leaf), 1, 1),
            0
        );
        // Splitting a page tail is the only new stock at the common ancestor.
        assert_eq!(
            MemoryCharge::socket_transfer_high_debt(&leaf, Some(&parent), 1, 2),
            1
        );
        leaf.set_policy(MemoryPolicy {
            high: Some(0),
            ..Default::default()
        });
        assert_eq!(
            MemoryCharge::socket_transfer_high_debt(&leaf, Some(&parent), 1, 2),
            2
        );
        // Same-account stocks have no exclusive branch and only net debt.
        assert_eq!(
            MemoryCharge::socket_transfer_high_debt(&leaf, Some(&leaf), 1, 2),
            1
        );
    }

    #[test]
    fn state_statistics_follow_original_ancestry_without_charging_again() {
        let root = MemoryAccount::new(1, None).unwrap();
        let child = MemoryAccount::new(2, Some(root.clone())).unwrap();
        let charge = child.try_charge(MemoryKind::File, 1).unwrap();
        child.adjust_stat(MemoryStat::FileMapped, 1, true);
        child.adjust_stat(MemoryStat::FileDirty, 1, true);
        child.offline();
        assert_eq!(root.current_pages(), 1);
        assert_eq!(root.snapshot().stats[MemoryStat::FileMapped as usize], 1);
        assert_eq!(root.snapshot().stats[MemoryStat::FileDirty as usize], 1);
        child.adjust_stat(MemoryStat::FileMapped, 1, false);
        child.adjust_stat(MemoryStat::FileDirty, 1, false);
        child.adjust_stat(MemoryStat::PgFault, 3, true);
        drop(charge);
        assert_eq!(root.current_pages(), 0);
        assert_eq!(root.snapshot().stats[MemoryStat::FileMapped as usize], 0);
        assert_eq!(root.snapshot().stats[MemoryStat::FileDirty as usize], 0);
        assert_eq!(root.snapshot().stats[MemoryStat::PgFault as usize], 3);
    }

    #[test]
    fn budget_merge_preserves_or_rolls_back_unique_reservations() {
        let root = MemoryAccount::new(1, None).unwrap();
        let child = MemoryAccount::new(2, Some(root.clone())).unwrap();
        let mut budget = child.try_charge(MemoryKind::KernelObject, 1).unwrap();
        let extra = child.try_charge(MemoryKind::KernelObject, 2).unwrap();
        budget.merge(extra).unwrap();
        assert_eq!(budget.pages(), 3);
        let foreign = root.try_charge(MemoryKind::KernelObject, 4).unwrap();
        drop(budget.merge(foreign).unwrap_err());
        assert_eq!(budget.pages(), 3);
        assert_eq!(root.snapshot().current, 3);
        let other_kind = child.try_charge(MemoryKind::Anon, 1).unwrap();
        drop(budget.merge(other_kind).unwrap_err());
        assert_eq!(child.snapshot().anon, 0);
        drop(budget);
        assert_eq!(root.snapshot().current, 0);
    }

    #[test]
    fn hierarchy_and_raii_release() {
        let root = MemoryAccount::new(1, None).unwrap();
        let child = MemoryAccount::new(2, Some(root.clone())).unwrap();
        let charge = child.try_charge(MemoryKind::Anon, 4).unwrap();
        assert_eq!(child.snapshot().current, 4);
        assert_eq!(root.snapshot().current, 4);
        assert_eq!(root.snapshot().local_current, 0);
        drop(charge);
        assert_eq!(root.snapshot().current, 0);
        assert_eq!(child.snapshot().anon, 0);
        assert_eq!(child.snapshot().peak, 4);
    }

    #[test]
    fn ancestor_failure_rolls_back_prefix() {
        let root = MemoryAccount::new(1, None).unwrap();
        let parent = MemoryAccount::new(2, Some(root.clone())).unwrap();
        parent.set_policy(MemoryPolicy {
            max: Some(2),
            ..Default::default()
        });
        let child = MemoryAccount::new(3, Some(parent.clone())).unwrap();
        let failure = child.try_charge(MemoryKind::Anon, 3).unwrap_err();
        assert!(Arc::ptr_eq(&failure.account, &parent));
        assert_eq!(failure.reason, ChargeFailureReason::Max);
        assert_eq!(child.snapshot().current, 0);
        assert_eq!(child.snapshot().anon, 0);
        assert_eq!(parent.snapshot().current, 0);
        assert_eq!(root.snapshot().current, 0);
    }

    #[test]
    fn shmem_is_file_subset_not_duplicate_usage() {
        let root = MemoryAccount::new(1, None).unwrap();
        let charge = root.try_charge(MemoryKind::Shmem, 3).unwrap();
        let snapshot = root.snapshot();
        assert_eq!(snapshot.current, 3);
        assert_eq!(snapshot.file, 3);
        assert_eq!(snapshot.shmem, 3);
        drop(charge);
        assert_eq!(root.snapshot().file, 0);
    }

    #[test]
    fn root_ignores_max_and_split_preserves_ownership() {
        let root = MemoryAccount::new(1, None).unwrap();
        root.set_policy(MemoryPolicy {
            max: Some(0),
            ..Default::default()
        });
        let mut charge = root.try_charge(MemoryKind::PageTables, 4).unwrap();
        assert!(charge.split_off(5).is_err());
        assert_eq!(charge.pages(), 4);
        let part = charge.split_off(3).unwrap();
        drop(charge);
        assert_eq!(root.snapshot().current, 3);
        drop(part);
        assert_eq!(root.snapshot().current, 0);
    }

    #[test]
    fn offline_keeps_charges_and_event_ancestry() {
        let root = MemoryAccount::new(1, None).unwrap();
        let child = MemoryAccount::new(2, Some(root.clone())).unwrap();
        let charge = child.try_charge(MemoryKind::File, 2).unwrap();
        child.record_event(MemoryEvent::Oom);
        child.offline();
        assert!(!child.is_online());
        assert_eq!(
            child.try_charge(MemoryKind::File, 1).unwrap_err().reason,
            ChargeFailureReason::Offline
        );
        assert_eq!(child.snapshot().current, 2);
        assert_eq!(root.snapshot().events[MemoryEvent::Oom as usize], 1);
        assert_eq!(root.snapshot().events_local[MemoryEvent::Oom as usize], 0);
        drop(child);
        assert_eq!(root.snapshot().current, 2);
        drop(charge);
        assert_eq!(root.snapshot().current, 0);
    }

    #[test]
    fn offline_generation_does_not_adopt_reenabled_instance_charges() {
        let root = MemoryAccount::new(1, None).unwrap();
        let old = MemoryAccount::new(2, Some(root.clone())).unwrap();
        let old_charge = old.try_charge(MemoryKind::Anon, 3).unwrap();
        old.offline();
        let new = MemoryAccount::new(2, Some(root.clone())).unwrap();
        let new_charge = new.try_charge(MemoryKind::Anon, 1).unwrap();
        assert!(!Arc::ptr_eq(&old, &new));
        assert_eq!(old.snapshot().current, 3);
        assert_eq!(new.snapshot().current, 1);
        assert_eq!(root.snapshot().current, 4);
        drop(old_charge);
        assert_eq!(new.snapshot().current, 1);
        assert_eq!(root.snapshot().current, 1);
        drop(new_charge);
        assert_eq!(root.snapshot().current, 0);
    }

    #[test]
    fn offline_removes_protection_without_resetting_historical_limits() {
        let root = MemoryAccount::new(1, None).unwrap();
        let child = MemoryAccount::new(2, Some(root)).unwrap();
        child.set_policy(MemoryPolicy {
            min: 2,
            low: 4,
            high: Some(6),
            max: Some(8),
            oom_group: true,
        });
        child.offline();
        assert_eq!(
            child.policy(),
            MemoryPolicy {
                min: 0,
                low: 0,
                high: Some(6),
                max: Some(8),
                oom_group: true,
            }
        );
    }

    #[test]
    fn kernel_budget_reparent_preserves_common_ancestors_and_final_release() {
        let root = MemoryAccount::new(1, None).unwrap();
        let parent = MemoryAccount::new(2, Some(root.clone())).unwrap();
        let leaf = MemoryAccount::new(3, Some(parent.clone())).unwrap();
        let mut budget = leaf.try_charge(MemoryKind::KernelObject, 3).unwrap();
        parent.set_policy(MemoryPolicy {
            max: Some(0),
            ..Default::default()
        });
        budget.reparent_to_ancestor(&parent).unwrap();
        assert_eq!(leaf.snapshot().current, 0);
        assert_eq!(leaf.snapshot().local_current, 0);
        assert_eq!(parent.snapshot().current, 3);
        assert_eq!(parent.snapshot().local_current, 3);
        assert_eq!(root.snapshot().kernel_object, 3);
        budget.reparent_to_ancestor(&root).unwrap();
        assert_eq!(parent.snapshot().current, 0);
        assert_eq!(root.snapshot().current, 3);
        assert_eq!(root.snapshot().local_current, 3);
        drop(budget);
        assert_eq!(root.snapshot().current, 0);
        assert_eq!(root.snapshot().kernel_object, 0);
    }

    #[test]
    fn reparent_rejects_foreign_scope_and_data_pages_without_mutation() {
        let root = MemoryAccount::new(1, None).unwrap();
        let leaf = MemoryAccount::new(2, Some(root.clone())).unwrap();
        let sibling = MemoryAccount::new(2, Some(root.clone())).unwrap();
        assert!(!leaf.is_descendant_of(&sibling)); // ids are not instance identity
        let mut budget = leaf.try_charge(MemoryKind::PageTables, 2).unwrap();
        assert!(budget.reparent_to_ancestor(&sibling).is_err());
        assert!(Arc::ptr_eq(budget.account(), &leaf));
        assert_eq!(leaf.snapshot().current, 2);
        assert_eq!(sibling.snapshot().current, 0);
        for kind in [
            MemoryKind::Anon,
            MemoryKind::File,
            MemoryKind::Shmem,
            MemoryKind::Sock,
        ] {
            let mut page = leaf.try_charge(kind, 1).unwrap();
            assert!(page.reparent_to_ancestor(&root).is_err());
            assert!(Arc::ptr_eq(page.account(), &leaf));
        }
        let mut empty = leaf.try_charge(MemoryKind::KernelStack, 0).unwrap();
        empty.reparent_to_ancestor(&root).unwrap();
        budget.reparent_to_ancestor(&leaf).unwrap();
        assert_eq!(leaf.snapshot().current, 2);
        drop(budget);
        assert_eq!(root.snapshot().current, 0);
    }

    #[test]
    fn checked_overflow_does_not_publish_or_leak() {
        let root = MemoryAccount::new(1, None).unwrap();
        let charge = root.try_charge(MemoryKind::Anon, PAGE_COUNTER_MAX).unwrap();
        assert_eq!(
            root.try_charge(MemoryKind::Anon, 1).unwrap_err().reason,
            ChargeFailureReason::CounterOverflow
        );
        assert_eq!(root.snapshot().current, PAGE_COUNTER_MAX);
        assert_eq!(root.snapshot().events, [0; EVENT_COUNT]);
        drop(charge);
        assert_eq!(root.snapshot().current, 0);
    }

    #[test]
    fn lowering_limit_preserves_existing_owners() {
        let root = MemoryAccount::new(1, None).unwrap();
        let child = MemoryAccount::new(2, Some(root.clone())).unwrap();
        let charge = child.try_charge(MemoryKind::KernelStack, 4).unwrap();
        child.set_policy(MemoryPolicy {
            max: Some(2),
            ..Default::default()
        });
        assert_eq!(child.snapshot().kernel_stack, 4);
        assert!(child.try_charge(MemoryKind::KernelStack, 1).is_err());
        // Empty reservations do not need resources, even above a lowered max.
        drop(child.try_charge(MemoryKind::Anon, 0).unwrap());
        drop(charge);
        let charge = child.try_charge(MemoryKind::Sock, 2).unwrap();
        assert_eq!(root.snapshot().sock, 2);
        drop(charge);
        assert_eq!(root.snapshot().current, 0);
    }

    #[test]
    fn release_epoch_tracks_returned_budget_not_events_or_token_splits() {
        let root = MemoryAccount::new(1, None).unwrap();
        let child = MemoryAccount::new(2, Some(root.clone())).unwrap();
        let mut charge = child.try_charge(MemoryKind::File, 3).unwrap();
        child.record_event(MemoryEvent::High);
        let tail = charge.split_off(1).unwrap();
        drop(child.try_charge(MemoryKind::Anon, 0).unwrap());
        assert_eq!(child.release_epoch(), 0);
        assert_eq!(root.release_epoch(), 0);
        drop(tail);
        assert_eq!(child.current_pages(), 2);
        assert_eq!(child.release_epoch(), 1);
        assert_eq!(root.release_epoch(), 1);
        drop(charge);
        assert_eq!(child.release_epoch(), 2);
        assert_eq!(root.release_epoch(), 2);
        assert_eq!(child.snapshot().events[MemoryEvent::High as usize], 1);
    }

    #[test]
    fn offline_does_not_fabricate_file_events_but_old_css_keeps_real_events() {
        let root = MemoryAccount::new(1, None).unwrap();
        let child = MemoryAccount::new(2, Some(root.clone())).unwrap();
        child.record_event(MemoryEvent::Oom);
        let before = child.snapshot();
        child.offline();
        let after = child.snapshot();
        assert!(!child.is_online());
        assert_eq!(after.events, before.events);
        assert_eq!(after.events_local, before.events_local);
        assert_eq!(after.event_sequence, before.event_sequence);
        assert_eq!(after.local_event_sequence, before.local_event_sequence);
        // Tokens/sockets can retain an offline generation; its genuine event
        // stream remains usable and is not redirected to a replacement CSS.
        child.record_event(MemoryEvent::OomKill);
        assert_eq!(child.event_sequence(false), before.event_sequence + 1);
        assert_eq!(child.event_sequence(true), before.local_event_sequence + 1);
        assert_eq!(root.snapshot().events[MemoryEvent::OomKill as usize], 1);
    }

    #[test]
    fn failed_ancestor_reservation_publishes_only_rolled_back_space() {
        let root = MemoryAccount::new(1, None).unwrap();
        let parent = MemoryAccount::new(2, Some(root.clone())).unwrap();
        let child = MemoryAccount::new(3, Some(parent.clone())).unwrap();
        parent.set_policy(MemoryPolicy {
            max: Some(0),
            ..Default::default()
        });
        assert!(child.try_charge(MemoryKind::Anon, 1).is_err());
        assert_eq!(child.current_pages(), 0);
        assert_eq!(child.release_epoch(), 1);
        assert_eq!(parent.release_epoch(), 0);
        assert_eq!(root.release_epoch(), 0);
    }

    #[test]
    fn object_reparent_returns_old_scope_space_without_releasing_ancestor_budget() {
        let root = MemoryAccount::new(1, None).unwrap();
        let parent = MemoryAccount::new(2, Some(root.clone())).unwrap();
        let child = MemoryAccount::new(3, Some(parent.clone())).unwrap();
        let mut charge = child.try_charge(MemoryKind::KernelObject, 1).unwrap();
        charge.reparent_to_ancestor(&parent).unwrap();
        assert_eq!(child.current_pages(), 0);
        assert_eq!(child.release_epoch(), 1);
        assert_eq!(parent.current_pages(), 1);
        assert_eq!(parent.release_epoch(), 0);
        assert_eq!(root.release_epoch(), 0);
        drop(charge);
        assert_eq!(child.release_epoch(), 1);
        assert_eq!(parent.release_epoch(), 1);
        assert_eq!(root.release_epoch(), 1);
    }

    #[test]
    fn deep_offline_parent_chain_teardown_is_iterative() {
        let root = MemoryAccount::new(1, None).unwrap();
        let mut leaf = root.clone();
        for id in 2..4096 {
            leaf = MemoryAccount::new(id, Some(leaf)).unwrap();
        }
        let charge = leaf.try_charge(MemoryKind::File, 1).unwrap();
        drop(leaf);
        assert_eq!(root.snapshot().current, 1);
        drop(charge);
        assert_eq!(root.snapshot().current, 0);
    }
}
