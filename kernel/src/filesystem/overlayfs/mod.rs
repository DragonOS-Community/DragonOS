mod config;
mod copy_up;
mod cred;
mod dir;
mod entry;
mod file;
mod fs;
mod inode;
mod lookup;
mod metadata;
mod path;
mod readdir;
mod rename;
mod whiteout;
mod workdir;

pub use file::OverlayFilePrivateData;
pub(crate) use inode::OvlInode;

use self::fs::OverlayFS;
use super::vfs::{FileSystem, MountableFileSystem, FSMAKER};
use crate::filesystem::vfs::FileSystemMakerData;
use crate::register_mountable_fs;
use alloc::sync::Arc;
use linkme::distributed_slice;
use system_error::SystemError;

register_mountable_fs!(OverlayFS, OVERLAYFSMAKER, "overlay");

#[cfg(all(
    feature = "mm_selftest",
    any(target_arch = "x86_64", target_arch = "riscv64")
))]
pub(crate) use metadata::selftest_report as xattr_selftest_report;
