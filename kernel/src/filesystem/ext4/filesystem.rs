use crate::{
    driver::base::{block::gendisk::GenDisk, device::device_number::DeviceNumber},
    filesystem::{
        ext4::inode::Ext4Inode,
        vfs::{
            self,
            fcntl::AtFlags,
            utils::{user_path_at, DName},
            vcore::{generate_inode_id, try_find_gendisk},
            FileSystem, FileSystemMakerData, IndexNode, Magic, MountableFileSystem, FSMAKER,
            VFS_MAX_FOLLOW_SYMLINK_TIMES,
        },
    },
    libs::mutex::Mutex,
    mm::{
        fault::{PageFaultHandler, PageFaultMessage},
        VmFaultReason,
    },
    process::ProcessManager,
    register_mountable_fs,
};
use alloc::{
    collections::BTreeMap,
    sync::{Arc, Weak},
    vec::Vec,
};
use kdepends::another_ext4;
use linkme::distributed_slice;
use system_error::SystemError;

use super::inode::LockedExt4Inode;

pub struct Ext4FileSystem {
    /// 对应 another_ext4 中的实际文件系统
    pub(super) fs: another_ext4::Ext4,
    /// 当前文件系统对应的设备号
    pub(super) raw_dev: DeviceNumber,

    /// 根 inode
    root_inode: Arc<LockedExt4Inode>,

    /// 元数据（size/mtime）脏但尚未刷盘的 inode 列表。
    dirty_inodes: Mutex<Vec<Weak<LockedExt4Inode>>>,
}

impl FileSystem for Ext4FileSystem {
    fn root_inode(&self) -> Arc<dyn IndexNode> {
        self.root_inode.clone()
    }

    fn info(&self) -> vfs::FsInfo {
        todo!()
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn name(&self) -> &str {
        "ext4"
    }

    fn super_block(&self) -> vfs::SuperBlock {
        vfs::SuperBlock::new(Magic::EXT4_MAGIC, another_ext4::BLOCK_SIZE as u64, 255)
    }

    fn statfs(&self, _inode: &Arc<dyn IndexNode>) -> Result<vfs::SuperBlock, SystemError> {
        self.read_statfs_from_superblock()
    }

    unsafe fn fault(&self, pfm: &mut PageFaultMessage) -> VmFaultReason {
        PageFaultHandler::filemap_fault(pfm)
    }

    unsafe fn map_pages(
        &self,
        pfm: &mut PageFaultMessage,
        start_pgoff: usize,
        end_pgoff: usize,
    ) -> VmFaultReason {
        PageFaultHandler::filemap_map_pages(pfm, start_pgoff, end_pgoff)
    }

    fn sync_fs(&self, _wait: bool) -> Result<(), SystemError> {
        self.flush_dirty_inodes()
    }
}

impl Ext4FileSystem {
    pub(super) fn mark_inode_dirty(inode: &Arc<LockedExt4Inode>) {
        {
            let guard = inode.0.lock();
            if guard.on_dirty_list {
                return;
            }
        }

        if let Some(fs) = inode.filesystem() {
            let mut guard = fs.dirty_inodes.lock();
            {
                let mut inode_guard = inode.0.lock();
                if inode_guard.on_dirty_list {
                    return;
                }
                inode_guard.on_dirty_list = true;
            }
            guard.push(Arc::downgrade(inode));
        }
    }

    fn flush_dirty_inodes(&self) -> Result<(), SystemError> {
        let dirty: Vec<Weak<LockedExt4Inode>> = {
            let mut guard = self.dirty_inodes.lock();
            if guard.is_empty() {
                return Ok(());
            }
            core::mem::take(&mut *guard)
        };

        let mut last_err = Ok(());
        let mut failed: Vec<Weak<LockedExt4Inode>> = Vec::new();
        for weak in dirty {
            if let Some(inode) = weak.upgrade() {
                if let Err(e) = inode.flush_metadata(false) {
                    log::warn!("flush_dirty_inodes: 元数据刷盘失败: {:?}", e);
                    last_err = Err(e);
                    // on_dirty_list 保持 true，防止 mark_inode_dirty 重复添加
                    failed.push(Arc::downgrade(&inode));
                } else {
                    // 成功才标记为不在脏列表
                    inode.0.lock().on_dirty_list = false;
                }
            }
        }
        if !failed.is_empty() {
            let mut guard = self.dirty_inodes.lock();
            guard.extend(failed);
        }
        last_err
    }

