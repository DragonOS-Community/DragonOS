//! Unified rendering for `/proc/mounts`, `/proc/[pid]/mounts`, `/proc/[pid]/mountinfo`,
//! and `/proc/[pid]/mountstats`.

mod collect;
mod escape;
mod fields;
pub(crate) mod format;
pub(crate) mod inode;
mod render;

pub(crate) use render::{open_mount_file_for_target, read_cached_mount_file, ProcMountRenderKind};
