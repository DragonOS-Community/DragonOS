//! Pids accounting survives directory removal until the last task is reaped.
//!
//! Directory topology remains weak-parented. Only this small counter identity
//! pins its immutable accounting ancestors, as a pids CSS does in Linux.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};
use system_error::SystemError;

#[derive(Debug)]
pub(super) struct PidsAccount {
    parent: Option<Arc<Self>>,
    current: AtomicUsize,
}

impl PidsAccount {
    pub(super) fn try_new(parent: Option<Arc<Self>>) -> Result<Arc<Self>, SystemError> {
        Arc::try_new(Self {
            parent,
            current: AtomicUsize::new(0),
        })
        .map_err(|_| SystemError::ENOMEM)
    }

    pub(super) fn charge(&self, count: usize) {
        let mut account = Some(self);
        while let Some(node) = account {
            node.current.fetch_add(count, Ordering::Release);
            account = node.parent.as_deref();
        }
    }

    pub(super) fn uncharge(&self, count: usize) {
        let mut account = Some(self);
        while let Some(node) = account {
            let old = node.current.fetch_sub(count, Ordering::Release);
            debug_assert!(old >= count, "pids accounting underflow");
            account = node.parent.as_deref();
        }
    }

    pub(super) fn current(&self) -> usize {
        self.current.load(Ordering::Acquire)
    }
}

impl Drop for PidsAccount {
    fn drop(&mut self) {
        // A reaped zombie may own the sole surviving chain of removed groups.
        let mut parent = self.parent.take();
        while let Some(account) = parent {
            let Some(mut account) = Arc::into_inner(account) else {
                break;
            };
            parent = account.parent.take();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deleted_ancestors_remain_chargeable_until_late_reap() {
        let root = PidsAccount::try_new(None).unwrap();
        let parent = PidsAccount::try_new(Some(root.clone())).unwrap();
        let leaf = PidsAccount::try_new(Some(parent.clone())).unwrap();
        let old_parent = Arc::downgrade(&parent);
        leaf.charge(2);
        drop(parent);
        assert_eq!(root.current(), 2);
        assert_eq!(old_parent.upgrade().unwrap().current(), 2);
        leaf.uncharge(1);
        leaf.uncharge(1);
        assert_eq!(root.current(), 0);
        drop(leaf);
        assert!(old_parent.upgrade().is_none());
    }

    #[test]
    fn replacement_identity_does_not_inherit_zombie_charge() {
        let root = PidsAccount::try_new(None).unwrap();
        let old = PidsAccount::try_new(Some(root.clone())).unwrap();
        old.charge(1);
        let replacement = PidsAccount::try_new(Some(root.clone())).unwrap();
        assert_eq!(replacement.current(), 0);
        replacement.charge(1);
        old.uncharge(1);
        assert_eq!(replacement.current(), 1);
        assert_eq!(root.current(), 1);
        replacement.uncharge(1);
    }

    #[test]
    fn migration_shared_ancestor_and_cancelled_birth_balance() {
        let root = PidsAccount::try_new(None).unwrap();
        let parent = PidsAccount::try_new(Some(root.clone())).unwrap();
        let src = PidsAccount::try_new(Some(parent.clone())).unwrap();
        let dst = PidsAccount::try_new(Some(parent.clone())).unwrap();
        src.charge(2);
        src.uncharge(1);
        dst.charge(1);
        assert_eq!(parent.current(), 2);
        dst.charge(1);
        dst.uncharge(1); // cancelled unpublished fork
        src.uncharge(1);
        dst.uncharge(1);
        assert_eq!(parent.current(), 0);
        assert_eq!(root.current(), 0);
    }

    #[test]
    fn final_deep_chain_is_retired_iteratively() {
        let root = PidsAccount::try_new(None).unwrap();
        let mut leaf = root.clone();
        for _ in 0..8192 {
            leaf = PidsAccount::try_new(Some(leaf)).unwrap();
        }
        leaf.charge(1);
        leaf.uncharge(1);
        drop(leaf);
        assert_eq!(Arc::strong_count(&root), 1);
        assert_eq!(root.current(), 0);
    }
}
