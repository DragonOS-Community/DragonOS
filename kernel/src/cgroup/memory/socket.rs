//! Socket-local page stocks pinned to the socket's original memory account.
//!
//! Unlike objcg, socket memory is not reparented during controller offline.
//! This module only admits and refunds storage; buffer allocation and any
//! reclaim/OOM recovery belong to callers outside socket/resource locks.

use alloc::sync::Arc;

use crate::{arch::MMArch, libs::spinlock::SpinLock, mm::MemoryManagementArch};

use super::accounting::{
    ChargeFailure, ChargeFailureReason, MemoryAccount, MemoryCharge, MemoryKind,
};

const PAGE_BYTES: u64 = MMArch::PAGE_SIZE as u64;

#[derive(Debug)]
struct SockState {
    budget: MemoryCharge,
    bytes: u64,
}

#[derive(Debug)]
pub(crate) struct SockAccount {
    state: SpinLock<SockState>,
}

/// Unique receipt for actual socket buffer storage, not payload length.
/// Release it only after the associated allocation has been returned.
#[derive(Debug)]
#[must_use]
pub(crate) struct SockCharge {
    owner: Arc<SockAccount>,
    bytes: u64,
}

fn pages(bytes: u64) -> u64 {
    bytes / PAGE_BYTES + u64::from(!bytes.is_multiple_of(PAGE_BYTES))
}

impl SockAccount {
    /// Bind all actual buffer receipts of one pending connection to its new
    /// accepted socket in one admission transaction. A partial transfer would
    /// require fallible reverse admission and is therefore never published.
    pub fn transfer_birth_receipts(
        receipts: &mut [&mut SockCharge],
        target: &Arc<Self>,
    ) -> Result<(), ChargeFailure> {
        let Some(first) = receipts.first() else {
            return Ok(());
        };
        let source = first.owner.clone();
        let bytes = receipts
            .iter()
            .try_fold(0u64, |bytes, receipt| {
                assert!(
                    Arc::ptr_eq(&source, &receipt.owner),
                    "one pending connection stock"
                );
                bytes.checked_add(receipt.bytes)
            })
            .expect("receipt bytes are part of the source stock");
        if Arc::ptr_eq(&source, target) {
            return Ok(());
        }
        // Stable lock ordering covers parallel transfers in both directions.
        let (mut lower, mut upper) = if Arc::as_ptr(&source) < Arc::as_ptr(target) {
            (source.state.lock_irqsave(), target.state.lock_irqsave())
        } else {
            (target.state.lock_irqsave(), source.state.lock_irqsave())
        };
        let (old, new) = if Arc::as_ptr(&source) < Arc::as_ptr(target) {
            (&mut *lower, &mut *upper)
        } else {
            (&mut *upper, &mut *lower)
        };
        let remaining = old.bytes.checked_sub(bytes).expect("live source receipts");
        let total = new.bytes.checked_add(bytes).ok_or_else(|| ChargeFailure {
            account: new.budget.account().clone(),
            reason: ChargeFailureReason::CounterOverflow,
            request_kind: MemoryKind::Sock,
            requested_pages: pages(bytes),
        })?;
        let released = old.budget.pages() - pages(remaining);
        let added = pages(total) - new.budget.pages();
        MemoryCharge::transfer_socket_stock(&mut old.budget, &mut new.budget, released, added)?;
        old.bytes = remaining;
        new.bytes = total;
        for receipt in receipts {
            receipt.owner = target.clone();
        }
        // The local source Arc above keeps the old stock alive while swapping
        // receipt handles; its possible final destruction is outside guards.
        drop(upper);
        drop(lower);
        drop(source);
        Ok(())
    }

    /// Called at socket birth, outside resource locks, with an explicit creator.
    /// New sockets cannot reuse an already offline generation. TCP accept must
    /// capture the accepting task's account, not inherit the listener's stock.
    pub fn prepare(account: &Arc<MemoryAccount>) -> Result<Self, ChargeFailure> {
        let budget = account.try_new_socket_budget()?;
        Ok(Self {
            state: SpinLock::new(SockState { budget, bytes: 0 }),
        })
    }

    /// Nonblocking admission. No allocation or task lookup occurs under state.
    pub fn try_charge(self: &Arc<Self>, bytes: u64) -> Result<SockCharge, ChargeFailure> {
        let mut state = self.state.lock_irqsave();
        let total = state
            .bytes
            .checked_add(bytes)
            .ok_or_else(|| ChargeFailure {
                account: state.budget.account().clone(),
                reason: ChargeFailureReason::CounterOverflow,
                request_kind: MemoryKind::Sock,
                requested_pages: pages(bytes),
            })?;
        let extra = pages(total) - state.budget.pages();
        let reservation = state.budget.account().try_charge_pinned_socket(extra)?;
        state
            .budget
            .merge(reservation)
            .expect("socket stock account and kind are immutable");
        state.bytes = total;
        Ok(SockCharge {
            owner: self.clone(),
            bytes,
        })
    }
}

