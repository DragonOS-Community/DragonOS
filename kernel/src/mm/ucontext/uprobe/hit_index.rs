use alloc::sync::Arc;

use super::UprobeSite;

/// Immutable compressed radix index used by the #BP read path.
///
/// Writers path-copy at most one machine-word worth of branch nodes. Readers
/// traverse the RCU-published root without allocating or taking a shared lock.
#[derive(Clone, Debug, Default)]
pub(super) struct UprobeHitIndex {
    root: Option<Arc<HitNode>>,
}

// The index closes a type-level ownership cycle through
// UprobeSite -> UprobeDefinition -> PageCache -> VMA -> AddressSpace -> index.
// Every edge is an Arc/shared immutable edge (site mutability remains behind
// its existing synchronization), but rustc's auto-trait solver cannot prove
// the recursive Send/Sync obligation. The previous RCU BTree snapshot held the
// same UprobeSite values and required these exact bounds.
unsafe impl Send for UprobeHitIndex {}
unsafe impl Sync for UprobeHitIndex {}

#[derive(Debug)]
enum HitNode {
    Leaf {
        key: usize,
        site: Arc<UprobeSite>,
    },
    Branch {
        bit: u32,
        zero: Arc<HitNode>,
        one: Arc<HitNode>,
    },
}

impl UprobeHitIndex {
    pub(super) fn get(&self, key: usize) -> Option<&Arc<UprobeSite>> {
        let mut node = self.root.as_deref()?;
        loop {
            match node {
                HitNode::Leaf {
                    key: leaf_key,
                    site,
                } => return (*leaf_key == key).then_some(site),
                HitNode::Branch { bit, zero, one } => {
                    node = if key_bit(key, *bit) { one } else { zero };
                }
            }
        }
    }

    pub(super) fn insert(&mut self, key: usize, site: Arc<UprobeSite>) {
        let Some(root) = self.root.take() else {
            self.root = Some(Arc::new(HitNode::Leaf { key, site }));
            return;
        };

        let existing_key = leaf_key_for(&root, key);
        self.root = Some(if existing_key == key {
            replace_leaf(&root, key, &site)
        } else {
            let differing_bit = usize::BITS - 1 - (existing_key ^ key).leading_zeros();
            insert_leaf(&root, key, &site, differing_bit)
        });
    }

    pub(super) fn remove(&mut self, key: usize) {
        self.root = self.root.take().and_then(|root| remove_leaf(&root, key));
    }
}

#[inline]
fn key_bit(key: usize, bit: u32) -> bool {
    key & (1usize << bit) != 0
}

fn leaf_key_for(mut node: &HitNode, key: usize) -> usize {
    loop {
        match node {
            HitNode::Leaf { key, .. } => return *key,
            HitNode::Branch { bit, zero, one } => {
                node = if key_bit(key, *bit) { one } else { zero };
            }
        }
    }
}

fn branch_with_leaf(
    existing: &Arc<HitNode>,
    key: usize,
    site: &Arc<UprobeSite>,
    bit: u32,
) -> Arc<HitNode> {
    let leaf = Arc::new(HitNode::Leaf {
        key,
        site: site.clone(),
    });
    let (zero, one) = if key_bit(key, bit) {
        (existing.clone(), leaf)
    } else {
        (leaf, existing.clone())
    };
    Arc::new(HitNode::Branch { bit, zero, one })
}

fn insert_leaf(
    node: &Arc<HitNode>,
    key: usize,
    site: &Arc<UprobeSite>,
    differing_bit: u32,
) -> Arc<HitNode> {
    match node.as_ref() {
        HitNode::Branch { bit, zero, one } if *bit > differing_bit => {
            let (zero, one) = if key_bit(key, *bit) {
                (zero.clone(), insert_leaf(one, key, site, differing_bit))
            } else {
                (insert_leaf(zero, key, site, differing_bit), one.clone())
            };
            Arc::new(HitNode::Branch {
                bit: *bit,
                zero,
                one,
            })
        }
        _ => branch_with_leaf(node, key, site, differing_bit),
    }
}

fn replace_leaf(node: &Arc<HitNode>, key: usize, site: &Arc<UprobeSite>) -> Arc<HitNode> {
    match node.as_ref() {
        HitNode::Leaf { key: leaf_key, .. } => {
            debug_assert_eq!(*leaf_key, key);
            Arc::new(HitNode::Leaf {
                key,
                site: site.clone(),
            })
        }
        HitNode::Branch { bit, zero, one } => {
            let (zero, one) = if key_bit(key, *bit) {
                (zero.clone(), replace_leaf(one, key, site))
            } else {
                (replace_leaf(zero, key, site), one.clone())
            };
            Arc::new(HitNode::Branch {
                bit: *bit,
                zero,
                one,
            })
        }
    }
}

fn remove_leaf(node: &Arc<HitNode>, key: usize) -> Option<Arc<HitNode>> {
    match node.as_ref() {
        HitNode::Leaf { key: leaf_key, .. } => (*leaf_key != key).then(|| node.clone()),
        HitNode::Branch { bit, zero, one } => {
            if key_bit(key, *bit) {
                match remove_leaf(one, key) {
                    Some(new_one) => Some(Arc::new(HitNode::Branch {
                        bit: *bit,
                        zero: zero.clone(),
                        one: new_one,
                    })),
                    None => Some(zero.clone()),
                }
            } else {
                match remove_leaf(zero, key) {
                    Some(new_zero) => Some(Arc::new(HitNode::Branch {
                        bit: *bit,
                        zero: new_zero,
                        one: one.clone(),
                    })),
                    None => Some(one.clone()),
                }
            }
        }
    }
}
