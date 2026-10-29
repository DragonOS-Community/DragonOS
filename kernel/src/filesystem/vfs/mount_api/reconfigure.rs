//! Shared superblock reconfiguration, independent of a mount's namespace.
//!
//! The caller owns an active superblock reference and holds `umount_write`.
//! Legacy remount can publish mount flags in the same topology transaction;
//! fsconfig publishes only the shared superblock flags.

use alloc::sync::Arc;
use system_error::SystemError;

use crate::{
    filesystem::vfs::{
        mount::{writer::SuperBlockReadonlyTransition, MountFS, MountFlags, SuperBlockState},
        FsReconfigureRequest,
    },
    process::cred::{ns_capable, CAPFlags},
};

#[must_use = "a successful backend reconfiguration must be published"]
pub(crate) struct PreparedReconfiguration {
    superblock: Arc<SuperBlockState>,
    transition: Option<SuperBlockReadonlyTransition>,
    flags: MountFlags,
}

impl PreparedReconfiguration {
    /// Infallible publication; no backend callbacks or allocation occur here.
    pub(crate) fn commit(self) {
        match self.transition {
            Some(transition) => transition.commit(self.flags),
            None => self.superblock.set_flags(self.flags),
        }
    }
}

pub(crate) fn prepare_reconfigure_locked(
    mount: &Arc<MountFS>,
    request: FsReconfigureRequest<'_>,
) -> Result<PreparedReconfiguration, SystemError> {
    let superblock = mount.super_block_state();
    if !ns_capable(superblock.owner_user_ns(), CAPFlags::CAP_SYS_ADMIN) {
        return Err(SystemError::EPERM);
    }
    if !(request.sb_flags_mask & !MountFlags::RMT_MASK).is_empty() {
        return Err(SystemError::EINVAL);
    }

    let old_flags = superblock.flags();
    let transition = if request.sb_flags_mask.contains(MountFlags::RDONLY) {
        superblock
            .begin_readonly_transition(request.sb_flags.contains(MountFlags::RDONLY), false)?
    } else {
        None
    };
    // Preserve the context's requested flags as callback input. In particular,
    // overlayfs checks requested RDONLY even when the mask does not select it.
    let mask = request.sb_flags_mask;
    let callback_flags = mount.inner_filesystem().reconfigure(request)?;
    let flags = (old_flags & !mask) | (callback_flags & mask);
    Ok(PreparedReconfiguration {
        superblock,
        transition,
        flags,
    })
}
