use crate::filesystem::devfs::LockedDevFSInode;
use crate::{
    driver::base::{
        block::{
            block_device::{BlockDevice, BlockId, GeneralBlockRange, LBA_SIZE},
            disk_info::Partition,
            manager::{block_dev_manager, BlockDevMeta},
        },
        class::Class,
        device::{
            bus::{bus_manager, Bus},
            device_number::{DeviceNumber, Major},
            driver::{Driver, DriverCommonData},
            DevName, Device, DeviceCommonData, DeviceId, DeviceType, IdTable,
        },
        kobject::{KObjType, KObject, KObjectCommonData, KObjectState, LockedKObjectState},
        kset::KSet,
        subsys::SubSysPrivate,
    },
    filesystem::{
        devfs::{self, devfs_register, DevFS, DeviceINode},
        kernfs::KernFSInode,
        vfs::{IndexNode, InodeId, Metadata},
    },
    init::initcall::INITCALL_POSTCORE,
    libs::{
        rwlock::{RwLock, RwLockReadGuard, RwLockWriteGuard},
        spinlock::{SpinLock, SpinLockGuard},
    },
};
use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use bitmap::traits::BitMapOps;
use core::{
    any::Any,
    fmt::{Debug, Formatter},
};
use log::error;
use system_error::SystemError;
use unified_init::macros::unified_init;
const LOOP_BASENAME: &str = "loop";
//LoopDevice是一个虚拟的块设备，它将文件映射到块设备上.
pub struct LoopDevice {
    inner: SpinLock<LoopDeviceInner>, //加锁保护LoopDeviceInner
    //有主设备次设备号
    block_dev_meta: BlockDevMeta,
    //dev_id: Arc<DeviceId>,
    locked_kobj_state: LockedKObjectState, //对Kobject状态的锁
    self_ref: Weak<Self>,                  //对自身的弱引用
    fs: RwLock<Weak<DevFS>>,               //文件系统弱引用
    parent: RwLock<Weak<LockedDevFSInode>>,
}
//Inner内数据会改变所以加锁
pub struct LoopDeviceInner {
    // 关联的文件节点
    pub file_inode: Option<Arc<dyn IndexNode>>,
    // 文件大小
    pub file_size: usize,
    // 设备名称 Major和Minor
    pub device_number: DeviceNumber,
    // 数据偏移量
    pub offset: usize,
    // 数据大小限制
    pub size_limit: usize,
    // 是否允许用户直接 I/O 操作
    pub user_direct_io: bool,
    // 是否只读
    pub read_only: bool,
    // 是否可见
    pub visible: bool,
    // 使用弱引用避免循环引用
    pub self_ref: Weak<LoopDevice>,
    // KObject的公共数据
    pub kobject_common: KObjectCommonData,
    // 设备的公共数据
    pub device_common: DeviceCommonData,
}
impl Debug for LoopDevice {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LoopDevice")
            .field("devname", &self.block_dev_meta.devname)
            .finish()
    }
}
impl LoopDevice {
    fn inner(&self) -> SpinLockGuard<LoopDeviceInner> {
        // 获取 LoopDeviceInner 的自旋锁
        self.inner.lock()
    }
    //注册一个新的空loop设备占位
    pub fn new_empty_loop_device(devname: DevName, minor: u32) -> Option<Arc<Self>> {
        // 创建一个空的 LoopDevice
        let dev = Arc::new_cyclic(|self_ref| Self {
            inner: SpinLock::new(LoopDeviceInner {
                file_inode: None, // 默认的虚拟 inode
                file_size: 0,
                device_number: DeviceNumber::new(Major::LOOP_MAJOR, minor), // Loop 设备主设备号为 7
                offset: 0,
                size_limit: 0,
                user_direct_io: false,
                read_only: false,
                visible: true,
                self_ref: self_ref.clone(),
                kobject_common: KObjectCommonData::default(),
                device_common: DeviceCommonData::default(),
            }),
            //只用重复8次，就会有从0-7八个次设备号
            block_dev_meta: BlockDevMeta::new(devname, Major::new(7)), // Loop 设备主设备号为 7
            locked_kobj_state: LockedKObjectState::default(),
            self_ref: self_ref.clone(),
            fs: RwLock::new(Weak::default()),
            parent: RwLock::new(Weak::default()),
        });

        Some(dev)
    }

