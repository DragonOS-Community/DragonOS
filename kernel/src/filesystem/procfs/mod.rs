//! ProcFS - 进程文件系统
//!
//! 实现 Linux 兼容的 /proc 文件系统

use crate::mm::ucontext::AddressSpace;
use alloc::sync::Arc;
use system_error::SystemError;

use crate::{
    libs::once::Once,
    process::{
        cred::Cred, namespace::net_namespace::NetNamespace,
        namespace::pid_namespace::INIT_PID_NAMESPACE, ProcessManager,
    },
};

use super::vfs::mount::MountFlags;
use super::vfs::InodeMode;
use mount::MountView;

mod cmdline;
mod cpuinfo;
pub mod klog;
pub mod kmsg;
mod kmsg_file;
mod loadavg;
mod meminfo;
mod mount;
mod net;
mod pid;
pub mod root;
mod self_;
mod stat;
mod sys;
mod syscall;
pub(super) mod template;
mod thread_self;
mod utils;
mod version;
mod version_signature;
mod vmstat;

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
    pub open_cred: Arc<Cred>,
    /// Address space this fd was opened on, taken by `open()` of the files that
    /// address one (`/proc/[pid]/mem`, `/proc/[pid]/maps`).
    ///
    /// The descriptor, deliberately not the memory: Linux `proc_mem_open()`
    /// grabs the `mm_struct` and then drops the user reference again
    /// ("but do not pin its memory"), so an `execve()` or an exit in the target
    /// still tears the mappings down while this fd stays open. Each read
    /// re-checks the user count (`mmget_not_zero()`), which is how both files
    /// learn that there is nothing left to serve.
    pub(crate) pinned_vm: Option<Arc<AddressSpace>>,
    /// Streaming state of a seq-style record (Linux `struct seq_file`). Only
    /// `utils::proc_read_seq()` and `utils::proc_read_snapshot()` touch it.
    pub(crate) seq: utils::ProcfsSeq,
    /// Mount namespace and root directory pinned by `open()` for
    /// `/proc/[pid]/{mounts,mountinfo,mountstats}`, as `mounts_open_common()`
    /// does; `None` for every other procfs file.
    pub(crate) mount_view: Option<MountView>,
    /// Network namespace pinned by `open()` for the files under `/proc/net`,
    /// as `seq_open_net()` stores it in `seq_net_private`; `None` for every
    /// other procfs file.
    pub(crate) net_ns: Option<Arc<NetNamespace>>,
    /// User namespace pinned by opening /proc/[pid]/{uid_map,gid_map,setgroups}.
    pub(crate) user_ns: Option<Arc<crate::process::namespace::user_namespace::UserNamespace>>,
}

impl ProcfsFilePrivateData {
    pub fn new() -> Self {
        ProcfsFilePrivateData {
            open_cred: ProcessManager::current_pcb().cred(),
            pinned_vm: None,
            seq: utils::ProcfsSeq::default(),
            mount_view: None,
            net_ns: None,
            user_ns: None,
        }
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
        let procfs: Arc<ProcFS> = ProcFS::new(INIT_PID_NAMESPACE.clone());
        let root_inode = ProcessManager::current_mntns().root_inode();
        // procfs 挂载
        root_inode
            .mkdir("proc", InodeMode::from_bits_truncate(0o755))
            .expect("Unable to create /proc")
            .mount(procfs, MountFlags::empty())
            .expect("Failed to mount at /proc");
        ::log::info!("ProcFS mounted at /proc");
        result = Some(Ok(()));
    });

    return result.unwrap();
}
