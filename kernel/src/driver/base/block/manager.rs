use core::{fmt::Formatter, sync::atomic::AtomicU32};

use alloc::sync::Arc;
use hashbrown::HashMap;
use system_error::SystemError;
use unified_init::macros::unified_init;

use crate::{
    driver::base::{
        block::gendisk::GenDisk,
        device::{device_number::Major, DevName},
    },
    filesystem::{
        devfs::devfs_register,
        mbr::MbrDiskPartionTable,
        vfs::{utils::DName, IndexNode},
    },
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
    /// 记录每个major对应的下一个可用的minor号
    minors: HashMap<Major, AtomicU32>,
}
impl BlockDevManager {
    pub fn new() -> Self {
        BlockDevManager {
            inner: SpinLock::new(InnerBlockDevManager {
                disks: HashMap::new(),
                minors: HashMap::new(),
            }),
        }
    }

    fn inner(&self) -> SpinLockGuard<'_, InnerBlockDevManager> {
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

        // 检测分区表，并创建gendisk
        let res = self.check_partitions(&dev);
        if res.is_err() {
            inner.disks.remove(dev_name);
        };
        res?;
        Ok(())
    }

    /// 检测分区表，并创建gendisk
    fn check_partitions(&self, dev: &Arc<dyn BlockDevice>) -> Result<(), SystemError> {
        if self.try_register_disk_by_mbr(dev).is_ok() {
            return Ok(());
        }

        // use entire disk as a gendisk
        self.register_entire_disk_as_gendisk(dev)
    }

    fn try_register_disk_by_mbr(&self, dev: &Arc<dyn BlockDevice>) -> Result<(), SystemError> {
        let mbr = MbrDiskPartionTable::from_disk(dev.clone())?;
        let piter = mbr.partitions_raw();
        let mut idx;
        for p in piter {
            idx = dev.blkdev_meta().inner().gendisks.alloc_idx();
            self.register_gendisk_with_range(dev, p.try_into()?, idx)?;
        }
        Ok(())
    }

    /// 将整个磁盘注册为gendisk
    fn register_entire_disk_as_gendisk(
        &self,
        dev: &Arc<dyn BlockDevice>,
    ) -> Result<(), SystemError> {
        let range = dev.disk_range();
        self.register_gendisk_with_range(dev, range, GenDisk::ENTIRE_DISK_IDX)
    }

    fn register_gendisk_with_range(
        &self,
        dev: &Arc<dyn BlockDevice>,
        range: GeneralBlockRange,
        idx: u32,
    ) -> Result<(), SystemError> {
        let weak_dev = Arc::downgrade(dev);

        // 这里先拿到硬盘的设备名，然后在根据idx来生成gendisk的名字
        // 如果是整个磁盘，则idx为 None，名字为/dev/sda
        // 如果是分区，例如idx为1，则名字为/dev/sda1
        // 以此类推
        let dev_name = dev.dev_name();
        let (idx, dev_name) = match idx {
            GenDisk::ENTIRE_DISK_IDX => (None, DName::from(dev_name.name())),
            id => (Some(id), DName::from(format!("{}{}", dev_name.name(), idx))),
        };

        let gendisk = GenDisk::new(weak_dev, range, idx, dev_name);
        // log::info!("Registering gendisk");
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

        // 注册到devfs
        let dname = gendisk.dname()?;
        devfs_register(dname.as_ref(), gendisk.clone()).map_err(|e| {
            log::error!(
                "Failed to register gendisk {:?} to devfs: {:?}",
                dname.as_ref(),
                e
            );
            e
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

    /// 获取对应major下一个可用的minor号
    pub(self) fn next_minor(&self, major: Major) -> u32 {
        let mut inner = self.inner();
        let base = inner
            .minors
            .entry(major)
            .or_insert_with(|| AtomicU32::new(0));
        let base_minor = base.load(core::sync::atomic::Ordering::SeqCst);
        base.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
        base_minor
    }
}

pub struct BlockDevMeta {
    pub devname: DevName,
    pub major: Major,
    pub base_minor: u32,
    inner: SpinLock<InnerBlockDevMeta>,
}

pub struct InnerBlockDevMeta {
    pub gendisks: GenDiskMap,
    pub dev_idx: usize,
}

impl BlockDevMeta {
    pub fn new(devname: DevName, major: Major) -> Self {
        BlockDevMeta {
            devname,
            major,
            base_minor: block_dev_manager().next_minor(major),
            inner: SpinLock::new(InnerBlockDevMeta {
                gendisks: GenDiskMap::new(),
                dev_idx: 0, // 默认索引为0
            }),
        }
    }

    pub(crate) fn inner(&self) -> SpinLockGuard<'_, InnerBlockDevMeta> {
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