    /// 设置 loop 设备关联的文件
    pub fn set_file(&self, file_inode: Arc<dyn IndexNode>) -> Result<(), SystemError> {
        let mut inner = self.inner();
        // 获取文件大小
        let file_size = file_inode.metadata()?.size;

        inner.file_inode = Some(file_inode);
        inner.file_size = file_size as usize;

        Ok(())
    }

    /// 获取文件大小
    pub fn file_size(&self) -> usize {
        self.inner().file_size
    }

    /// 设置只读模式
    pub fn set_read_only(&self, read_only: bool) {
        self.inner().read_only = read_only;
    }

    /// 检查是否为只读
    pub fn is_read_only(&self) -> bool {
        self.inner().read_only
    }
}

impl KObject for LoopDevice {
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

    fn set_kobj_type(&self, ktype: Option<&'static dyn KObjType>) {
        self.inner().kobject_common.kobj_type = ktype;
    }

    fn name(&self) -> String {
        LOOP_BASENAME.to_string()
    }

    fn set_name(&self, _name: String) {
        // do nothing,不支持设置loop为别的名称
    }

    fn kobj_state(&self) -> RwLockReadGuard<KObjectState> {
        self.locked_kobj_state.read()
    }

    fn kobj_state_mut(&self) -> RwLockWriteGuard<KObjectState> {
        self.locked_kobj_state.write()
    }

    fn set_kobj_state(&self, state: KObjectState) {
        *self.locked_kobj_state.write() = state;
    }
}

//对loopdevice进行抽象
impl IndexNode for LoopDevice {
    fn fs(&self) -> Arc<dyn crate::filesystem::vfs::FileSystem> {
        todo!()
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn read_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &mut [u8],
        _data: SpinLockGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<usize, SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &[u8],
        _data: SpinLockGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<usize, SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn list(&self) -> Result<alloc::vec::Vec<alloc::string::String>, system_error::SystemError> {
        Err(SystemError::ENOSYS)
    }
    fn metadata(&self) -> Result<crate::filesystem::vfs::Metadata, SystemError> {
        let file_metadata = match &self.inner().file_inode {
            Some(inode) => inode.metadata()?,
            None => {
                return Err(SystemError::EPERM);
            }
        };
        let metadata = Metadata {
            dev_id: 0,
            inode_id: InodeId::new(0), // Loop 设备通常没有实际的 inode ID
            size: self.inner().file_size as i64,
            blk_size: LBA_SIZE as usize,
            blocks: (self.inner().file_size + LBA_SIZE - 1) / LBA_SIZE as usize, // 计算块数
            atime: file_metadata.atime,
            mtime: file_metadata.mtime,
            ctime: file_metadata.ctime,
            btime: file_metadata.btime,
            file_type: crate::filesystem::vfs::FileType::BlockDevice,
            mode: crate::filesystem::vfs::syscall::ModeType::from_bits_truncate(0o644),
            nlinks: 1,
            uid: 0, // 默认用户 ID
            gid: 0, // 默认组 ID
            raw_dev: self.inner().device_number,
        };
        Ok(metadata.clone())
    }
}

impl DeviceINode for LoopDevice {
    fn set_fs(&self, fs: alloc::sync::Weak<crate::filesystem::devfs::DevFS>) {
        *self.fs.write() = fs;
    }
    fn set_parent(&self, parent: Weak<crate::filesystem::devfs::LockedDevFSInode>) {
        *self.parent.write() = parent;
    }
}

impl Device for LoopDevice {
    fn dev_type(&self) -> DeviceType {
        DeviceType::Block
    }

    fn id_table(&self) -> IdTable {
        IdTable::new(LOOP_BASENAME.to_string(), None)
    }

    fn bus(&self) -> Option<Weak<dyn Bus>> {
        self.inner().device_common.bus.clone()
    }

    fn set_bus(&self, bus: Option<Weak<dyn Bus>>) {
        self.inner().device_common.bus = bus;
    }

    fn class(&self) -> Option<Arc<dyn Class>> {
        let mut guard = self.inner();
        let r = guard.device_common.class.clone()?.upgrade();
        if r.is_none() {
            guard.device_common.class = None;
        }
        return r;
    }

