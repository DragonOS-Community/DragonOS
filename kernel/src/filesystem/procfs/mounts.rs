//! /proc/mounts - 系统挂载点信息
//!
//! 这个文件展示了系统当前的所有挂载点

use crate::{
    filesystem::{
        procfs::{
            template::{Builder, FileOps, ProcFileBuilder},
            utils::proc_read,
        },
        vfs::{syscall::ModeType, FilePrivateData, IndexNode},
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
        ProcFileBuilder::new(Self, ModeType::S_IRUGO) // 0444 - 所有用户可读
            .parent(parent)
            .build()
            .unwrap()
    }

    /// 生成 mounts 内容
    #[inline(never)]
    fn generate_mounts_content() -> String {
        let mntns = ProcessManager::current_mntns();
        let mounts = mntns.mount_list().clone_inner();

        let mut lines = Vec::with_capacity(mounts.len());
        let mut cap = 0;
        for (mp, mfs) in mounts {
            let mut line = String::new();
            let fs_type = mfs.fs_type();
            let source = match fs_type {
                // 特殊文件系统，直接显示文件系统名称
                "devfs" | "devpts" | "sysfs" | "procfs" | "tmpfs" | "ramfs" | "rootfs"
                | "debugfs" | "configfs" => fs_type.to_string(),
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

            line.push_str(&format!("{source} {m} {fs_type}", m = mp.as_str()));

            line.push(' ');
            line.push_str(&mfs.mount_flags().options_string());

            line.push_str(" 0 0\n");
            cap += line.len();
            lines.push(line);
        }

        let mut content = String::with_capacity(cap);
        for line in lines {
            content.push_str(&line);
        }

        content
    }
}

impl FileOps for MountsFileOps {
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: crate::libs::spinlock::SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let mounts_content = Self::generate_mounts_content();
        let bytes = mounts_content.as_bytes();

        proc_read(offset, len, buf, bytes)
    }
}
