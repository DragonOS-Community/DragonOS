//! The Defination of Ext4 Extent (Header, Index)
//!
//! Extents are arranged as a tree. Each node of the tree begins with a struct
//! [`ExtentHeader`].
//!
//! If the node is an interior node (eh.depth > 0), the header is followed by
//! eh.entries_count instances of struct [`ExtentIndex`]; each of these index
//! entries points to a block containing more nodes in the extent tree.
//!
//! If the node is a leaf node (eh.depth == 0), then the header is followed by
//! eh.entries_count instances of struct [`Extent`]; these instances point
//! to the file's data blocks. The root node of the extent tree is stored in
//! inode.i_block, which allows for the first four extents to be recorded without
//! the use of extra metadata blocks.

use crate::prelude::*;

#[derive(Debug, Default, Clone, Copy)]
#[repr(C)]
pub struct ExtentHeader {
    /// Magic number, 0xF30A.
    magic: u16,

    /// Number of valid entries following the header.
    entries_count: u16,

    /// Maximum number of entries that could follow the header.
    max_entries_count: u16,

    /// Depth of this extent node in the extent tree.
    /// 0 = this extent node points to data blocks;
    /// otherwise, this extent node points to other extent nodes.
    /// The extent tree can be at most 5 levels deep:
    /// a logical block number can be at most 2^32,
    /// and the smallest n that satisfies 4*(((blocksize - 12)/12)^n) >= 2^32 is 5.
    depth: u16,

    /// Generation of the tree. (Used by Lustre, but not standard ext4).
    generation: u32,
}

impl ExtentHeader {
    const EXTENT_MAGIC: u16 = 0xF30A;

    pub fn new(entries_count: u16, max_entries_count: u16, depth: u16, generation: u32) -> Self {
        Self {
            magic: Self::EXTENT_MAGIC,
            entries_count,
            max_entries_count,
            depth,
            generation,
        }
    }

    /// 获取extent header的条目数
    pub fn entries_count(&self) -> u16 {
        self.entries_count
    }

    /// 设置extent header的条目数
    pub fn set_entries_count(&mut self, count: u16) {
        self.entries_count = count;
    }

    /// 获取extent header的最大条目数
    pub fn max_entries_count(&self) -> u16 {
        self.max_entries_count
    }

    /// 获取extent header的深度
    pub fn depth(&self) -> u16 {
        self.depth
    }

    /// 设置extent header的深度
    pub fn set_depth(&mut self, depth: u16) {
        self.depth = depth;
    }

    /// 获取extent header的生成号
    pub fn generation(&self) -> u32 {
        self.generation
    }

    /// 设置extent header的生成号
    pub fn set_generation(&mut self, generation: u32) {
        self.generation = generation;
    }
}

#[derive(Debug, Default, Clone, Copy)]
#[repr(C)]
pub struct ExtentIndex {
    /// This index node covers file blocks from ‘block’ onward.
    pub first_block: u32,

    /// Lower 32-bits of the block number of the extent node that is
    /// the next level lower in the tree. The tree node pointed to
    /// can be either another internal node or a leaf node, described below.
    pub leaf_lo: u32,

    /// Upper 16-bits of the previous field.
    pub leaf_hi: u16,

    pub padding: u16,
}

impl ExtentIndex {
    /// Create a new extent index with the start logic block number and
    /// the physical block number of the child node
    pub fn new(first_block: LBlockId, leaf: PBlockId) -> Self {
        Self {
            first_block,
            leaf_lo: leaf as u32,
            leaf_hi: (leaf >> 32) as u16,
            padding: 0,
        }
    }

    /// The start logic block number that this extent index covers
    pub fn start_lblock(&self) -> LBlockId {
        self.first_block
    }

    /// The physical block number of the extent node that is the next level lower in the tree
    pub fn leaf(&self) -> PBlockId {
        ((self.leaf_hi as PBlockId) << 32) | self.leaf_lo as PBlockId
    }
}

#[derive(Debug, Default, Clone, Copy)]
#[repr(C)]
pub struct Extent {
    /// First file block number that this extent covers.
    first_block: u32,

    /// Number of blocks covered by extent.
    /// If the value of this field is <= 32768, the extent is initialized.
    /// If the value of the field is > 32768, the extent is uninitialized
    /// and the actual extent length is ee_len - 32768.
    /// Therefore, the maximum length of a initialized extent is 32768 blocks,
    /// and the maximum length of an uninitialized extent is 32767.
    block_count: u16,

    /// Upper 16-bits of the block number to which this extent points.
    start_hi: u16,

    /// Lower 32-bits of the block number to which this extent points.
    start_lo: u32,
}

impl Extent {
    /// Extent with `block_count` greater than 32768 is considered unwritten.
    const INIT_MAX_LEN: u16 = 32768;

