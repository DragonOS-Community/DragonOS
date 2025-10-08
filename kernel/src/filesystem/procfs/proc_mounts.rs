use alloc::vec::Vec;

use alloc::string::{String, ToString};

use system_error::SystemError;

use crate::{filesystem::vfs::IndexNode, process::ProcessManager};

use super::{ProcFSInode, ProcfsFilePrivateData};

impl ProcFSInode {
    /// 打开 meminfo 文件
    #[inline(never)]
    pub(super) fn open_mounts(
        &self,
        pdata: &mut ProcfsFilePrivateData,
    ) -> Result<i64, SystemError> {
        // 生成mount信息
        let mount_content = Self::generate_mounts_content();

        pdata.data = mount_content.into_bytes();
        return Ok(pdata.data.len() as i64);
    }

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

        return content;
    }
}
