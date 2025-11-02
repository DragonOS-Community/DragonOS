use crate::{
    driver::base::{
        block::{
            block_device::{BlockDevice, BlockId, GeneralBlockRange, LBA_SIZE},
            disk_info::Partition,
            manager::{block_dev_manager, BlockDevMeta},
        },
        class::Class,
        device::{
            bus::Bus,
            device_number::{DeviceNumber, Major},
            device_register, device_unregister,
            driver::{Driver, DriverCommonData},
            DevName, Device, DeviceCommonData, DeviceType, IdTable,
        },
        kobject::{KObjType, KObject, KObjectCommonData, KObjectState, LockedKObjectState},
        kset::KSet,
    },
    filesystem::{
        devfs::{devfs_register, DevFS, DeviceINode, LockedDevFSInode},
        kernfs::KernFSInode,
        vfs::{
            file::FileMode, FilePrivateData, FileType, IndexNode, InodeId, Metadata,
        },
    },
    libs::{
        rwlock::{RwLock, RwLockReadGuard, RwLockWriteGuard},
        spinlock::{SpinLock, SpinLockGuard},
    },
    process::ProcessManager,
};
use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    any::Any,
    fmt::{Debug, Formatter},
};
use log::error;
use system_error::SystemError;
use unified_init::macros::unified_init;
const LOOP_BASENAME: &str = "loop";
// // loop device 加密类型
// pub const LO_CRYPT_NONE: u32 = 0;
// pub const LO_CRYPT_XOR: u32 = 1;
// pub const LO_CRYPT_DES: u32 = 2;
// pub const LO_CRYPT_FISH2: u32 = 3; // Twofish encryption
// pub const LO_CRYPT_BLOW: u32 = 4;
// pub const LO_CRYPT_CAST128: u32 = 5;
// pub const LO_CRYPT_IDEA: u32 = 6;
// pub const LO_CRYPT_DUMMY: u32 = 9;
// pub const LO_CRYPT_SKIPJACK: u32 = 10;
// pub const LO_CRYPT_CRYPTOAPI: u32 = 18;
// pub const MAX_LO_CRYPT: u32 = 20;

// // IOCTL 命令 - 使用 0x4C ('L')
pub const LOOP_SET_FD: u32 = 0x4C00;
pub const LOOP_CLR_FD: u32 = 0x4C01;
// pub const LOOP_SET_STATUS: u32 = 0x4C02;
// pub const LOOP_GET_STATUS: u32 = 0x4C03;
// pub const LOOP_SET_STATUS64: u32 = 0x4C04;
// pub const LOOP_GET_STATUS64: u32 = 0x4C05;
// pub const LOOP_CHANGE_FD: u32 = 0x4C06;
// pub const LOOP_SET_CAPACITY: u32 = 0x4C07;
// pub const LOOP_SET_DIRECT_IO: u32 = 0x4C08;
// pub const LOOP_SET_BLOCK_SIZE: u32 = 0x4C09;
// pub const LOOP_CONFIGURE: u32 = 0x4C0A;

