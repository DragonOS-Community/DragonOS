use alloc::{string::String, string::ToString, sync::Arc, vec::Vec};

use system_error::SystemError;

use crate::{filesystem::vfs::MountFS, process::ProcessControlBlock};

#[derive(Debug)]
pub(crate) struct ProcMountEntry {
    pub mount: Arc<MountFS>,
    pub mountpoint_display: String,
    pub parent_mount_id: usize,
}

pub(crate) fn collect_visible_mounts(
    target: &Arc<ProcessControlBlock>,
) -> Result<(Vec<ProcMountEntry>, String), SystemError> {
    let nsproxy = target.nsproxy();
    let mount_list = nsproxy.mnt_ns.mount_list();
    let root_path = target
        .fs_struct()
        .root()
        .absolute_path()
        .unwrap_or_else(|_| "/".to_string());

    let mut mounts = mount_list
        .clone_inner()
        .into_iter()
        .map(|(path, mfs)| (path.as_str().to_string(), mfs))
        .collect::<Vec<_>>();

    mounts.sort_by_key(|(_, mfs)| {
        let mount_id: usize = mfs.mount_id().into();
        mount_id
    });

    let mut entries = Vec::new();
    for (mount_path, mount) in mounts {
        let Some(mountpoint_display) = visible_mountpoint(&mount_path, &root_path) else {
            continue;
        };

        let parent_mount_id: usize = mount
            .self_mountpoint()
            .map(|mountpoint_inode| mountpoint_inode.mount_fs().mount_id().into())
            .unwrap_or_else(|| mount.mount_id().into());

        entries.push(ProcMountEntry {
            mount,
            mountpoint_display,
            parent_mount_id,
        });
    }

    Ok((entries, root_path))
}

fn visible_mountpoint(mountpoint: &str, root_path: &str) -> Option<String> {
    if root_path == "/" {
        return Some(mountpoint.to_string());
    }

    let root_prefix_with_slash = if root_path.ends_with('/') {
        root_path.to_string()
    } else {
        format!("{root_path}/")
    };

    if mountpoint == root_path {
        return Some("/".to_string());
    }

    if let Some(stripped) = mountpoint.strip_prefix(&root_prefix_with_slash) {
        return Some(if stripped.is_empty() {
            "/".to_string()
        } else {
            format!("/{stripped}")
        });
    }

    None
}
