//! The Ext4 filesystem implementation in Rust.
#![no_std]
#![deny(clippy::all)]

mod constants;
mod error;
mod ext4;
mod ext4_defs;
mod jbd2;
mod prelude;

pub use constants::{BLOCK_SIZE, EXT4_ROOT_INO, INODE_BLOCK_SIZE};
pub use error::{ErrCode, Ext4Error};
pub use ext4::{Ext4, SetAttr};
pub use ext4_defs::{Block, BlockDevice, DirEntry, FileAttr, FileType, Inode, InodeMode, InodeRef};