    /// Create a new extent with start logic block number, start physical block number, and block count
    pub fn new(start_lblock: LBlockId, start_pblock: PBlockId, block_count: u16) -> Self {
        Self {
            first_block: start_lblock,
            block_count,
            start_hi: (start_pblock >> 32) as u16,
            start_lo: start_pblock as u32,
        }
    }

    /// The start logic block number that this extent covers
    pub fn start_lblock(&self) -> LBlockId {
        self.first_block
    }

    /// Set the start logic block number that this extent covers
    pub fn set_start_lblock(&mut self, start_lblock: LBlockId) {
        self.first_block = start_lblock;
    }

    /// The start physical block number to which this extent points
    pub fn start_pblock(&self) -> PBlockId {
        self.start_lo as PBlockId | ((self.start_hi as PBlockId) << 32)
    }

    /// Set the start physical block number to which this extent points
    pub fn set_start_pblock(&mut self, start_pblock: PBlockId) {
        self.start_hi = (start_pblock >> 32) as u16;
        self.start_lo = start_pblock as u32;
    }

    /// The actual number of blocks covered by this extent
    pub fn block_count(&self) -> LBlockId {
        (if self.block_count <= Self::INIT_MAX_LEN {
            self.block_count
        } else {
            self.block_count - Self::INIT_MAX_LEN
        }) as LBlockId
    }

    /// Set the number of blocks covered by this extent
    pub fn set_block_count(&mut self, block_count: LBlockId) {
        self.block_count = block_count as u16;
    }

    /// Check if the extent is unwritten
    pub fn is_unwritten(&self) -> bool {
        self.block_count > Self::INIT_MAX_LEN
    }

    /// Mark the extent as unwritten
    pub fn mark_unwritten(&mut self) {
        self.block_count |= Self::INIT_MAX_LEN;
    }

    /// Check whether the `ex2` extent can be appended to the `ex1` extent
    pub fn can_append(ex1: &Extent, ex2: &Extent) -> bool {
        if ex1.start_pblock() + ex1.block_count() as u64 != ex2.start_pblock() {
            return false;
        }
        if ex1.is_unwritten() && ex1.block_count() + ex2.block_count() > 65535 as LBlockId {
            return false;
        }
        if ex1.block_count() + ex2.block_count() > Self::INIT_MAX_LEN as LBlockId {
            return false;
        }
        if ex1.first_block + ex1.block_count() != ex2.first_block {
            return false;
        }
        true
    }
}

/// When only `first_block` field in `Extent` and `ExtentIndex` are used, they can
/// both be interpreted as the common type `FakeExtent`. This provides convenience
/// to some tree operations.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct FakeExtent {
    /// The `first_block` field in `Extent` and `ExtentIndex`
    first_block: u32,
    /// Ignored field, should not be accessed
    protected: [u8; 8],
}

impl From<Extent> for FakeExtent {
    fn from(extent: Extent) -> Self {
        unsafe { mem::transmute(extent) }
    }
}

impl From<ExtentIndex> for FakeExtent {
    fn from(extent_index: ExtentIndex) -> Self {
        unsafe { mem::transmute(extent_index) }
    }
}

/// Interpret an immutable byte slice as an extent node. Provide methods to
/// access the extent header and the following extents or extent indices.
///
/// The underlying `raw_data` could be of `[u32;15]` (root node) or a
/// data block `[u8;BLOCK_SIZE]` (other node).
pub struct ExtentNode<'a> {
    raw_data: &'a [u8],
}

impl<'a> ExtentNode<'a> {
    /// Interpret a byte slice as an extent node
    pub fn from_bytes(raw_data: &'a [u8]) -> Self {
        Self { raw_data }
    }

    /// Get a immutable reference to the extent header
    pub fn header(&self) -> &ExtentHeader {
        unsafe { &*(self.raw_data.as_ptr() as *const ExtentHeader) }
    }

    /// Get a immutable reference to the extent at a given position
    pub fn extent_at(&self, pos: usize) -> &Extent {
        unsafe { &*((self.header() as *const ExtentHeader).add(1) as *const Extent).add(pos) }
    }

    /// Get a immmutable reference to the extent indexat a given position
    pub fn extent_index_at(&self, pos: usize) -> &ExtentIndex {
        unsafe { &*((self.header() as *const ExtentHeader).add(1) as *const ExtentIndex).add(pos) }
    }

    /// Find the extent that covers the given logical block number.
    ///
    /// Return `Ok(index)` if found, and `eh.extent_at(index)` is the extent that covers
    /// the given logical block number. Return `Err(index)` if not found, and `index` is the
    /// position where the new extent should be inserted.
    pub fn search_extent(&self, lblock: LBlockId) -> core::result::Result<usize, usize> {
        // debug!("Search extent: {}", lblock);
        let mut i = 0;
        while i < self.header().entries_count as usize {
            let extent = self.extent_at(i);
            if extent.start_lblock() <= lblock {
                if extent.start_lblock() + (extent.block_count() as LBlockId) > lblock {
                    let res = if extent.is_unwritten() { Err(i) } else { Ok(i) };
                    // debug!("Search res: {:?}", res);
                    return res;
                }
                i += 1;
            } else {
                break;
            }
        }

        // debug!("Search res: {:?}", res);
        Err(i)
    }

