use alloc::{boxed::Box, sync::Arc};
use core::{
    alloc::AllocError,
    cmp::{max, Ordering},
};

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
pub(crate) struct FairTreeNode {
    key: EntityKey,
    entity: Option<Arc<FairSchedEntity>>,
    left: Option<Box<FairTreeNode>>,
    right: Option<Box<FairTreeNode>>,
    height: i32,
    min_deadline: u64,
}

impl FairTreeNode {
    pub(crate) fn try_new() -> Result<Box<Self>, AllocError> {
        Box::try_new(Self {
            key: EntityKey { vruntime: 0, id: 0 },
            entity: None,
            left: None,
            right: None,
            height: 1,
            min_deadline: 0,
        })
    }

    fn attach(&mut self, entity: Arc<FairSchedEntity>) {
        assert!(self.entity.is_none() && self.left.is_none() && self.right.is_none());
        let key = EntityKey::new(&entity);
        let min_deadline = entity.deadline;
        entity.force_mut().min_deadline = min_deadline;
        self.key = key;
        self.entity = Some(entity);
        self.height = 1;
        self.min_deadline = min_deadline;
    }

    fn entity(&self) -> &Arc<FairSchedEntity> {
        self.entity
            .as_ref()
            .expect("queued Fair node has an entity")
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

        let mut min_deadline = self.entity().deadline;
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
        self.entity().force_mut().min_deadline = min_deadline;
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
        let mut prepared = entity
            .force_mut()
            .timeline_node
            .take()
            .expect("Fair entity already owns a queued timeline node");
        prepared.attach(entity);
        self.root = Some(Self::insert_node(self.root.take(), prepared));
        self.len += 1;
    }

    pub fn remove(&mut self, entity: &Arc<FairSchedEntity>) -> Option<Arc<FairSchedEntity>> {
        let key = EntityKey::new(entity);
        let (root, removed) = Self::remove_node(self.root.take(), key);
        self.root = root;
        let mut removed = removed?;
        self.len -= 1;
        let owner = removed
            .entity
            .take()
            .expect("removed Fair node has an entity");
        assert!(removed.left.is_none() && removed.right.is_none());
        let spare = &mut owner.force_mut().timeline_node;
        assert!(
            spare.is_none(),
            "queued Fair entity must not own a spare node"
        );
        removed.height = 1;
        removed.min_deadline = 0;
        *spare = Some(removed);
        Some(owner)
    }

