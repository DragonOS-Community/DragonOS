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

impl Ext4 {
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
            &sb.uuid(),
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
                &self.read_super_block_cached().uuid(),
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
                &self.read_super_block_cached().uuid(),
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
                    &self.read_super_block_cached().uuid(),
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

    fn set_extent_block_checksum(uuid: &[u8], inode_ref: &InodeRef, image: &mut [u8; BLOCK_SIZE]) {
        let tail_offset = BLOCK_SIZE - core::mem::size_of::<crate::ext4_defs::ExtentTail>();
        let checksum =
            extent_block_checksum(uuid, inode_ref.id, inode_ref.inode.generation(), image);
        image[tail_offset..tail_offset + 4].copy_from_slice(&checksum.to_le_bytes());
    }

    /// Write an extent block to disk with checksum in the extent tail.
    fn write_extent_block(&self, block: &mut Block, inode_ref: &InodeRef) -> Result<()> {
        let tail_offset = BLOCK_SIZE - core::mem::size_of::<crate::ext4_defs::ExtentTail>();
        let csum = extent_block_checksum(
            &self.read_super_block_cached().uuid(),
            inode_ref.id,
            inode_ref.inode.generation(),
            &*block.data,
        );
        // Write checksum into the tail
        block.data[tail_offset..tail_offset + 4].copy_from_slice(&csum.to_le_bytes());
        self.write_block(block)
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
            alloc_lock: spin::Mutex::new(()),
            namespace_lock: spin::Mutex::new(()),
            metadata_mutation_barrier: crate::ext4::MetadataMutationGate::new(),
            poisoned: spin::Mutex::new(None),
            journal: None,
            inode_mutation_locks: (0..crate::ext4::INODE_MUTATION_LOCK_SHARDS)
                .map(|_| spin::Mutex::new(()))
                .collect(),
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
            alloc_lock: spin::Mutex::new(()),
            namespace_lock: spin::Mutex::new(()),
            metadata_mutation_barrier: crate::ext4::MetadataMutationGate::new(),
            poisoned: spin::Mutex::new(None),
            journal: None,
            inode_mutation_locks: (0..crate::ext4::INODE_MUTATION_LOCK_SHARDS)
                .map(|_| spin::Mutex::new(()))
                .collect(),
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
        Ext4::set_extent_block_checksum(&fs.read_super_block_cached().uuid(), &inode, &mut image);
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
}
