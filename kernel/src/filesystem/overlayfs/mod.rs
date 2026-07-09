mod config;
mod copy_up;
mod dir;
mod entry;
mod file;
mod fs;
mod inode;
mod lookup;
mod path;
mod readdir;
mod rename;
mod whiteout;
mod workdir;

pub use file::OverlayFilePrivateData;

use self::fs::OverlayFS;
use super::vfs::{FileSystem, MountableFileSystem, FSMAKER};
use crate::filesystem::vfs::FileSystemMakerData;
use crate::register_mountable_fs;
use alloc::sync::Arc;
use linkme::distributed_slice;
use system_error::SystemError;

register_mountable_fs!(OverlayFS, OVERLAYFSMAKER, "overlay");
