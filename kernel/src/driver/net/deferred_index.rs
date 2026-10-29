use super::local_queue::MAX_LOCAL_OUTPUT_FRAMES;
use crate::libs::paged_vec::PagedVec;
use system_error::SystemError;

const MAX_INDEX_NODES: usize = MAX_LOCAL_OUTPUT_FRAMES * 2 - 1;

pub(super) type NodeId = usize;
pub(super) type RouteIndexKey = [u8; 21];

#[derive(Clone, Copy, Debug)]
enum Node {
    Free { next: Option<NodeId> },
    Leaf { key: RouteIndexKey, slot: usize },
    Branch { bit: u8, children: [NodeId; 2] },
}

const _: () = assert!(PagedVec::<Node, MAX_INDEX_NODES>::layout_supported());

/// A fallibly allocated Patricia index for complete (family, ifindex, address) keys.
///
/// Branch bits strictly increase from root to leaf, bounding every lookup by
/// the key width without depending on secret hash entropy. Node IDs remain
/// stable across removals so heap swaps can update leaf slots in O(1).
#[derive(Debug, Default)]
pub(super) struct DeferredRouteIndex {
    nodes: PagedVec<Node, MAX_INDEX_NODES>,
    root: Option<NodeId>,
    free_head: Option<NodeId>,
    free_count: usize,
    leaves: usize,
}

impl DeferredRouteIndex {
    pub(super) fn len(&self) -> usize {
        self.leaves
    }

    pub(super) fn get(&self, key: RouteIndexKey) -> Option<(NodeId, usize)> {
        let leaf = self.find_leaf(key)?;
        match self.nodes[leaf] {
            Node::Leaf {
                key: leaf_key,
                slot,
            } if leaf_key == key => Some((leaf, slot)),
            _ => None,
        }
    }

    /// Prepare the worst-case live binary tree before packet emission. Free
    /// node IDs count as reusable slots, not as additional live ownership.
    pub(super) fn prepare_capacity(&mut self, leaves: usize) -> Result<(), SystemError> {
        if leaves > MAX_LOCAL_OUTPUT_FRAMES {
            return Err(SystemError::ENOBUFS);
        }
        let required = leaves.saturating_mul(2).saturating_sub(1);
        let live = self.nodes.len() - self.free_count;
        let additional_live = required.saturating_sub(live);
        self.nodes
            .try_reserve(additional_live.saturating_sub(self.free_count))
    }

    pub(super) fn has_insert_capacity(&self) -> bool {
        let required = if self.root.is_some() { 2 } else { 1 };
        self.leaves < MAX_LOCAL_OUTPUT_FRAMES
            && self.nodes.capacity() - self.nodes.len() + self.free_count >= required
    }

    /// Inserts a key after `prepare_capacity`; this method cannot allocate.
    pub(super) fn insert_prepared(&mut self, key: RouteIndexKey, slot: usize) -> NodeId {
        debug_assert!(self.get(key).is_none());
        assert!(
            self.has_insert_capacity(),
            "Patricia insert without prepared capacity"
        );
        let Some(root) = self.root else {
            let leaf = self.alloc_prepared(Node::Leaf { key, slot });
            self.root = Some(leaf);
            self.leaves = 1;
            return leaf;
        };

        let existing_leaf = self
            .find_leaf(key)
            .expect("a non-empty Patricia index has a leaf");
        let existing_key = match self.nodes[existing_leaf] {
            Node::Leaf { key, .. } => key,
            _ => unreachable!("Patricia traversal ends at a leaf"),
        };
        let differing_byte = key
            .iter()
            .zip(existing_key)
            .position(|(a, b)| *a != b)
            .expect("inserted Patricia keys are distinct");
        let differing_bit = (differing_byte * 8
            + (key[differing_byte] ^ existing_key[differing_byte]).leading_zeros() as usize)
            as u8;

        let mut parent = None;
        let mut current = root;
        while let Node::Branch { bit, children } = self.nodes[current] {
            if bit >= differing_bit {
                break;
            }
            let direction = Self::direction(key, bit);
            parent = Some((current, direction));
            current = children[direction];
        }

        let leaf = self.alloc_prepared(Node::Leaf { key, slot });
        let direction = Self::direction(key, differing_bit);
        let mut children = [current, current];
        children[direction] = leaf;
        let branch = self.alloc_prepared(Node::Branch {
            bit: differing_bit,
            children,
        });
        if let Some((parent, direction)) = parent {
            self.branch_children_mut(parent)[direction] = branch;
        } else {
            self.root = Some(branch);
        }
        self.leaves += 1;
        leaf
    }

    pub(super) fn set_slot(&mut self, leaf: NodeId, key: RouteIndexKey, slot: usize) {
        match &mut self.nodes[leaf] {
            Node::Leaf {
                key: leaf_key,
                slot: leaf_slot,
            } => {
                debug_assert_eq!(*leaf_key, key);
                *leaf_slot = slot;
            }
            _ => unreachable!("a deferred bucket owns a Patricia leaf"),
        }
    }

