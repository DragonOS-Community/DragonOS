use alloc::{string::String, string::ToString, sync::Arc, vec::Vec};

use system_error::SystemError;

use crate::{
    filesystem::vfs::{
        mount::{append_comma_options, MountFSInode, MountSnapshotGuard},
        FileSystem, MountFS,
    },
    libs::casting::DowncastArc,
    process::ProcessControlBlock,
};

#[derive(Debug)]
pub(crate) struct ProcMountEntry {
    pub mount: Arc<MountFS>,
    pub mountpoint_display: String,
    pub mountinfo_root: String,
    pub parent_mount_id: usize,
    pub _lifecycle_pin: MountSnapshotGuard,
    pub mount_id: usize,
    pub fstype: String,
    pub per_mount_options: String,
    pub super_block_options: String,
    pub mountinfo_tags: String,
}

pub(crate) fn collect_visible_mounts(
    target: &Arc<ProcessControlBlock>,
) -> Result<(Vec<ProcMountEntry>, String), SystemError> {
    let root = target
        .try_fs_struct()
        .ok_or(SystemError::ESRCH)?
        .root()
        .downcast_arc::<MountFSInode>()
        .ok_or(SystemError::EINVAL)?;

    if root.is_disconnected() {
        return Ok((Vec::new(), "/".to_string()));
    }
    let root_mount = root.mount_fs();
    let mut mounts = Vec::new();
    let mount_root = root_mount
        .root_inode()
        .downcast_arc::<MountFSInode>()
        .ok_or(SystemError::EINVAL)?;
    // Mirror seq_path_root(): a containing mount whose root lies above a
    // chrooted ordinary directory is not visible and must not be
    // synthesized as '/'.  When the chroot is exactly a mount root, keep
    // its real (possibly invisible) parent mount id.
    if let Some(mountpoint_display) = mount_root.relative_path_from_snapshot(&root)? {
        let parent_mount_id = root_mount
            .self_mountpoint()
            .map(|mountpoint| mountpoint.mount_fs().mount_id().into())
            .unwrap_or_else(|| root_mount.mount_id().into());
        mounts.push((
            mountpoint_display,
            root_mount.root_path_from_snapshot()?,
            parent_mount_id,
            root_mount.clone(),
        ));
    }
    let mut pending = root_mount.mount_children();
    while let Some(mount) = pending.pop() {
        let mountpoint = mount.self_mountpoint().ok_or(SystemError::EINVAL)?;
        let Some(mountpoint_display) = mountpoint.relative_path_from_snapshot(&root)? else {
            continue;
        };
        let parent_mount_id = mountpoint.mount_fs().mount_id().into();
        pending.extend(mount.mount_children());
        mounts.push((
            mountpoint_display,
            mount.root_path_from_snapshot()?,
            parent_mount_id,
            mount,
        ));
    }

    mounts.sort_by_key(|(_, _, _, mfs)| {
        let mount_id: usize = mfs.mount_id().into();
        mount_id
    });
    let entries = mounts
        .into_iter()
        .map(
            |(mountpoint_display, mountinfo_root, parent_mount_id, mount)| {
                let mount_flags = mount.mount_flags();
                let mut per_mount_options = mount_flags.proc_rw_token().to_string();
                append_comma_options(&mut per_mount_options, mount_flags.proc_per_mount_options());
                let super_block_flags = mount.super_block_flags();
                let mut super_block_options = super_block_flags.proc_rw_token().to_string();
                append_comma_options(
                    &mut super_block_options,
                    super_block_flags.proc_super_block_options(),
                );
                Ok(ProcMountEntry {
                    _lifecycle_pin: mount.try_pin_snapshot()?,
                    mount_id: mount.mount_id().into(),
                    fstype: mount.fs_type().to_string(),
                    mountinfo_tags: mount.propagation().proc_mountinfo_tags(),
                    per_mount_options,
                    super_block_options,
                    mount,
                    mountpoint_display,
                    mountinfo_root,
                    parent_mount_id,
                })
            },
        )
        .collect::<Result<Vec<_>, SystemError>>()?;
    Ok((entries, "/".to_string()))
}
