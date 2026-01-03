#![allow(unused)]

use crate::prelude::*;

/// The maximum number of blocks in the file system
pub const MAX_BLOCKS: LBlockId = LBlockId::MAX;

/// Maximum bytes in a path
pub const PATH_MAX: usize = 4096;

/// Maximum bytes in a file name
pub const NAME_MAX: usize = 255;

/// The upper limit for resolving symbolic links
pub const SYMLINKS_MAX: usize = 40;

/// The inode number of root inode
#[cfg(feature = "fuser_root_inode")]
pub const EXT4_ROOT_INO: InodeId = 1;
#[cfg(not(feature = "fuser_root_inode"))]
pub const EXT4_ROOT_INO: InodeId = 2;

/// The base offset of the super block
pub const BASE_OFFSET: usize = 1024;

/// The size of a block
pub const BLOCK_SIZE: usize = 4096;

/// For simplicity define this the same as block size
pub const INODE_BLOCK_SIZE: usize = 512;

/// CRC32 initial value
pub const CRC32_INIT: u32 = 0xFFFFFFFF;

/// The value of super block `inode_size` field.
/// We implement the larger version of inode size for simplicity.
pub const SB_GOOD_INODE_SIZE: usize = 256;

/// The value of super block `desc_size` field.
/// We implement the 64-bit block group descriptor for simplicity.
pub const SB_GOOD_DESC_SIZE: usize = 64;