    /// Find the extent index that covers the given logical block number. The extent index
    /// gives the next lower node to search.
    ///
    /// Return `Ok(index)` if found, and `eh.extent_index_at(index)` is the target extent index.
    /// Return `Err(index)` if not found, and `index` is the position where the new extent index
    /// should be inserted.
    pub fn search_extent_index(&self, lblock: LBlockId) -> core::result::Result<usize, usize> {
        // debug!("Search extent index: {}", lblock);
        let mut i = 0;
        while i < self.header().entries_count as usize {
            let extent_index = self.extent_index_at(i);
            if extent_index.start_lblock() > lblock {
                break;
            }
            i += 1;
        }

        // debug!("Search res: {:?}", res);
        Ok(i - 1)
    }

    pub fn print(&self) {
        debug!("Extent header {:?}", self.header());
        let mut i = 0;
        while i < self.header().entries_count() as usize {
            if self.header().depth == 0 {
                let ext = self.extent_at(i);
                debug!(
                    "extent[{}] start_lblock={}, start_pblock={}, len={}",
                    i,
                    ext.start_lblock(),
                    ext.start_pblock(),
                    ext.block_count()
                );
            } else {
                let ext_idx = self.extent_index_at(i);
                debug!(
                    "extent_index[{}] start_lblock={}, leaf={}",
                    i,
                    ext_idx.start_lblock(),
                    ext_idx.leaf()
                )
            }
            i += 1;
        }
    }
}

/// Interpret a mutable byte slice as an extent node. Provide methods to
/// modify the extent header and the following extents or extent indices.
///
/// The underlying `raw_data` could be of `[u8;15]` (root node) or a
/// data block `[u8;BLOCK_SIZE]` (other node).
pub struct ExtentNodeMut<'a> {
    raw_data: &'a mut [u8],
}

impl<'a> ExtentNodeMut<'a> {
    /// Interpret a byte slice as an extent node
    pub fn from_bytes(raw_data: &'a mut [u8]) -> Self {
        Self { raw_data }
    }

