//! Page-cache-specific prepared ordered indices. Each live entry prepares
//! one node for membership, dirty and writeback indexing before publication.
//! Index mutations only transfer the original node; they never allocate.
use crate::cgroup::memory::{accounting::KernelByteKind, object::ObjectAccount, owned::OwnedBox};
use crate::mm::allocation::MemoryAllocationError;
use alloc::sync::Arc;
use core::ops::{Bound, RangeBounds};
use system_error::SystemError;

#[derive(Debug)]
pub(super) struct Node {
    pub(super) key: usize,
    left: Option<OwnedBox<Node>>,
    right: Option<OwnedBox<Node>>,
    height: i32,
}

impl Node {
    pub(super) fn try_new(
        creator: &Arc<ObjectAccount>,
    ) -> Result<OwnedBox<Self>, MemoryAllocationError> {
        OwnedBox::prepare(creator, KernelByteKind::SlabUnreclaimable)
            .map_err(MemoryAllocationError::Charge)?
            .try_init(Self {
                key: 0,
                left: None,
                right: None,
                height: 1,
            })
            .map_err(|_| MemoryAllocationError::System(SystemError::ENOMEM))
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

#[derive(Debug)]
pub(super) struct CacheIndex {
    root: Option<OwnedBox<Node>>,
}

impl CacheIndex {
    pub(super) const fn new() -> Self {
        Self { root: None }
    }

    #[cfg(test)]
    pub(super) fn validate(&self) -> usize {
        fn visit(
            node: &Option<OwnedBox<Node>>,
            low: Option<usize>,
            high: Option<usize>,
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

    pub(super) fn first_key(&self) -> Option<usize> {
        let mut node = self.root.as_deref()?;
        while let Some(left) = node.left.as_deref() {
            node = left;
        }
        Some(node.key)
    }

    pub(super) fn contains_key(&self, key: &usize) -> bool {
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

    pub(super) fn remove(&mut self, key: &usize) -> Option<OwnedBox<Node>> {
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
            core::cmp::Ordering::Equal => panic!("page-cache index already linked"),
        }
        Self::rebalance(root)
    }

    fn remove_node(
        root: Option<OwnedBox<Node>>,
        key: usize,
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

impl CacheIndex {
    pub(super) fn is_empty(&self) -> bool {
        self.root.is_none()
    }
    pub(super) fn contains(&self, key: &usize) -> bool {
        self.contains_key(key)
    }
    pub(super) fn iter(&self) -> IndexIter<'_> {
        self.range(..)
    }
    /// Find the final key of a bounded range by a single AVL descent. The
    /// writeback frontier does not need a second iterator stack or an O(n)
    /// forward scan just to inspect its rightmost tagged entry.
    pub(super) fn last_in_range(&self, bounds: impl RangeBounds<usize>) -> Option<usize> {
        let mut node = self.root.as_deref();
        let mut last = None;
        while let Some(current) = node {
            let above = match bounds.end_bound() {
                Bound::Included(key) => current.key > *key,
                Bound::Excluded(key) => current.key >= *key,
                Bound::Unbounded => false,
            };
            if above {
                node = current.left.as_deref();
            } else {
                last = Some(current.key);
                node = current.right.as_deref();
            }
        }
        last.filter(|last| match bounds.start_bound() {
            Bound::Included(key) => last >= key,
            Bound::Excluded(key) => last > key,
            Bound::Unbounded => true,
        })
    }
    pub(super) fn range(&self, bounds: impl RangeBounds<usize>) -> IndexIter<'_> {
        let lower = bounds.start_bound().cloned();
        let upper = bounds.end_bound().cloned();
        let mut iter = IndexIter {
            stack: [None; HEIGHT_BOUND],
            depth: 0,
            upper,
        };
        let mut node = self.root.as_deref();
        while let Some(current) = node {
            let below = match lower {
                Bound::Included(key) => current.key < key,
                Bound::Excluded(key) => current.key <= key,
                Bound::Unbounded => false,
            };
            if below {
                node = current.right.as_deref();
            } else {
                iter.push(current);
                node = current.left.as_deref();
            }
        }
        iter
    }
}

// An AVL tree with usize-addressable nodes has height < 2*usize::BITS.
// This is a mathematical address-space bound, not a cache size restriction.
const HEIGHT_BOUND: usize = 2 * usize::BITS as usize + 1;
pub(super) struct IndexIter<'a> {
    stack: [Option<&'a Node>; HEIGHT_BOUND],
    depth: usize,
    upper: Bound<usize>,
}
impl<'a> IndexIter<'a> {
    fn push(&mut self, node: &'a Node) {
        assert!(self.depth < HEIGHT_BOUND, "invalid page-cache AVL height");
        self.stack[self.depth] = Some(node);
        self.depth += 1;
    }
}
impl<'a> Iterator for IndexIter<'a> {
    type Item = &'a usize;
    fn next(&mut self) -> Option<Self::Item> {
        if self.depth == 0 {
            return None;
        }
        self.depth -= 1;
        let current = self.stack[self.depth].take().unwrap();
        let above = match self.upper {
            Bound::Included(key) => current.key > key,
            Bound::Excluded(key) => current.key >= key,
            Bound::Unbounded => false,
        };
        if above {
            self.depth = 0;
            return None;
        }
        let mut node = current.right.as_deref();
        while let Some(current) = node {
            self.push(current);
            node = current.left.as_deref();
        }
        Some(&current.key)
    }
}