    pub(super) fn remove(&mut self, key: RouteIndexKey) -> Option<usize> {
        let root = self.root?;
        if let Node::Leaf {
            key: leaf_key,
            slot,
        } = self.nodes[root]
        {
            if leaf_key != key {
                return None;
            }
            self.root = None;
            self.free_node(root);
            self.leaves = 0;
            return Some(slot);
        }

        let mut grandparent = None;
        let mut parent = None;
        let mut current = root;
        while let Node::Branch { bit, children } = self.nodes[current] {
            let direction = Self::direction(key, bit);
            grandparent = parent;
            parent = Some((current, direction));
            current = children[direction];
        }
        let slot = match self.nodes[current] {
            Node::Leaf {
                key: leaf_key,
                slot,
            } if leaf_key == key => slot,
            _ => return None,
        };
        let (parent, direction) = parent.expect("a non-root leaf has a parent");
        let sibling = self.branch_children(parent)[1 - direction];
        if let Some((grandparent, parent_direction)) = grandparent {
            self.branch_children_mut(grandparent)[parent_direction] = sibling;
        } else {
            self.root = Some(sibling);
        }
        self.free_node(current);
        self.free_node(parent);
        self.leaves -= 1;
        Some(slot)
    }

    fn find_leaf(&self, key: RouteIndexKey) -> Option<NodeId> {
        let mut current = self.root?;
        loop {
            match self.nodes[current] {
                Node::Leaf { .. } => return Some(current),
                Node::Branch { bit, children } => {
                    current = children[Self::direction(key, bit)];
                }
                Node::Free { .. } => unreachable!("free nodes are unreachable from the root"),
            }
        }
    }

    fn direction(key: RouteIndexKey, bit: u8) -> usize {
        ((key[bit as usize / 8] >> (7 - bit % 8)) & 1) as usize
    }

    fn branch_children(&self, node: NodeId) -> [NodeId; 2] {
        match self.nodes[node] {
            Node::Branch { children, .. } => children,
            _ => unreachable!("expected a Patricia branch"),
        }
    }

    fn branch_children_mut(&mut self, node: NodeId) -> &mut [NodeId; 2] {
        match &mut self.nodes[node] {
            Node::Branch { children, .. } => children,
            _ => unreachable!("expected a Patricia branch"),
        }
    }

    fn alloc_prepared(&mut self, node: Node) -> NodeId {
        if let Some(id) = self.free_head {
            let next = match self.nodes[id] {
                Node::Free { next } => next,
                _ => unreachable!("free-list head is free"),
            };
            self.free_head = next;
            self.free_count -= 1;
            self.nodes[id] = node;
            id
        } else {
            let id = self.nodes.len();
            self.nodes.push(node);
            id
        }
    }

    fn free_node(&mut self, node: NodeId) {
        self.nodes[node] = Node::Free {
            next: self.free_head,
        };
        self.free_head = Some(node);
        self.free_count += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_bit_survives_insert_remove_and_slot_updates() {
        let mut index = DeferredRouteIndex::default();
        // Include the all-zero key and every differing bit, especially the
        // high IPv6 bytes which the old 64-bit key could not represent.
        let mut keys = alloc::vec![[0; 21]];
        for bit in 0..168 {
            let mut key = [0; 21];
            key[bit / 8] = 1 << (7 - bit % 8);
            keys.push(key);
        }
        for (slot, key) in keys.iter().copied().enumerate() {
            index.prepare_capacity(index.len() + 1).unwrap();
            index.insert_prepared(key, slot);
        }
        assert_eq!(index.len(), keys.len());
        for (slot, key) in keys.iter().copied().enumerate() {
            let (leaf, actual) = index.get(key).unwrap();
            assert_eq!(actual, slot);
            index.set_slot(leaf, key, slot + 1000);
        }
        // Alternating removals exercise root replacement and stable leaf IDs.
        for parity in 0..2 {
            for slot in (parity..keys.len()).step_by(2) {
                assert_eq!(index.remove(keys[slot]), Some(slot + 1000));
                assert!(index.get(keys[slot]).is_none());
            }
        }
        assert_eq!(index.len(), 0);
        for (slot, key) in keys.iter().copied().enumerate().rev() {
            index.prepare_capacity(index.len() + 1).unwrap();
            index.insert_prepared(key, slot);
        }
        for (slot, key) in keys.iter().copied().enumerate() {
            assert_eq!(index.get(key).unwrap().1, slot);
        }
    }

    #[test]
    fn worst_case_capacity_is_prepared_and_node_ids_are_reused() {
        let mut index = DeferredRouteIndex::default();
        index.prepare_capacity(MAX_LOCAL_OUTPUT_FRAMES).unwrap();
        let key = |slot: usize| {
            let mut key = [0; 21];
            key[..core::mem::size_of::<usize>()].copy_from_slice(&slot.to_be_bytes());
            key
        };
        for slot in 0..MAX_LOCAL_OUTPUT_FRAMES {
            assert!(index.has_insert_capacity());
            index.insert_prepared(key(slot), slot);
        }
        assert_eq!(index.nodes.len(), MAX_INDEX_NODES);
        assert!(!index.has_insert_capacity());
        assert_eq!(
            index.prepare_capacity(MAX_LOCAL_OUTPUT_FRAMES + 1),
            Err(SystemError::ENOBUFS)
        );
        for slot in (0..MAX_LOCAL_OUTPUT_FRAMES).rev() {
            assert_eq!(index.remove(key(slot)), Some(slot));
        }
        let allocated = index.nodes.len();
        for slot in 0..MAX_LOCAL_OUTPUT_FRAMES {
            index.insert_prepared(key(slot), slot + 7);
        }
        assert_eq!(index.nodes.len(), allocated);
        for slot in 0..MAX_LOCAL_OUTPUT_FRAMES {
            assert_eq!(index.get(key(slot)).unwrap().1, slot + 7);
        }
    }
}
