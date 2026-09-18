use alloc::{string::String, string::ToString, sync::Arc, vec::Vec};

use system_error::SystemError;

use crate::{
    filesystem::vfs::{
        mount::{append_comma_options, with_topology_snapshot, MountFSInode, MountSnapshotGuard},
        FileSystem, MountFS,
    },
    libs::casting::DowncastArc,
};

use super::MountView;

/// One mount the table may list, identified without building its record.
///
/// The table is ordered by mount id and a reader resumes it from an id, so a
/// slice still has to enumerate every mount above the cursor to know which ones
/// follow. Keeping a mount that a slice does not render down to identity is
/// what makes that enumeration a topology step instead of a whole record: the
/// fields of a mount are built by [`ProcMountEntry::resolve()`], and only for
/// the mounts the slice hands out.
#[derive(Debug)]
pub(crate) struct ProcMountCandidate {
    /// Mount id: the key the table order and the reader's cursor use.
    pub mount_id: usize,
    pub mount: Arc<MountFS>,
}

/// One record of the table, with the fields its renderer needs.
#[derive(Debug)]
pub(crate) struct ProcMountEntry {
    pub mount: Arc<MountFS>,
    pub mountpoint_display: String,
    pub mountinfo_root: String,
    pub parent_mount_id: usize,
    /// Keeps the superblock backend alive while the record is rendered, without
    /// making an ordinary umount report the mount busy.
    pub _lifecycle_pin: MountSnapshotGuard,
    pub mount_id: usize,
    pub fstype: String,
    pub per_mount_options: String,
    pub super_block_options: String,
    pub mountinfo_tags: String,
}

/// One mount of the table as the current topology shows it: the mount point and
/// root path it is rendered from, and the pin that keeps its superblock behind
/// them.
///
/// This is what a record needs from the mount topology, so it is resolved as
/// one piece while that snapshot is held.  The fields left over are built from
/// it afterwards, because they run filesystem code: see
/// [`ProcMountEntry::resolve()`].
struct VisibleMount {
    mount: Arc<MountFS>,
    mountpoint_display: String,
    parent_mount_id: usize,
    mountinfo_root: String,
    /// Keeps the superblock backend alive while the rest of the record is
    /// built from it, without making an ordinary umount report the mount busy.
    pin: MountSnapshotGuard,
}

impl ProcMountEntry {
    /// Resolves the record of `candidate`, or `None` when the mount is not part
    /// of `view`'s table.
    ///
    /// The mount's place in the topology -- the paths it is rendered from and
    /// the pin its superblock needs -- is taken under one topology snapshot per
    /// record, the way `seq_path_root()` renders a path from the topology of the
    /// call it serves.  The rest of the record is built with that snapshot
    /// released, because it runs filesystem code (the source name and the extra
    /// mount options of the filesystem, then the metadata of the mount root,
    /// which reads the on-disk inode of a disk filesystem): a filesystem that
    /// needs the topology lock itself must not be called under it, and a slow
    /// one must not stall the mount lifecycle lock every mount in the system
    /// shares.
    pub(crate) fn resolve(
        candidate: &ProcMountCandidate,
        view: &MountView,
    ) -> Result<Option<Self>, SystemError> {
        let Some(visible) = with_topology_snapshot(|| VisibleMount::resolve(candidate, view))?
        else {
            return Ok(None);
        };
        Ok(Some(Self::from_visible(visible)?))
    }

    /// Builds the fields every mount-family record shares, from a mount whose
    /// superblock the snapshot pin already keeps alive.
    fn from_visible(visible: VisibleMount) -> Result<Self, SystemError> {
        let VisibleMount {
            mount,
            mountpoint_display,
            parent_mount_id,
            mountinfo_root,
            pin,
        } = visible;
        let mount_flags = mount.mount_flags();
        let mut per_mount_options = mount_flags.proc_rw_token().to_string();
        append_comma_options(&mut per_mount_options, mount_flags.proc_per_mount_options());
        let super_block_flags = mount.super_block_flags();
        let mut super_block_options = super_block_flags.proc_rw_token().to_string();
        append_comma_options(
            &mut super_block_options,
            super_block_flags.proc_super_block_options(),
        );
        Ok(Self {
            mount_id: mount.mount_id().into(),
            fstype: mount.fs_type().to_string(),
            mountinfo_tags: mount.propagation().proc_mountinfo_tags(),
            per_mount_options,
            super_block_options,
            mount,
            mountpoint_display,
            mountinfo_root,
            parent_mount_id,
            _lifecycle_pin: pin,
        })
    }
}

impl VisibleMount {
    /// Resolves `candidate` from `view`, or `None` when the mount is not part
    /// of `view`'s table.  The caller holds the topology snapshot.
    fn resolve(
        candidate: &ProcMountCandidate,
        view: &MountView,
    ) -> Result<Option<Self>, SystemError> {
        if Arc::ptr_eq(&candidate.mount, &view.root.mount_fs()) {
            Self::resolve_pinned_root(view)
        } else {
            Self::resolve_attached(candidate, view)
        }
    }

