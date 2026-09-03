use alloc::vec::Vec;
use system_error::SystemError;

pub(super) type NodeId = usize;

#[derive(Clone, Copy, Debug)]
enum Node {
    Free { next: Option<NodeId> },
    Leaf { key: u64, slot: usize },
    Branch { bit: u8, children: [NodeId; 2] },
}

/// A fallibly allocated Patricia index for attacker-controlled 64-bit keys.
///
/// Branch bits strictly increase from root to leaf, bounding every lookup by
/// the key width without depending on secret hash entropy. Node IDs remain
/// stable across removals so heap swaps can update leaf slots in O(1).
#[derive(Debug, Default)]
pub(super) struct DeferredRouteIndex {
    nodes: Vec<Node>,
    root: Option<NodeId>,
    free_head: Option<NodeId>,
    free_count: usize,
    leaves: usize,
}

impl DeferredRouteIndex {
    pub(super) fn len(&self) -> usize {
        self.leaves
    }

    pub(super) fn get(&self, key: u64) -> Option<(NodeId, usize)> {
        let leaf = self.find_leaf(key)?;
        match self.nodes[leaf] {
            Node::Leaf {
                key: leaf_key,
                slot,
            } if leaf_key == key => Some((leaf, slot)),
            _ => None,
        }
    }

    pub(super) fn try_reserve_insert(&mut self) -> Result<(), SystemError> {
        let required: usize = if self.root.is_some() { 2 } else { 1 };
        self.nodes
            .try_reserve(required.saturating_sub(self.free_count))
            .map_err(|_| SystemError::ENOMEM)
    }

    /// Inserts a key after `try_reserve_insert`; this method cannot allocate.
    pub(super) fn insert_prepared(&mut self, key: u64, slot: usize) -> NodeId {
        debug_assert!(self.get(key).is_none());
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
        let differing_bit = (key ^ existing_key).leading_zeros() as u8;
        debug_assert!(differing_bit < 64);

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

    pub(super) fn set_slot(&mut self, leaf: NodeId, key: u64, slot: usize) {
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

    pub(super) fn remove(&mut self, key: u64) -> Option<usize> {
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

    fn find_leaf(&self, key: u64) -> Option<NodeId> {
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

    fn direction(key: u64, bit: u8) -> usize {
        ((key >> (63 - bit)) & 1) as usize
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
