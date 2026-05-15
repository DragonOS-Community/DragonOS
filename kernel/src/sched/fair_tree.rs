use alloc::{boxed::Box, sync::Arc};
use core::cmp::{max, Ordering};

use crate::sched::fair::FairSchedEntity;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct EntityKey {
    vruntime: u64,
    id: usize,
}

impl EntityKey {
    fn new(entity: &Arc<FairSchedEntity>) -> Self {
        Self {
            vruntime: entity.vruntime,
            id: Arc::as_ptr(entity) as usize,
        }
    }

    #[inline]
    fn vruntime_before(left: u64, right: u64) -> bool {
        (left.wrapping_sub(right) as i64) < 0
    }
}

impl Ord for EntityKey {
    fn cmp(&self, other: &Self) -> Ordering {
        if self.vruntime == other.vruntime {
            self.id.cmp(&other.id)
        } else if Self::vruntime_before(self.vruntime, other.vruntime) {
            Ordering::Less
        } else {
            Ordering::Greater
        }
    }
}

impl PartialOrd for EntityKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug)]
struct FairTreeNode {
    key: EntityKey,
    entity: Arc<FairSchedEntity>,
    left: Option<Box<FairTreeNode>>,
    right: Option<Box<FairTreeNode>>,
    height: i32,
    min_deadline: u64,
}

impl FairTreeNode {
    fn new(entity: Arc<FairSchedEntity>) -> Box<Self> {
        let key = EntityKey::new(&entity);
        let min_deadline = entity.deadline;
        entity.force_mut().min_deadline = min_deadline;

        Box::new(Self {
            key,
            entity,
            left: None,
            right: None,
            height: 1,
            min_deadline,
        })
    }

    #[inline]
    fn height(node: &Option<Box<Self>>) -> i32 {
        node.as_ref().map_or(0, |node| node.height)
    }

    #[inline]
    fn deadline_gt(left: u64, right: u64) -> bool {
        (left.wrapping_sub(right) as i64) > 0
    }

    fn refresh(&mut self) {
        self.height = 1 + max(Self::height(&self.left), Self::height(&self.right));

        let mut min_deadline = self.entity.deadline;
        if let Some(left) = self.left.as_ref() {
            if Self::deadline_gt(min_deadline, left.min_deadline) {
                min_deadline = left.min_deadline;
            }
        }
        if let Some(right) = self.right.as_ref() {
            if Self::deadline_gt(min_deadline, right.min_deadline) {
                min_deadline = right.min_deadline;
            }
        }

        self.min_deadline = min_deadline;
        self.entity.force_mut().min_deadline = min_deadline;
    }

    fn balance_factor(&self) -> i32 {
        Self::height(&self.left) - Self::height(&self.right)
    }
}

#[derive(Debug, Default)]
pub struct FairTimeline {
    root: Option<Box<FairTreeNode>>,
    len: usize,
}

impl FairTimeline {
    pub fn insert(&mut self, entity: Arc<FairSchedEntity>) {
        let key = EntityKey::new(&entity);
        self.root = Some(Self::insert_node(self.root.take(), key, entity));
        self.len += 1;
    }

    pub fn remove(&mut self, entity: &Arc<FairSchedEntity>) -> Option<Arc<FairSchedEntity>> {
        let key = EntityKey::new(entity);
        let (root, removed) = Self::remove_node(self.root.take(), key);
        self.root = root;
        if removed.is_some() {
            self.len -= 1;
        }
        removed
    }

    pub fn leftmost(&self) -> Option<Arc<FairSchedEntity>> {
        let mut node = self.root.as_ref()?;
        while let Some(left) = node.left.as_ref() {
            node = left;
        }
        Some(node.entity.clone())
    }

    pub fn pick_eevdf(
        &self,
        curr: Option<&Arc<FairSchedEntity>>,
        mut eligible: impl FnMut(&Arc<FairSchedEntity>) -> bool,
    ) -> Option<Arc<FairSchedEntity>> {
        let mut best = curr.filter(|entity| eligible(entity)).cloned();
        let mut best_left: Option<&FairTreeNode> = None;
        let mut node = self.root.as_deref();

        while let Some(se_node) = node {
            if !eligible(&se_node.entity) {
                node = se_node.left.as_deref();
                continue;
            }

            if best
                .as_ref()
                .is_none_or(|best| Self::deadline_gt(best.deadline, se_node.entity.deadline))
            {
                best = Some(se_node.entity.clone());
            }

            if let Some(left) = se_node.left.as_deref() {
                if best_left.is_none_or(|best_left| {
                    Self::deadline_gt(best_left.min_deadline, left.min_deadline)
                }) {
                    best_left = Some(left);
                }

                if left.min_deadline == se_node.min_deadline {
                    break;
                }
            }

            if se_node.entity.deadline == se_node.min_deadline {
                break;
            }

            node = se_node.right.as_deref();
        }

        match (best, best_left) {
            (Some(best), Some(left)) if Self::deadline_gt(best.deadline, left.min_deadline) => {
                Self::pick_min_deadline(left)
            }
            (None, Some(left)) => Self::pick_min_deadline(left),
            (best, _) => best,
        }
    }

