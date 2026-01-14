//! /proc/mounts - 系统挂载点信息
//!
//! 这个文件展示了系统当前的所有挂载点

use crate::libs::mutex::MutexGuard;
use crate::{
    filesystem::{
        procfs::{
            template::{Builder, FileOps, ProcFileBuilder},
            utils::proc_read,
        },
        vfs::{FilePrivateData, IndexNode, InodeMode},
    },
    process::ProcessManager,
};
use alloc::{
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
        ProcFileBuilder::new(Self, InodeMode::S_IRUGO) // 0444 - 所有用户可读
            .parent(parent)
            .build()
            .unwrap()
    }
}

/// 生成 mounts 内容
#[inline(never)]
fn generate_mounts_like_content(fmt: MountsFormat) -> String {
    let mntns = ProcessManager::current_mntns();
    let mounts = mntns.mount_list().clone_inner();

    // 以进程 fs root 为基准做 chroot 视图裁剪/重写
    let pcb = ProcessManager::current_pcb();
    let root_inode = pcb.fs_struct().root();
    let root_prefix = root_inode
        .absolute_path()
        .unwrap_or_else(|_| "/".to_string());
    let is_chrooted = root_prefix != "/";
    let root_prefix_with_slash = if root_prefix.ends_with('/') {
        root_prefix.clone()
    } else {
        root_prefix.clone() + "/"
    };

    let mut lines = Vec::with_capacity(mounts.len());
    let mut cap = 0;
    let mut mid: usize = 1;

    for (mp, mfs) in mounts {
        let mut line = String::new();
        let fs_type = mfs.fs_type();
        let source = match fs_type {
            // 特殊文件系统，直接显示文件系统名称
            "devfs" | "devpts" | "sysfs" | "procfs" | "tmpfs" | "ramfs" | "rootfs" | "debugfs"
            | "configfs" => fs_type.to_string(),
            // 其他文件系统，尝试显示挂载设备名称
            _ => {
                if let Some(s) = mfs.self_mountpoint() {
                    // 尝试从挂载点获取设备名称
                    if let Some(device_name) = s.dname().ok().map(|d| d.to_string()) {
                        device_name
                    } else {
                        // 如果获取不到设备名称，使用绝对路径
                        s.absolute_path().unwrap_or("unknown".to_string())
                    }
                } else {
                    // 没有挂载点信息，使用文件系统类型
                    fs_type.to_string()
                }
            }
        };

        // 过滤/改写 mountpoint（chroot 后应只暴露 root 下的挂载点，并重写为 chroot 视角）
        let mut mountpoint = mp.as_str().to_string();
        if is_chrooted {
            if mountpoint == root_prefix {
                mountpoint = "/".to_string();
            } else if mountpoint.starts_with(&root_prefix_with_slash) {
                // strip_prefix 会得到 "child/.."；保持以 '/' 开头
                let stripped = &mountpoint[root_prefix.len()..];
                mountpoint = if stripped.is_empty() {
                    "/".to_string()
                } else {
                    stripped.to_string()
                };
            } else {
                continue;
            }
        }

        match fmt {
            MountsFormat::Mounts => {
                line.push_str(&format!("{source} {m} {fs_type}", m = mountpoint));
                line.push(' ');
                line.push_str(&mfs.mount_flags().options_string());
                line.push_str(" 0 0\n");
            }
            MountsFormat::MountInfo => {
                // 极简 mountinfo：只保证 mountpoint 字段正确且不泄露 chroot 前缀
                // mount ID / parent ID / major:minor / root / mountpoint / options / - / fstype / source / superopts
                line.push_str(&format!(
                    "{id} {pid} 0:0 / {mp} {opts} - {fst} {src} {opts}\n",
                    id = mid,
                    pid = if mid == 1 { 0 } else { 1 },
                    mp = mountpoint,
                    opts = mfs.mount_flags().options_string(),
                    fst = fs_type,
                    src = source
                ));
            }
        }

        cap += line.len();
        lines.push(line);
        mid += 1;
    }

    let mut content = String::with_capacity(cap);
    for line in lines {
        content.push_str(&line);
    }

    return content;
}

impl FileOps for MountsFileOps {
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let mounts_content = generate_mounts_like_content(MountsFormat::Mounts);
        let bytes = mounts_content.as_bytes();

        proc_read(offset, len, buf, bytes)
    }
}

#[derive(Clone, Copy)]
enum MountsFormat {
    Mounts,
    MountInfo,
}

/// 为 /proc/<pid>/mountinfo 生成内容（极简版，满足 gVisor chroot_test）。
pub(super) fn generate_mountinfo_content() -> String {
    generate_mounts_like_content(MountsFormat::MountInfo)
}

/// 为 /proc/<pid>/mounts 生成内容。
pub(super) fn generate_mounts_content() -> String {
    generate_mounts_like_content(MountsFormat::Mounts)
}
