use alloc::sync::Arc;

use crate::driver::base::device::device_number::DeviceNumber;
use crate::filesystem::fsnotify::{self, FsEvent};
use crate::filesystem::vfs::mount::MountFSInode;
use crate::filesystem::vfs::permission::{check_inode_permission, PermissionMask};
use crate::filesystem::vfs::{FileType, IndexNode, InodeMode};
use crate::libs::casting::DowncastArc;
use crate::process::cred::{capable, CAPFlags};
use system_error::SystemError;

/// Create a node and publish its namespace event before another mutation of
/// the same parent can overtake it.
pub(super) fn mknod_and_notify(
    parent: &Arc<dyn IndexNode>,
    name: &str,
    mode: InodeMode,
    dev: DeviceNumber,
) -> Result<(), SystemError> {
    // Match Linux vfs_mknod()'s ordering: parent/mount permissions precede
    // CAP_MKNOD and the cgroup device hook. The underlying filesystem still
    // performs the final atomic namespace check under its create gate.
    let parent_md = parent.metadata()?;
    if parent_md.file_type != FileType::Dir {
        return Err(SystemError::ENOTDIR);
    }
    check_inode_permission(
        parent,
        &parent_md,
        PermissionMask::MAY_WRITE | PermissionMask::MAY_EXEC,
    )?;

    let kind = if mode & InodeMode::S_IFMT == InodeMode::S_IFCHR {
        Some(FileType::CharDevice)
    } else if mode & InodeMode::S_IFMT == InodeMode::S_IFBLK {
        Some(FileType::BlockDevice)
    } else {
        None
    };
    if let Some(kind) = kind {
        let whiteout = kind == FileType::CharDevice && dev.data() == 0;
        if !whiteout {
            if !capable(CAPFlags::CAP_MKNOD) {
                return Err(SystemError::EPERM);
            }
            crate::bpf::check_device_permission(kind, dev, 1)?;
        }
    }

    let notify = || fsnotify::fsnotify(FsEvent::CREATE, Some((parent, name)), None, 0);
    if let Some(mounted) = parent.clone().downcast_arc::<MountFSInode>() {
        mounted.mknod_with_post_commit(name, mode, dev, notify)
    } else {
        parent.mknod(name, mode, dev)?;
        notify();
        Ok(())
    }
}
