use alloc::sync::Arc;

use crate::driver::base::device::device_number::DeviceNumber;
use crate::filesystem::fsnotify::{self, FsEvent};
use crate::filesystem::vfs::mount::MountFSInode;
use crate::filesystem::vfs::{IndexNode, InodeMode};
use crate::libs::casting::DowncastArc;
use system_error::SystemError;

/// Create a node and publish its namespace event before another mutation of
/// the same parent can overtake it.
pub(super) fn mknod_and_notify(
    parent: &Arc<dyn IndexNode>,
    name: &str,
    mode: InodeMode,
    dev: DeviceNumber,
) -> Result<(), SystemError> {
    let notify = || fsnotify::fsnotify(FsEvent::CREATE, Some((parent, name)), None, 0);
    if let Some(mounted) = parent.clone().downcast_arc::<MountFSInode>() {
        mounted.mknod_with_post_commit(name, mode, dev, notify)
    } else {
        parent.mknod(name, mode, dev)?;
        notify();
        Ok(())
    }
}
