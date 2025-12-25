//! ProcFS - 进程文件系统
//!
//! 实现 Linux 兼容的 /proc 文件系统

use alloc::{sync::Arc, vec::Vec};
use system_error::SystemError;

use crate::{libs::once::Once, process::ProcessManager};

use super::vfs::mount::{MountFlags, MountPath};
use super::vfs::syscall::ModeType;

mod cmdline;
mod cpuinfo;
pub mod kmsg;
mod kmsg_file;
pub mod log;
mod meminfo;
mod mounts;
mod pid;
pub mod root;
mod self_;
mod syscall;
pub(super) mod template;
mod utils;
mod version;

// 重新导出 ProcFS
pub use root::ProcFS;

/// procfs 的 inode 名称的最大长度
pub(super) const PROCFS_MAX_NAMELEN: usize = 64;
/// procfs 的块大小
pub(super) const PROCFS_BLOCK_SIZE: u64 = 512;

/// 供 template 使用的 Builder trait
pub(super) use template::Builder;

/// procfs 文件私有数据
#[derive(Debug, Clone)]
pub struct ProcfsFilePrivateData {
    pub data: Vec<u8>,
}

impl ProcfsFilePrivateData {
    pub fn new() -> Self {
        ProcfsFilePrivateData { data: Vec::new() }
    }
}

impl Default for ProcfsFilePrivateData {
    fn default() -> Self {
        Self::new()
    }
}

/// 初始化 ProcFS
pub fn procfs_init() -> Result<(), SystemError> {
    static INIT: Once = Once::new();
    let mut result = None;
    INIT.call_once(|| {
        ::log::info!("Initializing ProcFS...");
        // 创建 procfs 实例
        let procfs: Arc<ProcFS> = ProcFS::new();
        let root_inode = ProcessManager::current_mntns().root_inode();
        // procfs 挂载
        let mntfs = root_inode
            .mkdir("proc", ModeType::from_bits_truncate(0o755))
            .expect("Unable to create /proc")
            .mount(procfs, MountFlags::empty())
            .expect("Failed to mount at /proc");
        let ino = root_inode.metadata().unwrap().inode_id;
        let mount_path = Arc::new(MountPath::from("/proc"));
        ProcessManager::current_mntns()
            .add_mount(Some(ino), mount_path, mntfs)
            .expect("Failed to add mount for /proc");
        ::log::info!("ProcFS mounted at /proc");
        result = Some(Ok(()));
    });

    return result.unwrap();
}
