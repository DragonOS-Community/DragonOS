//! Canonical cgroup-file permissions do not depend on a mount's visible root.
use crate::{
    cgroup::{cgroup_common_ancestor, CgroupNode},
    filesystem::vfs::{permission::PermissionMask, FileType, InodeMode, Metadata},
    process::cred::Cred,
};
use alloc::sync::Arc;
use system_error::SystemError;

pub(super) fn check_procs_write(node: &Arc<CgroupNode>, cred: &Cred) -> Result<(), SystemError> {
    let attrs = node.file_permissions("cgroup.procs", 0o644);
    let metadata = Metadata {
        uid: attrs.uid as usize,
        gid: attrs.gid as usize,
        mode: InodeMode::from_bits_truncate(attrs.mode as _),
        file_type: FileType::File,
        ..Metadata::default()
    };
    cred.inode_permission(&metadata, PermissionMask::MAY_WRITE.bits())
        .map_err(|_| SystemError::EACCES)
}

pub(super) fn check_common_ancestor(
    src: &Arc<CgroupNode>,
    dst: &Arc<CgroupNode>,
    cred: &Cred,
) -> Result<(), SystemError> {
    check_procs_write(&cgroup_common_ancestor(src, dst), cred)
}
