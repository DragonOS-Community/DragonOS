//! /proc/mounts - 系统挂载点信息
//!
//! 这个文件展示了系统当前的所有挂载点

use crate::libs::mutex::MutexGuard;
use crate::{
    driver::base::device::device_number::DeviceNumber,
    filesystem::{
        procfs::{
            template::{Builder, FileOps, ProcFileBuilder},
            utils::proc_read,
        },
        vfs::{mount::MountFS, FilePrivateData, IndexNode, InodeMode},
    },
    process::{namespace::mnt::MntNamespace, ProcessControlBlock, ProcessManager},
};
use alloc::{
    format,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

/// /proc/mounts 文件的 FileOps 实现
#[derive(Debug)]
pub struct MountsFileOps;

impl MountsFileOps {
    pub fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self, InodeMode::S_IRUGO)
            .parent(parent)
            .build()
            .unwrap()
    }
}

#[derive(Clone, Copy)]
enum MountsFormat {
    Mounts,
    MountInfo,
}

fn escape_mount_field(raw: &str) -> String {
    let mut escaped = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            ' ' => escaped.push_str("\\040"),
            '\t' => escaped.push_str("\\011"),
            '\n' => escaped.push_str("\\012"),
            '\\' => escaped.push_str("\\\\"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn mount_source_display(mfs: &Arc<MountFS>, fs_type: &str) -> String {
    mfs.mount_source().unwrap_or_else(|| match fs_type {
        "devfs" | "devpts" | "sysfs" | "procfs" | "tmpfs" | "ramfs" | "rootfs" | "debugfs"
        | "configfs" => fs_type.to_string(),
        _ => fs_type.to_string(),
    })
}

fn mount_root_display(mfs: &Arc<MountFS>) -> String {
    let root = mfs
        .root_inner_inode()
        .absolute_path()
        .unwrap_or_else(|_| "/".to_string());

    if root.is_empty() {
        "/".to_string()
    } else {
        root
    }
}

fn mount_dev_display(mfs: &Arc<MountFS>) -> DeviceNumber {
    mfs.mountpoint_root_inode()
        .metadata()
        .map(|md| DeviceNumber::from(md.dev_id as u32))
        .unwrap_or_default()
}

fn rewrite_mountpoint_for_root(mountpoint: &str, root_prefix: &str) -> Option<String> {
    if root_prefix == "/" {
        return Some(mountpoint.to_string());
    }

    let root_prefix_with_slash = if root_prefix.ends_with('/') {
        root_prefix.to_string()
    } else {
        format!("{root_prefix}/")
    };

    if mountpoint == root_prefix {
        Some("/".to_string())
    } else if let Some(stripped) = mountpoint.strip_prefix(&root_prefix_with_slash) {
        if stripped.is_empty() {
            Some("/".to_string())
        } else {
            Some(format!("/{stripped}"))
        }
    } else {
        None
    }
}

fn mountinfo_optional_fields(mfs: &Arc<MountFS>) -> String {
    let propagation = mfs.propagation();
    let mut fields = Vec::new();
    let info = propagation.info_string();

    if !info.is_empty() {
        fields.push(info);
    }
    if propagation.is_unbindable() {
        fields.push("unbindable".to_string());
    }

    if fields.is_empty() {
        String::new()
    } else {
        format!(" {}", fields.join(" "))
    }
}

fn collect_mounts(mntns: &Arc<MntNamespace>) -> Vec<(String, Arc<MountFS>)> {
    let mut mounts = mntns
        .mount_list()
        .clone_inner()
        .into_iter()
        .map(|(path, mfs)| (path.as_str().to_string(), mfs))
        .collect::<Vec<_>>();

    mounts.sort_by_key(|(_, mfs)| {
        let mount_id: usize = mfs.mount_id().into();
        mount_id
    });
    mounts
}

#[inline(never)]
fn generate_mounts_like_content_for_view(
    mntns: &Arc<MntNamespace>,
    root_inode: Arc<dyn IndexNode>,
    fmt: MountsFormat,
) -> String {
    let mounts = collect_mounts(mntns);
    let root_prefix = root_inode
        .absolute_path()
        .unwrap_or_else(|_| "/".to_string());

    let mut lines = Vec::with_capacity(mounts.len());
    let mut cap = 0;

    for (mount_path, mfs) in mounts {
        let Some(mountpoint) = rewrite_mountpoint_for_root(&mount_path, &root_prefix) else {
            continue;
        };

        let fs_type = mfs.fs_type();
        let source = escape_mount_field(&mount_source_display(&mfs, fs_type));
        let fs_type = escape_mount_field(fs_type);
        let mountpoint = escape_mount_field(&mountpoint);
        let mount_opts = mfs.mount_flags().options_string();

        let line = match fmt {
            MountsFormat::Mounts => {
                format!("{source} {mountpoint} {fs_type} {mount_opts} 0 0\n")
            }
            MountsFormat::MountInfo => {
                let mount_id: usize = mfs.mount_id().into();
                let parent_id: usize = mfs
                    .self_mountpoint()
                    .map(|mountpoint_inode| {
                        let parent_id: usize = mountpoint_inode.mount_fs().mount_id().into();
                        parent_id
                    })
                    .unwrap_or(mount_id);
                let dev = mount_dev_display(&mfs);
                let root = escape_mount_field(&mount_root_display(&mfs));
                let optional_fields = mountinfo_optional_fields(&mfs);

                format!(
                    "{mount_id} {parent_id} {}:{} {root} {mountpoint} {mount_opts}{optional_fields} - {fs_type} {source} {mount_opts}\n",
                    dev.major().data(),
                    dev.minor()
                )
            }
        };

        cap += line.len();
        lines.push(line);
    }

    let mut content = String::with_capacity(cap);
    for line in lines {
        content.push_str(&line);
    }
    content
}

fn generate_mounts_like_content_for_task(
    task: &Arc<ProcessControlBlock>,
    fmt: MountsFormat,
) -> String {
    let nsproxy = task.nsproxy();
    let fs = task.fs_struct();
    generate_mounts_like_content_for_view(nsproxy.mnt_namespace(), fs.root(), fmt)
}

pub(super) fn cache_procfs_file_content(
    data: &mut MutexGuard<FilePrivateData>,
    content: String,
) -> Result<(), SystemError> {
    let FilePrivateData::Procfs(pdata) = &mut **data else {
        return Err(SystemError::EIO);
    };
    pdata.data = content.into_bytes();
    Ok(())
}

pub(super) fn read_cached_procfs_file_content(
    offset: usize,
    len: usize,
    buf: &mut [u8],
    data: MutexGuard<FilePrivateData>,
) -> Result<usize, SystemError> {
    let bytes = match &*data {
        FilePrivateData::Procfs(pdata) => pdata.data.as_slice(),
        _ => return Err(SystemError::EIO),
    };

    proc_read(offset, len, buf, bytes)
}

impl FileOps for MountsFileOps {
    fn open(&self, data: &mut MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        cache_procfs_file_content(data, generate_mounts_content())
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        read_cached_procfs_file_content(offset, len, buf, data)
    }
}

/// 为 /proc/<pid>/mountinfo 生成内容（目标任务视角）。
pub(super) fn generate_mountinfo_content_for_task(task: &Arc<ProcessControlBlock>) -> String {
    generate_mounts_like_content_for_task(task, MountsFormat::MountInfo)
}

/// 为 /proc/<pid>/mounts 生成内容。
pub(super) fn generate_mounts_content() -> String {
    let current = ProcessManager::current_pcb();
    generate_mounts_like_content_for_task(&current, MountsFormat::Mounts)
}

/// 为 /proc/<pid>/mounts 生成内容（目标任务视角）。
pub(super) fn generate_mounts_content_for_task(task: &Arc<ProcessControlBlock>) -> String {
    generate_mounts_like_content_for_task(task, MountsFormat::Mounts)
}
