use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{any::Any, fmt::Debug};
use system_error::SystemError;

use crate::{
    arch::MMArch,
    driver::base::{
        block::{
            block_device::{BlockDevice, BlockId, GeneralBlockRange, LBA_SIZE},
            disk_info::Partition,
            manager::BlockDevMeta,
        },
        class::Class,
        device::{
            bus::Bus,
            device_number::{DeviceNumber, Major},
            driver::Driver,
            DevName, Device, DeviceCommonData, DeviceType, IdTable,
        },
        kobject::{KObjType, KObject, KObjectCommonData, KObjectState, LockedKObjectState},
        kset::KSet,
    },
    filesystem::{
        devfs::{DevFS, DeviceINode, LockedDevFSInode},
        kernfs::KernFSInode,
        vfs::{utils::DName, FilePrivateData, IndexNode, InodeFlags, InodeId, InodeMode, Metadata},
    },
    libs::{
        align::{page_align_down, page_align_up},
        mutex::MutexGuard,
        rwlock::RwLock,
        rwsem::{RwSemReadGuard, RwSemWriteGuard},
        spinlock::{SpinLock, SpinLockGuard},
    },
    mm::{
        mmio_buddy::{mmio_pool, MMIOSpaceGuard},
        MemoryManagementArch, PhysAddr, VirtAddr,
    },
};

const PMEM_BASENAME: &str = "pmem";

#[derive(Debug)]
struct InnerPmemBlockDevice {
    device_common: DeviceCommonData,
    kobject_common: KObjectCommonData,
}

#[cast_to([sync] Device)]
pub struct PmemBlockDevice {
    blkdev_meta: BlockDevMeta,
    inner: SpinLock<InnerPmemBlockDevice>,
    locked_kobj_state: LockedKObjectState,
    self_ref: Weak<Self>,
    parent: RwLock<Weak<LockedDevFSInode>>,
    fs: RwLock<Weak<DevFS>>,
    metadata: Metadata,
    region_start: PhysAddr,
    usable_size: usize,
    mapped_start: Option<VirtAddr>,
    _mmio_guard: Option<MMIOSpaceGuard>,
}

impl Debug for PmemBlockDevice {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PmemBlockDevice")
            .field("devname", &self.blkdev_meta.devname)
            .field("region_start", &self.region_start)
            .field("usable_size", &self.usable_size)
            .field("mapped_start", &self.mapped_start)
            .finish()
    }
}

impl PmemBlockDevice {
    pub fn new(region_start: PhysAddr, region_size: usize, id: usize) -> Arc<Self> {
        let usable_size = region_size / LBA_SIZE * LBA_SIZE;
        let devname = DevName::new(format!("{PMEM_BASENAME}{id}"), id);
        let (mmio_guard, mapped_start) = match Self::try_map_region(region_start, usable_size) {
            Ok((guard, start)) => (Some(guard), Some(start)),
            Err(_) => (None, None),
        };

        Arc::new_cyclic(|self_ref| {
            let blkdev_meta = BlockDevMeta::new(devname, Major::PMEM_BLK_MAJOR);
            let raw_dev = DeviceNumber::new(blkdev_meta.major, blkdev_meta.base_minor);

            Self {
                blkdev_meta,
                inner: SpinLock::new(InnerPmemBlockDevice {
                    device_common: DeviceCommonData::default(),
                    kobject_common: KObjectCommonData::default(),
                }),
                locked_kobj_state: LockedKObjectState::default(),
                self_ref: self_ref.clone(),
                parent: RwLock::new(Weak::default()),
                fs: RwLock::new(Weak::default()),
                metadata: Metadata {
                    dev_id: 0,
                    inode_id: InodeId::new(0),
                    size: (usable_size.min(i64::MAX as usize)) as i64,
                    blk_size: LBA_SIZE,
                    blocks: usable_size / LBA_SIZE,
                    atime: Default::default(),
                    mtime: Default::default(),
                    ctime: Default::default(),
                    btime: Default::default(),
                    file_type: crate::filesystem::vfs::FileType::BlockDevice,
                    mode: InodeMode::from_bits_truncate(0o644),
                    flags: InodeFlags::empty(),
                    nlinks: 1,
                    uid: 0,
                    gid: 0,
                    raw_dev,
                },
                region_start,
                usable_size,
                mapped_start,
                _mmio_guard: mmio_guard,
            }
        })
    }