// /dev/loop-control 接口
pub const LOOP_CTL_ADD: u32 = 0x4C80;
pub const LOOP_CTL_REMOVE: u32 = 0x4C81;
pub const LOOP_CTL_GET_FREE: u32 = 0x4C82;
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
#[derive(Debug, Clone)]
pub struct LoopPrivateData {
    //索引号
    pub parms: u32,
}
//Inner内数据会改变所以加锁
pub struct LoopDeviceInner {
    // 设备名称 Major和Minor
    pub device_number: DeviceNumber,
    //状态管理
    state: LoopState,
    state_lock: SpinLock<()>,
    //后端文件相关
    // 关联的文件节点
    pub file_inode: Option<Arc<dyn IndexNode>>,
    // 文件大小
    pub file_size: usize,
    // 数据偏移量
    pub offset: usize,
    // 是否只读
    pub read_only: bool,
    // KObject的公共数据
    pub kobject_common: KObjectCommonData,
    // 设备的公共数据
    pub device_common: DeviceCommonData,
    //工作管理 todo
    //work_queue: Option<Arc<WorkQueue>>,
}
impl LoopDeviceInner {
    fn set_state(&mut self, new_state: LoopState) -> Result<(), SystemError> {
        let _guard = self.state_lock.lock();

        // 状态转换检查
        match (&self.state, &new_state) {
            (LoopState::Unbound, LoopState::Bound) => {}
            (LoopState::Bound, LoopState::Unbound) => {}
            (LoopState::Bound, LoopState::Rundown) => {}
            (LoopState::Rundown, LoopState::Deleting) => {}
            (LoopState::Rundown, LoopState::Unbound) => {}
            (LoopState::Unbound, LoopState::Deleting) => {}
            _ => return Err(SystemError::EINVAL),
        }

        self.state = new_state;
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoopState {
    Unbound,
    Bound,
    Rundown,
    Deleting,
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
                read_only: false,
                kobject_common: KObjectCommonData::default(),
                device_common: DeviceCommonData::default(),
                state: LoopState::Unbound,
                state_lock: SpinLock::new(()),
                //work_queue: None,
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
        let metadata = file_inode.metadata()?;
        if metadata.size < 0 {
            return Err(SystemError::EINVAL);
        }
        let file_size = metadata.size as usize;

        let mut inner = self.inner();
        inner.file_inode = Some(file_inode);
        inner.file_size = file_size;
        inner.offset = 0;

        Ok(())
    }

    // 获取文件大小
    pub fn file_size(&self) -> usize {
        self.inner().file_size
    }

    // 设置只读模式
    pub fn set_read_only(&self, read_only: bool) {
        self.inner().read_only = read_only;
    }

    // 检查是否为只读
    pub fn is_read_only(&self) -> bool {
        self.inner().read_only
    }

    pub fn is_bound(&self) -> bool {
        matches!(self.inner().state, LoopState::Bound)
    }

    pub fn bind_file(
        &self,
        file_inode: Arc<dyn IndexNode>,
        read_only: bool,
    ) -> Result<(), SystemError> {
        {
            let inner = self.inner();
            if matches!(inner.state, LoopState::Bound) {
                return Err(SystemError::EBUSY);
            }
        }

        self.set_file(file_inode.clone())?;

        let mut inner = self.inner();
        inner.set_state(LoopState::Bound)?;
        inner.read_only = read_only;
        Ok(())
    }

    pub fn clear_file(&self) -> Result<(), SystemError> {
        let mut inner = self.inner();
        match inner.state {
            LoopState::Bound | LoopState::Rundown => inner.set_state(LoopState::Unbound)?,
            LoopState::Unbound => {}
            LoopState::Deleting => return Err(SystemError::EBUSY),
        }

        inner.file_inode = None;
        inner.file_size = 0;
        inner.offset = 0;
        inner.read_only = false;
        Ok(())
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
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: SpinLockGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<usize, SystemError> {
        if len > buf.len() {
            return Err(SystemError::ENOBUFS);
        }
        BlockDevice::read_at_bytes(self, offset, len, buf)
    }

    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        _data: SpinLockGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<usize, SystemError> {
        if len > buf.len() {
            return Err(SystemError::E2BIG);
        }
        BlockDevice::write_at_bytes(self, offset, len, &buf[..len])
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
            blk_size: LBA_SIZE,
            blocks: (self.inner().file_size + LBA_SIZE - 1) / LBA_SIZE, // 计算块数
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
    fn ioctl(
        &self,
        cmd: u32,
        data: usize,
        _private_data: &FilePrivateData,
    ) -> Result<usize, SystemError> {
        match cmd {
            LOOP_SET_FD => {
                let file_fd = data as i32;
                let fd_table = ProcessManager::current_pcb().fd_table();
                let file = {
                    let guard = fd_table.read();
                    guard.get_file_by_fd(file_fd)
                }
                .ok_or(SystemError::EBADF)?;

                let mode = file.mode();
                let read_only = !mode.contains(FileMode::O_WRONLY) && !mode.contains(FileMode::O_RDWR);

                let inode = file.inode();
                let metadata = inode.metadata()?;
                match metadata.file_type {
                    FileType::File | FileType::BlockDevice => {}
                    _ => return Err(SystemError::EINVAL),
                }

                self.bind_file(inode, read_only)?;
                Ok(0)
            }
            LOOP_CLR_FD => {
                self.clear_file()?;
                Ok(0)
            }
            _ => Err(SystemError::ENOSYS),
        }
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
        let blocks = if inner.file_size == 0 {
            0
        } else {
            //饱和式加法，溢出返回类型最大值
            inner.file_size.saturating_add(LBA_SIZE - 1) / LBA_SIZE
        };
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
        if count == 0 {
            return Ok(0);
        }
        let len = count
            .checked_mul(LBA_SIZE)
            .ok_or(SystemError::EOVERFLOW)?;
        if len > buf.len() {
            return Err(SystemError::EINVAL);
        }

        let (file_inode, base_offset) = {
            let inner = self.inner();
            let inode = inner.file_inode.clone().ok_or(SystemError::ENODEV)?;
            (inode, inner.offset)
        };

        let block_offset = lba_id_start
            .checked_mul(LBA_SIZE)
            .ok_or(SystemError::EOVERFLOW)?;
        let file_offset = base_offset
            .checked_add(block_offset)
            .ok_or(SystemError::EOVERFLOW)?;

        let data = SpinLock::new(FilePrivateData::Unused);
        let data_guard = data.lock();

        file_inode.read_at(file_offset, len, &mut buf[..len], data_guard)
    }

    fn write_at_sync(
        &self,
        lba_id_start: BlockId,
        count: usize,
        buf: &[u8],
    ) -> Result<usize, SystemError> {
        if count == 0 {
            return Ok(0);
        }
        let len = count
            .checked_mul(LBA_SIZE)
            .ok_or(SystemError::EOVERFLOW)?;
        if len > buf.len() {
            return Err(SystemError::EINVAL);
        }

        let (file_inode, base_offset, read_only) = {
            let inner = self.inner();
            let inode = inner.file_inode.clone().ok_or(SystemError::ENODEV)?;
            (inode, inner.offset, inner.read_only)
        };

        if read_only {
            return Err(SystemError::EROFS);
        }

        let block_offset = lba_id_start
            .checked_mul(LBA_SIZE)
            .ok_or(SystemError::EOVERFLOW)?;
        let file_offset = base_offset
            .checked_add(block_offset)
            .ok_or(SystemError::EOVERFLOW)?;

        let data = SpinLock::new(FilePrivateData::Unused);
        let data_guard = data.lock();

        let written = file_inode.write_at(file_offset, len, &buf[..len], data_guard)?;

        if written > 0 {
            if let Ok(metadata) = file_inode.metadata() {
                if metadata.size >= 0 {
                    let mut inner = self.inner();
                    inner.file_size = metadata.size as usize;
                }
            }
        }

        Ok(written)
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
struct InnerLoopDeviceDriver {
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
        let inner = InnerLoopDeviceDriver {
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
    fn new_loop_device(&self, minor: usize) -> Result<Arc<LoopDevice>, SystemError> {
        let devname = DevName::new(format!("{}{}", LOOP_BASENAME, minor), minor);
         let loop_dev = LoopDevice::new_empty_loop_device(devname.clone(), minor as u32)
            .ok_or_else(|| {
                error!("Failed to create loop device for minor {}", minor);
                SystemError::ENOMEM // 如果创建失败，返回具体的错误
            })?;
        log::info!(
            "Registering loop device: {}",
            loop_dev.block_dev_meta.devname
        );
        // 先注册到块设备管理器，让它可用
        block_dev_manager().register(loop_dev.clone())?;
        
        // 返回创建的设备，让 LoopManager 能够存储它
        Ok(loop_dev)
    }
}
//初始化函数，注册1个loopcontrol设备和8个loop设备备用
use crate::init::initcall::INITCALL_DEVICE;
#[unified_init(INITCALL_DEVICE)]
pub fn loop_init() -> Result<(), SystemError> {
    let loop_mgr = Arc::new(LoopManager::new());
    // 获取 LoopDeviceDriver 的单例并调用初始化函数
    let driver = LoopDeviceDriver::new();
    let loop_ctl = LoopControlDevice::new(loop_mgr.clone());
    //注册loop_control设备
    device_register(loop_ctl.clone())?;
    log::info!("Loop control device registered.");
    devfs_register("loop-control", loop_ctl.clone())?;
    log::info!("Loop control device initialized.");
    loop_mgr.loop_init(driver)?;
    Ok(())
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

pub struct LoopManager {
    inner: SpinLock<LoopManagerInner>,
}
pub struct LoopManagerInner {
    devices: [Option<Arc<LoopDevice>>; LoopManager::MAX_DEVICES],
    next_free_minor: u32,
}
impl LoopManager {
    const MAX_DEVICES: usize = 256; // 支持的最大 loop 设备数量
    const MAX_INIT_DEVICES: usize = 8; //初始化loop设备数量
    pub fn new() -> Self {
        Self {
            inner: SpinLock::new(LoopManagerInner {
                devices: [const { None }; Self::MAX_DEVICES],
                next_free_minor: 0,
            }),
        }
    }
    fn inner(&self) -> SpinLockGuard<LoopManagerInner> {
        self.inner.lock()
    }
    //index: 次设备号
    pub fn register_device(&self, index: usize, device: Arc<LoopDevice>) {
        if index < Self::MAX_DEVICES {
            let mut inner = self.inner();
            inner.devices[index] = Some(device);
        }
    }
    /*
    请求队列，工作队列未实现
     */

    pub fn loop_add(&self, requested_minor: Option<u32>) -> Result<Arc<LoopDevice>, SystemError> {
        let mut inner = self.inner();
        match requested_minor {
            Some(req_minor) => self.loop_add_specific_locked(&mut inner, req_minor),
            None => self.loop_add_first_available_locked(&mut inner),
        }
    }

    fn loop_add_specific_locked(
        &self,
        inner: &mut LoopManagerInner,
        minor: u32,
    ) -> Result<Arc<LoopDevice>, SystemError> {
        if minor >= Self::MAX_DEVICES as u32 {
            return Err(SystemError::EINVAL);
        }

        if let Some(device) = inner.devices[minor as usize].as_ref() {
            if device.is_bound() {
                return Err(SystemError::EEXIST);
            }
            inner.next_free_minor = (minor + 1) % Self::MAX_DEVICES as u32;
            return Ok(device.clone());
        }

        self.create_and_register_device_locked(inner, minor)
    }

    fn loop_add_first_available_locked(
        &self,
        inner: &mut LoopManagerInner,
    ) -> Result<Arc<LoopDevice>, SystemError> {
        for _ in 0..Self::MAX_DEVICES {
            let idx = inner.next_free_minor;
            inner.next_free_minor = (inner.next_free_minor + 1) % Self::MAX_DEVICES as u32;

            match &inner.devices[idx as usize] {
                Some(device) if !device.is_bound() => return Ok(device.clone()),
                Some(_) => continue,
                None => {
                    return self.create_and_register_device_locked(inner, idx);
                }
            }
        }

        Err(SystemError::ENOSPC)
    }

    fn create_and_register_device_locked(
        &self,
        inner: &mut LoopManagerInner,
        minor: u32,
    ) -> Result<Arc<LoopDevice>, SystemError> {
        if minor >= Self::MAX_DEVICES as u32 {
            return Err(SystemError::EINVAL);
        }

        let devname = DevName::new(format!("{}{}", LOOP_BASENAME, minor), minor as usize);
        let loop_dev =
            LoopDevice::new_empty_loop_device(devname, minor).ok_or(SystemError::ENOMEM)?;

        if let Err(e) = block_dev_manager().register(loop_dev.clone()) {
            if e == SystemError::EEXIST {
                if let Some(existing) = inner.devices[minor as usize].clone() {
                    return Ok(existing);
                }
            }
            return Err(e);
        }

        inner.devices[minor as usize] = Some(loop_dev.clone());
        inner.next_free_minor = (minor + 1) % Self::MAX_DEVICES as u32;
        log::info!("Loop device loop{} added successfully.", minor);
        Ok(loop_dev)
    }

    pub fn loop_clear(&self, minor: u32) -> Result<(), SystemError> {
        if minor >= Self::MAX_DEVICES as u32 {
            return Err(SystemError::EINVAL);
        }

        let device = {
            let inner = self.inner();
            inner.devices[minor as usize].clone()
        }
        .ok_or(SystemError::ENODEV)?;

        device.clear_file()?;

        let mut inner = self.inner();
        inner.next_free_minor = minor;
        log::info!("Loop device loop{} cleared.", minor);
        Ok(())
    }

    pub fn loop_remove(&self, minor: u32) -> Result<(), SystemError> {
        if minor >= Self::MAX_DEVICES as u32 {
            return Err(SystemError::EINVAL);
        }

        let device = {
            let inner = self.inner();
            inner.devices[minor as usize].clone()
        };

        let device = match device {
            Some(dev) => dev,
            None => return Err(SystemError::ENODEV),
        };

        {
            let mut guard = device.inner();
            match guard.state {
                LoopState::Bound => {
                    guard.set_state(LoopState::Rundown)?;
                }
                LoopState::Rundown | LoopState::Unbound => {}
                LoopState::Deleting => return Ok(()),
            }
        }

        device.clear_file()?;

        {
            let mut guard = device.inner();
            guard.set_state(LoopState::Deleting)?;
        }

        let block_dev: Arc<dyn BlockDevice> = device.clone();
        block_dev_manager().unregister(&block_dev)?;

        {
            let mut inner = self.inner();
            inner.devices[minor as usize] = None;
            inner.next_free_minor = minor;
        }

        log::info!("Loop device loop{} removed.", minor);
        Ok(())
    }

    pub fn find_free_minor(&self) -> Option<u32> {
        let mut inner = self.inner();
        for _ in 0..Self::MAX_DEVICES {
            let idx = inner.next_free_minor;
            inner.next_free_minor = (inner.next_free_minor + 1) % Self::MAX_DEVICES as u32;
            match &inner.devices[idx as usize] {
                Some(device) if device.is_bound() => continue,
                _ => return Some(idx),
            }
        }
        None
    }
    pub fn find_device_by_minor(&self, minor: u32) -> Option<Arc<LoopDevice>> {
        let inner = self.inner();
        if minor < Self::MAX_DEVICES as u32 {
            inner.devices[minor as usize].clone()
        } else {
            None
        }
    }
    // pub fn loop_remove(&self ,minor:u32)-> Result<(),SystemError>{
    //     let mut inner_guard=self.inner();
    //     if minor >=Self::MAX_DEVICES as u32{
    //         return Err(SystemError::EINVAL);
    //     }
    //     if let Some(loop_dev)=inner_guard.devices[minor as usize ].take(){
    //         //loop_dev.clear_file()?;
    //         //loop_dev.inner().set_stable(LoopState::Deleting)?;

    //         block_dev_manager().unregister(loop_dev.dev_name())?;
    //     }
    // }
    // 动态分配空闲的loop设备，与指定文件inode关联
    pub fn alloc_device(
        &self,
        file_inode: Arc<dyn IndexNode>,
    ) -> Result<Arc<LoopDevice>, SystemError> {
        let mut inner = self.inner();
        for (i, device) in inner.devices.iter_mut().enumerate() {
            if device.is_none() {
                let devname = DevName::new(format!("{}{}", LOOP_BASENAME, i), i);
                let loop_device = LoopDevice::new_empty_loop_device(devname, i as u32)
                    .ok_or(SystemError::ENOMEM)?;
                loop_device.set_file(file_inode.clone())?;
                *device = Some(loop_device.clone());
                return Ok(loop_device);
            }
        }
        Err(SystemError::ENOSPC)
    }
    pub fn deallocate_device(&self, device: &Arc<LoopDevice>) -> Result<(), SystemError> {
        /*
        重置状态unbound
         */
        let mut inner_guard = device.inner();
        inner_guard.set_state(LoopState::Unbound)?;
        inner_guard.file_inode = None;
        inner_guard.file_size = 0;
        inner_guard.offset = 0;
        inner_guard.read_only = false;
        drop(inner_guard);
        let minor = device.inner().device_number.minor() as usize;
        let mut loop_mgr_inner = self.inner(); // Lock the LoopManager
        if minor < LoopManager::MAX_DEVICES {
            if let Some(removed_device) = loop_mgr_inner.devices[minor].take() {
                log::info!("Deallocated loop device loop{} from manager.", minor);
                // Unregister from block device manager
                device_unregister(removed_device.clone());
            } else {
                log::warn!(
                    "Attempted to deallocate loop device loop{} but it was not found in manager.",
                    minor
                );
            }
        } else {
            return Err(SystemError::EINVAL); // Minor out of bounds
        }

        Ok(()) // Indicate success
    }
    pub fn loop_init(&self, driver: Arc<LoopDeviceDriver>) -> Result<(), SystemError> {
        let mut inner =self.inner();
        // 注册 loop 设备
        for minor in 0..Self::MAX_INIT_DEVICES {
            let loop_dev =driver.new_loop_device(minor)?;
            inner.devices[minor]=Some(loop_dev);
        }
        log::info!("Loop devices initialized");

        //添加到loop_manager中

        log::info!("Loop devices initialized.");
        Ok(())
    }
}
//一个字符设备，作为一个抽象接口控制loop设备的创建，绑定和删除
/*
设备分配和查找
设备绑定和解绑
设备状态查询和配置（配置设备参数，如偏移量、大小限制等）
*/

pub struct LoopControlDevice {
    inner: SpinLock<LoopControlDeviceInner>,
    locked_kobj_state: LockedKObjectState,
    loop_mgr: Arc<LoopManager>,
}
struct LoopControlDeviceInner {
    // 设备的公共数据
    pub device_common: DeviceCommonData,
    // KObject的公共数据
    pub kobject_common: KObjectCommonData,

    parent: RwLock<Weak<LockedDevFSInode>>,
    device_inode_fs: RwLock<Option<Weak<DevFS>>>,
    devfs_metadata: Metadata,
}
impl LoopControlDevice {
    pub fn loop_add(&self, index: u32) -> Result<Arc<LoopDevice>, SystemError> {
        //let loop_dri= LoopDeviceDriver::new();
        self.loop_mgr.loop_add(Some(index))
    }
    pub fn new(loop_mgr: Arc<LoopManager>) -> Arc<Self> {
        Arc::new(Self {
            inner: SpinLock::new(LoopControlDeviceInner {
                kobject_common: KObjectCommonData::default(),
                device_common: DeviceCommonData::default(),
                parent: RwLock::new(Weak::default()),
                device_inode_fs: RwLock::new(None),
                devfs_metadata: Metadata::default(),
            }),
            locked_kobj_state: LockedKObjectState::default(),
            loop_mgr,
        })
    }
    pub fn inner(&self) -> SpinLockGuard<LoopControlDeviceInner> {
        self.inner.lock()
    }
}
impl DeviceINode for LoopControlDevice {
    fn set_fs(&self, fs: alloc::sync::Weak<crate::filesystem::devfs::DevFS>) {
        *self.inner().device_inode_fs.write() = Some(fs);
    }
    fn set_parent(&self, parent: Weak<crate::filesystem::devfs::LockedDevFSInode>) {
        *self.inner().parent.write() = parent;
    }
}
impl Debug for LoopControlDevice {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LoopControlDevice").finish()
    }
}
impl IndexNode for LoopControlDevice {
    fn open(
        &self,
        _data: SpinLockGuard<FilePrivateData>,
        _mode: &FileMode,
    ) -> Result<(), SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Ok(());
    }
    fn close(
        &self,
        _data: SpinLockGuard<FilePrivateData>,
    ) -> Result<(), SystemError> {
        Ok(())
    }
    fn metadata(&self) -> Result<Metadata, SystemError> {
        use crate::filesystem::vfs::{syscall::ModeType, FileType, InodeId};
        use crate::time::PosixTimeSpec;

        let metadata = Metadata {
            dev_id: 0,
            inode_id: InodeId::new(0), // Loop control 设备的 inode ID
            size: 0,                   // 字符设备大小通常为0
            blk_size: 0,               // 字符设备不使用块大小
            blocks: 0,                 // 字符设备不使用块数
            atime: PosixTimeSpec::default(),
            mtime: PosixTimeSpec::default(),
            ctime: PosixTimeSpec::default(),
            btime: PosixTimeSpec::default(),
            file_type: FileType::CharDevice,           // 字符设备类型
            mode: ModeType::from_bits_truncate(0o600), // 读写权限，仅owner可访问
            nlinks: 1,
            uid: 0,                                          // root用户
            gid: 0,                                          // root组
            raw_dev: DeviceNumber::new(Major::new(10), 237), // loop-control设备号通常是(10, 237)
        };
        Ok(metadata)
    }
    fn fs(&self) -> Arc<dyn crate::filesystem::vfs::FileSystem> {
        todo!()
    }
    fn ioctl(
        &self,
        cmd: u32,
        data: usize,
        _private_data: &FilePrivateData,
    ) -> Result<usize, SystemError> {
        match cmd {
            LOOP_CTL_ADD => {
                log::info!("Starting LOOP_CTL_ADD ioctl");
                let requested_index = data as u32;
                let loop_dev = if requested_index == u32::MAX {
                    self.loop_mgr.loop_add(None)?
                } else {
                    self.loop_mgr.loop_add(Some(requested_index))?
                };
                let minor = {
                    let inner = loop_dev.inner();
                    let minor = inner.device_number.minor();
                    log::info!(
                        "LOOP_CTL_ADD ioctl succeeded, allocated loop device loop{}",
                        minor
                    );
                    minor
                };
                Ok(minor as usize)
            }
            LOOP_CTL_REMOVE => {
                let minor_to_remove = data as u32;
                self.loop_mgr.loop_remove(minor_to_remove)?;
                Ok(0)
            }
            LOOP_CTL_GET_FREE => {
                match self.loop_mgr.find_free_minor() {
                    Some(minor) => Ok(minor as usize),
                    None => Err(SystemError::ENOSPC),
                }
            }
            _ => Err(SystemError::ENOSYS),
        }
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
    // fn metadata(&self) -> Result<Metadata, SystemError> {
    //    Metadata
    // }
}
impl Device for LoopControlDevice {
    fn dev_type(&self) -> DeviceType {
        DeviceType::Char
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
impl KObject for LoopControlDevice {
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
        "loop-control".to_string()
    }

    fn set_name(&self, _name: String) {
        // do nothing
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
