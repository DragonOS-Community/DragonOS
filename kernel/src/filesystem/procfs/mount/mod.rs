//! Unified rendering for `/proc/mounts`, `/proc/[pid]/mounts`, `/proc/[pid]/mountinfo`,
//! and `/proc/[pid]/mountstats`.

mod collect;
mod escape;
mod fields;
pub(crate) mod format;
pub(crate) mod inode;
mod render;
mod view;

pub(crate) use render::{render_mount_file, ProcMountRenderKind};
pub(crate) use view::MountView;
