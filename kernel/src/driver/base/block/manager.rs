use core::fmt::Formatter;

use alloc::sync::Arc;
use hashbrown::HashMap;
use system_error::SystemError;
use unified_init::macros::unified_init;

use crate::{
    driver::base::{block::gendisk::GenDisk, device::DevName},
    filesystem::mbr::MbrDiskPartionTable,
    init::initcall::INITCALL_POSTCORE,
    libs::spinlock::{SpinLock, SpinLockGuard},
};

use super::{
    block_device::{BlockDevice, GeneralBlockRange},
    gendisk::GenDiskMap,
};

static mut BLOCK_DEV_MANAGER: Option<BlockDevManager> = None;

#[inline]
pub fn block_dev_manager() -> &'static BlockDevManager {
    unsafe { BLOCK_DEV_MANAGER.as_ref().unwrap() }
}

#[unified_init(INITCALL_POSTCORE)]
pub fn block_dev_manager_init() -> Result<(), SystemError> {
    unsafe {
        BLOCK_DEV_MANAGER = Some(BlockDevManager::new());
    }
    Ok(())
}

/// 磁盘设备管理器
pub struct BlockDevManager {
    inner: SpinLock<InnerBlockDevManager>,
}

struct InnerBlockDevManager {
    disks: HashMap<DevName, Arc<dyn BlockDevice>>,
}
impl BlockDevManager {
    pub fn new() -> Self {
        BlockDevManager {
            inner: SpinLock::new(InnerBlockDevManager {
                disks: HashMap::new(),
            }),
        }
    }

    fn inner(&self) -> SpinLockGuard<InnerBlockDevManager> {
        self.inner.lock()
    }

    /// 注册磁盘设备
    pub fn register(&self, dev: Arc<dyn BlockDevice>) -> Result<(), SystemError> {
        let mut inner = self.inner();
        let dev_name = dev.dev_name();
        if inner.disks.contains_key(dev_name) {
            return Err(SystemError::EEXIST);
        }
        inner.disks.insert(dev_name.clone(), dev.clone());

        let mut out_remove = || {
            inner.disks.remove(dev_name);
        };

        // 检测分区表，并创建gendisk
        self.check_partitions(&dev).inspect_err(|_| out_remove())?;
        Ok(())
    }

    /// 检测分区表，并创建gendisk
    fn check_partitions(&self, dev: &Arc<dyn BlockDevice>) -> Result<(), SystemError> {
        if self.check_mbr(dev).is_ok() {
            return Ok(());
        }

        // use entire disk as a gendisk
        self.register_entire_disk_as_gendisk(dev)
    }

    fn check_mbr(&self, dev: &Arc<dyn BlockDevice>) -> Result<(), SystemError> {
        let mbr = MbrDiskPartionTable::from_disk(dev.clone())?;
        let piter = mbr.partitions_raw();
        for p in piter {
            self.register_gendisk_with_range(dev, p.try_into()?)?;
        }
        Ok(())
    }

    /// 将整个磁盘注册为gendisk
    fn register_entire_disk_as_gendisk(
        &self,
        dev: &Arc<dyn BlockDevice>,
    ) -> Result<(), SystemError> {
        let range = dev.disk_range();
        self.register_gendisk_with_range(dev, range)
    }

    fn register_gendisk_with_range(
        &self,
        dev: &Arc<dyn BlockDevice>,
        range: GeneralBlockRange,
    ) -> Result<(), SystemError> {
        let weak_dev = Arc::downgrade(dev);
        let gendisk = GenDisk::new(
            weak_dev,
            range,
            Some(dev.blkdev_meta().inner().gendisks.alloc_idx()),
        );
        self.register_gendisk(dev, gendisk)
    }

    fn register_gendisk(
        &self,
        dev: &Arc<dyn BlockDevice>,
        gendisk: Arc<GenDisk>,
    ) -> Result<(), SystemError> {
        let blk_meta = dev.blkdev_meta();
        let idx = gendisk.idx();
        let mut meta_inner = blk_meta.inner();
        // 检查是否重复
        if meta_inner.gendisks.intersects(gendisk.range()) {
            return Err(SystemError::EEXIST);
        }

        meta_inner.gendisks.insert(idx, gendisk.clone());
        dev.callback_gendisk_registered(&gendisk).inspect_err(|_| {
            meta_inner.gendisks.remove(&idx);
        })?;
        Ok(())
    }

    /// 卸载磁盘设备
    #[allow(dead_code)]
    pub fn unregister(&self, dev: &Arc<dyn BlockDevice>) {
        let mut inner = self.inner();
        inner.disks.remove(dev.dev_name());
        // todo: 这里应该callback一下磁盘设备，但是现在还没实现热插拔，所以暂时没做这里
        todo!("BlockDevManager: unregister disk")
    }

    /// 通过路径查找gendisk
    ///
    /// # 参数
    ///
    /// - `path`: 分区路径 `/dev/sda1` 或者 `sda1`，或者是`/dev/sda`
    pub fn lookup_gendisk_by_path(&self, path: &str) -> Option<Arc<GenDisk>> {
        let (devname, partno) = self.path2devname(path)?;
        let inner = self.inner();
        for dev in inner.disks.values() {
            if dev.dev_name().as_str() == devname {
                return dev.blkdev_meta().inner().gendisks.get(&partno).cloned();
            }
        }
        None
    }

    /// 打印所有的gendisk的路径
    pub fn print_gendisks(&self) {
        let mut disks = alloc::vec::Vec::new();

        let inner = self.inner();
        for dev in inner.disks.values() {
            let meta = dev.blkdev_meta().inner();
            for idx in meta.gendisks.keys() {
                if idx == &GenDisk::ENTIRE_DISK_IDX {
                    disks.push(format!("/dev/{}", dev.dev_name()));
                } else {
                    disks.push(format!("/dev/{}{}", dev.dev_name(), idx));
                }
            }
        }

        log::debug!("All gendisks: {:?}", disks);
    }

    /// 将路径转换为设备名以及分区号
    ///
    /// 例如: sda1 -> (sda, 1)  nvme0n1p1 -> (nvme0n1, 1)
    fn path2devname<'a>(&self, mut path: &'a str) -> Option<(&'a str, u32)> {
        // 去除开头的"/dev/"
        if path.starts_with("/dev/") {
            path = path.strip_prefix("/dev/")?;
        }

        let mut partno = GenDisk::ENTIRE_DISK_IDX;
        // 截取末尾数字
        let mut last_digit = path.len();
        while last_digit > 0 && path.chars().nth(last_digit - 1).unwrap().is_ascii_digit() {
            last_digit -= 1;
        }
        if last_digit == 0 {
            return (path, GenDisk::ENTIRE_DISK_IDX).into();
        }

        if last_digit < path.len() {
            partno = path[last_digit..].parse().ok()?;
        }

        let path = &path[..last_digit];

        Some((path, partno))
    }
}

pub struct BlockDevMeta {
    pub devname: DevName,
    inner: SpinLock<InnerBlockDevMeta>,
}

pub struct InnerBlockDevMeta {
    pub gendisks: GenDiskMap,
}

impl BlockDevMeta {
    pub fn new(devname: DevName) -> Self {
        BlockDevMeta {
            devname,
            inner: SpinLock::new(InnerBlockDevMeta {
                gendisks: GenDiskMap::new(),
            }),
        }
    }

    fn inner(&self) -> SpinLockGuard<InnerBlockDevMeta> {
        self.inner.lock()
    }
}

impl core::fmt::Debug for BlockDevMeta {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BlockDevMeta")
            .field("devname", &self.devname)
            .finish()
    }
}
