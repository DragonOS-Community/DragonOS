//! Ownership projection at the boundary of a selected mount and its backend.

use super::MountFSInode;
use crate::{
    filesystem::vfs::{permission::InodeOpContext, Metadata, SetMetadataMask},
    process::ProcessManager,
};
use system_error::SystemError;

impl MountFSInode {
    /// Capture this layer's identity. A stacked filesystem may subsequently
    /// enter a different mount and must capture that inner mount's own context.
    pub(crate) fn op_context(&self) -> InodeOpContext {
        InodeOpContext {
            cred: ProcessManager::initialized().then(|| ProcessManager::current_pcb().cred()),
            idmap: self.mount_fs.idmap(),
            fs_userns: self.mount_fs.super_block_state.owner_user_ns().clone(),
        }
    }
}

/// Build a backing request from a raw snapshot. Never leak unselected view
/// IDs through an old backend's full-metadata fallback.
pub(super) fn raw_metadata_request(
    context: &InodeOpContext,
    raw: &Metadata,
    requested: &Metadata,
    mask: SetMetadataMask,
) -> Result<Metadata, SystemError> {
    context.backing_metadata_request(raw, requested, mask)
}

pub(super) fn require_mapped_owner(
    context: &InodeOpContext,
    raw: &Metadata,
    error: SystemError,
) -> Result<(), SystemError> {
    let view = context.view_metadata(raw);
    if view.uid == u32::MAX as usize || view.gid == u32::MAX as usize {
        return Err(error);
    }
    Ok(())
}