    fn set_class(&self, class: Option<Weak<dyn Class>>) {
        self.inner().device_common.class = class;
    }

    fn driver(&self) -> Option<Arc<dyn Driver>> {
        let r = self.inner().device_common.driver.clone()?.upgrade();
        if r.is_none() {
            self.inner().device_common.driver = None;
        }
        return r;
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

impl BlockDevice for LoopDevice {
    fn dev_name(&self) -> &DevName {
        &self.block_dev_meta.devname
    }

    fn blkdev_meta(&self) -> &BlockDevMeta {
        &self.block_dev_meta
    }

    fn disk_range(&self) -> GeneralBlockRange {
        let inner = self.inner();
        let blocks = inner.file_size / LBA_SIZE;
        drop(inner);
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
        let inner = self.inner();
        let offset = inner.offset + lba_id_start * LBA_SIZE;
        let len = count * LBA_SIZE;

        // 通过文件 inode 读取数据
        // 使用一个空的 FilePrivateData 作为占位符
        use crate::filesystem::vfs::FilePrivateData;
        use crate::libs::spinlock::SpinLock;
        let data = SpinLock::new(FilePrivateData::Unused);
        let data_guard = data.lock();

        // 处理 Option 类型的 file_inode
        match &inner.file_inode {
            Some(inode) => {
                // 计算实际的文件偏移量
                let file_offset = inner.offset + offset;
                inode.read_at(file_offset, len, buf, data_guard)
            }
            None => {
                // 如果没有关联的文件，返回错误
                Err(SystemError::ENODEV)
            }
        }
    }

    fn write_at_sync(
        &self,
        lba_id_start: BlockId,
        count: usize,
        buf: &[u8],
    ) -> Result<usize, SystemError> {
        let inner = self.inner();

        // 检查是否只读
        if inner.read_only {
            return Err(SystemError::EROFS);
        }

        let offset = inner.offset + lba_id_start * LBA_SIZE;
        let len = count * LBA_SIZE;

        // 通过文件 inode 写入数据
        // 使用一个空的 FilePrivateData 作为占位符
        use crate::filesystem::vfs::FilePrivateData;
        use crate::libs::spinlock::SpinLock;
        let data = SpinLock::new(FilePrivateData::Unused);
        let data_guard = data.lock();

        // 处理 Option 类型的 file_inode
        match &inner.file_inode {
            Some(inode) => {
                // 计算实际的文件偏移量
                let file_offset = inner.offset + offset;
                inode.write_at(file_offset, len, buf, data_guard)
            }
            None => {
                // 如果没有关联的文件，返回错误
                Err(SystemError::ENODEV)
            }
        }
    }

    fn sync(&self) -> Result<(), SystemError> {
        // Loop 设备的同步操作
        Ok(())
    }

    fn blk_size_log2(&self) -> u8 {
        9 // 512 bytes = 2^9
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
        // Loop 设备通常不支持分区
        Vec::new()
    }
}

/// Loop设备驱动
/// 参考Virtio_blk驱动的实现
#[derive(Debug)]
#[cast_to([sync] Driver)]
pub struct LoopDeviceDriver {
    inner: SpinLock<InnerLoopDeviceDriver>,
    kobj_state: LockedKObjectState,
}
struct InnerLoopDeviceDriver{
    driver_common: DriverCommonData,
    kobj_common: KObjectCommonData,
}
impl Debug for InnerLoopDeviceDriver {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("InnerLoopDeviceDriver")
            .field("driver_common", &self.driver_common)
            .field("kobj_common", &self.kobj_common)
            .finish()
    }
}
impl LoopDeviceDriver {
    
