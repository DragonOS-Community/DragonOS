use alloc::sync::Arc;
use system_error::SystemError;

use crate::{filesystem::{
    ext2fs::fs::Ext2FileSystem,
    vfs::ROOT_INODE,
}, libs::once::Once};

pub mod block_group_desc;
pub mod entry;
pub mod file_type;
pub mod fs;
pub mod inode;

// /// 全局的sysfs实例
// pub(self) static mut EXT2FS_INSTANCE: Option<Ext2FileSystem> = None;

// #[inline(always)]
// pub fn sysfs_instance() -> &'static Ext2FileSystem {
//     unsafe {
//         return &EXT2FS_INSTANCE.as_ref().unwrap();
//     }
// }
// pub fn procfs_init() -> Result<(), SystemError> {
//     static INIT: Once = Once::new();
//     let mut result = None;
//     INIT.call_once(|| {
//         kinfo!("Initializing Ext2FS...");
//         // 创建 Ext2FileSystem 实例
//         let procfs: Arc<Ext2FileSystem> = Ext2FileSystem::new(partition);

//         // procfs 挂载
//         let _t = ROOT_INODE()
//             .find("ext2")
//             .expect("Cannot find /ext2")
//             .mount(procfs)
//             .expect("Failed to mount ext2");
//         kinfo!("Ext2FS mounted.");
//         result = Some(Ok(()));
//     });

//     return result.unwrap();
// }