    fn try_map_region(
        region_start: PhysAddr,
        usable_size: usize,
    ) -> Result<(MMIOSpaceGuard, VirtAddr), SystemError> {
        if usable_size == 0 {
            return Err(SystemError::EINVAL);
        }

        let paddr_base = page_align_down(region_start.data());
        let offset = region_start.data() - paddr_base;
        let map_size = page_align_up(
            usable_size
                .checked_add(offset)
                .ok_or(SystemError::EOVERFLOW)?,
        );

        let mmio_guard = mmio_pool().create_mmio(map_size)?;
        let mapped_start = unsafe { mmio_guard.map_any_phys(region_start, usable_size)? };
        Ok((mmio_guard, mapped_start))
    }

    pub fn region_start(&self) -> PhysAddr {
        self.region_start
    }

    pub fn usable_size(&self) -> usize {
        self.usable_size
    }

    fn inner(&self) -> SpinLockGuard<'_, InnerPmemBlockDevice> {
        self.inner.lock_irqsave()
    }
}

impl IndexNode for PmemBlockDevice {
    fn fs(&self) -> Arc<dyn crate::filesystem::vfs::FileSystem> {
        self.fs
            .read()
            .upgrade()
            .expect("PmemBlockDevice fs is not set")
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn read_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &[u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        Err(SystemError::EROFS)
    }

    fn list(&self) -> Result<Vec<String>, SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        Ok(self.metadata.clone())
    }

    fn parent(&self) -> Result<Arc<dyn IndexNode>, SystemError> {
        let parent = self.parent.read();
        if let Some(parent) = parent.upgrade() {
            return Ok(parent as Arc<dyn IndexNode>);
        }
        Err(SystemError::ENOENT)
    }

    fn close(&self, _data: MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        Ok(())
    }

    fn dname(&self) -> Result<DName, SystemError> {
        Ok(DName::from(self.blkdev_meta.devname.clone().as_ref()))
    }

    fn open(
        &self,
        _data: MutexGuard<FilePrivateData>,
        _mode: &crate::filesystem::vfs::file::FileFlags,
    ) -> Result<(), SystemError> {
        Ok(())
    }
}

impl DeviceINode for PmemBlockDevice {
    fn set_fs(&self, fs: Weak<DevFS>) {
        *self.fs.write() = fs;
    }

    fn set_parent(&self, parent: Weak<LockedDevFSInode>) {
        *self.parent.write() = parent;
    }
}

impl BlockDevice for PmemBlockDevice {
    fn dev_name(&self) -> &DevName {
        &self.blkdev_meta.devname
    }

    fn blkdev_meta(&self) -> &BlockDevMeta {
        &self.blkdev_meta
    }

    fn disk_range(&self) -> GeneralBlockRange {
        let blocks = self.usable_size / LBA_SIZE;
        GeneralBlockRange::new(0, blocks).unwrap_or(GeneralBlockRange {
            lba_start: 0,
            lba_end: 0,
        })
    }

    fn read_at_sync(
        &self,
        lba_id_start: BlockId,
        count: usize,
        buf: &mut [u8],
    ) -> Result<usize, SystemError> {
        if count == 0 {
            return Ok(0);
        }

        let offset = lba_id_start
            .checked_mul(LBA_SIZE)
            .ok_or(SystemError::EOVERFLOW)?;
        let len = count.checked_mul(LBA_SIZE).ok_or(SystemError::EOVERFLOW)?;

        if len > buf.len() {
            return Err(SystemError::EINVAL);
        }

        let end = offset.checked_add(len).ok_or(SystemError::EOVERFLOW)?;
        if end > self.usable_size {
            return Err(SystemError::ENOSPC);
        }

        let src = if let Some(mapped_start) = self.mapped_start {
            let src_vaddr = VirtAddr::new(
                mapped_start
                    .data()
                    .checked_add(offset)
                    .ok_or(SystemError::EOVERFLOW)?,
            );
            src_vaddr
        } else {
            let paddr = self.region_start.add(offset);
            unsafe { MMArch::phys_2_virt(paddr) }.ok_or(SystemError::EFAULT)?
        };
        unsafe {
            core::ptr::copy_nonoverlapping(src.data() as *const u8, buf.as_mut_ptr(), len);
        }

        Ok(len)
    }

