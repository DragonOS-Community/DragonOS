//! The Ext4 filesystem implementation in Rust.
#![no_std]
#![deny(clippy::all)]

#[cfg(test)]
extern crate std;

mod constants;
mod error;
mod ext4;
mod ext4_defs;
pub mod jbd2;
mod prelude;

pub use constants::{BLOCK_SIZE, EXT4_ROOT_INO, INODE_BLOCK_SIZE};
pub use error::{ErrCode, Ext4Error};
pub use ext4::{
    DelallocAppendBlockPublication, DelallocAppendBlockReservation,
    DelallocAppendBlockSubmitOutcome, DelallocAppendMapperAuthority, DelallocExtentNodePool,
    DelallocLease, Ext4, InodeOwner, MetadataMutationWaker, SetAttr,
};
// The bounded append mapper implementation is compiled in normal builds, but
// its raw facade remains test-only until the DragonOS VFS can supply the
// non-forgeable lifecycle, queue-head, EOF-ticket and truncate-drain proof.
#[cfg(any(test, feature = "test-api"))]
pub use ext4::DelallocAppendBlockWriteback;
// The raw mapped-tail receipt exists solely for the host recovery harness.
// Production callers must use the future VFS-owned token, which couples this
// receipt to the inode lifecycle lease, queue claim, and truncate drain.
#[cfg(feature = "test-api")]
pub use ext4::DelallocMappedWriteback;
pub use ext4_defs::{
    Block, BlockDevice, DirEntry, FileAttr, FileType, Inode, InodeMode, InodeReclaimError,
    InodeReclaimHandle, InodeRef,
};
