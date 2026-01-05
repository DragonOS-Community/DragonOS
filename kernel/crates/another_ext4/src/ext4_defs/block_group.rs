//! The Block Group Descriptor is the second field of Ext4 Block Group.
//!
//! | Super Block | Group Descriptor | Reserved GDT Blocks |
//! | Block Bitmap | Inode Bitmap | Inode Table | Data Blocks |
//!
//! See [`super`] for more information.

use super::crc::*;
use super::AsBytes;
use super::Bitmap;
use crate::constants::*;
use crate::prelude::*;

/// The Block Group Descriptor.
///
/// Each block group on the filesystem has one of these descriptors associated with it.
/// In ext2, ext3, and ext4 (when the 64bit feature is not enabled), the block group
/// descriptor was only 32 bytes long and therefore ends at bg_checksum. On an ext4
/// filesystem with the 64bit feature enabled, the block group descriptor expands to
/// at least the 64 bytes described below; the size is stored in the superblock.
///
/// We only implement the 64 bytes version for simplicity. Guarantee that `sb.desc_size`
/// equals to 64. This value will be checked when loading the filesystem.
#[derive(Debug, Default, Clone, Copy)]
#[repr(C, packed)]
pub struct BlockGroupDesc {
    block_bitmap_lo: u32,            // 块位图块
    inode_bitmap_lo: u32,            // 节点位图块
    inode_table_first_block_lo: u32, // 节点表块
    free_blocks_count_lo: u16,       // 空闲块数
    free_inodes_count_lo: u16,       // 空闲节点数
    used_dirs_count_lo: u16,         // 目录数
    flags: u16,                      // EXT4_BG_flags (INODE_UNINIT, etc)
    exclude_bitmap_lo: u32,          // 快照排除位图
    block_bitmap_csum_lo: u16,       // crc32c(s_uuid+grp_num+bbitmap) LE
    inode_bitmap_csum_lo: u16,       // crc32c(s_uuid+grp_num+ibitmap) LE
    itable_unused_lo: u16,           // 未使用的节点数
    checksum: u16,                   // crc16(sb_uuid+group+desc)

    block_bitmap_hi: u32,            // 块位图块 MSB
    inode_bitmap_hi: u32,            // 节点位图块 MSB
    inode_table_first_block_hi: u32, // 节点表块 MSB
    free_blocks_count_hi: u16,       // 空闲块数 MSB
    free_inodes_count_hi: u16,       // 空闲节点数 MSB
    used_dirs_count_hi: u16,         // 目录数 MSB
    itable_unused_hi: u16,           // 未使用的节点数 MSB
    exclude_bitmap_hi: u32,          // 快照排除位图 MSB
    block_bitmap_csum_hi: u16,       // crc32c(s_uuid+grp_num+bbitmap) BE
    inode_bitmap_csum_hi: u16,       // crc32c(s_uuid+grp_num+ibitmap) BE
    reserved: u32,                   // 填充
}

unsafe impl AsBytes for BlockGroupDesc {}

impl BlockGroupDesc {
    #[allow(unused)]
    const MIN_BLOCK_GROUP_DESC_SIZE: usize = 32;
    #[allow(unused)]
    const MAX_BLOCK_GROUP_DESC_SIZE: usize = 64;

    pub fn block_bitmap_block(&self) -> PBlockId {
        ((self.block_bitmap_hi as PBlockId) << 32) | self.block_bitmap_lo as PBlockId
    }

    pub fn inode_bitmap_block(&self) -> PBlockId {
        ((self.inode_bitmap_hi as PBlockId) << 32) | self.inode_bitmap_lo as PBlockId
    }

    pub fn itable_unused(&self) -> u32 {
        ((self.itable_unused_hi as u32) << 16) | self.itable_unused_lo as u32
    }

    pub fn used_dirs_count(&self) -> u32 {
        ((self.used_dirs_count_hi as u32) << 16) | self.used_dirs_count_lo as u32
    }

    pub fn set_used_dirs_count(&mut self, cnt: u32) {
        self.used_dirs_count_lo = cnt as u16;
        self.used_dirs_count_hi = (cnt >> 16) as u16;
    }

    pub fn set_itable_unused(&mut self, cnt: u32) {
        self.itable_unused_lo = cnt as u16;
        self.itable_unused_hi = (cnt >> 16) as u16;
    }

    pub fn set_free_inodes_count(&mut self, cnt: u32) {
        self.free_inodes_count_lo = cnt as u16;
        self.free_inodes_count_hi = (cnt >> 16) as u16;
    }

    pub fn free_inodes_count(&self) -> u32 {
        ((self.free_inodes_count_hi as u32) << 16) | self.free_inodes_count_lo as u32
    }

    pub fn inode_table_first_block(&self) -> PBlockId {
        ((self.inode_table_first_block_hi as u64) << 32) | self.inode_table_first_block_lo as u64
    }

    pub fn get_free_blocks_count(&self) -> u64 {
        ((self.free_blocks_count_hi as u64) << 32) | self.free_blocks_count_lo as u64
    }

    pub fn set_free_blocks_count(&mut self, cnt: u64) {
        self.free_blocks_count_lo = ((cnt << 32) >> 32) as u16;
        self.free_blocks_count_hi = (cnt >> 32) as u16;
    }

    pub fn set_inode_bitmap_csum(&mut self, uuid: &[u8], bitmap: &Bitmap) {
        let mut csum = crc32(CRC32_INIT, uuid);
        csum = crc32(csum, bitmap.as_bytes());
        self.inode_bitmap_csum_lo = csum as u16;
        self.block_bitmap_csum_hi = (csum >> 16) as u16;
    }

    pub fn set_block_bitmap_csum(&mut self, uuid: &[u8], bitmap: &Bitmap) {
        let mut csum = crc32(CRC32_INIT, uuid);
        csum = crc32(csum, bitmap.as_bytes());
        self.block_bitmap_csum_lo = csum as u16;
        self.block_bitmap_csum_hi = (csum >> 16) as u16;
    }
}

/// A combination of a `BlockGroupDesc` and its id
#[derive(Debug)]
pub struct BlockGroupRef {
    /// The block group id
    pub id: BlockGroupId,
    /// The block group descriptor
    pub desc: BlockGroupDesc,
}

impl BlockGroupRef {
    pub fn new(id: BlockGroupId, desc: BlockGroupDesc) -> Self {
        Self { id, desc }
    }

    pub fn set_checksum(&mut self, uuid: &[u8]) {
        // Same as inode checksum: clear the checksum field before calculation to avoid
        // including old value causing checksum mismatch.
        self.desc.checksum = 0;
        let mut checksum = crc32(CRC32_INIT, uuid);
        checksum = crc32(checksum, &self.id.to_le_bytes());
        checksum = crc32(checksum, self.desc.to_bytes());
        self.desc.checksum = checksum as u16;
    }
}
