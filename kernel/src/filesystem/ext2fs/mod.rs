use core::hint::spin_loop;

use alloc::{string::ToString, sync::Arc};
use system_error::SystemError;

use crate::{
    driver::disk::ahci,
    filesystem::{
        ext2fs::fs::Ext2FileSystem,
        vfs::{syscall::ModeType, FileType, ROOT_INODE},
    },
    libs::once::Once,
};

pub mod block_group_desc;
pub mod entry;
pub mod file_type;
pub mod fs;
pub mod inode;

/// 全局的sysfs实例
pub(self) static mut EXT2FS_INSTANCE: Option<Arc<Ext2FileSystem>> = None;

#[inline(always)]
pub fn ext2fs_instance() -> Arc<Ext2FileSystem> {
    unsafe {
        return EXT2FS_INSTANCE.as_ref().unwrap().clone();
    }
}
pub fn ext2fs_init() -> Result<(), SystemError> {
    static INIT: Once = Once::new();
    let mut result = None;
    INIT.call_once(|| {
        kinfo!("Initializing Ext2FS...");
        let partiton1 = ahci::get_disks_by_name("ahci_disk_1".to_string());
        if partiton1.is_err() {
            kerror!("Failed to find ahci_disk_1");
            result = Some(Err(SystemError::ENODEV));
            return;
        }
        let p1 = partiton1.unwrap().0.lock().partitions[0].clone();
        let ext2fs: Result<Arc<Ext2FileSystem>, SystemError> = Ext2FileSystem::new(p1);
        if ext2fs.is_err() {
            kerror!(
                "Failed to initialize ext2fs, code={:?}",
                ext2fs.as_ref().err()
            );
            loop {
                spin_loop();
            }
        }
        let ext2fs: Arc<Ext2FileSystem> = ext2fs.unwrap();
        unsafe { EXT2FS_INSTANCE = Some(ext2fs) };

        let root_i = ROOT_INODE();
        let mount_inode = root_i
            .create("ext2", FileType::Dir, ModeType::from_bits_truncate(0o755))
            .expect("Failed to create /ext2");

        if let Err(err) = mount_inode.mount(ext2fs_instance()) {
            result = Some(Err(err));
            return;
        };

        if let Err(err) = root_i.lookup("/ext2") {
            kdebug!("look up ext2 failed: {err:?}");
        };
        kinfo!("Successfully mount EXT2");
        result = Some(Ok(()));
    });

    return result.unwrap();
}