    #[inline]
    fn deadline_gt(left: u64, right: u64) -> bool {
        FairTreeNode::deadline_gt(left, right)
    }

    fn pick_min_deadline(node: &FairTreeNode) -> Option<Arc<FairSchedEntity>> {
        if node.entity.deadline == node.min_deadline {
            return Some(node.entity.clone());
        }

        if let Some(left) = node.left.as_deref() {
            if left.min_deadline == node.min_deadline {
                return Self::pick_min_deadline(left);
            }
        }

        node.right.as_deref().and_then(Self::pick_min_deadline)
    }

    fn insert_node(
        node: Option<Box<FairTreeNode>>,
        key: EntityKey,
        entity: Arc<FairSchedEntity>,
    ) -> Box<FairTreeNode> {
        let Some(mut node) = node else {
            return FairTreeNode::new(entity);
        };

        if key < node.key {
            node.left = Some(Self::insert_node(node.left.take(), key, entity));
        } else {
            node.right = Some(Self::insert_node(node.right.take(), key, entity));
        }

        Self::rebalance(node)
    }

    fn remove_node(
        node: Option<Box<FairTreeNode>>,
        key: EntityKey,
    ) -> (Option<Box<FairTreeNode>>, Option<Arc<FairSchedEntity>>) {
        let Some(mut node) = node else {
            return (None, None);
        };

        match key.cmp(&node.key) {
            Ordering::Less => {
                let (left, removed) = Self::remove_node(node.left.take(), key);
                node.left = left;
                (Some(Self::rebalance(node)), removed)
            }
            Ordering::Greater => {
                let (right, removed) = Self::remove_node(node.right.take(), key);
                node.right = right;
                (Some(Self::rebalance(node)), removed)
            }
            Ordering::Equal => {
                let removed = Some(node.entity.clone());
                (Self::merge(node.left.take(), node.right.take()), removed)
            }
        }
    }

    fn merge(
        left: Option<Box<FairTreeNode>>,
        right: Option<Box<FairTreeNode>>,
    ) -> Option<Box<FairTreeNode>> {
        match (left, right) {
            (None, right) => right,
            (left, None) => left,
            (left, Some(right)) => {
                let (right, mut min) = Self::remove_min(right);
                min.left = left;
                min.right = right;
                Some(Self::rebalance(min))
            }
        }
    }

    fn remove_min(mut node: Box<FairTreeNode>) -> (Option<Box<FairTreeNode>>, Box<FairTreeNode>) {
        let Some(left) = node.left.take() else {
            return (node.right.take(), node);
        };

        let (left, min) = Self::remove_min(left);
        node.left = left;
        (Some(Self::rebalance(node)), min)
    }

    fn rebalance(mut node: Box<FairTreeNode>) -> Box<FairTreeNode> {
        node.refresh();
        let balance = node.balance_factor();

        if balance > 1 {
            if node.left.as_ref().map_or(0, |left| left.balance_factor()) < 0 {
                let left = node.left.take().map(Self::rotate_left);
                node.left = left;
            }
            return Self::rotate_right(node);
        }

        if balance < -1 {
            if node
                .right
                .as_ref()
                .map_or(0, |right| right.balance_factor())
                > 0
            {
                let right = node.right.take().map(Self::rotate_right);
                node.right = right;
            }
            return Self::rotate_left(node);
        }

        node
    }

    fn rotate_left(mut root: Box<FairTreeNode>) -> Box<FairTreeNode> {
        let mut new_root = root
            .right
            .take()
            .expect("rotate_left requires a right child");
        root.right = new_root.left.take();
        root.refresh();

        new_root.left = Some(root);
        new_root.refresh();
        new_root
    }

    fn rotate_right(mut root: Box<FairTreeNode>) -> Box<FairTreeNode> {
        let mut new_root = root
            .left
            .take()
            .expect("rotate_right requires a left child");
        root.left = new_root.right.take();
        root.refresh();

        new_root.right = Some(root);
        new_root.refresh();
        new_root
    }
}
