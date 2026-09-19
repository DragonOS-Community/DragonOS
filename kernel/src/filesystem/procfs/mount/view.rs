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
/// an already open fd reports. `/proc/[pid]/mounts` is registered system-wide
/// and `/proc/mounts` and `/proc/self/mounts` resolve to it through the
/// `self` symlink.
///
/// The two halves are also taken as one pair, because a mount namespace switch
/// publishes them together: see [`MountView::capture()`].
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
    /// before it gets here), from one [`ProcessControlBlock::namespace_state()`]
    /// snapshot: a `setns(CLONE_NEWNS)` or `unshare(CLONE_NEWNS)` running in
    /// another thread of that task publishes a new mount namespace and a new
    /// root together, and this fd must not report a mixture of the two
    /// generations.
    ///
    /// A task without a root directory is `ENOENT`, like the `!task->fs` check
    /// there. A root that is not a mount cannot happen for one that has an
    /// `fs_struct`, so that arm is defensive.
    pub(crate) fn capture(task: &Arc<ProcessControlBlock>) -> Result<Self, SystemError> {
        // The mount namespace and the root are published together by every
        // mount namespace switch, so they are taken together here: one
        // snapshot cannot mix the namespace of one generation with the root of
        // the next.
        let state = task.namespace_state();
        let ns = state.nsproxy.mnt_ns.clone();
        let root = state
            .fs
            .ok_or(SystemError::ENOENT)?
            .root()
            .downcast_arc::<MountFSInode>()
            .ok_or(SystemError::EINVAL)?;
        Ok(Self { ns, root })
    }
}
