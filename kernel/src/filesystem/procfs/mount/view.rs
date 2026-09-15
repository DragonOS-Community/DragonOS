use core::fmt::Debug;

use alloc::sync::Arc;

use system_error::SystemError;

use crate::{
    filesystem::vfs::mount::MountFSInode,
    libs::casting::DowncastArc,
    process::{namespace::mnt::MntNamespace, ProcessControlBlock},
};

/// The mount namespace and root directory a `/proc/[pid]/{mounts,mountinfo,
/// mountstats}` fd renders from.
///
/// Linux `mounts_open_common()` resolves the target once at open time: under
/// `task_lock()` it takes `task->nsproxy->mnt_ns` (`get_mnt_ns()`) and
/// `get_fs_root(task->fs, &root)`, and stores them in the seq private data
/// (`p->ns`, `p->root`, the path `seq_path_root()` renders from). A `setns()`,
/// `unshare()` or `chroot()` performed afterwards therefore cannot change what
/// an already open fd reports.
#[derive(Clone)]
pub(crate) struct MountView {
    /// Mount namespace the record is collected from.
    pub ns: Arc<MntNamespace>,
    /// Root directory of the target, used for path rendering the way
    /// `seq_path_root()` uses `proc_mounts::root`.
    pub root: Arc<MountFSInode>,
}

impl Debug for MountView {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MountView")
            .field("ns", &Arc::as_ptr(&self.ns))
            .field("root", &self.root)
            .finish()
    }
}

impl MountView {
    /// Pins the view of `task`, which the caller resolved from the proc inode
    /// already (`mounts_open_common()` reports `EINVAL` for a task that is gone
    /// before it gets here).
    ///
    /// A task without a root directory is `ENOENT`, like the `!task->fs` check
    /// there. A root that is not a mount cannot happen for one that has an
    /// `fs_struct`, so that arm is defensive.
    pub(crate) fn capture(task: &Arc<ProcessControlBlock>) -> Result<Self, SystemError> {
        let ns = task.nsproxy().mnt_ns.clone();
        let root = task
            .try_fs_struct()
            .ok_or(SystemError::ENOENT)?
            .root()
            .downcast_arc::<MountFSInode>()
            .ok_or(SystemError::EINVAL)?;
        Ok(Self { ns, root })
    }
}