impl Drop for SockCharge {
    fn drop(&mut self) {
        let mut state = self.owner.state.lock_irqsave();
        state.bytes -= self.bytes;
        let excess = state.budget.pages() - pages(state.bytes);
        drop(
            state
                .budget
                .split_off(excess)
                .expect("socket stock covers live storage"),
        );
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::super::accounting::{KernelByteKind, MemoryPolicy};
    use super::super::object::ObjectAccount;
    use super::*;
    use std::{sync::Barrier, thread};

    fn new_socket(account: &Arc<MemoryAccount>) -> Arc<SockAccount> {
        Arc::try_new(SockAccount::prepare(account).unwrap()).unwrap()
    }

    fn child() -> (Arc<MemoryAccount>, Arc<MemoryAccount>, Arc<SockAccount>) {
        let root = MemoryAccount::new(1, None).unwrap();
        let leaf = MemoryAccount::new(2, Some(root.clone())).unwrap();
        let socket = new_socket(&leaf);
        (root, leaf, socket)
    }

    #[test]
    fn accept_transfer_does_not_recharge_full_common_ancestor() {
        let root = MemoryAccount::new(1, None).unwrap();
        let parent = MemoryAccount::new(2, Some(root.clone())).unwrap();
        let a = MemoryAccount::new(3, Some(parent.clone())).unwrap();
        let b = MemoryAccount::new(4, Some(parent.clone())).unwrap();
        let old = new_socket(&a);
        let new = new_socket(&b);
        let mut rx = old.try_charge(PAGE_BYTES).unwrap();
        let mut tx = old.try_charge(PAGE_BYTES).unwrap();
        parent.set_policy(MemoryPolicy {
            max: Some(2),
            ..Default::default()
        });
        SockAccount::transfer_birth_receipts(&mut [&mut rx, &mut tx], &new).unwrap();
        assert_eq!(parent.snapshot().current, 2);
        assert_eq!(parent.snapshot().peak, 2);
        assert_eq!(a.snapshot().sock, 0);
        assert_eq!(b.snapshot().sock, 2);
        assert_eq!(old.state.lock_irqsave().bytes, 0);
        drop(rx);
        drop(tx);
        assert_eq!(root.snapshot().current, 0);
    }

    #[test]
    fn accept_refusal_rolls_back_target_prefix_without_changing_receipts() {
        let root = MemoryAccount::new(1, None).unwrap();
        let a = MemoryAccount::new(2, Some(root.clone())).unwrap();
        let branch = MemoryAccount::new(3, Some(root.clone())).unwrap();
        let b = MemoryAccount::new(4, Some(branch.clone())).unwrap();
        let old = new_socket(&a);
        let new = new_socket(&b);
        let mut receipt = old.try_charge(PAGE_BYTES).unwrap();
        branch.set_policy(MemoryPolicy {
            max: Some(0),
            ..Default::default()
        });
        let error = SockAccount::transfer_birth_receipts(&mut [&mut receipt], &new).unwrap_err();
        assert!(Arc::ptr_eq(&error.account, &branch));
        assert_eq!(error.reason, ChargeFailureReason::Max);
        assert_eq!(b.snapshot().current, 0);
        assert_eq!(branch.snapshot().current, 0);
        assert_eq!(root.snapshot().current, 1);
        assert_eq!(a.snapshot().sock, 1);
        assert!(Arc::ptr_eq(&receipt.owner, &old));
        drop(receipt);
        assert_eq!(root.snapshot().current, 0);
    }

    #[test]
    fn transfer_same_account_observes_only_real_page_tail_difference() {
        let (_, leaf, old) = child();
        let new = new_socket(&leaf);
        let mut part = old.try_charge(17).unwrap();
        let rest = old.try_charge(31).unwrap();
        leaf.set_policy(MemoryPolicy {
            max: Some(1),
            ..Default::default()
        });
        assert!(SockAccount::transfer_birth_receipts(&mut [&mut part], &new).is_err());
        assert_eq!(leaf.snapshot().current, 1);
        assert!(Arc::ptr_eq(&part.owner, &old));
        drop(rest);
        // Moving the entire remaining stock does not add a second page.
        SockAccount::transfer_birth_receipts(&mut [&mut part], &new).unwrap();
        assert_eq!(leaf.snapshot().current, 1);
        SockAccount::transfer_birth_receipts(&mut [&mut part], &new).unwrap();
        assert_eq!(leaf.snapshot().current, 1);
        drop(part);
        assert_eq!(leaf.snapshot().current, 0);
    }

    #[test]
    fn transfer_into_existing_stock_can_release_common_rounding_page() {
        let (root, leaf, old) = child();
        let new = new_socket(&leaf);
        let mut receipt = old.try_charge(17).unwrap();
        let already = new.try_charge(31).unwrap();
        assert_eq!(root.snapshot().current, 2);
        SockAccount::transfer_birth_receipts(&mut [&mut receipt], &new).unwrap();
        assert_eq!(root.snapshot().current, 1);
        assert_eq!(leaf.snapshot().sock, 1);
        drop(receipt);
        drop(already);
        assert_eq!(root.snapshot().current, 0);
    }

    #[test]
    fn socket_birth_checks_online_even_with_zero_stock() {
        let (_, leaf, socket) = child();
        assert_eq!(leaf.snapshot().current, 0);
        leaf.offline();
        let failure = SockAccount::prepare(&leaf).unwrap_err();
        assert_eq!(failure.reason, ChargeFailureReason::Offline);
        assert_eq!(failure.request_kind, MemoryKind::Sock);
        assert_eq!(failure.requested_pages, 0);
        assert!(Arc::ptr_eq(&failure.account, &leaf));
        // Ordinary zero-page metadata preparation retains its existing rule.
        let zero = leaf.try_charge(MemoryKind::KernelObject, 0).unwrap();
        assert!(matches!(
            leaf.try_charge(MemoryKind::Anon, 1),
            Err(ChargeFailure {
                reason: ChargeFailureReason::Offline,
                ..
            })
        ));
        let charge = socket.try_charge(17).unwrap();
        assert_eq!(leaf.snapshot().sock, 1);
        drop(charge);
        drop(zero);
        assert_eq!(leaf.snapshot().current, 0);
    }

    #[test]
    fn buffers_share_a_socket_tail_but_not_other_sockets() {
        let (_, leaf, socket) = child();
        let other = new_socket(&leaf);
        let a = socket.try_charge(17).unwrap();
        let b = socket.try_charge(31).unwrap();
        let c = other.try_charge(1).unwrap();
        assert_eq!(leaf.snapshot().current, 2);
        assert_eq!(leaf.snapshot().sock, 2);
        assert_eq!(socket.state.lock_irqsave().bytes, 48);
        drop(a);
        assert_eq!(leaf.snapshot().current, 2);
        drop(b);
        assert_eq!(leaf.snapshot().current, 1);
        drop(c);
        assert_eq!(leaf.snapshot().current, 0);
    }

    #[test]
    fn socket_stock_never_adds_to_kernel_byte_classes() {
        let (_, leaf, socket) = child();
        let object = ObjectAccount::new(&leaf).unwrap();
        let kernel = object
            .try_charge(KernelByteKind::SlabUnreclaimable, 17)
            .unwrap();
        let buffer = socket.try_charge(17).unwrap();
        let snapshot = leaf.snapshot();
        assert_eq!(snapshot.current, 2);
        assert_eq!(snapshot.sock, 1);
        assert_eq!(snapshot.kernel_object, 1);
        assert_eq!(
            snapshot.kernel_bytes[KernelByteKind::SlabUnreclaimable as usize],
            17
        );
        drop(buffer);
        drop(kernel);
        assert_eq!(leaf.snapshot().current, 0);
    }

    #[test]
    fn rejected_growth_and_cancellation_preserve_the_previous_stock() {
        let (_, leaf, socket) = child();
        leaf.set_policy(MemoryPolicy {
            max: Some(1),
            ..MemoryPolicy::default()
        });
        let original = socket.try_charge(PAGE_BYTES).unwrap();
        let failure = socket.try_charge(1).unwrap_err();
        assert_eq!(failure.reason, ChargeFailureReason::Max);
        assert_eq!(failure.request_kind, MemoryKind::Sock);
        assert_eq!(failure.requested_pages, 1);
        assert_eq!(socket.state.lock_irqsave().bytes, PAGE_BYTES);
        assert_eq!(leaf.snapshot().current, 1);
        drop(original);
        let cancelled = socket.try_charge(PAGE_BYTES + 1).unwrap_err();
        assert_eq!(cancelled.reason, ChargeFailureReason::Max);
        assert_eq!(socket.state.lock_irqsave().bytes, 0);
        assert_eq!(leaf.snapshot().current, 0);
    }

    #[test]
    fn offline_old_socket_retains_its_limits_and_original_ancestry() {
        let (root, old, socket) = child();
        let before = socket.try_charge(1).unwrap();
        old.set_policy(MemoryPolicy {
            max: Some(1),
            ..MemoryPolicy::default()
        });
        old.offline();
        let replacement = MemoryAccount::new(2, Some(root.clone())).unwrap();
        let after = socket.try_charge(PAGE_BYTES - 1).unwrap();
        assert_eq!(old.snapshot().sock, 1);
        assert_eq!(replacement.snapshot().sock, 0);
        assert_eq!(root.snapshot().sock, 1);
        assert_eq!(
            socket.try_charge(1).unwrap_err().reason,
            ChargeFailureReason::Max
        );
        drop(before);
        drop(after);
        assert_eq!(root.snapshot().current, 0);
        assert_eq!(old.snapshot().current, 0);
    }

    #[test]
    fn ancestor_refusal_rolls_back_only_the_reserved_prefix() {
        let root = MemoryAccount::new(1, None).unwrap();
        let parent = MemoryAccount::new(2, Some(root.clone())).unwrap();
        let leaf = MemoryAccount::new(3, Some(parent.clone())).unwrap();
        let socket = new_socket(&leaf);
        parent.set_policy(MemoryPolicy {
            max: Some(0),
            ..MemoryPolicy::default()
        });
        leaf.offline();
        let failure = socket.try_charge(1).unwrap_err();
        assert!(Arc::ptr_eq(&failure.account, &parent));
        assert_eq!(failure.reason, ChargeFailureReason::Max);
        assert_eq!(leaf.snapshot().current, 0);
        assert_eq!(parent.snapshot().current, 0);
        assert_eq!(root.snapshot().current, 0);
        assert_eq!(leaf.release_epoch(), 1);
    }

    #[test]
    fn zero_and_counter_overflow_do_not_mutate_live_stock() {
        let (_, leaf, socket) = child();
        let zero = socket.try_charge(0).unwrap();
        assert_eq!(leaf.snapshot().current, 0);
        let failure = socket.try_charge(u64::MAX).unwrap_err();
        assert_eq!(failure.reason, ChargeFailureReason::CounterOverflow);
        assert_eq!(socket.state.lock_irqsave().bytes, 0);
        assert_eq!(leaf.snapshot().current, 0);
        let live = socket.try_charge(1).unwrap();
        let overflow = socket.try_charge(u64::MAX).unwrap_err();
        assert_eq!(overflow.reason, ChargeFailureReason::CounterOverflow);
        assert_eq!(overflow.request_kind, MemoryKind::Sock);
        assert_eq!(socket.state.lock_irqsave().bytes, 1);
        assert_eq!(leaf.snapshot().current, 1);
        drop(live);
        drop(zero);
    }

    #[test]
    fn birth_before_offline_authorizes_later_publication_without_moving_owner() {
        let root = MemoryAccount::new(1, None).unwrap();
        let leaf = MemoryAccount::new(2, Some(root.clone())).unwrap();
        let prepared = SockAccount::prepare(&leaf).unwrap();
        leaf.offline();
        let socket = Arc::try_new(prepared).unwrap();
        let charge = socket.try_charge(1).unwrap();
        assert_eq!(leaf.snapshot().sock, 1);
        assert_eq!(root.snapshot().sock, 1);
        drop(charge);
        assert_eq!(root.snapshot().current, 0);
    }

    #[test]
    fn receipt_pins_stock_until_the_last_physical_buffer_owner_releases_it() {
        let (_, leaf, socket) = child();
        let charge = socket.try_charge(PAGE_BYTES + 1).unwrap();
        let weak = Arc::downgrade(&socket);
        drop(socket);
        assert!(weak.upgrade().is_some());
        assert_eq!(leaf.snapshot().current, 2);
        drop(charge);
        assert!(weak.upgrade().is_none());
        assert_eq!(leaf.snapshot().current, 0);
    }

    #[test]
    fn concurrent_charge_refund_and_offline_use_the_same_original_stock() {
        let (root, leaf, socket) = child();
        let barrier = Arc::new(Barrier::new(5));
        let mut workers = std::vec::Vec::new();
        for _ in 0..4 {
            let socket = socket.clone();
            let barrier = barrier.clone();
            workers.push(thread::spawn(move || {
                barrier.wait();
                for _ in 0..300 {
                    let a = socket.try_charge(17).unwrap();
                    let b = socket.try_charge(PAGE_BYTES + 1).unwrap();
                    drop(a);
                    drop(b);
                }
            }));
        }
        barrier.wait();
        leaf.offline();
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(socket.state.lock_irqsave().bytes, 0);
        assert_eq!(leaf.snapshot().current, 0);
        assert_eq!(root.snapshot().current, 0);
    }
}
