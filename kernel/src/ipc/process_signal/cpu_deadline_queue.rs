//! CPU-clock wait registrations: one prepared node for sorting and wakeup.
//! Tree operations move the original owned allocation and never allocate.
use super::{Key, Registration};
use crate::cgroup::memory::{accounting::KernelByteKind, object::ObjectAccount, owned::OwnedBox};
use crate::mm::allocation::MemoryAllocationError;
use alloc::sync::Arc;
use system_error::SystemError;

pub(super) struct Node {
    pub(super) key: Key,
    pub(super) pending: Option<Arc<Registration>>,
    left: Option<OwnedBox<Node>>,
    right: Option<OwnedBox<Node>>,
    height: i32,
}

impl Node {
    pub(super) fn try_new(
        creator: &Arc<ObjectAccount>,
    ) -> Result<OwnedBox<Self>, MemoryAllocationError> {
        Ok(
            OwnedBox::prepare(creator, KernelByteKind::SlabUnreclaimable)?
                .try_init(Self {
                    key: (0, 0),
                    pending: None,
                    left: None,
                    right: None,
                    height: 1,
                })
                .map_err(|_| SystemError::ENOMEM)?,
        )
    }

    fn height(node: &Option<OwnedBox<Self>>) -> i32 {
        node.as_ref().map_or(0, |node| node.height)
    }

    fn refresh(&mut self) {
        self.height = 1 + Self::height(&self.left).max(Self::height(&self.right));
    }

    fn balance(&self) -> i32 {
        Self::height(&self.left) - Self::height(&self.right)
    }
}

pub(super) struct CpuDeadlineQueue {
    root: Option<OwnedBox<Node>>,
}

impl CpuDeadlineQueue {
    pub(super) const fn new() -> Self {
        Self { root: None }
    }

    #[cfg(test)]
    pub(super) fn validate(&self) -> usize {
        fn visit(
            node: &Option<OwnedBox<Node>>,
            low: Option<Key>,
            high: Option<Key>,
        ) -> (usize, i32) {
            let Some(node) = node.as_ref() else {
                return (0, 0);
            };
            assert!(low.is_none_or(|key| key < node.key));
            assert!(high.is_none_or(|key| node.key < key));
            let (left_count, left_height) = visit(&node.left, low, Some(node.key));
            let (right_count, right_height) = visit(&node.right, Some(node.key), high);
            assert!((left_height - right_height).abs() <= 1);
            assert_eq!(node.height, 1 + left_height.max(right_height));
            (left_count + right_count + 1, node.height)
        }
        visit(&self.root, None, None).0
    }

    pub(super) fn first_key(&self) -> Option<Key> {
        let mut node = self.root.as_deref()?;
        while let Some(left) = node.left.as_deref() {
            node = left;
        }
        Some(node.key)
    }

    pub(super) fn contains_key(&self, key: &Key) -> bool {
        let mut node = self.root.as_deref();
        while let Some(current) = node {
            match key.cmp(&current.key) {
                core::cmp::Ordering::Less => node = current.left.as_deref(),
                core::cmp::Ordering::Greater => node = current.right.as_deref(),
                core::cmp::Ordering::Equal => return true,
            }
        }
        false
    }

    pub(super) fn insert(&mut self, node: OwnedBox<Node>) {
        assert!(node.left.is_none() && node.right.is_none() && node.height == 1);
        self.root = Some(Self::insert_node(self.root.take(), node));
    }

    pub(super) fn remove(&mut self, key: &Key) -> Option<OwnedBox<Node>> {
        let (root, removed) = Self::remove_node(self.root.take(), *key);
        self.root = root;
        removed
    }

    pub(super) fn pop_first(&mut self) -> Option<OwnedBox<Node>> {
        let root = self.root.take()?;
        let (root, first) = Self::remove_min(root);
        self.root = root;
        Some(first)
    }

    fn insert_node(root: Option<OwnedBox<Node>>, node: OwnedBox<Node>) -> OwnedBox<Node> {
        let Some(mut root) = root else { return node };
        match node.key.cmp(&root.key) {
            core::cmp::Ordering::Less => {
                root.left = Some(Self::insert_node(root.left.take(), node))
            }
            core::cmp::Ordering::Greater => {
                root.right = Some(Self::insert_node(root.right.take(), node))
            }
            core::cmp::Ordering::Equal => panic!("deadline node already queued"),
        }
        Self::rebalance(root)
    }

    fn remove_node(
        root: Option<OwnedBox<Node>>,
        key: Key,
    ) -> (Option<OwnedBox<Node>>, Option<OwnedBox<Node>>) {
        let Some(mut root) = root else {
            return (None, None);
        };
        match key.cmp(&root.key) {
            core::cmp::Ordering::Less => {
                let (left, removed) = Self::remove_node(root.left.take(), key);
                root.left = left;
                (Some(Self::rebalance(root)), removed)
            }
            core::cmp::Ordering::Greater => {
                let (right, removed) = Self::remove_node(root.right.take(), key);
                root.right = right;
                (Some(Self::rebalance(root)), removed)
            }
            core::cmp::Ordering::Equal => {
                let replacement = Self::merge(root.left.take(), root.right.take());
                root.height = 1;
                (replacement, Some(root))
            }
        }
    }

    fn merge(
        left: Option<OwnedBox<Node>>,
        right: Option<OwnedBox<Node>>,
    ) -> Option<OwnedBox<Node>> {
        match (left, right) {
            (None, right) => right,
            (left, None) => left,
            (left, Some(right)) => {
                let (right, mut first) = Self::remove_min(right);
                first.left = left;
                first.right = right;
                Some(Self::rebalance(first))
            }
        }
    }

    fn remove_min(mut root: OwnedBox<Node>) -> (Option<OwnedBox<Node>>, OwnedBox<Node>) {
        let Some(left) = root.left.take() else {
            let right = root.right.take();
            root.height = 1;
            return (right, root);
        };
        let (left, first) = Self::remove_min(left);
        root.left = left;
        (Some(Self::rebalance(root)), first)
    }

    fn rebalance(mut root: OwnedBox<Node>) -> OwnedBox<Node> {
        root.refresh();
        if root.balance() > 1 {
            if root.left.as_ref().unwrap().balance() < 0 {
                root.left = root.left.take().map(Self::rotate_left);
            }
            return Self::rotate_right(root);
        }
        if root.balance() < -1 {
            if root.right.as_ref().unwrap().balance() > 0 {
                root.right = root.right.take().map(Self::rotate_right);
            }
            return Self::rotate_left(root);
        }
        root
    }

    fn rotate_left(mut root: OwnedBox<Node>) -> OwnedBox<Node> {
        let mut next = root.right.take().unwrap();
        root.right = next.left.take();
        root.refresh();
        next.left = Some(root);
        next.refresh();
        next
    }

    fn rotate_right(mut root: OwnedBox<Node>) -> OwnedBox<Node> {
        let mut next = root.left.take().unwrap();
        root.left = next.right.take();
        root.refresh();
        next.right = Some(root);
        next.refresh();
        next
    }
}
