use super::Ext4;
use crate::constants::*;
use crate::ext4_defs::*;
use crate::format_error;
use crate::prelude::*;
use core::cmp::min;

/// One contiguous tail of the right-most data extent removed from a tree.
///
/// The caller owns allocation accounting: `metadata_blocks` have already been
/// disconnected from the staged tree and can therefore be freed atomically in
/// the same transaction as `data` and the inode-table image.
#[derive(Debug, Eq, PartialEq)]
pub(super) struct ExtentTailRemoval {
    pub start_lblock: LBlockId,
    pub start_pblock: PBlockId,
    pub block_count: u32,
    pub metadata_blocks: Vec<PBlockId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ExtentTail {
    pub start_lblock: LBlockId,
    pub start_pblock: PBlockId,
    pub block_count: u32,
    pub unwritten: bool,
}

pub(super) struct DirectAppendShape {
    pub preferred_first: Option<PBlockId>,
    pub requires_merge: bool,
    /// The inline root is full and must become a depth-one root. This is
    /// journal-only because allocating and publishing the new leaf has to be
    /// one metadata transaction with the bitmap, descriptor, superblock and
    /// inode image.
    pub requires_root_split: bool,
    /// The right-most external leaf is full.  The current depth-one root has
    /// room for another index, so the append needs a new right-most leaf and
    /// an inode-root update in the same journal transaction.
    pub requires_leaf_split: bool,
    /// Physical home of the right-most extent leaf. `None` denotes the
    /// inline extent root stored in the inode-table entry.
    pub leaf_home: Option<PBlockId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ExtentRightSpineProjection {
    pub next_lblock: LBlockId,
    /// Right-most node occupancy from leaf through the inline inode root.
    counts: Vec<u16>,
    capacities: Vec<u16>,
    external_capacity: u16,
    inline_root_capacity: u16,
}

#[cfg(test)]
impl ExtentRightSpineProjection {
    pub(super) fn test_empty(next_lblock: LBlockId) -> Self {
        Self {
            next_lblock,
            counts: Vec::new(),
            capacities: Vec::new(),
            external_capacity: 0,
            inline_root_capacity: 0,
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct RightSpineAppendPlan {
    pub start_lblock: LBlockId,
    pub preferred_first: Option<PBlockId>,
    /// External node homes from the inode root's child through the leaf.
    path: Vec<PBlockId>,
    new_nodes: usize,
    tail: Option<Extent>,
}

pub(super) struct JournaledAppendExtent {
    pub leaf_home: Option<PBlockId>,
    pub root_split_leaf_home: Option<PBlockId>,
    pub leaf_split_new_home: Option<PBlockId>,
    pub start_lblock: LBlockId,
    pub start_pblock: PBlockId,
    pub count: u32,
}

impl RightSpineAppendPlan {
    pub(super) const fn new_nodes(&self) -> usize {
        self.new_nodes
    }

    pub(super) fn can_merge(&self, start_pblock: PBlockId, count: u32) -> bool {
        self.tail.is_some_and(|tail| {
            Extent::can_append(
                &tail,
                &Extent::new(self.start_lblock, start_pblock, count as u16),
            )
        })
    }
}

impl ExtentRightSpineProjection {
    /// Advance a right-edge extent at or after the current mapped frontier.
    /// A logical gap is a sparse hole and consumes no extent-tree entry.
    pub(super) fn append_nonmerge_at(
        &mut self,
        start_lblock: LBlockId,
        block_count: u32,
    ) -> Result<u64> {
        if start_lblock < self.next_lblock {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }
        self.next_lblock = start_lblock;
        self.append_nonmerge(block_count)
    }

    /// Advance one worst-case non-merging extent and return the number of new
    /// physical extent-tree nodes required by that transition.
    pub(super) fn append_nonmerge(&mut self, block_count: u32) -> Result<u64> {
        if block_count == 0 {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }
        self.next_lblock = self
            .next_lblock
            .checked_add(block_count)
            .ok_or_else(|| Ext4Error::new(ErrCode::EFBIG))?;
        self.counts[0] = self.counts[0]
            .checked_add(1)
            .ok_or_else(|| Ext4Error::new(ErrCode::EFBIG))?;
        let mut new_nodes = 0u64;
        let mut level = 0usize;
        loop {
            if self.counts[level] <= self.capacities[level] {
                return Ok(new_nodes);
            }
            if level + 1 == self.counts.len() {
                if self.counts.len() >= 6 {
                    return Err(Ext4Error::new(ErrCode::EFBIG));
                }
                let promoted_entries = self.counts[level];
                if promoted_entries > self.external_capacity {
                    return Err(Ext4Error::new(ErrCode::EFBIG));
                }
                self.counts[level] = promoted_entries;
                self.capacities[level] = self.external_capacity;
                self.counts.push(1);
                self.capacities.push(self.inline_root_capacity);
                return new_nodes
                    .checked_add(1)
                    .ok_or_else(|| Ext4Error::new(ErrCode::ERANGE));
            }
            new_nodes = new_nodes
                .checked_add(1)
                .ok_or_else(|| Ext4Error::new(ErrCode::ERANGE))?;
            self.counts[level] = 1;
            self.counts[level + 1] = self.counts[level + 1]
                .checked_add(1)
                .ok_or_else(|| Ext4Error::new(ErrCode::EFBIG))?;
            level += 1;
        }
    }
}

impl Ext4 {
    pub(super) fn right_spine_append_plan(
        &self,
        inode: &InodeRef,
        start_lblock: LBlockId,
    ) -> Result<RightSpineAppendPlan> {
        self.right_spine_append_plan_from_view(inode, start_lblock, None)
    }

    /// Plan against the transaction-private final image so consecutive
    /// appends in one delayed-allocation batch observe earlier staged extent
    /// mutations instead of rereading stale home blocks.
    pub(super) fn transaction_right_spine_append_plan(
        &self,
        transaction: &super::journal_transaction::Transaction<'_>,
        inode: &InodeRef,
        start_lblock: LBlockId,
    ) -> Result<RightSpineAppendPlan> {
        self.right_spine_append_plan_from_view(inode, start_lblock, Some(transaction))
    }

    fn right_spine_append_plan_from_view(
        &self,
        inode: &InodeRef,
        start_lblock: LBlockId,
        transaction: Option<&super::journal_transaction::Transaction<'_>>,
    ) -> Result<RightSpineAppendPlan> {
        let root = inode.inode.extent_root();
        self.validate_extent_node(inode.id, &root)?;
        if root.header().entries_count() == 0 {
            if root.header().depth() != 0 {
                return Err(Ext4Error::new(ErrCode::EINVAL));
            }
            return Ok(RightSpineAppendPlan {
                start_lblock,
                preferred_first: None,
                path: Vec::new(),
                new_nodes: 0,
                tail: None,
            });
        }

        let mut path = Vec::new();
        let mut expected_depth = root.header().depth();
        let mut node_entries = root.header().entries_count() as usize;
        let mut leaf_block = None;
        if expected_depth > 0 {
            let mut home = root.extent_index_at(node_entries - 1).leaf();
            loop {
                path.push(home);
                let block = if let Some(transaction) = transaction {
                    self.ensure_valid_pblock(inode.id, home, "extent tree node")?;
                    self.validate_data_blocks(home, 1)?;
                    let block = transaction.read(self.block_device.as_ref(), home)?;
                    self.verify_transaction_extent_block(inode, &*block)?;
                    block
                } else {
                    super::journal_transaction::BlockView::Device(
                        self.read_extent_block(inode, home)?,
                    )
                };
                let node = ExtentNode::from_bytes(&*block);
                self.validate_extent_node(inode.id, &node)?;
                if node.header().depth() + 1 != expected_depth || node.header().entries_count() == 0
                {
                    return Err(Ext4Error::new(ErrCode::EIO));
                }
                expected_depth = node.header().depth();
                node_entries = node.header().entries_count() as usize;
                if expected_depth == 0 {
                    leaf_block = Some(block);
                    break;
                }
                home = node.extent_index_at(node_entries - 1).leaf();
            }
        }
        let leaf = leaf_block
            .as_ref()
            .map(|block| ExtentNode::from_bytes(&**block));
        let node = leaf.as_ref().unwrap_or(&root);
        let last = node.extent_at(node_entries - 1);
        let mapped_frontier = last
            .start_lblock()
            .checked_add(last.block_count())
            .ok_or_else(|| Ext4Error::new(ErrCode::EFBIG))?;
        if last.is_unwritten() || mapped_frontier > start_lblock {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }
        let preferred_first = last
            .start_pblock()
            .checked_add(last.block_count() as PBlockId)
            .ok_or_else(|| Ext4Error::new(ErrCode::EFBIG))?;

        let mut projection = self.extent_right_spine_projection_from_view(inode, transaction)?;
        if projection.next_lblock != mapped_frontier {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }
        let new_nodes = usize::try_from(projection.append_nonmerge_at(start_lblock, 1)?)
            .map_err(|_| Ext4Error::new(ErrCode::EFBIG))?;
        Ok(RightSpineAppendPlan {
            start_lblock,
            preferred_first: Some(preferred_first),
            path,
            new_nodes,
            tail: Some(*last),
        })
    }

    pub(super) fn stage_journaled_right_spine_append(
        &self,
        transaction: &mut super::journal_transaction::Transaction<'_>,
        inode: &mut InodeRef,
        plan: &RightSpineAppendPlan,
        new_node_homes: &[PBlockId],
        start_pblock: PBlockId,
        count: u32,
    ) -> Result<()> {
        let merge = plan.can_merge(start_pblock, count);
        let required_new_nodes = if merge { 0 } else { plan.new_nodes };
        if new_node_homes.len() != required_new_nodes {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }
        let new_extent = Extent::new(plan.start_lblock, start_pblock, count as u16);
        let seed = self.read_super_block_cached().metadata_checksum_seed();
        let mut homes = new_node_homes.iter().copied();

        if plan.path.is_empty() {
            let root = inode.inode.extent_root();
            self.validate_extent_node(inode.id, &root)?;
            let entries = root.header().entries_count() as usize;
            if merge || entries < root.entry_capacity() {
                self.stage_direct_append_extent(inode, plan.start_lblock, start_pblock, count)?;
                return Ok(());
            } else {
                let home = homes
                    .next()
                    .ok_or_else(|| Ext4Error::new(ErrCode::EINVAL))?;
                let generation = root.header().generation();
                let first_lblock = root.extent_at(0).start_lblock();
                let old: Vec<Extent> = (0..entries).map(|index| *root.extent_at(index)).collect();
                let image = self.transaction_block_for_update(transaction, home)?;
                let mut leaf = ExtentNodeMut::from_bytes(image);
                leaf.init(0, generation);
                for (index, extent) in old.into_iter().enumerate() {
                    *leaf.fake_extent_mut_at(index) = extent.into();
                }
                *leaf.fake_extent_mut_at(entries) = new_extent.into();
                leaf.header_mut().set_entries_count((entries + 1) as u16);
                Self::set_extent_block_checksum(seed, inode, image);
                let mut root = inode.inode.extent_root_mut();
                root.init(1, generation);
                root.header_mut().set_entries_count(1);
                *root.extent_index_mut_at(0) = ExtentIndex::new(first_lblock, home);
            }
        } else {
            let leaf_home = *plan
                .path
                .last()
                .ok_or_else(|| Ext4Error::new(ErrCode::EINVAL))?;
            let leaf_view = transaction.read(self.block_device.as_ref(), leaf_home)?;
            self.verify_transaction_extent_block(inode, &*leaf_view)?;
            let leaf = ExtentNode::from_bytes(&*leaf_view);
            self.validate_extent_node(inode.id, &leaf)?;
            let leaf_entries = leaf.header().entries_count() as usize;
            if merge || leaf_entries < leaf.header().max_entries_count() as usize {
                let image = self.transaction_block_for_update(transaction, leaf_home)?;
                let mut leaf = ExtentNodeMut::from_bytes(image);
                let last = *leaf.extent_at(leaf_entries - 1);
                if Extent::can_append(&last, &new_extent) {
                    leaf.extent_mut_at(leaf_entries - 1)
                        .set_block_count(last.block_count() + count);
                } else {
                    leaf.insert_extent(&new_extent, leaf_entries)
                        .map_err(|_| Ext4Error::new(ErrCode::EIO))?;
                }
                Self::set_extent_block_checksum(seed, inode, image);
            } else {
                let new_leaf_home = homes
                    .next()
                    .ok_or_else(|| Ext4Error::new(ErrCode::EINVAL))?;
                let generation = leaf.header().generation();
                let image = self.transaction_block_for_update(transaction, new_leaf_home)?;
                let mut new_leaf = ExtentNodeMut::from_bytes(image);
                new_leaf.init(0, generation);
                *new_leaf.fake_extent_mut_at(0) = new_extent.into();
                new_leaf.header_mut().set_entries_count(1);
                Self::set_extent_block_checksum(seed, inode, image);
                let mut carry = Some(ExtentIndex::new(plan.start_lblock, new_leaf_home));

                for parent_home in plan.path[..plan.path.len() - 1].iter().rev() {
                    let carry_index = carry.ok_or_else(|| Ext4Error::new(ErrCode::EIO))?;
                    let view = transaction.read(self.block_device.as_ref(), *parent_home)?;
                    self.verify_transaction_extent_block(inode, &*view)?;
                    let parent = ExtentNode::from_bytes(&*view);
                    self.validate_extent_node(inode.id, &parent)?;
                    let entries = parent.header().entries_count() as usize;
                    if entries < parent.header().max_entries_count() as usize {
                        let image = self.transaction_block_for_update(transaction, *parent_home)?;
                        let mut parent = ExtentNodeMut::from_bytes(image);
                        parent
                            .insert_extent_index(&carry_index, entries)
                            .map_err(|_| Ext4Error::new(ErrCode::EIO))?;
                        Self::set_extent_block_checksum(seed, inode, image);
                        carry = None;
                        break;
                    }
                    let sibling_home = homes
                        .next()
                        .ok_or_else(|| Ext4Error::new(ErrCode::EINVAL))?;
                    let depth = parent.header().depth();
                    let generation = parent.header().generation();
                    let image = self.transaction_block_for_update(transaction, sibling_home)?;
                    let mut sibling = ExtentNodeMut::from_bytes(image);
                    sibling.init(depth, generation);
                    *sibling.extent_index_mut_at(0) = carry_index;
                    sibling.header_mut().set_entries_count(1);
                    Self::set_extent_block_checksum(seed, inode, image);
                    carry = Some(ExtentIndex::new(plan.start_lblock, sibling_home));
                }

                if let Some(carry) = carry {
                    let root = inode.inode.extent_root();
                    self.validate_extent_node(inode.id, &root)?;
                    let entries = root.header().entries_count() as usize;
                    if entries < root.header().max_entries_count() as usize {
                        let mut root = inode.inode.extent_root_mut();
                        root.insert_extent_index(&carry, entries)
                            .map_err(|_| Ext4Error::new(ErrCode::EIO))?;
                    } else {
                        let promoted_home = homes
                            .next()
                            .ok_or_else(|| Ext4Error::new(ErrCode::EINVAL))?;
                        let depth = root.header().depth();
                        let generation = root.header().generation();
                        let first_lblock = root.extent_index_at(0).start_lblock();
                        let old: Vec<ExtentIndex> = (0..entries)
                            .map(|index| *root.extent_index_at(index))
                            .collect();
                        let image =
                            self.transaction_block_for_update(transaction, promoted_home)?;
                        let mut promoted = ExtentNodeMut::from_bytes(image);
                        promoted.init(depth, generation);
                        for (index, old_index) in old.into_iter().enumerate() {
                            *promoted.extent_index_mut_at(index) = old_index;
                        }
                        *promoted.extent_index_mut_at(entries) = carry;
                        promoted
                            .header_mut()
                            .set_entries_count((entries + 1) as u16);
                        Self::set_extent_block_checksum(seed, inode, image);
                        let mut root = inode.inode.extent_root_mut();
                        root.init(
                            depth
                                .checked_add(1)
                                .ok_or_else(|| Ext4Error::new(ErrCode::EFBIG))?,
                            generation,
                        );
                        root.header_mut().set_entries_count(1);
                        *root.extent_index_mut_at(0) =
                            ExtentIndex::new(first_lblock, promoted_home);
                    }
                }
            }
        }
        if homes.next().is_some() {
            return Err(Ext4Error::new(ErrCode::EIO));
        }
        let blocks = inode
            .inode
            .fs_block_count()
            .checked_add(count as u64 + required_new_nodes as u64)
            .ok_or_else(|| Ext4Error::new(ErrCode::EFBIG))?;
        inode.inode.set_fs_block_count(blocks);
        Ok(())
    }

    pub(super) fn extent_right_spine_projection(
        &self,
        inode: &InodeRef,
    ) -> Result<ExtentRightSpineProjection> {
        self.extent_right_spine_projection_from_view(inode, None)
    }

    fn extent_right_spine_projection_from_view(
        &self,
        inode: &InodeRef,
        transaction: Option<&super::journal_transaction::Transaction<'_>>,
    ) -> Result<ExtentRightSpineProjection> {
        if !inode.inode.uses_extents() {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }
        let root = inode.inode.extent_root();
        self.validate_extent_node(inode.id, &root)?;
        let root_header = root.header();
        let root_capacity = root_header.max_entries_count();
        if root_header.depth() == 0 {
            let next_lblock = if root_header.entries_count() == 0 {
                0
            } else {
                let last = root.extent_at(root_header.entries_count() as usize - 1);
                if last.is_unwritten() {
                    return Err(Ext4Error::new(ErrCode::ENOTSUP));
                }
                last.start_lblock()
                    .checked_add(last.block_count())
                    .ok_or_else(|| Ext4Error::new(ErrCode::EFBIG))?
            };
            let external_capacity = ((BLOCK_SIZE
                - core::mem::size_of::<ExtentHeader>()
                - core::mem::size_of::<crate::ext4_defs::ExtentTail>())
                / core::mem::size_of::<Extent>()) as u16;
            return Ok(ExtentRightSpineProjection {
                next_lblock,
                counts: vec![root_header.entries_count()],
                capacities: vec![root_capacity],
                external_capacity,
                inline_root_capacity: root_capacity,
            });
        }

        let mut counts_root_to_leaf = vec![root_header.entries_count()];
        let mut capacities_root_to_leaf = vec![root_capacity];
        let mut expected_depth = root_header.depth();
        let mut home = root
            .extent_index_at(root_header.entries_count() as usize - 1)
            .leaf();
        let mut external_capacity = None;
        let next_lblock = loop {
            let block = if let Some(transaction) = transaction {
                self.ensure_valid_pblock(inode.id, home, "extent tree node")?;
                self.validate_data_blocks(home, 1)?;
                let block = transaction.read(self.block_device.as_ref(), home)?;
                self.verify_transaction_extent_block(inode, &*block)?;
                block
            } else {
                super::journal_transaction::BlockView::Device(self.read_extent_block(inode, home)?)
            };
            let node = ExtentNode::from_bytes(&*block);
            self.validate_extent_node(inode.id, &node)?;
            if node.header().depth() + 1 != expected_depth || node.header().entries_count() == 0 {
                return Err(Ext4Error::new(ErrCode::EIO));
            }
            counts_root_to_leaf.push(node.header().entries_count());
            capacities_root_to_leaf.push(node.header().max_entries_count());
            external_capacity.get_or_insert(node.header().max_entries_count());
            expected_depth = node.header().depth();
            if expected_depth == 0 {
                let last = node.extent_at(node.header().entries_count() as usize - 1);
                if last.is_unwritten() {
                    return Err(Ext4Error::new(ErrCode::ENOTSUP));
                }
                break last
                    .start_lblock()
                    .checked_add(last.block_count())
                    .ok_or_else(|| Ext4Error::new(ErrCode::EFBIG))?;
            }
            home = node
                .extent_index_at(node.header().entries_count() as usize - 1)
                .leaf();
        };
        counts_root_to_leaf.reverse();
        capacities_root_to_leaf.reverse();
        Ok(ExtentRightSpineProjection {
            next_lblock,
            counts: counts_root_to_leaf,
            capacities: capacities_root_to_leaf,
            external_capacity: external_capacity.ok_or_else(|| Ext4Error::new(ErrCode::EIO))?,
            inline_root_capacity: root_capacity,
        })
    }

    /// Return the logical tail and the number of non-merging extents which
    /// still fit in the current right-most leaf. This is a read-only
    /// projection primitive for admitting consecutive delayed entries before
    /// their predecessors reach the on-disk tree.
    pub(super) fn extent_rightmost_append_capacity(
        &self,
        inode: &InodeRef,
    ) -> Result<(LBlockId, usize)> {
        if !inode.inode.uses_extents() {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }
        let root = inode.inode.extent_root();
        self.validate_extent_node(inode.id, &root)?;
        let root_header = root.header();
        if root_header.entries_count() == 0 {
            if root_header.depth() != 0 {
                return Err(Ext4Error::new(ErrCode::EIO));
            }
            return Ok((0, root.entry_capacity()));
        }

        let mut entries = root_header.entries_count() as usize;
        let mut leaf_block = None;
        if root_header.depth() > 0 {
            let mut expected_depth = root_header.depth();
            let mut home = root.extent_index_at(entries - 1).leaf();
            loop {
                let block = self.read_extent_block(inode, home)?;
                let node = ExtentNode::from_bytes(&block.data[..]);
                self.validate_extent_node(inode.id, &node)?;
                if node.header().depth() + 1 != expected_depth {
                    return Err(Ext4Error::new(ErrCode::EIO));
                }
                entries = node.header().entries_count() as usize;
                if entries == 0 {
                    return Err(Ext4Error::new(ErrCode::EIO));
                }
                expected_depth = node.header().depth();
                if expected_depth == 0 {
                    leaf_block = Some(block);
                    break;
                }
                home = node.extent_index_at(entries - 1).leaf();
            }
        }
        let leaf = leaf_block
            .as_ref()
            .map(|block| ExtentNode::from_bytes(&block.data[..]));
        let node = leaf.as_ref().unwrap_or(&root);
        let last = node.extent_at(entries - 1);
        if last.is_unwritten() {
            return Err(Ext4Error::new(ErrCode::ENOTSUP));
        }
        let next_lblock = last
            .start_lblock()
            .checked_add(last.block_count())
            .ok_or_else(|| Ext4Error::new(ErrCode::EFBIG))?;
        let free_entries = node
            .header()
            .max_entries_count()
            .checked_sub(node.header().entries_count())
            .ok_or_else(|| Ext4Error::new(ErrCode::EIO))? as usize;
        Ok((next_lblock, free_entries))
    }

    pub(super) fn direct_append_shape(
        &self,
        inode: &InodeRef,
        start_lblock: LBlockId,
        count: u32,
    ) -> Result<Option<DirectAppendShape>> {
        if count == 0 || count > u16::MAX as u32 || !inode.inode.uses_extents() {
            return Ok(None);
        }
        let root = inode.inode.extent_root();
        let header = root.header();
        self.validate_extent_node(inode.id, &root)?;
        if header.max_entries_count() as usize != root.entry_capacity()
            || header.entries_count() as usize > root.entry_capacity()
        {
            return Ok(None);
        }
        let mut entries = header.entries_count() as usize;
        if entries == 0 {
            return Ok(
                (header.depth() == 0 && start_lblock == 0).then_some(DirectAppendShape {
                    preferred_first: None,
                    requires_merge: false,
                    requires_root_split: false,
                    requires_leaf_split: false,
                    leaf_home: None,
                }),
            );
        }

        let mut leaf_block = None;
        let leaf_home = if header.depth() == 0 {
            None
        } else {
            // Range staging for an external leaf is journal-only.  The
            // nojournal backend has a special publication order for an
            // inline-root update and cannot safely publish another metadata
            // home without a more general undo protocol.
            if !self.uses_journal() {
                return Ok(None);
            }
            let mut expected_depth = header.depth();
            let mut home = root.extent_index_at(entries - 1).leaf();
            loop {
                let block = self.read_extent_block(inode, home)?;
                let node = ExtentNode::from_bytes(&block.data[..]);
                self.validate_extent_node(inode.id, &node)?;
                if node.header().depth() + 1 != expected_depth {
                    return Err(format_error!(
                        ErrCode::EIO,
                        "extent depth mismatch on inode {} at block {}",
                        inode.id,
                        home
                    ));
                }
                entries = node.header().entries_count() as usize;
                if entries == 0 {
                    return Err(format_error!(
                        ErrCode::EIO,
                        "empty reachable extent node on inode {} at block {}",
                        inode.id,
                        home
                    ));
                }
                expected_depth = node.header().depth();
                if expected_depth == 0 {
                    leaf_block = Some(block);
                    break Some(home);
                }
                home = node.extent_index_at(entries - 1).leaf();
            }
        };
        let leaf = leaf_block
            .as_ref()
            .map(|block| ExtentNode::from_bytes(&block.data[..]));
        let node = leaf.as_ref().unwrap_or(&root);
        let last = node.extent_at(entries - 1);
        if last.is_unwritten() {
            return Ok(None);
        }
        for index in 0..entries {
            let extent = node.extent_at(index);
            self.validate_data_blocks(extent.start_pblock(), extent.block_count() as u64)?;
        }
        let next_lblock = last
            .start_lblock()
            .checked_add(last.block_count())
            .ok_or_else(|| Ext4Error::new(ErrCode::EFBIG))?;
        if next_lblock != start_lblock {
            return Ok(None);
        }
        let preferred_first = last
            .start_pblock()
            .checked_add(last.block_count() as PBlockId)
            .ok_or_else(|| Ext4Error::new(ErrCode::EFBIG))?;
        let candidate = Extent::new(start_lblock, preferred_first, count as u16);
        let can_merge = Extent::can_append(last, &candidate);
        let leaf_full = entries == node.header().max_entries_count() as usize;
        if leaf_full && header.depth() == 0 && self.uses_journal() {
            // The allocator can only confirm physical adjacency after it has
            // staged bitmap changes. Avoid falling back to the non-journaled
            // legacy insertion when a full inline root's preferred run is
            // unavailable: publish a compact depth-one tree in the same
            // journal transaction instead. Keeping a mergeable pair as two
            // extents is valid and is far safer than splitting after direct
            // metadata writes have escaped the journal.
            return Ok(Some(DirectAppendShape {
                preferred_first: Some(preferred_first),
                requires_merge: false,
                requires_root_split: true,
                requires_leaf_split: false,
                leaf_home: None,
            }));
        }
        if leaf_full
            && header.depth() == 1
            && header.entries_count() < header.max_entries_count()
            && self.uses_journal()
        {
            // Keep the existing full leaf intact and append a one-entry
            // right-most leaf.  This has no underflow invariant in ext4's
            // extent tree and, unlike the legacy split path, lets the
            // allocation bitmap, new leaf and inode root commit atomically.
            // Deeper trees and full roots still use the general insertion
            // path until they receive an equally atomic split protocol.
            return Ok(Some(DirectAppendShape {
                preferred_first: Some(preferred_first),
                requires_merge: false,
                requires_root_split: false,
                requires_leaf_split: true,
                leaf_home,
            }));
        }
        if leaf_full && !can_merge {
            return Ok(None);
        }
        Ok(Some(DirectAppendShape {
            preferred_first: Some(preferred_first),
            requires_merge: leaf_full,
            requires_root_split: false,
            requires_leaf_split: false,
            leaf_home,
        }))
    }

    pub(super) fn stage_direct_append_extent(
        &self,
        inode: &mut InodeRef,
        start_lblock: LBlockId,
        start_pblock: PBlockId,
        count: u32,
    ) -> Result<()> {
        let new_extent = Extent::new(start_lblock, start_pblock, count as u16);
        let mut root = inode.inode.extent_root_mut();
        let entries = root.header().entries_count() as usize;
        if entries > 0 {
            let last = *root.extent_at(entries - 1);
            if Extent::can_append(&last, &new_extent) {
                root.extent_mut_at(entries - 1)
                    .set_block_count(last.block_count() + count);
            } else if root.insert_extent(&new_extent, entries).is_err() {
                return Err(format_error!(
                    ErrCode::ENOTSUP,
                    "Inline extent root requires a split"
                ));
            }
        } else if root.insert_extent(&new_extent, 0).is_err() {
            return Err(format_error!(
                ErrCode::ENOTSUP,
                "Inline extent root requires a split"
            ));
        }
        let blocks = inode
            .inode
            .fs_block_count()
            .checked_add(count as u64)
            .ok_or_else(|| Ext4Error::new(ErrCode::EFBIG))?;
        inode.inode.set_fs_block_count(blocks);
        Ok(())
    }

    /// Stage a contiguous append in either the inline extent root or the
    /// existing right-most external leaf.
    ///
    /// A full inline root and a full right-most leaf below a depth-one root
    /// are handled here because their allocation and publication fit in one
    /// bounded journal transaction. Deeper trees or full inode roots still
    /// use the general insertion path until they receive an equally atomic
    /// split protocol.
    pub(super) fn stage_journaled_append_extent(
        &self,
        transaction: &mut super::journal_transaction::Transaction<'_>,
        inode: &mut InodeRef,
        append: JournaledAppendExtent,
    ) -> Result<()> {
        let JournaledAppendExtent {
            leaf_home,
            root_split_leaf_home,
            leaf_split_new_home,
            start_lblock,
            start_pblock,
            count,
        } = append;
        if root_split_leaf_home.is_some() && leaf_split_new_home.is_some() {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }
        if let Some(new_leaf_home) = root_split_leaf_home {
            if leaf_home.is_some() {
                return Err(Ext4Error::new(ErrCode::EINVAL));
            }
            let new_extent = Extent::new(start_lblock, start_pblock, count as u16);
            let (old_entries, generation, first_lblock, old_extents) = {
                let root = inode.inode.extent_root();
                self.validate_extent_node(inode.id, &root)?;
                if root.header().depth() != 0
                    || root.header().entries_count() != root.header().max_entries_count()
                {
                    return Err(Ext4Error::new(ErrCode::EINVAL));
                }
                let entries = root.header().entries_count() as usize;
                let extents: Vec<Extent> =
                    (0..entries).map(|index| *root.extent_at(index)).collect();
                (
                    entries,
                    root.header().generation(),
                    root.extent_at(0).start_lblock(),
                    extents,
                )
            };

            let image = self.transaction_block_for_update(transaction, new_leaf_home)?;
            let mut leaf = ExtentNodeMut::from_bytes(image);
            leaf.init(0, generation);
            for (index, extent) in old_extents.into_iter().enumerate() {
                *leaf.fake_extent_mut_at(index) = extent.into();
            }
            *leaf.fake_extent_mut_at(old_entries) = new_extent.into();
            leaf.header_mut()
                .set_entries_count((old_entries + 1) as u16);
            Self::set_extent_block_checksum(
                self.read_super_block_cached().metadata_checksum_seed(),
                inode,
                image,
            );

            let mut root = inode.inode.extent_root_mut();
            root.init(1, generation);
            root.header_mut().set_entries_count(1);
            *root.extent_index_mut_at(0) = ExtentIndex::new(first_lblock, new_leaf_home);
            let blocks = inode
                .inode
                .fs_block_count()
                .checked_add(count as u64 + 1)
                .ok_or_else(|| Ext4Error::new(ErrCode::EFBIG))?;
            inode.inode.set_fs_block_count(blocks);
            return Ok(());
        }
        if let Some(new_leaf_home) = leaf_split_new_home {
            let home = leaf_home.ok_or_else(|| Ext4Error::new(ErrCode::EINVAL))?;
            let generation = {
                let root = inode.inode.extent_root();
                self.validate_extent_node(inode.id, &root)?;
                if root.header().depth() != 1
                    || root.header().entries_count() == 0
                    || root.header().entries_count() >= root.header().max_entries_count()
                    || root
                        .extent_index_at(root.header().entries_count() as usize - 1)
                        .leaf()
                        != home
                {
                    return Err(Ext4Error::new(ErrCode::EINVAL));
                }

                let image = transaction.read(self.block_device.as_ref(), home)?;
                self.verify_transaction_extent_block(inode, &*image)?;
                let leaf = ExtentNode::from_bytes(&*image);
                self.validate_extent_node(inode.id, &leaf)?;
                if leaf.header().depth() != 0
                    || leaf.header().entries_count() != leaf.header().max_entries_count()
                {
                    return Err(Ext4Error::new(ErrCode::EINVAL));
                }
                leaf.header().generation()
            };

            let new_extent = Extent::new(start_lblock, start_pblock, count as u16);
            let image = self.transaction_block_for_update(transaction, new_leaf_home)?;
            let mut leaf = ExtentNodeMut::from_bytes(image);
            leaf.init(0, generation);
            *leaf.fake_extent_mut_at(0) = new_extent.into();
            leaf.header_mut().set_entries_count(1);
            Self::set_extent_block_checksum(
                self.read_super_block_cached().metadata_checksum_seed(),
                inode,
                image,
            );

            let mut root = inode.inode.extent_root_mut();
            let entries = root.header().entries_count() as usize;
            *root.extent_index_mut_at(entries) = ExtentIndex::new(start_lblock, new_leaf_home);
            root.header_mut().set_entries_count((entries + 1) as u16);
            let blocks = inode
                .inode
                .fs_block_count()
                .checked_add(count as u64 + 1)
                .ok_or_else(|| Ext4Error::new(ErrCode::EFBIG))?;
            inode.inode.set_fs_block_count(blocks);
            return Ok(());
        }
        if let Some(home) = leaf_home {
            {
                let image = transaction.read(self.block_device.as_ref(), home)?;
                self.verify_transaction_extent_block(inode, &*image)?;
                let node = ExtentNode::from_bytes(&*image);
                self.validate_extent_node(inode.id, &node)?;
                if node.header().depth() != 0 || node.header().entries_count() == 0 {
                    return Err(format_error!(
                        ErrCode::EIO,
                        "invalid append leaf on inode {} at block {}",
                        inode.id,
                        home
                    ));
                }
            }
            let seed = self.read_super_block_cached().metadata_checksum_seed();
            let image = self.transaction_block_for_update(transaction, home)?;
            let new_extent = Extent::new(start_lblock, start_pblock, count as u16);
            {
                let mut node = ExtentNodeMut::from_bytes(image);
                let entries = node.header().entries_count() as usize;
                let last = *node.extent_at(entries - 1);
                if Extent::can_append(&last, &new_extent) {
                    node.extent_mut_at(entries - 1)
                        .set_block_count(last.block_count() + count);
                } else if node.insert_extent(&new_extent, entries).is_err() {
                    return Err(format_error!(
                        ErrCode::ENOTSUP,
                        "External extent leaf requires a split"
                    ));
                }
            }
            Self::set_extent_block_checksum(seed, inode, image);
            let blocks = inode
                .inode
                .fs_block_count()
                .checked_add(count as u64)
                .ok_or_else(|| Ext4Error::new(ErrCode::EFBIG))?;
            inode.inode.set_fs_block_count(blocks);
            Ok(())
        } else {
            self.stage_direct_append_extent(inode, start_lblock, start_pblock, count)
        }
    }

    fn verify_extent_block_checksum(&self, inode_ref: &InodeRef, image: &[u8]) -> Result<()> {
        let sb = self.read_super_block_cached();
        if !sb.has_read_only_compatible_feature(SuperBlock::FEATURE_RO_COMPAT_METADATA_CSUM) {
            return Ok(());
        }
        if image.len() != BLOCK_SIZE {
            return Err(Ext4Error::new(ErrCode::EIO));
        }
        let node = ExtentNode::from_bytes(image);
        let expected_max =
            (BLOCK_SIZE - core::mem::size_of::<ExtentHeader>()) / core::mem::size_of::<Extent>();
        if node.header().max_entries_count() as usize != expected_max {
            return Err(format_error!(
                ErrCode::EIO,
                "invalid extent block capacity on inode {}",
                inode_ref.id
            ));
        }
        let tail_offset = BLOCK_SIZE - core::mem::size_of::<crate::ext4_defs::ExtentTail>();
        let stored = u32::from_le_bytes(
            image[tail_offset..tail_offset + 4]
                .try_into()
                .map_err(|_| Ext4Error::new(ErrCode::EIO))?,
        );
        let calculated = extent_block_checksum(
            sb.metadata_checksum_seed(),
            inode_ref.id,
            inode_ref.inode.generation(),
            image,
        );
        if stored != calculated {
            return Err(format_error!(
                ErrCode::EIO,
                "extent block checksum mismatch on inode {}",
                inode_ref.id
            ));
        }
        Ok(())
    }

    /// Read and authenticate a non-root extent node before interpreting any
    /// header or entry.  Linux performs the equivalent check in
    /// `ext4_extent_block_csum_verify()`.
    fn read_extent_block(&self, inode_ref: &InodeRef, pblock: PBlockId) -> Result<Block> {
        self.ensure_valid_pblock(inode_ref.id, pblock, "extent tree node")?;
        self.validate_data_blocks(pblock, 1)?;
        let block = self.read_block(pblock)?;
        self.prepare_stats.record_extent_io();
        self.verify_extent_block_checksum(inode_ref, &block.data[..])?;
        Ok(block)
    }

    fn verify_transaction_extent_block(&self, inode_ref: &InodeRef, image: &[u8]) -> Result<()> {
        self.verify_extent_block_checksum(inode_ref, image)
    }

    /// Inspect the authoritative right-most extent without changing the tree.
    /// This lets the allocator shorten a removal at a block-group boundary
    /// before asking [`Self::extent_remove_tail_in_transaction`] to mutate it.
    pub(super) fn extent_tail(
        &self,
        transaction: &super::journal_transaction::Transaction<'_>,
        inode_ref: &InodeRef,
    ) -> Result<Option<ExtentTail>> {
        let root = inode_ref.inode.extent_root();
        self.validate_extent_node(inode_ref.id, &root)?;
        if root.header().entries_count() == 0 {
            return Ok(None);
        }

        let mut depth = root.header().depth();
        let mut next = {
            let last = root.header().entries_count() as usize - 1;
            (depth > 0).then(|| root.extent_index_at(last).leaf())
        };
        while let Some(pblock) = next {
            self.ensure_valid_pblock(inode_ref.id, pblock, "extent tail node")?;
            let image = transaction.read(self.block_device.as_ref(), pblock)?;
            self.verify_transaction_extent_block(inode_ref, &*image)?;
            let node = ExtentNode::from_bytes(&*image);
            self.validate_extent_node(inode_ref.id, &node)?;
            if node.header().depth() + 1 != depth {
                return Err(format_error!(
                    ErrCode::EIO,
                    "extent depth mismatch on inode {} at block {}",
                    inode_ref.id,
                    pblock
                ));
            }
            let entries = node.header().entries_count() as usize;
            if entries == 0 {
                return Err(format_error!(
                    ErrCode::EIO,
                    "empty reachable extent node on inode {} at block {}",
                    inode_ref.id,
                    pblock
                ));
            }
            depth = node.header().depth();
            if depth == 0 {
                let extent = *node.extent_at(entries - 1);
                return Ok(Some(Self::tail_description(&extent)));
            }
            next = Some(node.extent_index_at(entries - 1).leaf());
        }

        let extent = *root.extent_at(root.header().entries_count() as usize - 1);
        Ok(Some(Self::tail_description(&extent)))
    }

    fn tail_description(extent: &Extent) -> ExtentTail {
        ExtentTail {
            start_lblock: extent.start_lblock(),
            start_pblock: extent.start_pblock(),
            block_count: extent.block_count(),
            unwritten: extent.is_unwritten(),
        }
    }

    /// Remove at most `max_blocks` from the right edge of exactly one extent.
    ///
    /// Every changed non-root extent block is transaction-private and receives
    /// a checksum for its final image.  Empty right-edge nodes are recursively
    /// detached from their parents and returned to the caller, but are not
    /// themselves staged because their bitmap bits must be cleared in this same
    /// transaction.  The inode's inline root is changed in memory only.
    pub(super) fn extent_remove_tail_in_transaction(
        &self,
        transaction: &mut super::journal_transaction::Transaction<'_>,
        inode_ref: &mut InodeRef,
        max_blocks: u32,
    ) -> Result<Option<ExtentTailRemoval>> {
        if max_blocks == 0 {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }

        let root = inode_ref.inode.extent_root();
        self.validate_extent_node(inode_ref.id, &root)?;
        if root.header().entries_count() == 0 {
            return Ok(None);
        }

        // Store the physical block of every non-root node on the right spine.
        // The corresponding parent is root for path[0], otherwise path[i-1].
        let mut path = Vec::new();
        let mut expected_depth = root.header().depth();
        if expected_depth > 0 {
            let last = root.header().entries_count() as usize - 1;
            let mut pblock = root.extent_index_at(last).leaf();
            loop {
                self.ensure_valid_pblock(inode_ref.id, pblock, "extent tail node")?;
                let image = transaction.read(self.block_device.as_ref(), pblock)?;
                self.verify_transaction_extent_block(inode_ref, &*image)?;
                let node = ExtentNode::from_bytes(&*image);
                self.validate_extent_node(inode_ref.id, &node)?;
                if node.header().depth() + 1 != expected_depth || node.header().entries_count() == 0
                {
                    return Err(format_error!(
                        ErrCode::EIO,
                        "invalid right extent spine on inode {} at block {}",
                        inode_ref.id,
                        pblock
                    ));
                }
                path.push(pblock);
                expected_depth = node.header().depth();
                if expected_depth == 0 {
                    break;
                }
                let last = node.header().entries_count() as usize - 1;
                pblock = node.extent_index_at(last).leaf();
            }
        }

        let tail = self
            .extent_tail(transaction, inode_ref)?
            .ok_or(format_error!(
                ErrCode::EIO,
                "extent tail disappeared on inode {}",
                inode_ref.id
            ))?;
        let remove = min(max_blocks, tail.block_count);
        let result = ExtentTailRemoval {
            start_lblock: tail.start_lblock + tail.block_count - remove,
            start_pblock: tail.start_pblock + (tail.block_count - remove) as PBlockId,
            block_count: remove,
            metadata_blocks: Vec::new(),
        };

        if path.is_empty() {
            let mut root = inode_ref.inode.extent_root_mut();
            Self::trim_leaf_tail(&mut root, remove)?;
            return Ok(Some(result));
        }

        let leaf_pblock = *path.last().unwrap();
        if remove < tail.block_count {
            let image = self.transaction_block_for_update(transaction, leaf_pblock)?;
            let mut leaf = ExtentNodeMut::from_bytes(&mut image[..]);
            Self::trim_leaf_tail(&mut leaf, remove)?;
            Self::set_extent_block_checksum(
                self.read_super_block_cached().metadata_checksum_seed(),
                inode_ref,
                image,
            );
            return Ok(Some(result));
        }

        // A full last extent leaves its leaf empty only when it was that leaf's
        // sole entry.  Otherwise stage the shortened leaf and stop cascading.
        let leaf_entries = {
            let image = transaction.read(self.block_device.as_ref(), leaf_pblock)?;
            ExtentNode::from_bytes(&*image).header().entries_count()
        };
        if leaf_entries > 1 {
            let image = self.transaction_block_for_update(transaction, leaf_pblock)?;
            let mut leaf = ExtentNodeMut::from_bytes(&mut image[..]);
            leaf.remove_last_entry();
            Self::set_extent_block_checksum(
                self.read_super_block_cached().metadata_checksum_seed(),
                inode_ref,
                image,
            );
            return Ok(Some(result));
        }

        let mut result = result;
        result.metadata_blocks.push(leaf_pblock);
        let mut child_empty = true;
        for level in (0..path.len() - 1).rev() {
            if !child_empty {
                break;
            }
            let pblock = path[level];
            let entries = {
                let image = transaction.read(self.block_device.as_ref(), pblock)?;
                ExtentNode::from_bytes(&*image).header().entries_count()
            };
            if entries == 0 {
                return Err(format_error!(
                    ErrCode::EIO,
                    "empty extent parent on inode {}",
                    inode_ref.id
                ));
            }
            child_empty = entries == 1;
            if child_empty {
                result.metadata_blocks.push(pblock);
            } else {
                let image = self.transaction_block_for_update(transaction, pblock)?;
                let mut node = ExtentNodeMut::from_bytes(&mut image[..]);
                node.remove_last_entry();
                Self::set_extent_block_checksum(
                    self.read_super_block_cached().metadata_checksum_seed(),
                    inode_ref,
                    image,
                );
            }
        }

        if child_empty {
            let mut root = inode_ref.inode.extent_root_mut();
            if !root.remove_last_entry() {
                return Err(format_error!(
                    ErrCode::EIO,
                    "empty extent root on inode {}",
                    inode_ref.id
                ));
            }
            if root.header().entries_count() == 0 {
                // Every node below the root was detached.  Restore the canonical
                // empty inline leaf root; this also drops the obsolete depth.
                root.init(0, 0);
            }
        }
        Ok(Some(result))
    }

    fn trim_leaf_tail(node: &mut ExtentNodeMut<'_>, remove: u32) -> Result<()> {
        let entries = node.header().entries_count() as usize;
        if node.header().depth() != 0 || entries == 0 {
            return Err(Ext4Error::new(ErrCode::EIO));
        }
        let last = node.extent_mut_at(entries - 1);
        let old_len = last.block_count();
        if remove == 0 || remove > old_len {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }
        if remove == old_len {
            node.remove_last_entry();
        } else {
            last.set_block_count(old_len - remove);
        }
        Ok(())
    }

    fn set_extent_block_checksum(
        seed: MetadataChecksumSeed,
        inode_ref: &InodeRef,
        image: &mut [u8; BLOCK_SIZE],
    ) {
        let tail_offset = BLOCK_SIZE - core::mem::size_of::<crate::ext4_defs::ExtentTail>();
        let checksum =
            extent_block_checksum(seed, inode_ref.id, inode_ref.inode.generation(), image);
        image[tail_offset..tail_offset + 4].copy_from_slice(&checksum.to_le_bytes());
    }

    /// Write an extent block to disk with checksum in the extent tail.
    fn write_extent_block(&self, block: &mut Block, inode_ref: &InodeRef) -> Result<()> {
        let tail_offset = BLOCK_SIZE - core::mem::size_of::<crate::ext4_defs::ExtentTail>();
        let csum = extent_block_checksum(
            self.read_super_block_cached().metadata_checksum_seed(),
            inode_ref.id,
            inode_ref.inode.generation(),
            &*block.data,
        );
        // Write checksum into the tail
        block.data[tail_offset..tail_offset + 4].copy_from_slice(&csum.to_le_bytes());
        self.write_block(block)
            .inspect(|_| self.prepare_stats.record_extent_io())
    }
}

#[derive(Debug)]
struct ExtentSearchStep {
    /// The physical block where this extent node is stored.
    /// For a root node, this field is 0.
    pblock: PBlockId,
    /// Index of the found `ExtentIndex` or `Extent` if found, the position where the
    /// `ExtentIndex` or `Extent` should be inserted if not found.
    index: core::result::Result<usize, usize>,
}

impl ExtentSearchStep {
    /// Create a new extent search step
    fn new(pblock: PBlockId, index: core::result::Result<usize, usize>) -> Self {
        Self { pblock, index }
    }
}

impl Ext4 {
    /// Given a logic block id, find the corresponding fs block id.
    pub(super) fn extent_query(&self, inode_ref: &InodeRef, iblock: LBlockId) -> Result<PBlockId> {
        let path = self.find_extent(inode_ref, iblock)?;
        // Leaf is the last element of the path
        let leaf = path.last().ok_or(format_error!(
            ErrCode::EIO,
            "extent_query: empty extent search path on inode {}",
            inode_ref.id
        ))?;
        if let Ok(index) = leaf.index {
            // Note: block data must be defined here to keep it alive
            let block_data: Block;
            let ex_node = if leaf.pblock != 0 {
                // Load the extent node
                self.ensure_valid_pblock(inode_ref.id, leaf.pblock, "extent leaf node")?;
                block_data = self.read_extent_block(inode_ref, leaf.pblock)?;
                // Load the next extent header
                ExtentNode::from_bytes(&*block_data.data)
            } else {
                // Root node
                inode_ref.inode.extent_root()
            };
            let ex = ex_node.extent_at(index);
            let pblock = ex.start_pblock() + (iblock - ex.start_lblock()) as PBlockId;
            self.ensure_valid_pblock(inode_ref.id, pblock, "extent data block")?;
            self.validate_data_blocks(pblock, 1)?;
            Ok(pblock)
        } else {
            Err(format_error!(
                ErrCode::ENOENT,
                "extent_query: inode {} query iblock {} not found",
                inode_ref.id,
                iblock
            ))
        }
    }

    /// Given a logic block id, find the corresponding fs block id.
    /// Create a new extent if not found.
    pub(super) fn extent_query_or_create(
        &self,
        inode_ref: &mut InodeRef,
        iblock: LBlockId,
        block_count: u32,
    ) -> Result<PBlockId> {
        self.extent_query_or_create_initialized(inode_ref, iblock, block_count, None)
    }

    pub(super) fn extent_query_or_create_initialized(
        &self,
        inode_ref: &mut InodeRef,
        iblock: LBlockId,
        block_count: u32,
        initial_image: Option<Box<[u8; BLOCK_SIZE]>>,
    ) -> Result<PBlockId> {
        let path = self.find_extent(inode_ref, iblock)?;
        // Leaf is the last element of the path
        let leaf = path.last().ok_or(format_error!(
            ErrCode::EIO,
            "extent_query_or_create: empty extent search path on inode {}",
            inode_ref.id
        ))?;
        // Note: block data must be defined here to keep it alive
        let mut block_data: Block;
        let ex_node = if leaf.pblock != 0 {
            block_data = self.read_extent_block(inode_ref, leaf.pblock)?;
            ExtentNodeMut::from_bytes(&mut *block_data.data)
        } else {
            // Root node
            inode_ref.inode.extent_root_mut()
        };
        match leaf.index {
            Ok(index) => {
                // Found, return the corresponding fs block id
                let ex = ex_node.extent_at(index);
                Ok(ex.start_pblock() + (iblock - ex.start_lblock()) as PBlockId)
            }
            Err(insert_pos) => {
                // Not found, check if we can merge with the previous extent
                // before allocating. We extract the merge candidate info here
                // while we still hold ex_node, then release it before allocating.
                let merge_candidate = if insert_pos > 0 {
                    let prev = ex_node.extent_at(insert_pos - 1);
                    Some((prev.start_lblock(), prev.start_pblock(), prev.block_count()))
                } else {
                    None
                };
                let leaf_pblock = leaf.pblock;

                // ex_node borrow is released here when it goes out of scope

                let block_count = min(block_count, MAX_BLOCKS - iblock);
                // Allocate physical block
                // Data initialization must be durable before the new extent
                // becomes reachable from either the inode root or an external
                // extent node.  Metadata-node allocations below continue to
                // use alloc_block directly.
                let fblock = if let Some(image) = initial_image {
                    self.alloc_initialized_data_block(inode_ref, image)?
                } else {
                    self.alloc_zeroed_data_block(inode_ref)?
                };
                let new_ext = Extent::new(iblock, fblock, block_count as u16);

                // Try to merge with the previous extent
                if let Some((prev_lblock, prev_pblock, prev_count)) = merge_candidate {
                    let prev_as_ext = Extent::new(prev_lblock, prev_pblock, prev_count as u16);
                    if Extent::can_append(&prev_as_ext, &new_ext) {
                        // Merge: extend the previous extent's block_count
                        let merged_count = (prev_count + new_ext.block_count()) as u16;
                        let merged = Extent::new(prev_lblock, prev_pblock, merged_count);
                        let prev_idx = insert_pos - 1;
                        if leaf_pblock != 0 {
                            // Re-read the leaf block and update
                            let mut leaf_block = self.read_extent_block(inode_ref, leaf_pblock)?;
                            let mut leaf_node = ExtentNodeMut::from_bytes(&mut *leaf_block.data);
                            *leaf_node.extent_mut_at(prev_idx) = merged;
                            self.write_extent_block(&mut leaf_block, inode_ref)?;
                        } else {
                            // Root node
                            let mut root = inode_ref.inode.extent_root_mut();
                            *root.extent_mut_at(prev_idx) = merged;
                            self.write_inode_with_csum(inode_ref)?;
                        }
                        return Ok(fblock);
                    }
                }

                // Cannot merge, insert as a new extent entry
                self.insert_extent(inode_ref, &path, &new_ext)?;
                Ok(fblock)
            }
        }
    }

    /// Get the next logical block id to append (= one past the last allocated data block).
    /// This is computed from the extent tree, not from i_blocks, because i_blocks
    /// may include tree metadata blocks.
    pub(super) fn extent_next_data_lblock(&self, inode_ref: &InodeRef) -> Result<LBlockId> {
        let ex_node = inode_ref.inode.extent_root();
        if ex_node.header().entries_count() == 0 {
            return Ok(0);
        }
        self.extent_last_lblock_recursive(inode_ref, &ex_node)
    }

    fn extent_last_lblock_recursive(
        &self,
        inode_ref: &InodeRef,
        ex_node: &ExtentNode,
    ) -> Result<LBlockId> {
        let last = ex_node.header().entries_count() as usize - 1;
        if ex_node.header().depth() == 0 {
            // Leaf: return start_lblock + block_count of the last extent
            let ex = ex_node.extent_at(last);
            Ok(ex.start_lblock() + ex.block_count())
        } else {
            // Non-leaf: descend into the last child
            let ex_idx = ex_node.extent_index_at(last);
            let child_block = self.read_extent_block(inode_ref, ex_idx.leaf())?;
            let child_node = ExtentNode::from_bytes(&*child_block.data);
            self.extent_last_lblock_recursive(inode_ref, &child_node)
        }
    }

    /// Get all data blocks recorded in the extent tree
    pub(super) fn extent_all_data_blocks(&self, inode_ref: &InodeRef) -> Result<Vec<PBlockId>> {
        let mut pblocks = Vec::new();
        let ex_node = inode_ref.inode.extent_root();
        self.get_all_pblocks_recursive(inode_ref, &ex_node, &mut pblocks)?;
        Ok(pblocks)
    }

    /// Get all physical blocks for saving the extent tree
    pub(super) fn extent_all_tree_blocks(&self, inode_ref: &InodeRef) -> Result<Vec<PBlockId>> {
        let mut pblocks = Vec::new();
        let ex_node = inode_ref.inode.extent_root();
        self.get_all_nodes_recursive(inode_ref, &ex_node, &mut pblocks)?;
        Ok(pblocks)
    }

    pub(super) fn validate_complete_extent_tree(&self, inode_ref: &InodeRef) -> Result<()> {
        let root = inode_ref.inode.extent_root();
        self.validate_extent_node(inode_ref.id, &root)?;
        let mut visited = BTreeSet::new();
        self.validate_extent_subtree(inode_ref, &root, &mut visited)
    }

    fn validate_extent_subtree(
        &self,
        inode_ref: &InodeRef,
        node: &ExtentNode<'_>,
        visited: &mut BTreeSet<PBlockId>,
    ) -> Result<()> {
        if node.header().depth() == 0 {
            return Ok(());
        }
        for index in 0..node.header().entries_count() as usize {
            let pblock = node.extent_index_at(index).leaf();
            if !visited.insert(pblock) {
                return Err(format_error!(
                    ErrCode::EIO,
                    "duplicate or cyclic extent node on inode {}",
                    inode_ref.id
                ));
            }
            let child_block = self.read_extent_block(inode_ref, pblock)?;
            let child = ExtentNode::from_bytes(&*child_block.data);
            self.validate_extent_node(inode_ref.id, &child)?;
            if child.header().depth() + 1 != node.header().depth() {
                return Err(format_error!(
                    ErrCode::EIO,
                    "extent depth mismatch on inode {} at block {}",
                    inode_ref.id,
                    pblock
                ));
            }
            self.validate_extent_subtree(inode_ref, &child, visited)?;
        }
        Ok(())
    }

    fn get_all_pblocks_recursive(
        &self,
        inode_ref: &InodeRef,
        ex_node: &ExtentNode,
        pblocks: &mut Vec<PBlockId>,
    ) -> Result<()> {
        if ex_node.header().depth() == 0 {
            // Leaf
            for i in 0..ex_node.header().entries_count() as usize {
                let ex = ex_node.extent_at(i);
                for j in 0..ex.block_count() {
                    pblocks.push(ex.start_pblock() + j as PBlockId);
                }
            }
        } else {
            // Non-leaf
            for i in 0..ex_node.header().entries_count() as usize {
                let ex_idx = ex_node.extent_index_at(i);
                let child_block = self.read_extent_block(inode_ref, ex_idx.leaf())?;
                let child_node = ExtentNode::from_bytes(&*child_block.data);
                self.validate_extent_node(inode_ref.id, &child_node)?;
                self.get_all_pblocks_recursive(inode_ref, &child_node, pblocks)?;
            }
        }
        Ok(())
    }

    fn get_all_nodes_recursive(
        &self,
        inode_ref: &InodeRef,
        ex_node: &ExtentNode,
        pblocks: &mut Vec<PBlockId>,
    ) -> Result<()> {
        if ex_node.header().depth() != 0 {
            // Non-leaf
            for i in 0..ex_node.header().entries_count() as usize {
                let ex_idx = ex_node.extent_index_at(i);
                pblocks.push(ex_idx.leaf());
                let child_block = self.read_extent_block(inode_ref, ex_idx.leaf())?;
                let child_node = ExtentNode::from_bytes(&*child_block.data);
                self.validate_extent_node(inode_ref.id, &child_node)?;
                self.get_all_nodes_recursive(inode_ref, &child_node, pblocks)?;
            }
        }
        Ok(())
    }

    /// Find the given logic block id in the extent tree, return the search path
    fn find_extent(&self, inode_ref: &InodeRef, iblock: LBlockId) -> Result<Vec<ExtentSearchStep>> {
        let mut path: Vec<ExtentSearchStep> = Vec::new();
        let mut ex_node = inode_ref.inode.extent_root();
        let mut pblock = 0;
        let mut block_data: Block;
        self.validate_extent_node(inode_ref.id, &ex_node)?;

        // Go until leaf
        while ex_node.header().depth() > 0 {
            let index = ex_node.search_extent_index(iblock).map_err(|_| {
                format_error!(
                    ErrCode::EIO,
                    "find_extent: inode {} failed to locate extent index for iblock {}",
                    inode_ref.id,
                    iblock
                )
            })?;
            path.push(ExtentSearchStep::new(pblock, Ok(index)));
            // Get the target extent index
            let ex_idx = ex_node.extent_index_at(index);
            // Load the next extent node
            let next = ex_idx.leaf();
            self.ensure_valid_pblock(inode_ref.id, next, "extent index target")?;
            // Note: block data cannot be released until the next assigment
            block_data = self.read_extent_block(inode_ref, next)?;
            // Load the next extent header
            ex_node = ExtentNode::from_bytes(&*block_data.data);
            self.validate_extent_node(inode_ref.id, &ex_node)?;
            pblock = next;
        }
        // Leaf
        let index = ex_node.search_extent(iblock);
        path.push(ExtentSearchStep::new(pblock, index));

        Ok(path)
    }

    /// Insert a new extent into the extent tree.
    fn insert_extent(
        &self,
        inode_ref: &mut InodeRef,
        path: &[ExtentSearchStep],
        new_ext: &Extent,
    ) -> Result<()> {
        let leaf = path.last().ok_or(format_error!(
            ErrCode::EIO,
            "insert_extent: empty extent search path on inode {}",
            inode_ref.id
        ))?;
        // 1. Check If leaf is root
        if leaf.pblock == 0 {
            let mut leaf_node = inode_ref.inode.extent_root_mut();
            // Insert the extent
            let res = leaf_node.insert_extent(new_ext, leaf.index.unwrap_err());
            self.write_inode_with_csum(inode_ref)?;
            // Handle split
            return if let Err(split) = res {
                self.split_root(inode_ref, &split)
            } else {
                Ok(())
            };
        }
        // 2. Leaf is not root, load the leaf node
        let mut leaf_block = self.read_extent_block(inode_ref, leaf.pblock)?;
        let mut leaf_node = ExtentNodeMut::from_bytes(&mut *leaf_block.data);
        // Insert the extent
        let res = leaf_node.insert_extent(new_ext, leaf.index.unwrap_err());
        self.write_extent_block(&mut leaf_block, inode_ref)?;
        // Handle split
        if let Err(mut split) = res {
            // Handle split until root
            for parent in path.iter().rev().skip(1) {
                // The split node is at `parent.index.unwrap()`
                // Call `self.split` to store the split part and update `parent`
                let parent_index = parent.index.map_err(|_| {
                    format_error!(
                        ErrCode::EIO,
                        "insert_extent: invalid parent extent index on inode {}",
                        inode_ref.id
                    )
                })?;
                let res = self.split(inode_ref, parent.pblock, parent_index, &split)?;
                // Handle split again
                if let Err(split_again) = res {
                    // Insertion to parent also causes split, continue to solve
                    split = split_again;
                } else {
                    return Ok(());
                }
            }
            // Root node needs to be split
            self.split_root(inode_ref, &split)
        } else {
            Ok(())
        }
    }

    /// Split an extent node. Given the block id where the parent node is
    /// stored, and the child position that `parent_node.extent_at(child_pos)`
    /// points to the child.
    ///
    /// The child node has already been split by calling `insert_extent` or
    /// `insert_extent_index`, and the split part is stored in `split`.
    /// This function will create a new leaf node to store the split part.
    fn split(
        &self,
        inode_ref: &mut InodeRef,
        parent_pblock: PBlockId,
        child_pos: usize,
        split: &[FakeExtent],
    ) -> Result<core::result::Result<(), Vec<FakeExtent>>> {
        let right_bid = self.alloc_block(inode_ref)?;
        let mut right_block = self.read_block(right_bid)?;
        let mut right_node = ExtentNodeMut::from_bytes(&mut *right_block.data);

        // Insert the split half to right node
        right_node.init(0, 0);
        for (i, fake_extent) in split.iter().enumerate() {
            *right_node.fake_extent_mut_at(i) = *fake_extent;
        }
        right_node
            .header_mut()
            .set_entries_count(split.len() as u16);
        // Create an extent index pointing to the right node
        let extent_index =
            ExtentIndex::new(right_node.extent_index_at(0).start_lblock(), right_bid);

        let res;
        let parent_depth;
        if parent_pblock == 0 {
            // Parent is root
            let mut parent_node = inode_ref.inode.extent_root_mut();
            parent_depth = parent_node.header().depth();
            res = parent_node.insert_extent_index(&extent_index, child_pos + 1);
            self.write_inode_with_csum(inode_ref)?;
        } else {
            // Parent is not root
            let mut parent_block = self.read_extent_block(inode_ref, parent_pblock)?;
            let mut parent_node = ExtentNodeMut::from_bytes(&mut *parent_block.data);
            parent_depth = parent_node.header().depth();
            res = parent_node.insert_extent_index(&extent_index, child_pos + 1);
            self.write_extent_block(&mut parent_block, inode_ref)?;
        }

        // Right node is the child of parent, so its depth is 1 less than parent
        right_node.header_mut().set_depth(parent_depth - 1);
        self.write_extent_block(&mut right_block, inode_ref)?;

        Ok(res)
    }

    /// Split the root extent node. This function will create 2 new leaf
    /// nodes and increase the height of the tree by 1.
    ///
    /// The root node has already been split by calling `insert_extent` or
    /// `insert_extent_index`, and the split part is stored in `split`.
    /// This function will create a new leaf node to store the split part.
    fn split_root(&self, inode_ref: &mut InodeRef, split: &[FakeExtent]) -> Result<()> {
        // Create left and right blocks
        let l_bid = self.alloc_block(inode_ref)?;
        let r_bid = self.alloc_block(inode_ref)?;
        let mut l_block = self.read_block(l_bid)?;
        let mut r_block = self.read_block(r_bid)?;

        // Load root, left, right nodes
        let mut root = inode_ref.inode.extent_root_mut();
        let mut left = ExtentNodeMut::from_bytes(&mut *l_block.data);
        let mut right = ExtentNodeMut::from_bytes(&mut *r_block.data);

        // Copy the left half to left node
        left.init(root.header().depth(), 0);
        for i in 0..root.header().entries_count() as usize {
            *left.fake_extent_mut_at(i) = *root.fake_extent_at(i);
        }
        left.header_mut()
            .set_entries_count(root.header().entries_count());

        // Copy the right half to right node
        right.init(root.header().depth(), 0);
        for (i, fake_extent) in split.iter().enumerate() {
            *right.fake_extent_mut_at(i) = *fake_extent;
        }
        right.header_mut().set_entries_count(split.len() as u16);

        // Update the root node
        let depth = root.header().depth() + 1;
        root.header_mut().set_depth(depth);
        root.header_mut().set_entries_count(2);
        *root.extent_index_mut_at(0) = ExtentIndex::new(left.extent_at(0).start_lblock(), l_bid);
        *root.extent_index_mut_at(1) = ExtentIndex::new(right.extent_at(0).start_lblock(), r_bid);

        // Sync to disk
        self.write_extent_block(&mut l_block, inode_ref)?;
        self.write_extent_block(&mut r_block, inode_ref)?;
        self.write_inode_with_csum(inode_ref)?;

        Ok(())
    }

    fn validate_extent_node(&self, inode_id: InodeId, ex_node: &ExtentNode) -> Result<()> {
        const MAX_EXTENT_DEPTH: u16 = 5;
        let header = ex_node.header();
        if !header.check_magic() {
            return Err(format_error!(
                ErrCode::EIO,
                "extent header magic invalid on inode {}",
                inode_id
            ));
        }
        if header.depth() > MAX_EXTENT_DEPTH {
            return Err(format_error!(
                ErrCode::EIO,
                "extent depth {} too large on inode {}",
                header.depth(),
                inode_id
            ));
        }
        if header.entries_count() > header.max_entries_count() {
            return Err(format_error!(
                ErrCode::EIO,
                "extent entries {} > max {} on inode {}",
                header.entries_count(),
                header.max_entries_count(),
                inode_id
            ));
        }
        if header.max_entries_count() as usize > ex_node.entry_capacity()
            || header.entries_count() as usize > ex_node.entry_capacity()
        {
            return Err(format_error!(
                ErrCode::EIO,
                "extent header exceeds node capacity on inode {}",
                inode_id
            ));
        }
        let entries = header.entries_count() as usize;
        if header.depth() > 0 {
            if entries == 0 {
                return Err(format_error!(
                    ErrCode::EIO,
                    "non-leaf extent node has no entries on inode {}",
                    inode_id
                ));
            }
            let mut prev_lblock = None;
            for i in 0..entries {
                let ex_idx = ex_node.extent_index_at(i);
                let cur = ex_idx.start_lblock();
                if let Some(prev) = prev_lblock {
                    if cur <= prev {
                        return Err(format_error!(
                            ErrCode::EIO,
                            "extent index order invalid at pos {} on inode {}",
                            i,
                            inode_id
                        ));
                    }
                }
                prev_lblock = Some(cur);
            }
        } else {
            let mut prev_end_lblock = None;
            for i in 0..entries {
                let ex = ex_node.extent_at(i);
                let cur_start = ex.start_lblock();
                let cur_len = ex.block_count();
                if cur_len == 0 {
                    return Err(format_error!(
                        ErrCode::EIO,
                        "extent len is 0 at pos {} on inode {}",
                        i,
                        inode_id
                    ));
                }
                if let Some(prev_end) = prev_end_lblock {
                    if cur_start < prev_end {
                        return Err(format_error!(
                            ErrCode::EIO,
                            "extent overlap/order invalid at pos {} on inode {}",
                            i,
                            inode_id
                        ));
                    }
                }
                let cur_end = cur_start.checked_add(cur_len).ok_or(format_error!(
                    ErrCode::EIO,
                    "extent end overflow at pos {} on inode {}",
                    i,
                    inode_id
                ))?;
                prev_end_lblock = Some(cur_end);
            }
        }
        Ok(())
    }

    fn ensure_valid_pblock(&self, inode_id: InodeId, pblock: PBlockId, what: &str) -> Result<()> {
        let sb = self.read_super_block_cached();
        let block_count = sb.block_count();
        if pblock >= block_count {
            return Err(format_error!(
                ErrCode::EIO,
                "inode {} {} out of range: pblock={}, block_count={}",
                inode_id,
                what,
                pblock,
                block_count
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubBlockDevice {
        sb_block: Block,
    }

    impl StubBlockDevice {
        fn with_block_count(block_count: u32) -> Self {
            let mut data = [0u8; BLOCK_SIZE];
            let off = BASE_OFFSET;
            data[off..off + 4].copy_from_slice(&block_count.to_le_bytes());
            Self {
                sb_block: Block::new(0, Box::new(data)),
            }
        }
    }

    impl BlockDevice for StubBlockDevice {
        fn read_block(&self, block_id: PBlockId) -> Result<Block> {
            if block_id == 0 {
                Ok(self.sb_block.clone())
            } else {
                Ok(Block::new(block_id, Box::new([0u8; BLOCK_SIZE])))
            }
        }

        fn write_block(&self, _block: &Block) -> Result<()> {
            Ok(())
        }

        fn flush(&self) -> Result<()> {
            Ok(())
        }
        fn supports_reliable_flush(&self) -> bool {
            true
        }
    }

    fn make_test_fs(block_count: u32) -> Ext4 {
        let block_device = Arc::new(StubBlockDevice::with_block_count(block_count));
        let block = block_device.read_block(0).unwrap();
        let sb = block.read_offset_as::<SuperBlock>(BASE_OFFSET);
        Ext4 {
            block_device,
            cached_super_block: spin::Mutex::new(sb),
            cached_block_groups: Vec::new(),
            system_metadata_ranges: Vec::new(),
            inode_cache: spin::Mutex::new(crate::ext4::InodeCache::new(16)),
            alloc_lock: spin::Mutex::new(crate::ext4::AllocationState::new().unwrap()),
            namespace_lock: spin::Mutex::new(()),
            metadata_mutation_barrier: crate::ext4::MetadataMutationGate::new(),
            poisoned: spin::Mutex::new(None),
            delalloc_mapper_authority_issued: core::sync::atomic::AtomicBool::new(false),
            metadata_mode: crate::ext4::MetadataMutationMode::ReadOnly,
            write_barrier: true,
            direct_restore_clean: false,
            inode_mutation_locks: (0..crate::ext4::INODE_MUTATION_LOCK_SHARDS)
                .map(|_| spin::Mutex::new(()))
                .collect(),
            prepare_stats: crate::ext4::PrepareStats::new(),
        }
    }

    fn make_metadata_csum_test_fs(block_count: u32) -> Ext4 {
        let mut device = StubBlockDevice::with_block_count(block_count);
        // ext4_super_block: s_feature_ro_compat at byte 100, UUID at 104.
        let base = BASE_OFFSET;
        device.sb_block.data[base + 100..base + 104]
            .copy_from_slice(&SuperBlock::FEATURE_RO_COMPAT_METADATA_CSUM.to_le_bytes());
        device.sb_block.data[base + 104..base + 120].copy_from_slice(&[0x5a; 16]);
        let block_device = Arc::new(device);
        let sb = block_device
            .read_block(0)
            .unwrap()
            .read_offset_as::<SuperBlock>(BASE_OFFSET);
        Ext4 {
            block_device,
            cached_super_block: spin::Mutex::new(sb),
            cached_block_groups: Vec::new(),
            system_metadata_ranges: Vec::new(),
            inode_cache: spin::Mutex::new(crate::ext4::InodeCache::new(16)),
            alloc_lock: spin::Mutex::new(crate::ext4::AllocationState::new().unwrap()),
            namespace_lock: spin::Mutex::new(()),
            metadata_mutation_barrier: crate::ext4::MetadataMutationGate::new(),
            poisoned: spin::Mutex::new(None),
            delalloc_mapper_authority_issued: core::sync::atomic::AtomicBool::new(false),
            metadata_mode: crate::ext4::MetadataMutationMode::ReadOnly,
            write_barrier: true,
            direct_restore_clean: false,
            inode_mutation_locks: (0..crate::ext4::INODE_MUTATION_LOCK_SHARDS)
                .map(|_| spin::Mutex::new(()))
                .collect(),
            prepare_stats: crate::ext4::PrepareStats::new(),
        }
    }

    #[test]
    fn ensure_valid_pblock_rejects_out_of_range() {
        let fs = make_test_fs(16);
        let err = fs.ensure_valid_pblock(2, 16, "test").unwrap_err();
        assert_eq!(err.code(), ErrCode::EIO);
    }

    #[test]
    fn extent_block_tail_corruption_is_rejected_with_metadata_csum() {
        let fs = make_metadata_csum_test_fs(1024);
        let mut inode = InodeRef::new(17, Box::new(Inode::default()));
        inode.inode.set_generation(23);
        let mut image = [0u8; BLOCK_SIZE];
        ExtentNodeMut::from_bytes(&mut image).init(0, 0);
        Ext4::set_extent_block_checksum(
            fs.read_super_block_cached().metadata_checksum_seed(),
            &inode,
            &mut image,
        );
        fs.verify_extent_block_checksum(&inode, &image).unwrap();

        image[BLOCK_SIZE - 1] ^= 0x80;
        let err = fs
            .verify_extent_block_checksum(&inode, &image)
            .expect_err("damaged extent tail must fail authentication");
        assert_eq!(err.code(), ErrCode::EIO);
    }

    #[test]
    fn validate_extent_node_rejects_overlapped_leaf_extents() {
        let fs = make_test_fs(1024);
        let mut raw = [0u8; 60];
        let mut node = ExtentNodeMut::from_bytes(&mut raw);
        node.init(0, 0);
        node.header_mut().set_entries_count(2);
        *node.extent_mut_at(0) = Extent::new(10, 100, 4);
        *node.extent_mut_at(1) = Extent::new(12, 200, 2);
        let err = fs
            .validate_extent_node(3, &node.as_immut())
            .expect_err("overlap must be rejected");
        assert_eq!(err.code(), ErrCode::EIO);
    }

    #[test]
    fn validate_extent_node_rejects_unsorted_index() {
        let fs = make_test_fs(1024);
        let mut raw = [0u8; 60];
        let mut node = ExtentNodeMut::from_bytes(&mut raw);
        node.init(1, 0);
        node.header_mut().set_entries_count(2);
        *node.extent_index_mut_at(0) = ExtentIndex::new(10, 100);
        *node.extent_index_mut_at(1) = ExtentIndex::new(10, 200);
        let err = fs
            .validate_extent_node(4, &node.as_immut())
            .expect_err("unsorted index must be rejected");
        assert_eq!(err.code(), ErrCode::EIO);
    }

    #[test]
    fn validate_extent_node_rejects_header_larger_than_inline_storage() {
        let fs = make_test_fs(1024);
        let mut raw = [0u8; 60];
        ExtentNodeMut::from_bytes(&mut raw).init(0, 0);
        raw[2..4].copy_from_slice(&5u16.to_le_bytes());
        raw[4..6].copy_from_slice(&5u16.to_le_bytes());
        let node = ExtentNode::from_bytes(&raw);

        let error = fs.validate_extent_node(5, &node).unwrap_err();

        assert_eq!(error.code(), ErrCode::EIO);
    }

    #[test]
    fn direct_append_rejects_unwritten_tail() {
        let fs = make_test_fs(1024);
        let mut inode = InodeRef::new(6, Box::new(Inode::default()));
        inode.inode.extent_init();
        let mut extent = Extent::new(0, 100, 16);
        extent.mark_unwritten();
        inode
            .inode
            .extent_root_mut()
            .insert_extent(&extent, 0)
            .unwrap();

        assert!(fs.direct_append_shape(&inode, 16, 16).unwrap().is_none());
    }

    #[test]
    fn trim_depth_zero_partial_extent_preserves_prefix() {
        let mut raw = [0u8; 60];
        let mut node = ExtentNodeMut::from_bytes(&mut raw);
        node.init(0, 0);
        node.header_mut().set_entries_count(1);
        *node.extent_mut_at(0) = Extent::new(7, 100, 9);

        Ext4::trim_leaf_tail(&mut node, 4).unwrap();

        assert_eq!(node.header().entries_count(), 1);
        assert_eq!(node.extent_at(0).start_lblock(), 7);
        assert_eq!(node.extent_at(0).start_pblock(), 100);
        assert_eq!(node.extent_at(0).block_count(), 5);
    }

    #[test]
    fn trim_multi_extent_removes_only_rightmost_extent() {
        let mut raw = [0u8; 60];
        let mut node = ExtentNodeMut::from_bytes(&mut raw);
        node.init(0, 0);
        node.header_mut().set_entries_count(3);
        *node.extent_mut_at(0) = Extent::new(0, 100, 2);
        *node.extent_mut_at(1) = Extent::new(4, 200, 3);
        *node.extent_mut_at(2) = Extent::new(20, 300, 1);

        Ext4::trim_leaf_tail(&mut node, 1).unwrap();

        assert_eq!(node.header().entries_count(), 2);
        assert_eq!(node.extent_at(0).start_pblock(), 100);
        assert_eq!(node.extent_at(1).start_pblock(), 200);
        assert_eq!(node.extent_at(1).block_count(), 3);
    }

    #[test]
    fn trim_partial_unwritten_extent_preserves_unwritten_state() {
        let mut raw = [0u8; 60];
        let mut node = ExtentNodeMut::from_bytes(&mut raw);
        node.init(0, 0);
        node.header_mut().set_entries_count(1);
        *node.extent_mut_at(0) = Extent::new(0, 100, 8);
        node.extent_mut_at(0).mark_unwritten();

        Ext4::trim_leaf_tail(&mut node, 3).unwrap();

        assert_eq!(node.extent_at(0).block_count(), 5);
        assert!(node.extent_at(0).is_unwritten());
    }

    #[test]
    fn depth_greater_than_zero_detaches_only_empty_right_spine() {
        let mut root_raw = [0u8; 60];
        let mut root = ExtentNodeMut::from_bytes(&mut root_raw);
        root.init(2, 0);
        root.header_mut().set_entries_count(2);
        *root.extent_index_mut_at(0) = ExtentIndex::new(0, 10);
        *root.extent_index_mut_at(1) = ExtentIndex::new(100, 20);

        let mut parent_raw = [0u8; BLOCK_SIZE];
        let mut parent = ExtentNodeMut::from_bytes(&mut parent_raw);
        parent.init(1, 0);
        parent.header_mut().set_entries_count(1);
        *parent.extent_index_mut_at(0) = ExtentIndex::new(100, 30);

        let mut leaf_raw = [0u8; BLOCK_SIZE];
        let mut leaf = ExtentNodeMut::from_bytes(&mut leaf_raw);
        leaf.init(0, 0);
        leaf.header_mut().set_entries_count(1);
        *leaf.extent_mut_at(0) = Extent::new(100, 500, 1);

        Ext4::trim_leaf_tail(&mut leaf, 1).unwrap();
        assert_eq!(leaf.header().entries_count(), 0);
        assert!(parent.remove_last_entry());
        assert_eq!(parent.header().entries_count(), 0);
        assert!(root.remove_last_entry());
        assert_eq!(root.header().entries_count(), 1);
        assert_eq!(root.extent_index_at(0).leaf(), 10);
    }

    #[test]
    fn right_spine_projection_promotes_full_inline_root_once() {
        let mut projection = ExtentRightSpineProjection {
            next_lblock: 4,
            counts: vec![4],
            capacities: vec![4],
            external_capacity: 340,
            inline_root_capacity: 4,
        };

        assert_eq!(projection.append_nonmerge_at(9, 1).unwrap(), 1);
        assert_eq!(projection.next_lblock, 10);
        assert_eq!(projection.counts, vec![5, 1]);
        assert_eq!(projection.append_nonmerge_at(10, 1).unwrap(), 0);
        assert_eq!(projection.counts, vec![6, 1]);
    }

    #[test]
    fn right_spine_projection_counts_full_cascade_and_rejects_reverse() {
        let mut projection = ExtentRightSpineProjection {
            next_lblock: 700,
            counts: vec![340, 340, 4],
            capacities: vec![340, 340, 4],
            external_capacity: 340,
            inline_root_capacity: 4,
        };

        assert_eq!(projection.append_nonmerge_at(900, 1).unwrap(), 3);
        assert_eq!(projection.next_lblock, 901);
        assert_eq!(projection.counts, vec![1, 1, 5, 1]);
        let frozen = projection.clone();
        assert_eq!(
            projection.append_nonmerge_at(899, 1).unwrap_err().code(),
            ErrCode::EINVAL
        );
        assert_eq!(projection, frozen);
    }

    #[test]
    fn right_spine_plan_prefers_tail_merge_over_reserved_split_nodes() {
        let plan = RightSpineAppendPlan {
            start_lblock: 7,
            preferred_first: Some(107),
            path: vec![11, 12],
            new_nodes: 3,
            tail: Some(Extent::new(0, 100, 7)),
        };

        assert!(plan.can_merge(107, 1));
        assert!(!plan.can_merge(108, 1));
        assert_eq!(plan.new_nodes(), 3);
    }
}
