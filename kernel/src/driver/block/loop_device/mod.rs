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
            device_register,
            driver::{Driver, DriverCommonData},
            DevName, Device, DeviceCommonData, DeviceType, IdTable,
        },
        kobject::{
            KObjType, KObject, KObjectCommonData, KObjectManager, KObjectState, KObjectSysFSOps,
            LockedKObjectState,
        },
        kset::KSet,
    },
    filesystem::{
        devfs::{devfs_register, DevFS, DeviceINode, LockedDevFSInode},
        kernfs::KernFSInode,
        sysfs::{AttributeGroup, SysFSOps},
        vfs::{file::FileMode, FilePrivateData, FileType, IndexNode, InodeId, Metadata},
    },
    libs::{
        rwlock::{RwLock, RwLockReadGuard, RwLockWriteGuard},
        spinlock::{SpinLock, SpinLockGuard},
    },
    process::ProcessManager,
    syscall::user_access::{UserBufferReader, UserBufferWriter},
};
use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    any::Any,
    fmt::{Debug, Formatter},
    sync::atomic::{AtomicU32, Ordering},
};
use ida::IdAllocator;
use log::{error, info, warn};
use num_traits::FromPrimitive;
use system_error::SystemError;
use unified_init::macros::unified_init;
const LOOP_BASENAME: &str = "loop";
const LOOP_CONTROL_BASENAME: &str = "loop-control";
pub const LOOP_CONTROL_MINOR: u32 = 237;
#[repr(u32)]
#[derive(Debug, FromPrimitive)]
pub enum LoopIoctl {
    LoopSetFd = 0x4C00,
    LoopClrFd = 0x4C01,
    LoopSetStatus = 0x4C02,
    LoopGetStatus = 0x4C03,
    LoopSetStatus64 = 0x4C04,
    LoopGetStatus64 = 0x4C05,
    LoopChangeFd = 0x4C06,
    LoopSetCapacity = 0x4C07,
    LoopSetDirectIo = 0x4c08,
    LoopSetBlockSize = 0x4c09,
    LoopConfigure = 0x4c0a,
}
#[repr(u32)]
#[derive(Debug, FromPrimitive)]
pub enum LoopControlIoctl {
    Add = 0x4C80,
    Remove = 0x4C81,
    GetFree = 0x4C82,
}
pub const LO_FLAGS_READ_ONLY: u32 = 1 << 0;
pub const SUPPORTED_LOOP_FLAGS: u32 = LO_FLAGS_READ_ONLY;
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct LoopStatus64 {
    pub lo_offset: u64,
    pub lo_sizelimit: u64,
    pub lo_flags: u32,
    pub __pad: u32,
}
#[derive(Debug)]
pub struct LoopDeviceKObjType;

impl KObjType for LoopDeviceKObjType {
    fn release(&self, kobj: Arc<dyn KObject>) {
        if let Some(loop_dev) = kobj.as_any_ref().downcast_ref::<LoopDevice>() {
            loop_dev.final_cleanup();
        }
    }

    fn sysfs_ops(&self) -> Option<&dyn SysFSOps> {
        Some(&KObjectSysFSOps)
    }

    fn attribute_groups(&self) -> Option<&'static [&'static dyn AttributeGroup]> {
        None
    }
}
static LOOP_DEVICE_KOBJ_TYPE: LoopDeviceKObjType = LoopDeviceKObjType;
struct IoGuard<'a> {
    device: &'a LoopDevice,
}

impl<'a> IoGuard<'a> {
    fn new(device: &'a LoopDevice) -> Result<Self, SystemError> {
        device.io_start()?;
        Ok(Self { device })
    }
}

impl<'a> Drop for IoGuard<'a> {
    fn drop(&mut self) {
        self.device.io_end();
    }
}