    fn read_statfs_from_superblock(&self) -> Result<vfs::SuperBlock, SystemError> {
        let ext4_sb = self.fs.super_block()?;
        let block_size = ext4_sb.block_size();
        let blocks = ext4_sb.block_count();
        let overhead_blocks = ext4_sb.clusters_to_blocks(ext4_sb.overhead_clusters() as u64);
        let bfree = ext4_sb.free_blocks_count();
        let reserved = ext4_sb.reserved_blocks_count();

        let mut sb = vfs::SuperBlock::new(Magic::EXT4_MAGIC, block_size, 255);
        // Linux ext4 语义：f_blocks 不包含元数据开销。
        sb.blocks = blocks.saturating_sub(overhead_blocks);
        sb.bfree = bfree;
        sb.bavail = bfree.saturating_sub(reserved);
        sb.files = ext4_sb.inode_count() as u64;
        sb.ffree = ext4_sb.free_inodes_count() as u64;
        sb.frsize = block_size;
        Ok(sb)
    }

    /// 探测 gendisk 是否包含 ext4 文件系统
    pub fn probe(gendisk: &Arc<GenDisk>) -> Result<bool, SystemError> {
        Ok(another_ext4::Ext4::load(gendisk.clone())
            .map(|_| true)
            .unwrap_or(false))
    }

    pub fn from_gendisk(mount_data: Arc<GenDisk>) -> Result<Arc<dyn FileSystem>, SystemError> {
        let raw_dev = mount_data.device_num();
        let fs = another_ext4::Ext4::load(mount_data.clone())?;
        let root_inode: Arc<LockedExt4Inode> =
            Arc::new_cyclic(|self_ref: &Weak<LockedExt4Inode>| {
                LockedExt4Inode(
                    Mutex::new(Ext4Inode {
                        inner_inode_num: another_ext4::EXT4_ROOT_INO,
                        fs_ptr: Weak::default(),
                        page_cache: None,
                        children: BTreeMap::new(),
                        dname: DName::from("/"),
                        vfs_inode_id: generate_inode_id(),
                        parent: self_ref.clone(),
                        self_ref: self_ref.clone(),
                        special_node: None,
                        cached_file_size: None,
                        cached_mtime: None,
                        size_dirty: false,
                        mtime_dirty: false,
                        on_dirty_list: false,
                    }),
                    Mutex::new(()),
                )
            });

        let fs = Arc::new(Ext4FileSystem {
            fs,
            raw_dev,
            root_inode,
            dirty_inodes: Mutex::new(Vec::new()),
        });

        let mut guard = fs.root_inode.0.lock();
        guard.fs_ptr = Arc::downgrade(&fs);
        drop(guard);

        Ok(fs)
    }
}

impl MountableFileSystem for Ext4FileSystem {
    fn make_fs(
        data: Option<&dyn FileSystemMakerData>,
    ) -> Result<Arc<dyn FileSystem + 'static>, SystemError> {
        let mount_data = data
            .and_then(|d| d.as_any().downcast_ref::<Ext4MountData>())
            .ok_or(SystemError::EINVAL)?;

        Self::from_gendisk(mount_data.gendisk.clone())
    }
    fn make_mount_data(
        _raw_data: Option<&str>,
        source: &str,
    ) -> Result<Option<Arc<dyn FileSystemMakerData + 'static>>, SystemError> {
        let mount_data = Ext4MountData::from_source(source).map_err(|e| {
            log::error!(
                "Failed to create Ext4 mount data from source '{}': {:?}",
                source,
                e
            );
            e
        })?;
        Ok(Some(Arc::new(mount_data)))
    }
}

register_mountable_fs!(Ext4FileSystem, EXT4FSMAKER, "ext4");

pub struct Ext4MountData {
    gendisk: Arc<GenDisk>,
}

impl FileSystemMakerData for Ext4MountData {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

impl Ext4MountData {
    fn from_source(path: &str) -> Result<Self, SystemError> {
        let pcb = ProcessManager::current_pcb();
        let (current_node, rest_path) = user_path_at(&pcb, AtFlags::AT_FDCWD.bits(), path)?;
        let inode = current_node.lookup_follow_symlink(&rest_path, VFS_MAX_FOLLOW_SYMLINK_TIMES)?;
        if !inode.metadata()?.file_type.eq(&vfs::FileType::BlockDevice) {
            return Err(SystemError::ENOTBLK);
        }

        let disk = inode.dname()?;

        if let Some(gendisk) = try_find_gendisk(disk.0.as_str()) {
            return Ok(Self { gendisk });
        }
        Err(SystemError::ENOENT)
    }
}

impl core::fmt::Debug for Ext4FileSystem {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "ext4")
    }
}
