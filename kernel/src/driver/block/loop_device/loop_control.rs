use crate::{
    driver::base::{
        class::Class,
        device::{
            bus::Bus,
            device_number::{DeviceNumber, Major},
            driver::Driver,
            Device, DeviceCommonData, DeviceType, IdTable,
        },
        kobject::{KObjType, KObject, KObjectCommonData, KObjectState, LockedKObjectState},
        kset::KSet,
    },
    filesystem::{
        devfs::{DevFS, DeviceINode, LockedDevFSInode},
        kernfs::KernFSInode,
        vfs::{
            file::FileFlags, FilePrivateData, FileType, IndexNode, InodeFlags, InodeId, InodeMode,
            Metadata,
        },
    },
    libs::{
        mutex::MutexGuard,
        rwlock::RwLock,
        rwsem::{RwSemReadGuard, RwSemWriteGuard},
        spinlock::{SpinLock, SpinLockGuard},
    },
    process::ProcessManager,
    time::PosixTimeSpec,
};
use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
};
use core::{
    any::Any,
    fmt::{Debug, Formatter},
};
use num_traits::FromPrimitive;
use system_error::SystemError;

use super::{
    constants::{LoopControlIoctl, LOOP_CONTROL_BASENAME, LOOP_CONTROL_MINOR},
    manager::LoopManager,
};

/// Loop-control 设备
///
/// 一个字符设备，作为一个抽象接口控制loop设备的创建，绑定和删除
/// - 设备分配和查找
/// - 设备绑定和解绑
/// - 设备状态查询和配置（配置设备参数，如偏移量、大小限制等）
pub struct LoopControlDevice {
    inner: SpinLock<LoopControlDeviceInner>,
    locked_kobj_state: LockedKObjectState,
    loop_mgr: Arc<LoopManager>,
}

struct LoopControlDeviceInner {
    /// 设备的公共数据
    device_common: DeviceCommonData,
    /// KObject的公共数据
    kobject_common: KObjectCommonData,

    parent: RwLock<Weak<LockedDevFSInode>>,
    device_inode_fs: RwLock<Option<Weak<DevFS>>>,
}

impl LoopControlDevice {
    pub fn new(loop_mgr: Arc<LoopManager>) -> Arc<Self> {
        Arc::new(Self {
            inner: SpinLock::new(LoopControlDeviceInner {
                kobject_common: KObjectCommonData::default(),
                device_common: DeviceCommonData::default(),
                parent: RwLock::new(Weak::default()),
                device_inode_fs: RwLock::new(None),
            }),
            locked_kobj_state: LockedKObjectState::default(),
            loop_mgr,
        })
    }

    fn inner(&'_ self) -> SpinLockGuard<'_, LoopControlDeviceInner> {
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
        _data: MutexGuard<FilePrivateData>,
        _mode: &FileFlags,
    ) -> Result<(), SystemError> {
        Ok(())
    }

    fn close(&self, _data: MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        Ok(())
    }

    /// # 功能
    ///
    /// 获取 loop-control 设备的元数据信息。
    ///
    /// ## 参数
    ///
    /// - 无
    ///
    /// ## 返回值
    /// - `Ok(Metadata)`: 成功获取设备元数据
    /// - 包含设备类型、权限、设备号等信息
    fn metadata(&self) -> Result<Metadata, SystemError> {
        let metadata = Metadata {
            dev_id: 0,
            inode_id: InodeId::new(0),
            size: 0,
            blk_size: 0,
            blocks: 0,
            atime: PosixTimeSpec::default(),
            mtime: PosixTimeSpec::default(),
            ctime: PosixTimeSpec::default(),
            btime: PosixTimeSpec::default(),
            file_type: FileType::CharDevice,
            mode: InodeMode::from_bits_truncate(0o600),
            flags: InodeFlags::empty(),
            nlinks: 1,
            uid: 0,
            gid: 0,
            raw_dev: DeviceNumber::new(Major::LOOP_CONTROL_MAJOR, LOOP_CONTROL_MINOR),
        };
        Ok(metadata)
    }

    fn fs(&self) -> Arc<dyn crate::filesystem::vfs::FileSystem> {
        // loop-control 设备节点由 DevFS 注册；返回其所在的文件系统。
        if let Some(fs) = self
            .inner()
            .device_inode_fs
            .read()
            .as_ref()
            .and_then(|w| w.upgrade())
        {
            return fs;
        }
        ProcessManager::current_mntns()
            .root_inode()
            .find("dev")
            .expect("LoopControlDevice: DevFS not mounted at /dev")
            .fs()
    }

    fn ioctl(
        &self,
        cmd: u32,
        data: usize,
        _private_data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        match LoopControlIoctl::from_u32(cmd) {
            Some(LoopControlIoctl::Add) => {
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
            Some(LoopControlIoctl::Remove) => {
                let minor_to_remove = data as u32;
                self.loop_mgr.loop_remove(minor_to_remove)?;
                Ok(0)
            }
            Some(LoopControlIoctl::GetFree) => match self.loop_mgr.find_free_minor() {
                Some(minor) => Ok(minor as usize),
                None => Err(SystemError::ENOSPC),
            },
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
        _data: MutexGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<usize, SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &[u8],
        _data: MutexGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<usize, SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn list(&self) -> Result<alloc::vec::Vec<alloc::string::String>, system_error::SystemError> {
        Err(SystemError::ENOSYS)
    }
}

impl Device for LoopControlDevice {
    fn dev_type(&self) -> DeviceType {
        DeviceType::Char
    }

    fn id_table(&self) -> IdTable {
        IdTable::new(LOOP_CONTROL_BASENAME.to_string(), None)
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
        LOOP_CONTROL_BASENAME.to_string()
    }

    fn set_name(&self, _name: String) {
        // do nothing
    }

    fn kobj_state(&'_ self) -> RwSemReadGuard<'_, KObjectState> {
        self.locked_kobj_state.read()
    }

    fn kobj_state_mut(&'_ self) -> RwSemWriteGuard<'_, KObjectState> {
        self.locked_kobj_state.write()
    }

    fn set_kobj_state(&self, state: KObjectState) {
        *self.locked_kobj_state.write() = state;
    }
}