pub struct LoopDevice {
    id: usize,
    minor: u32,
    inner: SpinLock<LoopDeviceInner>,
    block_dev_meta: BlockDevMeta,
    locked_kobj_state: LockedKObjectState,
    self_ref: Weak<Self>,
    fs: RwLock<Weak<DevFS>>,
    parent: RwLock<Weak<LockedDevFSInode>>,
    /// 活跃的 I/O 操作计数
    active_io_count: AtomicU32,
}
#[derive(Debug, Clone)]
pub struct LoopPrivateData {}
pub struct LoopDeviceInner {
    pub device_number: DeviceNumber,
    state: LoopState,
    state_lock: SpinLock<()>,
    pub file_inode: Option<Arc<dyn IndexNode>>,
    pub file_size: usize,
    pub offset: usize,
    pub size_limit: usize,
    pub flags: u32,
    pub read_only: bool,
    pub kobject_common: KObjectCommonData,
    pub device_common: DeviceCommonData,
}
impl LoopDeviceInner {
    fn set_state(&mut self, new_state: LoopState) -> Result<(), SystemError> {
        let _guard = self.state_lock.lock();

        match (&self.state, &new_state) {
            (LoopState::Unbound, LoopState::Bound) => {}
            (LoopState::Bound, LoopState::Unbound) => {}
            (LoopState::Bound, LoopState::Rundown) => {}
            (LoopState::Rundown, LoopState::Draining) => {}
            (LoopState::Rundown, LoopState::Deleting) => {}
            (LoopState::Rundown, LoopState::Unbound) => {}
            (LoopState::Draining, LoopState::Deleting) => {}
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
    Draining,
    Deleting,
}
impl Debug for LoopDevice {
    fn fmt(&'_ self, f: &mut Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LoopDevice")
            .field("id", &self.id)
            .field("devname", &self.block_dev_meta.devname)
            .finish()
    }
}
impl LoopDevice {
    fn inner(&'_ self) -> SpinLockGuard<'_, LoopDeviceInner> {
        self.inner.lock()
    }
    pub fn id(&self) -> usize {
        self.id
    }
    pub fn minor(&self) -> u32 {
        self.minor
    }
    /// # 功能
    ///
    /// 创建一个未绑定文件的 loop 设备实例。
    ///
    /// ## 参数
    ///
    /// - `devname`: 设备名称。
    /// - `minor`: 次设备号。
    ///
    /// ## 返回值
    /// - `Some(Arc<Self>)`: 成功创建的 loop 设备。
    /// - `None`: 内存不足或创建失败。
    pub fn new_empty_loop_device(devname: DevName, id: usize, minor: u32) -> Option<Arc<Self>> {
        let dev = Arc::new_cyclic(|self_ref| Self {
            id,
            minor,
            inner: SpinLock::new(LoopDeviceInner {
                file_inode: None, // 默认的虚拟 inode
                file_size: 0,
                device_number: DeviceNumber::new(Major::LOOP_MAJOR, minor), // Loop 设备主设备号为 7
                offset: 0,
                size_limit: 0,
                flags: 0,
                read_only: false,
                kobject_common: KObjectCommonData::default(),
                device_common: DeviceCommonData::default(),
                state: LoopState::Unbound,
                state_lock: SpinLock::new(()),
            }),
            block_dev_meta: BlockDevMeta::new(devname, Major::LOOP_MAJOR), // Loop 设备主设备号为 7
            locked_kobj_state: LockedKObjectState::default(),
            self_ref: self_ref.clone(),
            fs: RwLock::new(Weak::default()),
            parent: RwLock::new(Weak::default()),
            active_io_count: AtomicU32::new(0),
        });

        // 设置 KObjType
        dev.set_kobj_type(Some(&LOOP_DEVICE_KOBJ_TYPE));

        Some(dev)
    }

    /// # 功能
    ///
    /// 为 loop 设备绑定后端文件并重置相关状态。
    ///
    /// ## 参数
    ///
    /// - `file_inode`: 需要绑定的文件节点。
    ///
    /// ## 返回值
    /// - `Ok(())`: 成功绑定文件。
    /// - `Err(SystemError)`: 绑定失败的错误原因。
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
        inner.size_limit = 0;
        inner.flags = 0; // Reset flags
        inner.read_only = false; // Reset read_only
        drop(inner);
        self.recalc_effective_size()?; // Recalculate size based on new file
        Ok(())
    }
    fn recalc_effective_size(&self) -> Result<(), SystemError> {
        let (file_inode, offset, size_limit) = {
            let inner = self.inner();
            (inner.file_inode.clone(), inner.offset, inner.size_limit)
        };

        let inode = file_inode.ok_or(SystemError::ENODEV)?;
        let metadata = inode.metadata()?;
        if metadata.size < 0 {
            return Err(SystemError::EINVAL);
        }
        let total_size = metadata.size as usize;
        if offset > total_size {
            return Err(SystemError::EINVAL);
        }
        let mut effective = total_size - offset;
        if size_limit > 0 {
            effective = effective.min(size_limit);
        }

        let mut inner = self.inner();
        inner.file_size = effective;
        Ok(())
    }

    pub fn is_bound(&self) -> bool {
        matches!(self.inner().state, LoopState::Bound)
    }
    /// # 功能
    ///
    /// 将文件绑定到 loop 设备并设置访问权限。
    ///
    /// ## 参数
    ///
    /// - `file_inode`: 需要绑定的文件节点。
    /// - `read_only`: 是否以只读方式绑定。
    ///
    /// ## 返回值
    /// - `Ok(())`: 成功绑定。
    /// - `Err(SystemError)`: 绑定失败的原因。
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
        inner.flags = if read_only { LO_FLAGS_READ_ONLY } else { 0 };
        inner.size_limit = 0;
        drop(inner);
        self.recalc_effective_size()?;
        Ok(())
    }
    /// # 功能
    ///
    /// 清除 loop 设备的文件绑定并复位状态。
    ///
    /// ## 参数
    ///
    /// - 无。
    ///
    /// ## 返回值
    /// - `Ok(())`: 成功清除。
    /// - `Err(SystemError)`: 清除过程中的错误。
    pub fn clear_file(&self) -> Result<(), SystemError> {
        let mut inner = self.inner();
        match inner.state {
            LoopState::Bound | LoopState::Rundown => inner.set_state(LoopState::Unbound)?,
            LoopState::Unbound => {}
            LoopState::Draining => return Err(SystemError::EBUSY),
            LoopState::Deleting => {
                // 在删除流程中，允许清理文件
                // 状态已经是Deleting，无需改变
            }
        }

        inner.file_inode = None;
        inner.file_size = 0;
        inner.offset = 0;
        inner.size_limit = 0;
        inner.read_only = false;
        inner.flags = 0;
        Ok(())
    }
    fn validate_loop_status64_params(info: &LoopStatus64) -> Result<(), SystemError> {
        if !info.lo_offset.is_multiple_of(LBA_SIZE as u64) {
            return Err(SystemError::EINVAL);
        }
        if info.lo_offset > usize::MAX as u64 || info.lo_sizelimit > usize::MAX as u64 {
            return Err(SystemError::EINVAL);
        }
        if info.lo_sizelimit != 0 && !info.lo_sizelimit.is_multiple_of(LBA_SIZE as u64) {
            return Err(SystemError::EINVAL);
        }
        if info.lo_flags & !SUPPORTED_LOOP_FLAGS != 0 {
            return Err(SystemError::EINVAL);
        }
        Ok(())
    }
    /// 设置 loop 设备的状态（64 位版本）。
    ///
    /// ## 参数
    ///
    /// - `user_ptr`: 用户空间传入的 `LoopStatus64` 结构体指针。
    ///
    /// ## 返回值
    /// - `Ok(())`: 状态设置成功。
    /// - `Err(SystemError::EINVAL)`: 无效的参数或标志位。
    /// - `Err(SystemError::ENXIO)`: 设备未绑定或已卸载。
    fn set_status64(&self, user_ptr: usize) -> Result<(), SystemError> {
        if user_ptr == 0 {
            return Err(SystemError::EINVAL);
        }

        let reader = UserBufferReader::new::<LoopStatus64>(
            user_ptr as *const LoopStatus64,
            core::mem::size_of::<LoopStatus64>(),
            true,
        )?;
        let mut info = LoopStatus64::default();
        reader.copy_one_from_user(&mut info, 0)?;
        Self::validate_loop_status64_params(&info)?;
        let new_offset = info.lo_offset as usize;
        let new_limit = if info.lo_sizelimit == 0 {
            0
        } else {
            info.lo_sizelimit as usize
        };
        let new_read_only = (info.lo_flags & LO_FLAGS_READ_ONLY) != 0;

        let (old_offset, old_limit, old_flags, old_ro) = {
            let inner = self.inner();
            if !matches!(inner.state, LoopState::Bound | LoopState::Rundown) {
                return Err(SystemError::ENXIO);
            }
            (inner.offset, inner.size_limit, inner.flags, inner.read_only)
        };

        {
            let mut inner = self.inner();
            if !matches!(inner.state, LoopState::Bound | LoopState::Rundown) {
                return Err(SystemError::ENXIO);
            }
            inner.offset = new_offset;
            inner.size_limit = new_limit;
            inner.flags = info.lo_flags;
            inner.read_only = new_read_only;
        }

        if let Err(err) = self.recalc_effective_size() {
            let mut inner = self.inner();
            inner.offset = old_offset;
            inner.size_limit = old_limit;
            inner.flags = old_flags;
            inner.read_only = old_ro;
            drop(inner);
            let _ = self.recalc_effective_size();
            return Err(err);
        }

        Ok(())
    }
    /// # 功能
    ///
    /// 获取 loop 设备的 LoopStatus64 信息并写回用户态。
    ///
    /// ## 参数
    ///
    /// - `user_ptr`: 用户态缓冲区地址。
    ///
    /// ## 返回值
    /// - `Ok(())`: 信息写回成功。
    /// - `Err(SystemError)`: 读取状态失败。
    ///
    fn get_status64(&self, user_ptr: usize) -> Result<(), SystemError> {
        if user_ptr == 0 {
            return Err(SystemError::EINVAL);
        }

        let info = {
            let inner = self.inner();
            if !matches!(inner.state, LoopState::Bound | LoopState::Rundown) {
                return Err(SystemError::ENXIO);
            }
            LoopStatus64 {
                lo_offset: inner.offset as u64,
                lo_sizelimit: inner.size_limit as u64,
                lo_flags: if inner.read_only {
                    inner.flags | LO_FLAGS_READ_ONLY
                } else {
                    inner.flags & !LO_FLAGS_READ_ONLY
                },
                __pad: 0,
            }
        };

        let mut writer = UserBufferWriter::new::<LoopStatus64>(
            user_ptr as *mut LoopStatus64,
            core::mem::size_of::<LoopStatus64>(),
            true,
        )?;
        writer.copy_one_to_user(&info, 0)?;
        Ok(())
    }
    fn set_status(&self, user_ptr: usize) -> Result<(), SystemError> {
        self.set_status64(user_ptr)
    }
    fn get_status(&self, user_ptr: usize) -> Result<(), SystemError> {
        self.get_status64(user_ptr)
    }
    /// # 功能
    ///
    /// 将 loop 设备切换到新的文件描述符。
    ///
    /// ## 参数
    ///
    /// - `new_file_fd`: 新的文件描述符。
    ///
    /// ## 返回值
    /// - `Ok(())`: 成功切换。
    /// - `Err(SystemError)`: 切换失败原因。
    fn change_fd(&self, new_file_fd: i32) -> Result<(), SystemError> {
        let fd_table = ProcessManager::current_pcb().fd_table();
        let file = {
            let guard = fd_table.read();
            guard.get_file_by_fd(new_file_fd)
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
        let mut inner = self.inner();
        inner.file_inode = Some(inode);
        inner.read_only = read_only;
        inner.flags = if read_only { LO_FLAGS_READ_ONLY } else { 0 };
        drop(inner);
        self.recalc_effective_size()?;
        Ok(())
    }
    fn set_capacity(&self, _arg: usize) -> Result<(), SystemError> {
        self.recalc_effective_size()?;
        Ok(())
    }

    /// # 功能
    ///
    /// I/O 操作开始时调用，增加活跃 I/O 计数
    ///
    /// ## 返回值
    /// - `Ok(())`: 成功增加计数
    /// - `Err(SystemError::ENODEV)`: 设备正在删除，拒绝新的 I/O
    fn io_start(&self) -> Result<(), SystemError> {
        let inner = self.inner();
        if matches!(
            inner.state,
            LoopState::Rundown | LoopState::Draining | LoopState::Deleting
        ) {
            return Err(SystemError::ENODEV);
        }

        self.active_io_count.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    /// # 功能
    ///
    /// I/O 操作完成时调用，减少活跃 I/O 计数
    fn io_end(&self) {
        let prev = self.active_io_count.fetch_sub(1, Ordering::AcqRel);
        if prev == 0 {
            warn!(
                "Loop device loop{}: I/O count underflow",
                self.inner().device_number.minor()
            );
        }
    }

    /// # 功能
    ///
    /// 进入 Rundown 状态，停止接受新的 I/O 请求
    ///
    /// ## 返回值
    /// - `Ok(())`: 成功进入 Rundown 状态
    /// - `Err(SystemError)`: 状态转换失败
    fn enter_rundown_state(&self) -> Result<(), SystemError> {
        let mut inner = self.inner();
        match inner.state {
            LoopState::Bound => {
                inner.set_state(LoopState::Rundown)?;
                info!(
                    "Loop device loop{} entering rundown state",
                    inner.device_number.minor()
                );
            }
            LoopState::Unbound => {
                // 空设备可以直接删除
                inner.set_state(LoopState::Deleting)?;
                info!(
                    "Loop device loop{} is unbound, skipping to deleting state",
                    inner.device_number.minor()
                );
            }
            LoopState::Rundown => {}
            LoopState::Draining | LoopState::Deleting => {
                return Err(SystemError::EBUSY);
            }
        }
        Ok(())
    }

    /// # 功能
    ///
    /// 等待所有活跃的 I/O 操作完成
    ///
    /// ## 返回值
    /// - `Ok(())`: 所有 I/O 已完成
    /// - `Err(SystemError::ETIMEDOUT)`: 等待超时
    fn drain_active_io(&self) -> Result<(), SystemError> {
        let mut inner = self.inner();
        if matches!(inner.state, LoopState::Rundown) {
            inner.set_state(LoopState::Draining)?;
            info!(
                "Loop device loop{} entering draining state",
                inner.device_number.minor()
            );
        }
        drop(inner);
        let timeout_ms = 30_000;
        let check_interval_us = 10_000;
        let max_checks = timeout_ms * 1000 / check_interval_us;

        for _i in 0..max_checks {
            let count = self.active_io_count.load(Ordering::Acquire);
            if count == 0 {
                break;
            }

            core::hint::spin_loop();
        }

        let final_count = self.active_io_count.load(Ordering::Acquire);
        if final_count != 0 {
            error!(
                "Timeout waiting for I/O to drain on loop{}: {} operations still active",
                self.minor(),
                final_count
            );
            return Err(SystemError::ETIMEDOUT);
        }

        info!(
            "All I/O operations drained for loop device loop{}",
            self.minor()
        );

        let mut inner = self.inner();
        inner.set_state(LoopState::Deleting)?;

        Ok(())
    }

    /// # 功能
    ///
    /// 从 sysfs 中移除设备
    ///
    /// ## 返回值
    /// - `Ok(())`: 成功移除
    /// - `Err(SystemError)`: 移除失败
    fn remove_from_sysfs(&self) -> Result<(), SystemError> {
        // 使用 KObjectManager 从 sysfs 中移除
        if let Some(kobj) = self.self_ref.upgrade() {
            KObjectManager::remove_kobj(kobj as Arc<dyn KObject>);
            info!("Removed loop device loop{} from sysfs", self.minor());
        }
        Ok(())
    }

    /// # 功能
    ///
    /// 最终清理函数，由 KObjType::release 调用
    /// 执行设备删除的最后清理工作
    fn final_cleanup(&self) {
        info!(
            "Final cleanup for loop device loop{} (id {})",
            self.minor(),
            self.id()
        );
        let mut inner = self.inner();
        if let Some(file_inode) = inner.file_inode.take() {
            drop(file_inode);
            warn!(
                "File inode was still present during final cleanup for loop{}",
                self.minor()
            );
        }
        inner.file_size = 0;
        inner.offset = 0;
        inner.size_limit = 0;
        info!("Loop device loop{} cleanup complete", self.minor());
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

    fn kobj_state(&'_ self) -> RwLockReadGuard<'_, KObjectState> {
        self.locked_kobj_state.read()
    }

    fn kobj_state_mut(&'_ self) -> RwLockWriteGuard<'_, KObjectState> {
        self.locked_kobj_state.write()
    }

    fn set_kobj_state(&self, state: KObjectState) {
        *self.locked_kobj_state.write() = state;
    }
}

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
            inode_id: InodeId::new(0),
            size: self.inner().file_size as i64,
            blk_size: LBA_SIZE,
            blocks: (self.inner().file_size.div_ceil(LBA_SIZE)),
            atime: file_metadata.atime,
            mtime: file_metadata.mtime,
            ctime: file_metadata.ctime,
            btime: file_metadata.btime,
            file_type: crate::filesystem::vfs::FileType::BlockDevice,
            mode: crate::filesystem::vfs::syscall::ModeType::from_bits_truncate(0o644),
            nlinks: 1,
            uid: 0,
            gid: 0,
            raw_dev: self.inner().device_number,
        };
        Ok(metadata)
    }
    fn ioctl(
        &self,
        cmd: u32,
        data: usize,
        _private_data: &FilePrivateData,
    ) -> Result<usize, SystemError> {
        match LoopIoctl::from_u32(cmd) {
            Some(LoopIoctl::LoopSetFd) => {
                let file_fd = data as i32;
                let fd_table = ProcessManager::current_pcb().fd_table();
                let file = {
                    let guard = fd_table.read();
                    guard.get_file_by_fd(file_fd)
                }
                .ok_or(SystemError::EBADF)?;

                let mode = file.mode();
                let read_only =
                    !mode.contains(FileMode::O_WRONLY) && !mode.contains(FileMode::O_RDWR);

                let inode = file.inode();
                let metadata = inode.metadata()?;
                match metadata.file_type {
                    FileType::File | FileType::BlockDevice => {}
                    _ => return Err(SystemError::EINVAL),
                }

                self.bind_file(inode, read_only)?;
                Ok(0)
            }
            Some(LoopIoctl::LoopClrFd) => {
                self.clear_file()?;
                Ok(0)
            }
            Some(LoopIoctl::LoopSetStatus) => {
                self.set_status(data)?;
                Ok(0)
            }
            Some(LoopIoctl::LoopGetStatus) => {
                self.get_status(data)?;
                Ok(0)
            }
            Some(LoopIoctl::LoopSetStatus64) => {
                self.set_status64(data)?;
                Ok(0)
            }
            Some(LoopIoctl::LoopGetStatus64) => {
                self.get_status64(data)?;
                Ok(0)
            }
            Some(LoopIoctl::LoopChangeFd) => {
                self.change_fd(data as i32)?;
                Ok(0)
            }
            Some(LoopIoctl::LoopSetCapacity) => {
                self.set_capacity(data)?;
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
        // 使用 IoGuard 确保 I/O 计数正确管理
        let _io_guard = IoGuard::new(self)?;

        if count == 0 {
            return Ok(0);
        }
        let len = count.checked_mul(LBA_SIZE).ok_or(SystemError::EOVERFLOW)?;
        if len > buf.len() {
            return Err(SystemError::EINVAL);
        }

        let (file_inode, base_offset, limit_end) = {
            let inner = self.inner();
            let inode = inner.file_inode.clone().ok_or(SystemError::ENODEV)?;
            let limit = inner
                .offset
                .checked_add(inner.file_size)
                .ok_or(SystemError::EOVERFLOW)?;
            (inode, inner.offset, limit)
        };

        let block_offset = lba_id_start
            .checked_mul(LBA_SIZE)
            .ok_or(SystemError::EOVERFLOW)?;
        let file_offset = base_offset
            .checked_add(block_offset)
            .ok_or(SystemError::EOVERFLOW)?;

        let end = file_offset.checked_add(len).ok_or(SystemError::EOVERFLOW)?;
        if end > limit_end {
            return Err(SystemError::ENOSPC);
        }

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
        // 使用 IoGuard 确保 I/O 计数正确管理
        let _io_guard = IoGuard::new(self)?;

        if count == 0 {
            return Ok(0);
        }
        let len = count.checked_mul(LBA_SIZE).ok_or(SystemError::EOVERFLOW)?;
        if len > buf.len() {
            return Err(SystemError::EINVAL);
        }

        let (file_inode, base_offset, limit_end, read_only) = {
            let inner = self.inner();
            let inode = inner.file_inode.clone().ok_or(SystemError::ENODEV)?;
            let limit = inner
                .offset
                .checked_add(inner.file_size)
                .ok_or(SystemError::EOVERFLOW)?;
            (inode, inner.offset, limit, inner.read_only)
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

        let end = file_offset.checked_add(len).ok_or(SystemError::EOVERFLOW)?;
        if end > limit_end {
            return Err(SystemError::ENOSPC);
        }

        let data = SpinLock::new(FilePrivateData::Unused);
        let data_guard = data.lock();

        let written = file_inode.write_at(file_offset, len, &buf[..len], data_guard)?;

        if written > 0 {
            let _ = self.recalc_effective_size();
        }

        Ok(written)
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
    fn inner(&'_ self) -> SpinLockGuard<'_, InnerLoopDeviceDriver> {
        self.inner.lock()
    }
}
use crate::init::initcall::INITCALL_DEVICE;
#[unified_init(INITCALL_DEVICE)]
pub fn loop_init() -> Result<(), SystemError> {
    let loop_mgr = Arc::new(LoopManager::new());
    let driver = LoopDeviceDriver::new();
    let loop_ctl = LoopControlDevice::new(loop_mgr.clone());

    device_register(loop_ctl.clone())?;
    log::info!("Loop control device registered.");
    devfs_register(LOOP_CONTROL_BASENAME, loop_ctl.clone())?;
    log::info!("Loop control device initialized.");
    loop_mgr.loop_init(driver)?;
    Ok(())
}

impl Driver for LoopDeviceDriver {
    fn id_table(&self) -> Option<IdTable> {
        Some(IdTable::new(LOOP_BASENAME.to_string(), None))
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
        LOOP_BASENAME.to_string()
    }

    fn set_name(&self, _name: String) {
        // do nothing
    }

    fn kobj_state(&'_ self) -> RwLockReadGuard<'_, KObjectState> {
        self.kobj_state.read()
    }

    fn kobj_state_mut(&'_ self) -> RwLockWriteGuard<'_, KObjectState> {
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
    id_alloc: IdAllocator,
    next_free_minor: u32,
}
impl LoopManager {
    const MAX_DEVICES: usize = 256;
    const MAX_INIT_DEVICES: usize = 8;
    pub fn new() -> Self {
        Self {
            inner: SpinLock::new(LoopManagerInner {
                devices: [const { None }; Self::MAX_DEVICES],
                id_alloc: IdAllocator::new(0, Self::MAX_DEVICES)
                    .expect("create IdAllocator failed"),
                next_free_minor: 0,
            }),
        }
    }
    fn inner(&'_ self) -> SpinLockGuard<'_, LoopManagerInner> {
        self.inner.lock()
    }
    #[inline]
    fn alloc_id_locked(inner: &mut LoopManagerInner) -> Option<usize> {
        inner.id_alloc.alloc()
    }
    #[inline]
    fn free_id_locked(inner: &mut LoopManagerInner, id: usize) {
        if id < Self::MAX_DEVICES && inner.id_alloc.exists(id) {
            inner.id_alloc.free(id);
        }
    }
    #[inline]
    pub fn format_name(id: usize) -> DevName {
        DevName::new(format!("{}{}", LOOP_BASENAME, id), id)
    }
    fn find_device_by_minor_locked(
        inner: &LoopManagerInner,
        minor: u32,
    ) -> Option<Arc<LoopDevice>> {
        inner
            .devices
            .iter()
            .flatten()
            .find(|device| device.minor() == minor)
            .map(Arc::clone)
    }

    fn find_unused_minor_locked(inner: &LoopManagerInner) -> Option<u32> {
        let mut candidate = inner.next_free_minor;
        for _ in 0..Self::MAX_DEVICES as u32 {
            let mut used = false;
            for dev in inner.devices.iter().flatten() {
                if dev.minor() == candidate {
                    used = true;
                    break;
                }
            }
            if !used {
                return Some(candidate);
            }
            candidate = (candidate + 1) % Self::MAX_DEVICES as u32;
        }
        None
    }
    /*
    请求队列，工作队列未实现
     */
    /// # 功能
    ///
    /// 根据请求的次设备号分配或复用 loop 设备。
    ///
    /// ## 参数
    ///
    /// - `requested_minor`: 指定的次设备号，`None` 表示自动分配。
    ///
    /// ## 返回值
    /// - `Ok(Arc<LoopDevice>)`: 成功获得的设备。
    /// - `Err(SystemError)`: 无可用设备或参数错误。
    pub fn loop_add(&self, requested_minor: Option<u32>) -> Result<Arc<LoopDevice>, SystemError> {
        let mut inner = self.inner();
        match requested_minor {
            Some(req_minor) => self.loop_add_specific_locked(&mut inner, req_minor),
            None => self.loop_add_first_available_locked(&mut inner),
        }
    }
    /// # 功能
    ///
    /// 在锁作用域内分配指定次设备号的 loop 设备。
    ///
    /// ## 参数
    ///
    /// - `inner`: 管理器内部状态锁。
    /// - `minor`: 目标次设备号。
    ///
    /// ## 返回值
    /// - `Ok(Arc<LoopDevice>)`: 成功获得的设备实例。
    /// - `Err(SystemError)`: 参数无效或设备已被占用。
    fn loop_add_specific_locked(
        &self,
        inner: &mut LoopManagerInner,
        minor: u32,
    ) -> Result<Arc<LoopDevice>, SystemError> {
        if minor >= Self::MAX_DEVICES as u32 {
            return Err(SystemError::EINVAL);
        }

        if let Some(device) = Self::find_device_by_minor_locked(inner, minor) {
            if device.is_bound() {
                return Err(SystemError::EEXIST);
            }
            return Ok(device);
        }

        let id = Self::alloc_id_locked(inner).ok_or(SystemError::ENOSPC)?;
        match self.create_and_register_device_locked(inner, id, minor) {
            Ok(device) => Ok(device),
            Err(e) => {
                Self::free_id_locked(inner, id);
                Err(e)
            }
        }
    }

    fn loop_add_first_available_locked(
        &self,
        inner: &mut LoopManagerInner,
    ) -> Result<Arc<LoopDevice>, SystemError> {
        if let Some(device) = inner
            .devices
            .iter()
            .flatten()
            .find(|device| !device.is_bound())
        {
            return Ok(device.clone());
        }

        let id = Self::alloc_id_locked(inner).ok_or(SystemError::ENOSPC)?;
        let minor = match Self::find_unused_minor_locked(inner) {
            Some(minor) => minor,
            None => {
                Self::free_id_locked(inner, id);
                return Err(SystemError::ENOSPC);
            }
        };
        let result = self.create_and_register_device_locked(inner, id, minor);
        if result.is_err() {
            Self::free_id_locked(inner, id);
        }
        result
    }

    fn create_and_register_device_locked(
        &self,
        inner: &mut LoopManagerInner,
        id: usize,
        minor: u32,
    ) -> Result<Arc<LoopDevice>, SystemError> {
        if minor >= Self::MAX_DEVICES as u32 {
            return Err(SystemError::EINVAL);
        }

        let devname = Self::format_name(id);
        let loop_dev =
            LoopDevice::new_empty_loop_device(devname, id, minor).ok_or(SystemError::ENOMEM)?;

        if let Err(e) = block_dev_manager().register(loop_dev.clone()) {
            if e == SystemError::EEXIST {
                if let Some(existing) = inner.devices[id].clone() {
                    return Ok(existing);
                }
            }
            return Err(e);
        }

        inner.devices[id] = Some(loop_dev.clone());
        inner.next_free_minor = (minor + 1) % Self::MAX_DEVICES as u32;
        log::info!(
            "Loop device id {} (minor {}) added successfully.",
            id,
            minor
        );
        Ok(loop_dev)
    }
    /// # 功能
    ///
    /// 删除指定 minor 的 loop 设备
    /// 实现规范的删除流程，包括状态转换、I/O 排空、资源清理
    ///
    /// ## 参数
    ///
    /// - `minor`: 要删除的设备的次设备号
    ///
    /// ## 返回值
    /// - `Ok(())`: 成功删除设备
    /// - `Err(SystemError)`: 删除失败
    pub fn loop_remove(&self, minor: u32) -> Result<(), SystemError> {
        if minor >= Self::MAX_DEVICES as u32 {
            return Err(SystemError::EINVAL);
        }
        let device = {
            let inner = self.inner();
            Self::find_device_by_minor_locked(&inner, minor)
        }
        .ok_or(SystemError::ENODEV)?;
        let id = device.id();
        info!("Starting removal of loop device loop{} (id {})", minor, id);
        device.enter_rundown_state()?;
        let needs_drain = {
            let inner = device.inner();
            !matches!(inner.state, LoopState::Deleting)
        };

        if needs_drain {
            device.drain_active_io()?;
        }

        device.clear_file()?;

        let _ = device.remove_from_sysfs();

        let block_dev: Arc<dyn BlockDevice> = device.clone();
        block_dev_manager().unregister(&block_dev)?;

        {
            let mut inner = self.inner();
            inner.devices[id] = None;
            Self::free_id_locked(&mut inner, id);
            inner.next_free_minor = minor;
        }
        info!(
            "Loop device id {} (minor {}) removed successfully.",
            id, minor
        );
        Ok(())
    }

    pub fn find_free_minor(&self) -> Option<u32> {
        let inner = self.inner();
        'outer: for minor in 0..Self::MAX_DEVICES as u32 {
            for dev in inner.devices.iter().flatten() {
                if dev.minor() == minor {
                    if !dev.is_bound() {
                        return Some(minor);
                    }
                    continue 'outer;
                }
            }
            return Some(minor);
        }
        None
    }
    pub fn loop_init(&self, _driver: Arc<LoopDeviceDriver>) -> Result<(), SystemError> {
        let mut inner = self.inner();
        for minor in 0..Self::MAX_INIT_DEVICES {
            let minor_u32 = minor as u32;
            if Self::find_device_by_minor_locked(&inner, minor_u32).is_some() {
                continue;
            }
            let id = Self::alloc_id_locked(&mut inner).ok_or(SystemError::ENOSPC)?;
            if let Err(e) = self.create_and_register_device_locked(&mut inner, id, minor_u32) {
                Self::free_id_locked(&mut inner, id);
                return Err(e);
            }
        }
        log::info!("Loop devices initialized");
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
    device_common: DeviceCommonData,
    // KObject的公共数据
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
        _data: SpinLockGuard<FilePrivateData>,
        _mode: &FileMode,
    ) -> Result<(), SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Ok(());
    }
    fn close(&self, _data: SpinLockGuard<FilePrivateData>) -> Result<(), SystemError> {
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
    ///
    fn metadata(&self) -> Result<Metadata, SystemError> {
        use crate::filesystem::vfs::{syscall::ModeType, FileType, InodeId};
        use crate::time::PosixTimeSpec;

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
            mode: ModeType::from_bits_truncate(0o600),
            nlinks: 1,
            uid: 0,
            gid: 0,
            raw_dev: DeviceNumber::new(Major::LOOP_CONTROL_MAJOR, LOOP_CONTROL_MINOR),
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

    fn kobj_state(&'_ self) -> RwLockReadGuard<'_, KObjectState> {
        self.locked_kobj_state.read()
    }

    fn kobj_state_mut(&'_ self) -> RwLockWriteGuard<'_, KObjectState> {
        self.locked_kobj_state.write()
    }

    fn set_kobj_state(&self, state: KObjectState) {
        *self.locked_kobj_state.write() = state;
    }
}
