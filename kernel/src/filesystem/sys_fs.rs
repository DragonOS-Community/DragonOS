//! 文件系统 sysfs 接口
//! 
//! 该模块负责创建和管理 `/sys/fs` 目录，为各种文件系统提供 sysfs 接口。
//! 各个文件系统可以在 `/sys/fs` 下创建自己的子目录来暴露文件系统特定的信息和控制接口。

use crate::{
    driver::base::{kset::KSet, kobject::KObject},
    filesystem::kernfs::{dynamic::DynamicLookup, KernFSInode},
    filesystem::vfs::{IndexNode, FileSystem},
    init::initcall::INITCALL_SUBSYS,
};
use alloc::{string::{String, ToString}, sync::{Arc, Weak}, vec::Vec};
use log::info;
use system_error::SystemError;
use unified_init::macros::unified_init;

/// `/sys/fs` 的 kset 实例
static mut FS_KSET_INSTANCE: Option<Arc<KSet>> = None;

/// 获取 `/sys/fs` 的 kset 实例
/// 
/// ## 返回值
/// 
/// 返回 `/sys/fs` 目录对应的 KSet 实例
#[inline(always)]
#[allow(dead_code)]
pub fn sys_fs_kset() -> Arc<KSet> {
    unsafe { 
        FS_KSET_INSTANCE.as_ref()
            .expect("FS kset not initialized")
            .clone()
    }
}

/// 初始化文件系统 sysfs 接口
/// 
/// 该函数会在系统启动时被调用，创建 `/sys/fs` 目录。
/// 各个文件系统可以在此目录下创建自己的子目录。
/// 
/// ## 错误
/// 
/// 如果创建目录失败，返回相应的 SystemError
#[unified_init(INITCALL_SUBSYS)]
fn fs_sysfs_init() -> Result<(), SystemError> {
    info!("Initializing filesystem sysfs interface...");

    // 创建 `/sys/fs` 目录对应的 kset
    let fs_kset = KSet::new("fs".to_string());
    
    // 将 fs kset 注册到 sysfs 根目录下，这会创建 `/sys/fs` 目录
    fs_kset.register(None).map_err(|e| {
        log::error!("Failed to register fs kset: {:?}", e);
        e
    })?;

    // 获取 KSet 对应的 KernFSInode 并设置动态查找提供者
    if let Some(fs_inode) = fs_kset.inode() {
        let dynamic_lookup = Arc::new(SysFsDynamicLookup::new(Arc::downgrade(&fs_inode)));
        fs_inode.set_dynamic_lookup(dynamic_lookup);
        info!("Set up dynamic lookup for /sys/fs directory");
    } else {
        log::warn!("Failed to get KernFSInode for /sys/fs, dynamic lookup not set");
    }

    // 保存全局实例
    unsafe {
        FS_KSET_INSTANCE = Some(fs_kset);
    }

    info!("Filesystem sysfs interface initialized successfully");
    Ok(())
}

/// 为指定的文件系统在 `/sys/fs` 下创建子目录
/// 
/// ## 参数
/// 
/// - `fs_name`: 文件系统名称，将作为子目录名
/// 
/// ## 返回值
/// 
/// 返回新创建的文件系统 kset，文件系统可以在此 kset 下继续创建文件和子目录
/// 
/// ## 示例
/// 
/// ```rust
/// // 为 ext4 文件系统创建 /sys/fs/ext4 目录
/// let ext4_kset = create_fs_kset("ext4".to_string())?;
/// ```
#[allow(dead_code)]
pub fn create_fs_kset(fs_name: String) -> Result<Arc<KSet>, SystemError> {
    let fs_kset = KSet::new(fs_name.clone());
    
    // 将新的文件系统 kset 注册到 `/sys/fs` 下
    fs_kset.register(Some(sys_fs_kset())).map_err(|e| {
        log::error!("Failed to create fs kset for '{}': {:?}", fs_name, e);
        e
    })?;

    info!("Created filesystem kset: /sys/fs/{}", fs_name);
    Ok(fs_kset)
}

/// 检查 `/sys/fs` 目录是否已经初始化
/// 
/// ## 返回值
/// 
/// 如果已初始化返回 true，否则返回 false
#[allow(dead_code)]
pub fn is_fs_sysfs_initialized() -> bool {
    unsafe { FS_KSET_INSTANCE.is_some() }
}

/// `/sys/fs` 目录的动态查找提供者
/// 
/// 这个提供者负责列出在 `/sys/fs` 下通过 `create_temporary_dir` 创建的临时目录，
/// 如 `cgroup` 目录等。
#[derive(Debug)]
struct SysFsDynamicLookup {
    fs_inode: Weak<KernFSInode>,
}

impl SysFsDynamicLookup {
    fn new(fs_inode: Weak<KernFSInode>) -> Self {
        Self { fs_inode }
    }
}

impl DynamicLookup for SysFsDynamicLookup {
    fn dynamic_find(&self, name: &str) -> Result<Option<Arc<dyn IndexNode>>, SystemError> {
        // 若对应的 inode 已被释放，则不再提供条目
        if self.fs_inode.upgrade().is_none() {
            return Ok(None);
        }
        // 对于已挂载的文件系统，返回其挂载点的根 inode
        match name {
            "cgroup" => {
                let mntns = crate::process::ProcessManager::current_mntns();
                if let Some((_mp, rest, fs)) = mntns.get_mount_point("/sys/fs/cgroup") {
                    if rest.is_empty() {
                        // 精确匹配，返回挂载文件系统的根 inode
                        return Ok(Some(fs.root_inode()));
                    }
                }
                Ok(None)
            }
            _ => Ok(None)
        }
    }

    fn dynamic_list(&self) -> Result<Vec<String>, SystemError> {
        // 若对应的 inode 已被释放，则不再提供条目
        if self.fs_inode.upgrade().is_none() {
            return Ok(Vec::new());
        }
        // 基于挂载表判断是否存在 /sys/fs/cgroup 挂载点
        let mut entries = Vec::new();

        let mntns = crate::process::ProcessManager::current_mntns();
        if let Some((_mp, rest, _fs)) = mntns.get_mount_point("/sys/fs/cgroup") {
            if rest.is_empty() {
                entries.push("cgroup".to_string());
            }
        }

        Ok(entries)
    }

    fn is_valid_entry(&self, name: &str) -> bool {
        // 目前支持的临时文件系统目录
        match name {
            "cgroup" => true,
            _ => false,
        }
    }
}