    pub fn leftmost(&self) -> Option<Arc<FairSchedEntity>> {
        let mut node = self.root.as_ref()?;
        while let Some(left) = node.left.as_ref() {
            node = left;
        }
        Some(node.entity().clone())
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
            if !eligible(se_node.entity()) {
                node = se_node.left.as_deref();
                continue;
            }

            if best
                .as_ref()
                .is_none_or(|best| Self::deadline_gt(best.deadline, se_node.entity().deadline))
            {
                best = Some(se_node.entity().clone());
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

            if se_node.entity().deadline == se_node.min_deadline {
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
        if node.entity().deadline == node.min_deadline {
            return Some(node.entity().clone());
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
        prepared: Box<FairTreeNode>,
    ) -> Box<FairTreeNode> {
        let Some(mut node) = node else {
            return prepared;
        };

        if prepared.key < node.key {
            node.left = Some(Self::insert_node(node.left.take(), prepared));
        } else {
            node.right = Some(Self::insert_node(node.right.take(), prepared));
        }

        Self::rebalance(node)
    }

    fn remove_node(
        node: Option<Box<FairTreeNode>>,
        key: EntityKey,
    ) -> (Option<Box<FairTreeNode>>, Option<Box<FairTreeNode>>) {
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
                let root = Self::merge(node.left.take(), node.right.take());
                (root, Some(node))
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    fn entity(vruntime: u64, deadline: u64) -> Arc<FairSchedEntity> {
        let entity = FairSchedEntity::try_new().unwrap();
        entity.force_mut().vruntime = vruntime;
        entity.force_mut().deadline = deadline;
        entity
    }

    fn identity(entity: &Arc<FairSchedEntity>) -> usize {
        entity.timeline_node.as_deref().unwrap() as *const FairTreeNode as usize
    }

    fn audit(node: &Option<Box<FairTreeNode>>) -> (i32, usize, Option<u64>) {
        fn order(
            node: &Option<Box<FairTreeNode>>,
            low: Option<EntityKey>,
            high: Option<EntityKey>,
        ) {
            if let Some(node) = node {
                assert!(low.is_none_or(|low| low < node.key));
                assert!(high.is_none_or(|high| node.key < high));
                order(&node.left, low, Some(node.key));
                order(&node.right, Some(node.key), high);
            }
        }
        order(node, None, None);
        audit_balance(node)
    }

    fn audit_balance(node: &Option<Box<FairTreeNode>>) -> (i32, usize, Option<u64>) {
        let Some(node) = node else {
            return (0, 0, None);
        };
        let (left_height, left_count, left_min) = audit_balance(&node.left);
        let (right_height, right_count, right_min) = audit_balance(&node.right);
        assert!((left_height - right_height).abs() <= 1);
        assert_eq!(node.height, 1 + left_height.max(right_height));
        if let Some(left) = &node.left {
            assert!(left.key < node.key);
        }
        if let Some(right) = &node.right {
            assert!(right.key > node.key);
        }
        let mut minimum = node.entity().deadline;
        for candidate in [left_min, right_min].into_iter().flatten() {
            if FairTimeline::deadline_gt(minimum, candidate) {
                minimum = candidate;
            }
        }
        assert_eq!(node.min_deadline, minimum);
        assert_eq!(node.entity().min_deadline, minimum);
        assert!(node.entity().timeline_node.is_none());
        (node.height, 1 + left_count + right_count, Some(minimum))
    }

    #[test]
    fn repeated_current_timeline_moves_reuse_the_original_node() {
        let entity = entity(7, 11);
        let original = identity(&entity);
        let mut timeline = FairTimeline::default();
        for step in 0..128 {
            entity.force_mut().vruntime = step;
            entity.force_mut().deadline = step + 19;
            timeline.insert(entity.clone());
            assert!(entity.timeline_node.is_none());
            assert!(Arc::ptr_eq(&timeline.leftmost().unwrap(), &entity));
            assert!(Arc::ptr_eq(&timeline.remove(&entity).unwrap(), &entity));
            assert_eq!(identity(&entity), original);
            assert_eq!(timeline.len, 0);
            assert!(timeline.root.is_none());
        }
    }

    #[test]
    fn two_child_removal_returns_own_node_not_the_successor_node() {
        let entities: Vec<_> = [4, 2, 6, 1, 3, 5, 7]
            .into_iter()
            .map(|value| entity(value, 20 - value))
            .collect();
        let identities: Vec<_> = entities.iter().map(identity).collect();
        let mut timeline = FairTimeline::default();
        for entity in &entities {
            timeline.insert(entity.clone());
        }
        assert_eq!(audit(&timeline.root).1, entities.len());
        for index in [0, 2, 1, 4, 5, 3, 6] {
            let removed = timeline.remove(&entities[index]).unwrap();
            assert!(Arc::ptr_eq(&removed, &entities[index]));
            assert_eq!(identity(&removed), identities[index]);
            assert_eq!(audit(&timeline.root).1, timeline.len);
        }
    }

    #[test]
    fn random_insert_remove_preserves_avl_and_deadline_augmentation() {
        let entities: Vec<_> = (0..96).map(|value| entity(value, 200 - value)).collect();
        let identities: Vec<_> = entities.iter().map(identity).collect();
        let mut present = [false; 96];
        let mut timeline = FairTimeline::default();
        let mut random = 19_u64;
        for _ in 0..4096 {
            random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
            let index = (random >> 32) as usize % entities.len();
            if present[index] {
                timeline.remove(&entities[index]).unwrap();
                assert_eq!(identity(&entities[index]), identities[index]);
            } else {
                timeline.insert(entities[index].clone());
            }
            present[index] = !present[index];
            assert_eq!(
                audit(&timeline.root).1,
                present.iter().filter(|value| **value).count()
            );
        }
        for (index, entity) in entities.iter().enumerate() {
            if present[index] {
                timeline.remove(entity).unwrap();
            }
            assert_eq!(identity(entity), identities[index]);
        }
    }

    #[test]
    fn tied_vruntime_uses_identity_and_wrapping_keys_keep_linux_order() {
        let entities = [
            entity(u64::MAX - 3, u64::MAX - 1),
            entity(1, 3),
            entity(1, 4),
        ];
        let mut timeline = FairTimeline::default();
        for entity in &entities {
            timeline.insert(entity.clone());
        }
        assert!(Arc::ptr_eq(&timeline.leftmost().unwrap(), &entities[0]));
        assert!(Arc::ptr_eq(
            &timeline.pick_eevdf(None, |_| true).unwrap(),
            &entities[0]
        ));
        timeline.remove(&entities[0]).unwrap();
        let first = if Arc::as_ptr(&entities[1]) < Arc::as_ptr(&entities[2]) {
            1
        } else {
            2
        };
        assert!(Arc::ptr_eq(&timeline.leftmost().unwrap(), &entities[first]));
        for entity in &entities[1..] {
            timeline.remove(entity).unwrap();
        }
    }

    #[test]
    fn eevdf_preserves_eligible_deadline_and_current_selection() {
        let a = entity(1, 8);
        let b = entity(2, 4);
        let ineligible = entity(3, 1);
        let current = entity(0, 3);
        let mut timeline = FairTimeline::default();
        for entity in [&a, &b, &ineligible] {
            timeline.insert(entity.clone());
        }
        assert!(Arc::ptr_eq(
            &timeline
                .pick_eevdf(None, |entity| entity.vruntime <= 2)
                .unwrap(),
            &b
        ));
        assert!(Arc::ptr_eq(
            &timeline
                .pick_eevdf(Some(&current), |entity| entity.vruntime <= 2)
                .unwrap(),
            &current
        ));
        for entity in [&a, &b, &ineligible] {
            timeline.remove(entity).unwrap();
        }
    }

    #[test]
    fn idle_spare_has_no_strong_self_cycle() {
        let entity = entity(1, 1);
        let weak = Arc::downgrade(&entity);
        let mut timeline = FairTimeline::default();
        timeline.insert(entity.clone());
        drop(timeline.remove(&entity));
        assert_eq!(Arc::strong_count(&entity), 1);
        drop(entity);
        assert!(weak.upgrade().is_none());
    }
}
