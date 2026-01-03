use super::Ext4;
use crate::constants::*;
use crate::ext4_defs::*;
use crate::format_error;
use crate::prelude::*;
use core::cmp::min;

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
        let path = self.find_extent(inode_ref, iblock);
        // Leaf is the last element of the path
        let leaf = path.last().unwrap();
        if let Ok(index) = leaf.index {
            // Note: block data must be defined here to keep it alive
            let block_data: Block;
            let ex_node = if leaf.pblock != 0 {
                // Load the extent node
                block_data = self.read_block(leaf.pblock);
                // Load the next extent header
                ExtentNode::from_bytes(&*block_data.data)
            } else {
                // Root node
                inode_ref.inode.extent_root()
            };
            let ex = ex_node.extent_at(index);
            Ok(ex.start_pblock() + (iblock - ex.start_lblock()) as PBlockId)
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
        let path = self.find_extent(inode_ref, iblock);
        // Leaf is the last element of the path
        let leaf = path.last().unwrap();
        // Note: block data must be defined here to keep it alive
        let mut block_data: Block;
        let ex_node = if leaf.pblock != 0 {
            block_data = self.read_block(leaf.pblock);
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
            Err(_) => {
                // Not found, create a new extent
                let block_count = min(block_count, MAX_BLOCKS - iblock);
                // Allocate physical block
                let fblock = self.alloc_block(inode_ref)?;
                // Create a new extent
                let new_ext = Extent::new(iblock, fblock, block_count as u16);
                // Insert the new extent
                self.insert_extent(inode_ref, &path, &new_ext)?;
                Ok(fblock)
            }
        }
    }

    /// Get all data blocks recorded in the extent tree
    pub(super) fn extent_all_data_blocks(&self, inode_ref: &InodeRef) -> Vec<PBlockId> {
        let mut pblocks = Vec::new();
        let ex_node = inode_ref.inode.extent_root();
        self.get_all_pblocks_recursive(&ex_node, &mut pblocks);
        pblocks
    }

    /// Get all physical blocks for saving the extent tree
    pub(super) fn extent_all_tree_blocks(&self, inode_ref: &InodeRef) -> Vec<PBlockId> {
        let mut pblocks = Vec::new();
        let ex_node = inode_ref.inode.extent_root();
        self.get_all_nodes_recursive(&ex_node, &mut pblocks);
        pblocks
    }

    fn get_all_pblocks_recursive(&self, ex_node: &ExtentNode, pblocks: &mut Vec<PBlockId>) {
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
                let child_block = self.read_block(ex_idx.leaf());
                let child_node = ExtentNode::from_bytes(&*child_block.data);
                self.get_all_pblocks_recursive(&child_node, pblocks);
            }
        }
    }

    fn get_all_nodes_recursive(&self, ex_node: &ExtentNode, pblocks: &mut Vec<PBlockId>) {
        if ex_node.header().depth() != 0 {
            // Non-leaf
            for i in 0..ex_node.header().entries_count() as usize {
                let ex_idx = ex_node.extent_index_at(i);
                pblocks.push(ex_idx.leaf());
                let child_block = self.read_block(ex_idx.leaf());
                let child_node = ExtentNode::from_bytes(&*child_block.data);
                self.get_all_nodes_recursive(&child_node, pblocks);
            }
        }
    }

    /// Find the given logic block id in the extent tree, return the search path
    fn find_extent(&self, inode_ref: &InodeRef, iblock: LBlockId) -> Vec<ExtentSearchStep> {
        let mut path: Vec<ExtentSearchStep> = Vec::new();
        let mut ex_node = inode_ref.inode.extent_root();
        let mut pblock = 0;
        let mut block_data: Block;

        // Go until leaf
        while ex_node.header().depth() > 0 {
            let index = ex_node.search_extent_index(iblock).expect("Must succeed");
            path.push(ExtentSearchStep::new(pblock, Ok(index)));
            // Get the target extent index
            let ex_idx = ex_node.extent_index_at(index);
            // Load the next extent node
            let next = ex_idx.leaf();
            // Note: block data cannot be released until the next assigment
            block_data = self.read_block(next);
            // Load the next extent header
            ex_node = ExtentNode::from_bytes(&*block_data.data);
            pblock = next;
        }
        // Leaf
        let index = ex_node.search_extent(iblock);
        path.push(ExtentSearchStep::new(pblock, index));

        path
    }

    /// Insert a new extent into the extent tree.
    fn insert_extent(
        &self,
        inode_ref: &mut InodeRef,
        path: &[ExtentSearchStep],
        new_ext: &Extent,
    ) -> Result<()> {
        let leaf = path.last().unwrap();
        // 1. Check If leaf is root
        if leaf.pblock == 0 {
            let mut leaf_node = inode_ref.inode.extent_root_mut();
            // Insert the extent
            let res = leaf_node.insert_extent(new_ext, leaf.index.unwrap_err());
            self.write_inode_with_csum(inode_ref);
            // Handle split
            return if let Err(split) = res {
                self.split_root(inode_ref, &split)
            } else {
                Ok(())
            };
        }
        // 2. Leaf is not root, load the leaf node
        let mut leaf_block = self.read_block(leaf.pblock);
        let mut leaf_node = ExtentNodeMut::from_bytes(&mut *leaf_block.data);
        // Insert the extent
        let res = leaf_node.insert_extent(new_ext, leaf.index.unwrap_err());
        self.write_block(&leaf_block);
        // Handle split
        if let Err(mut split) = res {
            // Handle split until root
            for parent in path.iter().rev().skip(1) {
                // The split node is at `parent.index.unwrap()`
                // Call `self.split` to store the split part and update `parent`
                let res = self.split(inode_ref, parent.pblock, parent.index.unwrap(), &split);
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
    ) -> core::result::Result<(), Vec<FakeExtent>> {
        let right_bid = self.alloc_block(inode_ref).unwrap();
        let mut right_block = self.read_block(right_bid);
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
            self.write_inode_with_csum(inode_ref);
        } else {
            // Parent is not root
            let mut parent_block = self.read_block(parent_pblock);
            let mut parent_node = ExtentNodeMut::from_bytes(&mut *parent_block.data);
            parent_depth = parent_node.header().depth();
            res = parent_node.insert_extent_index(&extent_index, child_pos + 1);
            self.write_block(&parent_block);
        }

        // Right node is the child of parent, so its depth is 1 less than parent
        right_node.header_mut().set_depth(parent_depth - 1);
        self.write_block(&right_block);

        res
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
        let mut l_block = self.read_block(l_bid);
        let mut r_block = self.read_block(r_bid);

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
        self.write_block(&l_block);
        self.write_block(&r_block);
        self.write_inode_with_csum(inode_ref);

        Ok(())
    }
}