    /// The record source of the mount the pinned root lives on.
    ///
    /// That mount is rendered from its root inode rather than from a mount
    /// point, and its parent is the namespace's hidden root parent when it is
    /// the namespace root. Mirror seq_path_root(): a containing mount whose root
    /// lies above a chrooted ordinary directory is not visible and must not be
    /// synthesized as '/'.  When the chroot is exactly a mount root, keep its
    /// real (possibly invisible) parent mount id.
    fn resolve_pinned_root(view: &MountView) -> Result<Option<Self>, SystemError> {
        let root = view.root.clone();
        let root_mount = root.mount_fs();
        let root_mount_inode = root_mount
            .root_inode()
            .downcast_arc::<MountFSInode>()
            .ok_or(SystemError::EINVAL)?;
        let Some(mountpoint_display) = root_mount_inode.relative_path_from_snapshot(&root)? else {
            return Ok(None);
        };
        let mount_namespace = view.ns.clone();
        let parent_mount_id = root_mount
            .self_mountpoint()
            .map(|mountpoint| mountpoint.mount_fs().mount_id().into())
            .or_else(|| {
                Arc::ptr_eq(&root_mount, &mount_namespace.root_mntfs())
                    .then(|| mount_namespace.root_parent_mount_id().map(|id| id.data()))
                    .flatten()
            })
            .unwrap_or_else(|| root_mount.mount_id().into());
        let mountinfo_root = root_mount.root_path_from_snapshot()?;
        Ok(Some(Self::from_mount(
            root_mount,
            mountpoint_display,
            parent_mount_id,
            mountinfo_root,
        )?))
    }

    /// The record source of a mount attached below the pinned root's mount.
    ///
    /// Returns `None` when the mount point is outside the pinned root: a mount
    /// a chroot does not reach is not part of the table, the way `seq_path_root()`
    /// makes `show_vfsmnt()` emit no record for it.
    fn resolve_attached(
        candidate: &ProcMountCandidate,
        view: &MountView,
    ) -> Result<Option<Self>, SystemError> {
        let mount = candidate.mount.clone();
        // A mount the walk reached is attached to the topology, and the walk is
        // re-taken per record, so a mount an umount took out of the pinned root
        // in between is passed over: it is no longer part of the table either.
        let Some(mountpoint) = mount.self_mountpoint() else {
            return Ok(None);
        };
        let Some(mountpoint_display) = mountpoint.relative_path_from_snapshot(&view.root)? else {
            return Ok(None);
        };
        let parent_mount_id = mountpoint.mount_fs().mount_id().into();
        let mountinfo_root = mount.root_path_from_snapshot()?;
        Ok(Some(Self::from_mount(
            mount,
            mountpoint_display,
            parent_mount_id,
            mountinfo_root,
        )?))
    }

    /// The paths of `mount` plus the pin its record is built on.
    fn from_mount(
        mount: Arc<MountFS>,
        mountpoint_display: String,
        parent_mount_id: usize,
        mountinfo_root: String,
    ) -> Result<Self, SystemError> {
        // The mount is in the topology of this snapshot, so its superblock is
        // still alive: a mount leaves the topology before it releases its claim
        // on the superblock, and the `try_pin_snapshot()` failure arm is
        // defensive.
        let pin = mount.try_pin_snapshot()?;
        Ok(Self {
            mount,
            mountpoint_display,
            parent_mount_id,
            mountinfo_root,
            pin,
        })
    }
}

/// The mounts reachable from `view`'s pinned root, in mount-id order, above
/// `after`.
///
/// `after` is the mount id a reader's previous slice reached: the mounts at or
/// below it keep their place in the table but no longer need their record
/// built, so a reader that resumes late does not pay again for the records it
/// already has. A sliced table is re-derived rather than resumed, because a
/// mount id is the only order the namespace offers; see
/// [`render_mount_slice()`](super::render_mount_slice) for how a slice turns the
/// result into records.
pub(crate) fn collect_mount_candidates(
    view: &MountView,
    after: Option<usize>,
) -> Result<Vec<ProcMountCandidate>, SystemError> {
    let root = view.root.clone();

    if root.is_disconnected() {
        return Ok(Vec::new());
    }
    let root_mount = root.mount_fs();
    let mut candidates = Vec::new();
    let mut pending = Vec::new();
    push_candidate(&mut candidates, &root_mount, after);
    pending.extend(root_mount.mount_children());

    while let Some(mount) = pending.pop() {
        push_candidate(&mut candidates, &mount, after);
        // The mount a chroot hides still has to be walked: a visible mount
        // below it is reachable through it, and only the record says whether
        // the mount point is inside the pinned root.
        pending.extend(mount.mount_children());
    }

    // A mount is always older than the mounts below it, but a mount created
    // inside one subtree is older than a sibling subtree's mount, so the tree
    // gives every mount once without ordering them by id.
    candidates.sort_unstable_by_key(|candidate| candidate.mount_id);
    Ok(candidates)
}

/// Adds `mount` to `candidates` unless a previous slice already reached it.
///
/// Mount ids are allocated in increasing order and never reused
/// (`MountId::alloc()`), so "already reached" is exactly "id at or below the
/// cursor".
fn push_candidate(
    candidates: &mut Vec<ProcMountCandidate>,
    mount: &Arc<MountFS>,
    after: Option<usize>,
) {
    let mount_id: usize = mount.mount_id().into();
    if after.is_some_and(|reached| mount_id <= reached) {
        return;
    }
    candidates.push(ProcMountCandidate {
        mount_id,
        mount: mount.clone(),
    });
}