    fn write_at_sync(
        &self,
        _lba_id_start: BlockId,
        _count: usize,
        _buf: &[u8],
    ) -> Result<usize, SystemError> {
        Err(SystemError::EROFS)
    }

    fn sync(&self) -> Result<(), SystemError> {
        Ok(())
    }

    fn blk_size_log2(&self) -> u8 {
        9
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn device(&self) -> Arc<dyn Device> {
        self.self_ref.upgrade().unwrap()
    }

    fn block_size(&self) -> usize {
        LBA_SIZE
    }

    fn partitions(&self) -> Vec<Arc<Partition>> {
        Vec::new()
    }
}

impl Device for PmemBlockDevice {
    fn dev_type(&self) -> DeviceType {
        DeviceType::Block
    }

    fn id_table(&self) -> IdTable {
        IdTable::new(PMEM_BASENAME.to_string(), None)
    }

    fn bus(&self) -> Option<Weak<dyn Bus>> {
        self.inner().device_common.bus.clone()
    }

    fn set_bus(&self, bus: Option<Weak<dyn Bus>>) {
        self.inner().device_common.bus = bus;
    }

    fn class(&self) -> Option<Arc<dyn Class>> {
        let mut guard = self.inner();
        let class = guard.device_common.class.clone()?.upgrade();
        if class.is_none() {
            guard.device_common.class = None;
        }
        class
    }

    fn set_class(&self, class: Option<Weak<dyn Class>>) {
        self.inner().device_common.class = class;
    }

    fn driver(&self) -> Option<Arc<dyn Driver>> {
        let mut guard = self.inner();
        let driver = guard.device_common.driver.clone()?.upgrade();
        if driver.is_none() {
            guard.device_common.driver = None;
        }
        driver
    }

    fn set_driver(&self, driver: Option<Weak<dyn Driver>>) {
        self.inner().device_common.driver = driver;
    }

    fn is_dead(&self) -> bool {
        false
    }

    fn can_match(&self) -> bool {
        self.inner().device_common.can_match
    }

    fn set_can_match(&self, can_match: bool) {
        self.inner().device_common.can_match = can_match;
    }

    fn state_synced(&self) -> bool {
        true
    }

    fn dev_parent(&self) -> Option<Weak<dyn Device>> {
        self.inner().device_common.get_parent_weak_or_clear()
    }

    fn set_dev_parent(&self, parent: Option<Weak<dyn Device>>) {
        self.inner().device_common.parent = parent;
    }
}

impl KObject for PmemBlockDevice {
    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn set_inode(&self, inode: Option<Arc<KernFSInode>>) {
        self.inner().kobject_common.kern_inode = inode;
    }

    fn inode(&self) -> Option<Arc<KernFSInode>> {
        self.inner().kobject_common.kern_inode.clone()
    }

    fn parent(&self) -> Option<Weak<dyn KObject>> {
        self.inner().kobject_common.parent.clone()
    }

    fn set_parent(&self, parent: Option<Weak<dyn KObject>>) {
        self.inner().kobject_common.parent = parent;
    }

    fn kset(&self) -> Option<Arc<KSet>> {
        self.inner().kobject_common.kset.clone()
    }

    fn set_kset(&self, kset: Option<Arc<KSet>>) {
        self.inner().kobject_common.kset = kset;
    }

    fn kobj_type(&self) -> Option<&'static dyn KObjType> {
        self.inner().kobject_common.kobj_type
    }

    fn name(&self) -> String {
        self.dev_name().to_string()
    }

    fn set_name(&self, _name: String) {}

    fn kobj_state(&self) -> RwSemReadGuard<'_, KObjectState> {
        self.locked_kobj_state.read()
    }

    fn kobj_state_mut(&self) -> RwSemWriteGuard<'_, KObjectState> {
        self.locked_kobj_state.write()
    }

    fn set_kobj_state(&self, state: KObjectState) {
        *self.locked_kobj_state.write() = state;
    }

    fn set_kobj_type(&self, ktype: Option<&'static dyn KObjType>) {
        self.inner().kobject_common.kobj_type = ktype;
    }
}
