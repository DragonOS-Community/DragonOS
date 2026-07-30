use crate::{
    driver::base::{
        block::{
            block_device::{BlockDevice, BlockId, GeneralBlockRange, LBA_SIZE},
            disk_info::Partition,
            gendisk::GenDisk,
            manager::BlockDevMeta,
        },
        class::Class,
        device::{
            bus::Bus,
            device_number::{DeviceNumber, Major},
            driver::Driver,
            DevName, Device, DeviceCommonData, DeviceType, IdTable,
        },
        kobject::{
            KObjType, KObject, KObjectCommonData, KObjectManager, KObjectState, KObjectSysFSOps,
            LockedKObjectState,
        },
        kset::KSet,
    },
    filesystem::{
        devfs::{DevFS, DeviceINode, LockedDevFSInode},
        kernfs::KernFSInode,
        sysfs::{AttributeGroup, SysFSOps},
        vfs::{
            file::{File, FileFlags},
            DelegatedWriteResult, FilePrivateData, FileType, IndexNode, InodeFlags, InodeId,
            InodeMode, Metadata, WriteSyncIntent,
        },
    },
    libs::{
        mutex::{Mutex, MutexGuard},
        rwlock::RwLock,
        rwsem::{RwSemReadGuard, RwSemWriteGuard},
        spinlock::{SpinLock, SpinLockGuard},
    },
    process::cred::{capable, CAPFlags},
    process::ProcessManager,
    syscall::user_access::{UserBufferReader, UserBufferWriter},
    time::{sleep::nanosleep, PosixTimeSpec},
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
use log::{info, warn};
use num_traits::FromPrimitive;
use system_error::SystemError;

use super::constants::{
    LoopFlags, LoopIoctl, LoopState, LoopStatus, LoopStatus64, LOOP_BASENAME,
    LOOP_IO_DRAIN_CHECK_INTERVAL_US, LOOP_IO_DRAIN_TIMEOUT_MS,
};

/// Serializes loop backing topology validation and publication.
///
/// The fixed lock order is:
/// `LOOP_BACKING_TOPOLOGY_LOCK -> LoopDevice::config_mutex -> inner`.
static LOOP_BACKING_TOPOLOGY_LOCK: Mutex<()> = Mutex::new(());

/// Loop 设备 KObject 类型
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

pub(super) static LOOP_DEVICE_KOBJ_TYPE: LoopDeviceKObjType = LoopDeviceKObjType;

/// I/O 操作 RAII 守卫
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

/// Loop 设备
#[cast_to([sync] Device, DeviceINode)]
pub struct LoopDevice {
    id: usize,
    minor: u32,
    inner: SpinLock<LoopDeviceInner>,
    block_dev_meta: BlockDevMeta,
    locked_kobj_state: LockedKObjectState,
    self_ref: Weak<Self>,
    fs: RwLock<Weak<DevFS>>,
    parent: RwLock<Weak<LockedDevFSInode>>,
    /// Serializes SET/CHANGE/CLEAR/DELETE configuration transactions.
    config_mutex: Mutex<()>,
    /// 活跃的 I/O 操作计数
    active_io_count: AtomicU32,
    /// Open file descriptions referring to this GenDisk.
    open_count: AtomicU32,
    /// Mounted filesystems using this GenDisk for their complete lifetime.
    mount_holder_count: AtomicU32,
}

/// Per-open loop control state.
#[derive(Debug, Clone, Default)]
pub struct LoopPrivateData {
    /// Whether this open description of `/dev/loopN` has write access.
    control_writable: bool,
    /// Ensures the BlockDevice open count is released exactly once.
    open_counted: bool,
    /// Pins the BlockDevice while GenDisk itself intentionally stores only Weak.
    _device_pin: Option<Arc<LoopDevice>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoopQuiesceOwner {
    Change,
    Clear,
    Reconfigure,
}

/// Loop 设备内部状态
pub struct LoopDeviceInner {
    pub device_number: DeviceNumber,
    state: LoopState,
    pub backing_file: Option<Arc<File>>,
    pub file_size: usize,
    pub offset: usize,
    pub size_limit: usize,
    pub flags: LoopFlags,
    pub kobject_common: KObjectCommonData,
    pub device_common: DeviceCommonData,
    /// Incremented whenever a backing/configuration generation is published.
    generation: u64,
    /// Monotonic owner token for quiesce transactions.
    quiesce_epoch: u64,
    quiesce_owner: Option<(u64, LoopQuiesceOwner)>,
}

impl LoopDeviceInner {
    /// 检查状态转换是否有效并执行转换
    ///
    /// 注意：调用者必须持有 LoopDeviceInner 的锁
    pub(super) fn set_state(&mut self, new_state: LoopState) -> Result<(), SystemError> {
        const VALID_TRANSITIONS: &[(LoopState, LoopState)] = &[
            (LoopState::Unbound, LoopState::Bound),
            (LoopState::Bound, LoopState::Unbound),
            (LoopState::Bound, LoopState::Draining),
            (LoopState::Bound, LoopState::Rundown),
            (LoopState::Rundown, LoopState::Draining),
            (LoopState::Rundown, LoopState::Deleting),
            (LoopState::Rundown, LoopState::Unbound),
            (LoopState::Draining, LoopState::Bound),
            (LoopState::Draining, LoopState::Rundown),
            (LoopState::Draining, LoopState::Deleting),
            (LoopState::Draining, LoopState::Unbound),
            (LoopState::Unbound, LoopState::Deleting),
            // 允许 Deleting 回滚到 Rundown：当 unregister() 失败时，
            // 允许回滚状态以便后续重试删除操作，避免设备成为"僵尸"状态。
            (LoopState::Deleting, LoopState::Rundown),
        ];
        if !VALID_TRANSITIONS.contains(&(self.state, new_state)) {
            return Err(SystemError::EINVAL);
        }
        self.state = new_state;
        Ok(())
    }

    /// 检查设备是否只读
    #[inline]
    pub(super) fn is_read_only(&self) -> bool {
        self.flags.contains(LoopFlags::READ_ONLY)
    }

    /// 获取当前状态
    #[inline]
    pub(super) fn state(&self) -> LoopState {
        self.state
    }
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
    pub(super) fn inner(&'_ self) -> SpinLockGuard<'_, LoopDeviceInner> {
        self.inner.lock()
    }

    #[inline]
    fn calc_effective_size(
        total_size: usize,
        offset: usize,
        size_limit: usize,
    ) -> Result<usize, SystemError> {
        if offset > total_size {
            return Err(SystemError::EINVAL);
        }
        let mut effective = total_size - offset;
        if size_limit > 0 {
            effective = effective.min(size_limit);
        }
        Ok(effective)
    }

    fn set_file_locked(inner: &mut LoopDeviceInner, backing_file: Arc<File>, file_size: usize) {
        inner.backing_file = Some(backing_file);
        inner.file_size = file_size;
        inner.offset = 0;
        inner.size_limit = 0;
        inner.generation = inner.generation.wrapping_add(1);
    }

    fn change_file_locked(
        inner: &mut LoopDeviceInner,
        backing_file: Arc<File>,
        total_size: usize,
    ) -> Result<(), SystemError> {
        let effective = Self::calc_effective_size(total_size, inner.offset, inner.size_limit)?;
        inner.backing_file = Some(backing_file);
        inner.file_size = effective;
        inner.generation = inner.generation.wrapping_add(1);
        Ok(())
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
                backing_file: None,
                file_size: 0,
                device_number: DeviceNumber::new(Major::LOOP_MAJOR, minor),
                offset: 0,
                size_limit: 0,
                flags: LoopFlags::empty(),
                kobject_common: KObjectCommonData::default(),
                device_common: DeviceCommonData::default(),
                state: LoopState::Unbound,
                generation: 0,
                quiesce_epoch: 0,
                quiesce_owner: None,
            }),
            block_dev_meta: BlockDevMeta::new(devname, Major::LOOP_MAJOR),
            locked_kobj_state: LockedKObjectState::default(),
            self_ref: self_ref.clone(),
            fs: RwLock::new(Weak::default()),
            parent: RwLock::new(Weak::default()),
            config_mutex: Mutex::new(()),
            active_io_count: AtomicU32::new(0),
            open_count: AtomicU32::new(0),
            mount_holder_count: AtomicU32::new(0),
        });

        // 设置 KObjType
        dev.set_kobj_type(Some(&LOOP_DEVICE_KOBJ_TYPE));

        Some(dev)
    }

    fn compute_effective_size(
        file: &Arc<File>,
        offset: usize,
        size_limit: usize,
    ) -> Result<usize, SystemError> {
        let metadata = file.inode().metadata()?;
        if metadata.size < 0 {
            return Err(SystemError::EINVAL);
        }
        let total_size = metadata.size as usize;
        Self::calc_effective_size(total_size, offset, size_limit)
    }

    pub fn is_bound(&self) -> bool {
        matches!(self.inner().state(), LoopState::Bound)
    }

    /// Validate a candidate while the global backing-topology lock is held.
    fn validate_backing_chain(&self, candidate: &Arc<File>) -> Result<(), SystemError> {
        let mut current = candidate.clone();
        let mut visited = Vec::new();

        loop {
            let inode = current.inode();
            let block_device = inode
                .as_any_ref()
                .downcast_ref::<GenDisk>()
                .map(GenDisk::block_device)
                .transpose()?;

            let loop_dev = if let Some(loop_dev) = inode.as_any_ref().downcast_ref::<LoopDevice>() {
                loop_dev
            } else if let Some(block_device) = block_device.as_ref() {
                let Some(loop_dev) =
                    BlockDevice::as_any_ref(block_device.as_ref()).downcast_ref::<LoopDevice>()
                else {
                    return Ok(());
                };
                loop_dev
            } else {
                return Ok(());
            };

            if core::ptr::eq(loop_dev, self) || visited.contains(&loop_dev.id()) {
                return Err(SystemError::EBADF);
            }
            visited.push(loop_dev.id());

            let inner = loop_dev.inner();
            if !matches!(inner.state(), LoopState::Bound) {
                return Err(SystemError::EINVAL);
            }
            current = inner.backing_file.clone().ok_or(SystemError::EINVAL)?;
        }
    }

    fn wait_for_active_io(&self) -> Result<(), SystemError> {
        let max_checks = LOOP_IO_DRAIN_TIMEOUT_MS * 1000 / LOOP_IO_DRAIN_CHECK_INTERVAL_US;
        let sleep_ts = PosixTimeSpec::new(
            0,
            (LOOP_IO_DRAIN_CHECK_INTERVAL_US as i64).saturating_mul(1000),
        );

        for _ in 0..max_checks {
            if self.active_io_count.load(Ordering::Acquire) == 0 {
                return Ok(());
            }
            let _ = nanosleep(sleep_ts);
        }
        Err(SystemError::ETIMEDOUT)
    }

    fn install_quiesce_owner(inner: &mut LoopDeviceInner, owner: LoopQuiesceOwner) -> (u64, u64) {
        inner.quiesce_epoch = inner.quiesce_epoch.wrapping_add(1);
        let epoch = inner.quiesce_epoch;
        let generation = inner.generation;
        inner.quiesce_owner = Some((epoch, owner));
        (epoch, generation)
    }

    fn quiesce_still_owned(
        inner: &LoopDeviceInner,
        epoch: u64,
        generation: u64,
        owner: LoopQuiesceOwner,
    ) -> bool {
        inner.generation == generation && inner.quiesce_owner == Some((epoch, owner))
    }

    fn rollback_bound_quiesce(&self, epoch: u64, generation: u64, owner: LoopQuiesceOwner) {
        let _topology = LOOP_BACKING_TOPOLOGY_LOCK.lock();
        let _config = self.config_mutex.lock();
        let mut inner = self.inner();
        if Self::quiesce_still_owned(&inner, epoch, generation, owner) {
            inner.quiesce_owner = None;
            let _ = inner.set_state(LoopState::Bound);
        }
    }

    pub fn bind_file(&self, backing_file: Arc<File>, read_only: bool) -> Result<(), SystemError> {
        // Metadata can block; fetch it before taking loop configuration locks.
        let metadata = backing_file.inode().metadata()?;
        if metadata.size < 0 {
            return Err(SystemError::EINVAL);
        }
        let total_size = metadata.size as usize;

        let _topology = LOOP_BACKING_TOPOLOGY_LOCK.lock();
        let _config = self.config_mutex.lock();
        self.validate_backing_chain(&backing_file)?;

        let mut inner = self.inner();
        match inner.state() {
            LoopState::Unbound => {}
            LoopState::Bound => return Err(SystemError::EBUSY),
            LoopState::Rundown | LoopState::Draining | LoopState::Deleting => {
                return Err(SystemError::ENODEV);
            }
        }

        inner.set_state(LoopState::Bound)?;
        Self::set_file_locked(&mut inner, backing_file, total_size);
        inner.flags = if read_only {
            LoopFlags::READ_ONLY
        } else {
            LoopFlags::empty()
        };
        Ok(())
    }

    pub fn clear_file(&self) -> Result<(), SystemError> {
        let (epoch, generation) = {
            let _topology = LOOP_BACKING_TOPOLOGY_LOCK.lock();
            let _config = self.config_mutex.lock();
            let mut inner = self.inner();
            match inner.state() {
                LoopState::Unbound => return Ok(()),
                LoopState::Bound => {
                    // The ioctl file itself owns one open description. Any
                    // additional open description includes nested-loop backing
                    // Files; mounted filesystems are tracked separately.
                    if self.open_count.load(Ordering::Acquire) > 1
                        || self.mount_holder_count.load(Ordering::Acquire) != 0
                    {
                        return Err(SystemError::EBUSY);
                    }
                    inner.set_state(LoopState::Draining)?;
                    Self::install_quiesce_owner(&mut inner, LoopQuiesceOwner::Clear)
                }
                LoopState::Rundown | LoopState::Draining | LoopState::Deleting => {
                    return Err(SystemError::EBUSY);
                }
            }
        };

        if let Err(error) = self.wait_for_active_io() {
            self.rollback_bound_quiesce(epoch, generation, LoopQuiesceOwner::Clear);
            return Err(error);
        }

        let topology = LOOP_BACKING_TOPOLOGY_LOCK.lock();
        let config = self.config_mutex.lock();
        let old_file = {
            let mut inner = self.inner();
            if !Self::quiesce_still_owned(&inner, epoch, generation, LoopQuiesceOwner::Clear) {
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }
            let old = inner.backing_file.take();
            inner.file_size = 0;
            inner.offset = 0;
            inner.size_limit = 0;
            inner.flags = LoopFlags::empty();
            inner.generation = inner.generation.wrapping_add(1);
            inner.quiesce_owner = None;
            inner.set_state(LoopState::Unbound)?;
            old
        };

        drop(config);
        drop(topology);
        drop(old_file);
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
        if LoopFlags::from_bits(info.lo_flags).is_none() {
            return Err(SystemError::EINVAL);
        }
        Ok(())
    }

    fn validate_loop_status_params(info: &LoopStatus) -> Result<(), SystemError> {
        // legacy loop_info 只有 32-bit offset
        if info.lo_offset < 0 {
            return Err(SystemError::EINVAL);
        }
        if !(info.lo_offset as u64).is_multiple_of(LBA_SIZE as u64) {
            return Err(SystemError::EINVAL);
        }

        // legacy 的 lo_flags 是 int，这里只支持 LoopFlags 已实现的位
        if info.lo_flags < 0 {
            return Err(SystemError::EINVAL);
        }
        let flags_u32 = info.lo_flags as u32;
        if LoopFlags::from_bits(flags_u32).is_none() {
            return Err(SystemError::EINVAL);
        }
        Ok(())
    }

    /// Publish an offset/size-limit/capacity change only after all I/O using
    /// the previous mapping has completed.
    ///
    /// Linux surrounds this transition with sync_blockdev/invalidate_bdev and
    /// a queue freeze which waits, rather than failing new requests. DragonOS
    /// currently has neither a blocking block-queue freeze nor a unified way
    /// to quiesce FAT/ext4 page, metadata, and allocation caches. Consequently
    /// a mounted mapping cannot be changed safely and is rejected with EBUSY.
    /// For unmounted/raw users, the Draining interval waits for every request
    /// using the old translated range before publishing the new mapping.
    fn reconfigure_mapping(
        &self,
        requested_offset: Option<usize>,
        requested_limit: Option<usize>,
    ) -> Result<(), SystemError> {
        const MAX_RETRY: usize = 16;

        for _ in 0..MAX_RETRY {
            let (backing_file, old_offset, old_limit, old_size, generation) = {
                let inner = self.inner();
                if !matches!(inner.state(), LoopState::Bound) {
                    return Err(SystemError::ENXIO);
                }
                (
                    inner.backing_file.clone().ok_or(SystemError::ENODEV)?,
                    inner.offset,
                    inner.size_limit,
                    inner.file_size,
                    inner.generation,
                )
            };
            let new_offset = requested_offset.unwrap_or(old_offset);
            let new_limit = requested_limit.unwrap_or(old_limit);
            let effective = Self::compute_effective_size(&backing_file, new_offset, new_limit)?;

            let owner = {
                let _topology = LOOP_BACKING_TOPOLOGY_LOCK.lock();
                let _config = self.config_mutex.lock();
                let mut inner = self.inner();
                let unchanged_snapshot = inner
                    .backing_file
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &backing_file))
                    && inner.generation == generation
                    && matches!(inner.state(), LoopState::Bound);
                if !unchanged_snapshot {
                    continue;
                }
                if inner.offset == new_offset
                    && inner.size_limit == new_limit
                    && inner.file_size == effective
                {
                    return Ok(());
                }
                if self.mount_holder_count.load(Ordering::Acquire) != 0 {
                    return Err(SystemError::EBUSY);
                }
                inner.set_state(LoopState::Draining)?;
                Self::install_quiesce_owner(&mut inner, LoopQuiesceOwner::Reconfigure)
            };

            if let Err(error) = self.wait_for_active_io() {
                self.rollback_bound_quiesce(owner.0, owner.1, LoopQuiesceOwner::Reconfigure);
                return Err(error);
            }

            let _topology = LOOP_BACKING_TOPOLOGY_LOCK.lock();
            let _config = self.config_mutex.lock();
            let mut inner = self.inner();
            if !Self::quiesce_still_owned(&inner, owner.0, owner.1, LoopQuiesceOwner::Reconfigure) {
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }
            debug_assert_eq!(inner.offset, old_offset);
            debug_assert_eq!(inner.size_limit, old_limit);
            debug_assert_eq!(inner.file_size, old_size);
            inner.offset = new_offset;
            inner.size_limit = new_limit;
            inner.file_size = effective;
            // READ_ONLY is fixed at bind time. DragonOS currently implements
            // no runtime-settable loop flags.
            inner.flags &= LoopFlags::READ_ONLY;
            inner.generation = inner.generation.wrapping_add(1);
            inner.quiesce_owner = None;
            inner.set_state(LoopState::Bound)?;
            return Ok(());
        }

        Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
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
        let info: LoopStatus64 = reader.buffer_protected(0)?.read_one(0)?;
        Self::validate_loop_status64_params(&info)?;

        let new_offset = info.lo_offset as usize;
        let new_limit = if info.lo_sizelimit == 0 {
            0
        } else {
            info.lo_sizelimit as usize
        };

        self.reconfigure_mapping(Some(new_offset), Some(new_limit))
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
    fn get_status64(&self, user_ptr: usize) -> Result<(), SystemError> {
        if user_ptr == 0 {
            return Err(SystemError::EINVAL);
        }

        let info = {
            let inner = self.inner();
            if !matches!(inner.state(), LoopState::Bound | LoopState::Rundown) {
                return Err(SystemError::ENXIO);
            }
            // Linux ABI: 对应 uapi `struct loop_info64`（字段顺序/大小必须匹配）
            // 目前 DragonOS 仅维护 offset/sizelimit/flags 等核心字段，其它字段置 0。
            LoopStatus64 {
                lo_offset: inner.offset as u64,
                lo_sizelimit: inner.size_limit as u64,
                lo_flags: inner.flags.bits(),
                lo_number: self.minor,
                ..LoopStatus64::default()
            }
        };

        let mut writer = UserBufferWriter::new::<LoopStatus64>(
            user_ptr as *mut LoopStatus64,
            core::mem::size_of::<LoopStatus64>(),
            true,
        )?;
        writer.buffer_protected(0)?.write_one(0, &info)?;
        Ok(())
    }

    fn set_status(&self, user_ptr: usize) -> Result<(), SystemError> {
        if user_ptr == 0 {
            return Err(SystemError::EINVAL);
        }

        let reader = UserBufferReader::new::<LoopStatus>(
            user_ptr as *const LoopStatus,
            core::mem::size_of::<LoopStatus>(),
            true,
        )?;
        let info: LoopStatus = reader.buffer_protected(0)?.read_one(0)?;
        Self::validate_loop_status_params(&info)?;

        let new_offset = info.lo_offset as usize;
        // legacy loop_info does not carry sizelimit.
        self.reconfigure_mapping(Some(new_offset), None)
    }

    fn get_status(&self, user_ptr: usize) -> Result<(), SystemError> {
        if user_ptr == 0 {
            return Err(SystemError::EINVAL);
        }

        let info = {
            let inner = self.inner();
            if !matches!(inner.state(), LoopState::Bound | LoopState::Rundown) {
                return Err(SystemError::ENXIO);
            }

            // legacy loop_info：只保证 offset/flags 等关键字段正确，其它字段目前置 0。
            // 这仍然能满足绝大多数使用 LOOP_GET_STATUS 的用户态程序。
            LoopStatus {
                lo_number: self.minor as i32,
                lo_offset: inner.offset as i32,
                lo_flags: inner.flags.bits() as i32,
                ..LoopStatus::default()
            }
        };

        let mut writer = UserBufferWriter::new::<LoopStatus>(
            user_ptr as *mut LoopStatus,
            core::mem::size_of::<LoopStatus>(),
            true,
        )?;
        writer.buffer_protected(0)?.write_one(0, &info)?;
        Ok(())
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
        let new_file = {
            let guard = fd_table.read();
            guard.get_file_by_fd(new_file_fd)
        }
        .ok_or(SystemError::EBADF)?;

        let inode = new_file.inode();
        let metadata = inode.metadata()?;
        match metadata.file_type {
            FileType::File | FileType::BlockDevice => {}
            _ => return Err(SystemError::EINVAL),
        }

        if metadata.size < 0 {
            return Err(SystemError::EINVAL);
        }
        let total_size = metadata.size as usize;

        let (epoch, generation) = {
            let _topology = LOOP_BACKING_TOPOLOGY_LOCK.lock();
            let _config = self.config_mutex.lock();
            self.validate_backing_chain(&new_file)?;
            let mut inner = self.inner();
            if !matches!(inner.state(), LoopState::Bound) {
                return Err(SystemError::ENODEV);
            }
            if !inner.flags.contains(LoopFlags::READ_ONLY) {
                return Err(SystemError::EINVAL);
            }
            if inner.backing_file.is_none() {
                return Err(SystemError::ENODEV);
            }

            let effective = Self::calc_effective_size(total_size, inner.offset, inner.size_limit)?;
            if effective != inner.file_size {
                return Err(SystemError::EINVAL);
            }

            inner.set_state(LoopState::Draining)?;
            Self::install_quiesce_owner(&mut inner, LoopQuiesceOwner::Change)
        };

        if let Err(error) = self.wait_for_active_io() {
            self.rollback_bound_quiesce(epoch, generation, LoopQuiesceOwner::Change);
            return Err(error);
        }

        let topology = LOOP_BACKING_TOPOLOGY_LOCK.lock();
        let config = self.config_mutex.lock();
        if let Err(error) = self.validate_backing_chain(&new_file) {
            drop(config);
            drop(topology);
            self.rollback_bound_quiesce(epoch, generation, LoopQuiesceOwner::Change);
            return Err(error);
        }
        let old_file = {
            let mut inner = self.inner();
            if !Self::quiesce_still_owned(&inner, epoch, generation, LoopQuiesceOwner::Change) {
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }
            let old = inner.backing_file.take();
            Self::change_file_locked(&mut inner, new_file, total_size)?;
            inner.quiesce_owner = None;
            inner.set_state(LoopState::Bound)?;
            old
        };

        drop(config);
        drop(topology);
        drop(old_file);
        Ok(())
    }

    fn set_capacity(&self, _arg: usize) -> Result<(), SystemError> {
        self.reconfigure_mapping(None, None)
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
        if !matches!(inner.state(), LoopState::Bound) {
            return Err(SystemError::ENODEV);
        }
        self.active_io_count.fetch_add(1, Ordering::AcqRel);
        // 通过显式 drop 延长锁守卫的生命周期，避免 NLL 提前释放导致 TOCTOU 竞态
        drop(inner);
        Ok(())
    }

    /// # 功能
    ///
    /// I/O 操作完成时调用，减少活跃 I/O 计数
    fn io_end(&self) {
        let prev = self.active_io_count.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(prev > 0, "Loop device I/O count underflow");
    }

    pub(super) fn prepare_delete(&self) -> Result<(), SystemError> {
        let _topology = LOOP_BACKING_TOPOLOGY_LOCK.lock();
        let _config = self.config_mutex.lock();
        let mut inner = self.inner();
        if matches!(inner.state(), LoopState::Deleting) {
            // BlockDevManager unregister can fail after the state is published;
            // the serialized LOOP_CTL_REMOVE path must be able to retry it.
            return Ok(());
        }
        if !matches!(inner.state(), LoopState::Unbound)
            || inner.backing_file.is_some()
            || self.open_count.load(Ordering::Acquire) != 0
            || self.mount_holder_count.load(Ordering::Acquire) != 0
        {
            return Err(SystemError::EBUSY);
        }
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
    pub(super) fn remove_from_sysfs(&self) -> Result<(), SystemError> {
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
        let _topology = LOOP_BACKING_TOPOLOGY_LOCK.lock();
        let _config = self.config_mutex.lock();
        let backing_file = {
            let mut inner = self.inner();
            let backing_file = inner.backing_file.take();
            inner.file_size = 0;
            inner.offset = 0;
            inner.size_limit = 0;
            backing_file
        };
        drop(_config);
        drop(_topology);
        if backing_file.is_some() {
            warn!(
                "Backing file was still present during final cleanup for loop{}",
                self.minor()
            );
        }
        drop(backing_file);
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
        self.dev_name().to_string()
    }

    fn set_name(&self, _name: String) {
        // do nothing, loop 设备名称由 devname 字段决定，不支持外部设置
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

impl IndexNode for LoopDevice {
    fn fs(&self) -> Arc<dyn crate::filesystem::vfs::FileSystem> {
        // 设备通常通过 DevFS 的包装 inode 访问；这里返回其所在的文件系统。
        // 优先使用 devfs 注册时注入的 Weak<DevFS>，避免在正常路径上做路径查找。
        if let Some(fs) = self.fs.read().upgrade() {
            return fs;
        }
        // 兜底：从当前挂载命名空间中找到 /dev 并取其 fs。
        // 该路径在系统正常初始化后应始终存在。
        ProcessManager::current_mntns()
            .root_inode()
            .find("dev")
            .expect("LoopDevice: DevFS not mounted at /dev")
            .fs()
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn sync_file(
        &self,
        _datasync: bool,
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<(), SystemError> {
        <Self as BlockDevice>::sync(self)
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: MutexGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<usize, SystemError> {
        if len > buf.len() {
            return Err(SystemError::ENOBUFS);
        }
        self.read_bytes_operation(offset, len, buf)
    }

    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        _data: MutexGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<usize, SystemError> {
        if len > buf.len() {
            return Err(SystemError::E2BIG);
        }
        self.write_bytes_operation(offset, len, &buf[..len], WriteSyncIntent::None)
            .map(|result| result.written_len)
    }

    fn list(&self) -> Result<alloc::vec::Vec<alloc::string::String>, system_error::SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn metadata(&self) -> Result<crate::filesystem::vfs::Metadata, SystemError> {
        let (backing_file, file_size, devnum) = {
            let inner = self.inner();
            let backing_file = inner.backing_file.clone().ok_or(SystemError::EPERM)?;
            (backing_file, inner.file_size, inner.device_number)
        };

        let file_metadata = backing_file.inode().metadata()?;
        let metadata = Metadata {
            dev_id: 0,
            inode_id: InodeId::new(0),
            size: file_size as i64,
            blk_size: LBA_SIZE,
            blocks: file_size.div_ceil(LBA_SIZE),
            atime: file_metadata.atime,
            mtime: file_metadata.mtime,
            ctime: file_metadata.ctime,
            btime: file_metadata.btime,
            file_type: FileType::BlockDevice,
            mode: InodeMode::from_bits_truncate(0o644),
            flags: InodeFlags::empty(),
            nlinks: 1,
            uid: 0,
            gid: 0,
            raw_dev: devnum,
        };
        Ok(metadata)
    }

    fn ioctl(
        &self,
        cmd: u32,
        data: usize,
        private_data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let ioctl_cmd = LoopIoctl::from_u32(cmd).ok_or(SystemError::ENOSYS)?;
        let control_writable = matches!(
            &*private_data,
            FilePrivateData::Loop(data) if data.control_writable
        );
        let may_reconfigure = control_writable || capable(CAPFlags::CAP_SYS_ADMIN);
        drop(private_data);

        match ioctl_cmd {
            LoopIoctl::LoopSetFd => {
                let file_fd = data as i32;
                let fd_table = ProcessManager::current_pcb().fd_table();
                let file = {
                    let guard = fd_table.read();
                    guard.get_file_by_fd(file_fd)
                }
                .ok_or(SystemError::EBADF)?;
                let inode = file.inode();
                let metadata = inode.metadata()?;
                match metadata.file_type {
                    FileType::File | FileType::BlockDevice => {}
                    _ => return Err(SystemError::EINVAL),
                }

                let read_only = file.flags().is_read_only() || !control_writable;
                self.bind_file(file, read_only)?;
                Ok(0)
            }
            LoopIoctl::LoopClrFd => {
                if !may_reconfigure {
                    return Err(SystemError::EPERM);
                }
                self.clear_file()?;
                Ok(0)
            }
            LoopIoctl::LoopSetStatus => {
                if !may_reconfigure {
                    return Err(SystemError::EPERM);
                }
                self.set_status(data)?;
                Ok(0)
            }
            LoopIoctl::LoopGetStatus => {
                self.get_status(data)?;
                Ok(0)
            }
            LoopIoctl::LoopSetStatus64 => {
                if !may_reconfigure {
                    return Err(SystemError::EPERM);
                }
                self.set_status64(data)?;
                Ok(0)
            }
            LoopIoctl::LoopGetStatus64 => {
                self.get_status64(data)?;
                Ok(0)
            }
            LoopIoctl::LoopChangeFd => {
                if !may_reconfigure {
                    return Err(SystemError::EPERM);
                }
                self.change_fd(data as i32)?;
                Ok(0)
            }
            LoopIoctl::LoopSetCapacity => {
                if !may_reconfigure {
                    return Err(SystemError::EPERM);
                }
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

impl LoopDevice {
    fn io_snapshot(&self, write: bool) -> Result<(Arc<File>, usize, usize), SystemError> {
        let inner = self.inner();
        if write && inner.is_read_only() {
            return Err(SystemError::EROFS);
        }
        let backing_file = inner.backing_file.clone().ok_or(SystemError::ENODEV)?;
        let limit_end = inner
            .offset
            .checked_add(inner.file_size)
            .ok_or(SystemError::EOVERFLOW)?;
        Ok((backing_file, inner.offset, limit_end))
    }

    fn translate_io_range(
        base_offset: usize,
        limit_end: usize,
        offset: usize,
        len: usize,
    ) -> Result<usize, SystemError> {
        let file_offset = base_offset
            .checked_add(offset)
            .ok_or(SystemError::EOVERFLOW)?;
        let end = file_offset.checked_add(len).ok_or(SystemError::EOVERFLOW)?;
        if end > limit_end {
            return Err(SystemError::ENOSPC);
        }
        Ok(file_offset)
    }

    fn sync_backing_file(
        backing_file: &Arc<File>,
        start: usize,
        end: usize,
        datasync: bool,
    ) -> Result<(), SystemError> {
        Self::normalize_backing_sync_result(
            backing_file.sync_range_and_check_wb_error(start, end, datasync),
        )
    }

    fn normalize_backing_sync_result(result: Result<(), SystemError>) -> Result<(), SystemError> {
        match result {
            Ok(()) | Err(SystemError::EINVAL) => Ok(()),
            Err(_) => Err(SystemError::EIO),
        }
    }

    fn read_bytes_operation(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
    ) -> Result<usize, SystemError> {
        if len > buf.len() {
            return Err(SystemError::EINVAL);
        }
        if len == 0 {
            return Ok(0);
        }

        let _io_guard = IoGuard::new(self)?;
        let (backing_file, base_offset, limit_end) = self.io_snapshot(false)?;
        let file_offset = Self::translate_io_range(base_offset, limit_end, offset, len)?;
        let read = backing_file.pread(file_offset, len, &mut buf[..len])?;
        if read < len {
            // Linux loop zero-fills a short backing read inside the advertised
            // device capacity rather than exposing stale destination bytes.
            buf[read..len].fill(0);
            return Ok(len);
        }
        Ok(read)
    }

    fn write_bytes_operation(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        sync_intent: WriteSyncIntent,
    ) -> Result<DelegatedWriteResult, SystemError> {
        if len > buf.len() {
            return Err(SystemError::EINVAL);
        }
        if len == 0 {
            return Ok(DelegatedWriteResult {
                written_len: 0,
                sync_result: Ok(()),
            });
        }

        // This guard and retained File snapshot cover both write and optional
        // flush, so CLEAR/DELETE cannot release the file between them.
        let _io_guard = IoGuard::new(self)?;
        let (backing_file, base_offset, limit_end) = self.io_snapshot(true)?;
        let file_offset = Self::translate_io_range(base_offset, limit_end, offset, len)?;
        // Delegate the operation-level intent to the retained File itself.
        // This merges it with backing O_SYNC/S_SYNC/SB_SYNCHRONOUS exactly
        // once and preserves positive write progress if the following sync
        // fails. Calling plain pwrite() here would collapse that failure into
        // Err and make the outer loop file lose the completed byte count.
        let mut result =
            backing_file.pwrite_with_sync_intent(file_offset, len, &buf[..len], sync_intent)?;
        result.sync_result = Self::normalize_backing_sync_result(result.sync_result);
        Ok(result)
    }
}

impl BlockDevice for LoopDevice {
    fn devfs_mode(&self) -> InodeMode {
        InodeMode::from_bits_truncate(0o600)
    }

    fn file_open(
        &self,
        mut data: MutexGuard<FilePrivateData>,
        flags: &FileFlags,
    ) -> Result<(), SystemError> {
        let _config = self.config_mutex.lock();
        if matches!(
            self.inner().state(),
            LoopState::Deleting | LoopState::Draining
        ) {
            return Err(SystemError::ENODEV);
        }
        let device_pin = self.self_ref.upgrade().ok_or(SystemError::ENODEV)?;
        self.open_count.fetch_add(1, Ordering::AcqRel);
        *data = FilePrivateData::Loop(LoopPrivateData {
            control_writable: !flags.is_read_only(),
            open_counted: true,
            _device_pin: Some(device_pin),
        });
        Ok(())
    }

    fn file_close(&self, mut data: MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        if let FilePrivateData::Loop(loop_data) = &mut *data {
            if loop_data.open_counted {
                loop_data.open_counted = false;
                let previous = self.open_count.fetch_sub(1, Ordering::AcqRel);
                debug_assert!(previous > 0, "loop open count underflow");
            }
        }
        Ok(())
    }

    fn mount_holder_acquire(&self) -> Result<(), SystemError> {
        let _config = self.config_mutex.lock();
        if !matches!(self.inner().state(), LoopState::Bound) {
            return Err(SystemError::ENODEV);
        }
        self.mount_holder_count.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    fn mount_holder_release(&self) {
        let previous = self.mount_holder_count.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "loop mount holder count underflow");
    }

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
        let len = count.checked_mul(LBA_SIZE).ok_or(SystemError::EOVERFLOW)?;
        let block_offset = lba_id_start
            .checked_mul(LBA_SIZE)
            .ok_or(SystemError::EOVERFLOW)?;
        self.read_bytes_operation(block_offset, len, buf)
    }

    fn write_at_sync(
        &self,
        lba_id_start: BlockId,
        count: usize,
        buf: &[u8],
    ) -> Result<usize, SystemError> {
        let len = count.checked_mul(LBA_SIZE).ok_or(SystemError::EOVERFLOW)?;
        let block_offset = lba_id_start
            .checked_mul(LBA_SIZE)
            .ok_or(SystemError::EOVERFLOW)?;
        self.write_bytes_operation(block_offset, len, buf, WriteSyncIntent::None)
            .map(|result| result.written_len)
    }

    fn read_at_bytes(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
    ) -> Result<usize, SystemError> {
        self.read_bytes_operation(offset, len, buf)
    }

    fn uses_delegated_write_sync(&self) -> bool {
        true
    }

    fn write_at_bytes_with_sync(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        sync_intent: WriteSyncIntent,
    ) -> Result<DelegatedWriteResult, SystemError> {
        self.write_bytes_operation(offset, len, buf, sync_intent)
    }

    fn sync(&self) -> Result<(), SystemError> {
        let _io_guard = IoGuard::new(self)?;
        let backing_file = self
            .inner()
            .backing_file
            .clone()
            .ok_or(SystemError::ENODEV)?;
        Self::sync_backing_file(&backing_file, 0, usize::MAX, false)
    }

    fn supports_reliable_flush(&self) -> bool {
        let backing_file = self.inner().backing_file.clone();
        backing_file
            .map(|file| file.inode().fs().supports_reliable_flush())
            .unwrap_or(false)
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