    /// Interpret self as immutable extent node
    pub fn as_immut(&self) -> ExtentNode<'_> {
        ExtentNode {
            raw_data: self.raw_data,
        }
    }

    /// Get a immutable reference to the extent header
    pub fn header(&self) -> &ExtentHeader {
        unsafe { &*(self.raw_data.as_ptr() as *const ExtentHeader) }
    }

    /// Get a mutable reference to the extent header
    pub fn header_mut(&mut self) -> &mut ExtentHeader {
        unsafe { &mut *(self.raw_data.as_mut_ptr() as *mut ExtentHeader) }
    }

    /// Get a immutable reference to the extent at a given position
    pub fn extent_at(&self, pos: usize) -> &Extent {
        unsafe { &*((self.header() as *const ExtentHeader).add(1) as *const Extent).add(pos) }
    }

    /// Get a mutable reference to the extent at a given position
    pub fn extent_mut_at(&mut self, pos: usize) -> &mut Extent {
        unsafe { &mut *((self.header_mut() as *mut ExtentHeader).add(1) as *mut Extent).add(pos) }
    }

    /// Get an immutable reference to the extent pos at a given position
    pub fn extent_index_at(&self, pos: usize) -> &ExtentIndex {
        unsafe { &*((self.header() as *const ExtentHeader).add(1) as *const ExtentIndex).add(pos) }
    }

    /// Get a mutable reference to the extent pos at a given position
    pub fn extent_index_mut_at(&mut self, pos: usize) -> &mut ExtentIndex {
        unsafe {
            &mut *((self.header_mut() as *mut ExtentHeader).add(1) as *mut ExtentIndex).add(pos)
        }
    }

    /// Get an immutable reference to the extent or extent index at a given position,
    /// ignore the detailed type information
    pub fn fake_extent_at(&self, pos: usize) -> &FakeExtent {
        unsafe { &*((self.header() as *const ExtentHeader).add(1) as *const FakeExtent).add(pos) }
    }

    /// Get a mutable reference to the extent or extent index at a given position,
    /// ignore the detailed type information
    pub fn fake_extent_mut_at(&mut self, pos: usize) -> &mut FakeExtent {
        unsafe {
            &mut *((self.header_mut() as *mut ExtentHeader).add(1) as *mut FakeExtent).add(pos)
        }
    }

    /// Initialize the extent node
    pub fn init(&mut self, depth: u16, generation: u32) {
        let max_entries_count =
            (self.raw_data.len() - size_of::<ExtentHeader>()) / size_of::<Extent>();
        *self.header_mut() = ExtentHeader::new(0, max_entries_count as u16, depth, generation);
    }

    /// Insert a new extent into current node.
    ///
    /// Return `Ok(())` if the insertion is successful. Return `Err(extents)` if
    /// the insertion failed and `extents` is a vector of split extents, which
    /// should be inserted into a new node.
    ///
    /// This function requires this extent node to be a leaf node.
    pub fn insert_extent(
        &mut self,
        extent: &Extent,
        pos: usize,
    ) -> core::result::Result<(), Vec<FakeExtent>> {
        if self.extent_at(pos).is_unwritten() {
            // The position has an uninitialized extent
            *self.extent_mut_at(pos) = *extent;
            self.header_mut().entries_count += 1;
            return Ok(());
        }
        // The position has a valid extent
        if self.header().entries_count() < self.header().max_entries_count() {
            // The extent node is not full
            // Insert the extent and move the following extents
            let mut i = pos;
            while i < self.header().entries_count() as usize {
                *self.extent_mut_at(i + 1) = *self.extent_at(i);
                i += 1;
            }
            *self.extent_mut_at(pos) = *extent;
            self.header_mut().entries_count += 1;
            return Ok(());
        }
        // The extent node is full
        // There may be some unwritten extents, we could find the first
        // unwritten extent and adjust the extents.
        let mut unwritten = None;
        for i in 0..self.header().entries_count() as usize {
            if self.extent_at(i).is_unwritten() {
                unwritten = Some(i);
                break;
            }
        }
        if let Some(unwritten) = unwritten {
            // There is an uninitialized extent, we could adjust the extents.
            if unwritten < pos {
                // Move the extents from `unwritten` to `pos`
                let mut i = unwritten;
                while i < pos {
                    *self.extent_mut_at(i) = *self.extent_at(i + 1);
                    i += 1;
                }
            } else {
                // Move the extents from `pos` to `unwritten`
                let mut i = pos;
                while i < unwritten {
                    *self.extent_mut_at(i + 1) = *self.extent_at(i);
                    i += 1;
                }
            }
            *self.extent_mut_at(pos) = *extent;
            self.header_mut().entries_count += 1;
            return Ok(());
        }
        // The extent node is full and all extents are valid
        // Split the node, return the extents in the right half
        let mut split = Vec::new();
        let mid = self.header().entries_count() as usize * 2 / 3;
        // If `pos` is on the right side, insert it to `split`
        for i in mid..self.header().entries_count() as usize {
            if i == pos {
                split.push((*extent).into());
            }
            split.push(*self.fake_extent_at(i));
        }
        if pos == self.header().entries_count() as usize {
            split.push((*extent).into());
        }
        // Update header
        self.header_mut().entries_count = mid as u16;
        // If `pos` is on the left side, insert it
        if pos < mid {
            self.insert_extent(extent, pos).expect("Must Succeed");
        }
        // Return the right half
        Err(split)
    }

    /// Insert a new extent index into current node.
    ///
    /// Return `Ok(())` if the insertion is successful. Return `Err(extent_indexs)` if
    /// the insertion failed and `extent_indexs` is a vector of split extent indexs,
    /// which should be inserted into a new node.
    ///
    /// This function requires this extent node to be a inner node.
    pub fn insert_extent_index(
        &mut self,
        extent_index: &ExtentIndex,
        pos: usize,
    ) -> core::result::Result<(), Vec<FakeExtent>> {
        if self.header().entries_count() < self.header().max_entries_count() {
            // The extent node is not full
            // Insert the extent index and move the following extent indexs
            let mut i = pos;
            while i < self.header().entries_count() as usize {
                *self.extent_index_mut_at(i + 1) = *self.extent_index_at(i);
                i += 1;
            }
            *self.extent_index_mut_at(pos) = *extent_index;
            self.header_mut().entries_count += 1;
            return Ok(());
        }
        // The extent node is full
        // Split the node, return the extent indexs in the right half
        let mut split = Vec::<FakeExtent>::new();
        let mid = self.header().entries_count() as usize * 2 / 3;
        // If `pos` is on the right side, insert it to `split`
        for i in mid..self.header().entries_count() as usize {
            if i == pos {
                split.push((*extent_index).into());
            }
            split.push(*self.fake_extent_at(i));
        }
        if pos == self.header().entries_count() as usize {
            split.push((*extent_index).into());
        }
        // Update header
        self.header_mut().entries_count = mid as u16;
        // If `pos` is on the left side, insert it
        if pos < mid {
            self.insert_extent_index(extent_index, pos)
                .expect("Must Succeed");
        }
        // Return the right half
        Err(split)
    }
}