    pub fn new() -> Arc<Self> {
        let inner = InnerLoopDeviceDriver{
            driver_common: DriverCommonData::default(),
            kobj_common: KObjectCommonData::default(),
        };
        Arc::new(Self {
            inner: SpinLock::new(inner),
            kobj_state: LockedKObjectState::default(),
        })
    }
    fn inner(&self) -> SpinLockGuard<InnerLoopDeviceDriver> {
        self.inner.lock()
    }
    fn loop_init(&self) -> Result<(), SystemError> {
        // 创建并初始化 LoopManager 单例
     //   let loop_mgr = LOOP_MANAGER.call_once(|| LoopManager::new());
        for minor in 0..LoopManager::MAX_DEVICES {
            let devname = DevName::new(format!("{}{}", LOOP_BASENAME, minor), minor);
            if let Some(loop_dev) = LoopDevice::new_empty_loop_device(devname.clone(), minor as u32) {
                log::info!(
                    "Registering loop device: {}",
                loop_dev.block_dev_meta.devname
            );
            block_dev_manager().register(loop_dev.clone())?;
        //devfs进行注册时[ ERROR ] (src/init/initcall.rs:22)      Failed to call initializer register_loop_devices: EPERM
        //devfs_register(&format!("loop{}", minor), loop_dev.clone())?;
        } else {
            error!("Failed to create loop device for minor {}", minor);
        }
    }
        // // 创建并注册 /dev/loop-control
        // let control_dev = Arc::new(LoopControlDevice::new()); // 假设的构造函数
        // block_dev_manager().register(control_dev.clone())?;
        // devfs::devfs_register("loop-control", control_dev.clone())?;
        // info!("LoopDeviceDriver and all devices initialized.");
        Ok(())
    }
}
use crate::init::initcall::INITCALL_DEVICE;
#[unified_init(INITCALL_DEVICE)]
pub fn loop_init()-> Result<(), SystemError> {
    // 获取 LoopDeviceDriver 的单例并调用初始化函数
    let driver = LoopDeviceDriver::new();
    driver.loop_init()
}


impl Driver for LoopDeviceDriver {
    fn id_table(&self) -> Option<IdTable> {
        Some(IdTable::new("loop".to_string(), None))
    }

     fn devices(&self) -> Vec<Arc<dyn Device>> {
        self.inner().driver_common.devices.clone()
    }

    fn add_device(&self, device: Arc<dyn Device>) {
        self.inner().driver_common.push_device(device);
    }

    fn delete_device(&self, device: &Arc<dyn Device>) {
        self.inner().driver_common.delete_device(device);
    }

    fn bus(&self) -> Option<Weak<dyn Bus>> {
        self.inner().driver_common.bus.clone()
    }

    fn set_bus(&self, bus: Option<Weak<dyn Bus>>) {
        self.inner().driver_common.bus = bus;
    }
}

impl KObject for LoopDeviceDriver {
    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn set_inode(&self, inode: Option<Arc<KernFSInode>>) {
        self.inner().kobj_common.kern_inode = inode;
    }

    fn inode(&self) -> Option<Arc<KernFSInode>> {
        self.inner().kobj_common.kern_inode.clone()
    }

    fn parent(&self) -> Option<Weak<dyn KObject>> {
        self.inner().kobj_common.parent.clone()
    }

    fn set_parent(&self, parent: Option<Weak<dyn KObject>>) {
        self.inner().kobj_common.parent = parent;
    }

    fn kset(&self) -> Option<Arc<KSet>> {
        self.inner().kobj_common.kset.clone()
    }

    fn set_kset(&self, kset: Option<Arc<KSet>>) {
        self.inner().kobj_common.kset = kset;
    }

    fn kobj_type(&self) -> Option<&'static dyn KObjType> {
        self.inner().kobj_common.kobj_type
    }

    fn set_kobj_type(&self, ktype: Option<&'static dyn KObjType>) {
        self.inner().kobj_common.kobj_type = ktype;
    }

    fn name(&self) -> String {
        "loop".to_string()
    }

    fn set_name(&self, _name: String) {
        // do nothing
    }

    fn kobj_state(&self) -> RwLockReadGuard<KObjectState> {
        self.kobj_state.read()
    }

    fn kobj_state_mut(&self) -> RwLockWriteGuard<KObjectState> {
        self.kobj_state.write()
    }

    fn set_kobj_state(&self, state: KObjectState) {
        *self.kobj_state.write() = state;
    }
}
pub struct LoopControlDevice {
    locked_kobj_state: LockedKObjectState,
    inner: SpinLock<LoopControlDeviceInner>,
}

pub struct LoopControlDeviceInner {
    kobject_common: KObjectCommonData,
    device_common: DeviceCommonData,
}
pub struct LoopManager {
    devices: Vec<Arc<LoopDevice>>,
}
impl LoopManager {
    const MAX_DEVICES: usize =8;
}