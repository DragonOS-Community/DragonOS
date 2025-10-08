pub mod devfs;
pub mod devpts;
pub mod epoll;
pub mod eventfd;
pub mod ext4;
pub mod fat;
pub mod fs;
pub mod kernfs;
pub mod mbr;
pub mod overlayfs;
pub mod page_cache;
pub mod poll;
pub mod procfs;
pub mod ramfs;
pub mod sysfs;
pub mod sys_fs;
pub mod vfs;

// 导入 shmem 模块（tmpfs 实现）
// 这个导入确保 tmpfs 文件系统被注册到 FSMAKER 中
#[allow(unused_imports)]
pub use crate::mm::shmem;

// 导出文件系统 sysfs 接口
#[allow(unused_imports)]
pub use sys_fs::{create_fs_kset, is_fs_sysfs_initialized, sys_fs_kset};
