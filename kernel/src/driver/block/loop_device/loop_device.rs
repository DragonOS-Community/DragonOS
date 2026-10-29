use crate::mm::allocation::MemoryAllocationError;
use crate::{
    driver::base::{
        block::{
            block_device::{
                BlockDevice, BlockDeviceIoLimits, BlockId, GeneralBlockRange, LBA_SIZE,
            },
            disk_info::Partition,
            gendisk::GenDisk,
            manager::{block_dev_manager, BlockDevMeta},
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
        rwsem::{RwSem, RwSemReadGuard, RwSemWriteGuard},
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
    LoopConfig, LoopFlags, LoopIoctl, LoopState, LoopStatus, LoopStatus64, LOOP_BASENAME,
    LOOP_IO_DRAIN_CHECK_INTERVAL_US, LOOP_IO_DRAIN_TIMEOUT_MS,
};
use super::mode::{
    configured_logical_block_size, effective_dio, sync_before_mode_change,
    validate_logical_block_size,
};

/// Serializes loop backing topology validation and publication.
///
/// The fixed lock order is:
/// `LOOP_BACKING_TOPOLOGY_LOCK -> LoopDevice::config_mutex -> inner`.
pub(super) static LOOP_BACKING_TOPOLOGY_LOCK: Mutex<()> = Mutex::new(());

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
    pub(super) config_mutex: Mutex<()>,
    /// Serializes sector RMW and freezes actual I/O during mode changes.
    pub(super) io_gate: RwSem<()>,
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
    /// Queue logical geometry; all internal BlockIds remain 512-byte sectors.
    pub(super) logical_block_size: u32,
    pub flags: LoopFlags,
    pub kobject_common: KObjectCommonData,
    pub device_common: DeviceCommonData,
    /// Incremented whenever a backing/configuration generation is published.
    pub(super) generation: u64,
    /// User-provided status name, terminated at byte 63.
    pub file_name: [u8; 64],
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
        let mut effective = total_size.saturating_sub(offset);
        if size_limit > 0 {
            effective = effective.min(size_limit);
        }
        // The mapping parameters are byte based, but a block device only
        // publishes complete 512-byte sectors. Use this same capacity for
        // metadata, generic queries and the retained I/O snapshot.
        Ok(effective / LBA_SIZE * LBA_SIZE)
    }

    fn set_file_locked(inner: &mut LoopDeviceInner, backing_file: Arc<File>, file_size: usize) {
        inner.backing_file = Some(backing_file);
        inner.file_size = file_size;
        inner.offset = 0;
        inner.size_limit = 0;
        inner.file_name = [0; 64];
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

    pub(super) fn has_mount_holders(&self) -> bool {
        self.mount_holder_count.load(Ordering::Acquire) != 0
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
                logical_block_size: LBA_SIZE as u32,
                flags: LoopFlags::empty(),
                kobject_common: KObjectCommonData::default(),
                device_common: DeviceCommonData::default(),
                state: LoopState::Unbound,
                generation: 0,
                file_name: [0; 64],
                quiesce_epoch: 0,
                quiesce_owner: None,
            }),
            block_dev_meta: BlockDevMeta::new(devname, Major::LOOP_MAJOR),
            locked_kobj_state: LockedKObjectState::default(),
            self_ref: self_ref.clone(),
            fs: RwLock::new(Weak::default()),
            parent: RwLock::new(Weak::default()),
            config_mutex: Mutex::new(()),
            io_gate: RwSem::new(()),
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

    fn backing_file_from_fd(fd: i32) -> Result<Arc<File>, SystemError> {
        let file = {
            let table = ProcessManager::current_pcb().fd_table();
            let guard = table.read();
            guard.get_file_by_fd(fd)
        }
        .ok_or(SystemError::EBADF)?;
        // Linux fget() excludes FMODE_PATH before owner/state admission.
        if file.flags().contains(FileFlags::O_PATH) {
            return Err(SystemError::EBADF);
        }
        Ok(file)
    }

    fn validate_candidate_type(file: &Arc<File>) -> Result<(), SystemError> {
        if !matches!(file.file_type(), FileType::File | FileType::BlockDevice) {
            return Err(SystemError::EINVAL);
        }
        Ok(())
    }

    fn candidate_size(file: &Arc<File>) -> Result<usize, SystemError> {
        Self::validate_candidate_type(file)?;
        let metadata = file.inode().metadata()?;
        if !matches!(metadata.file_type, FileType::File | FileType::BlockDevice)
            || metadata.size < 0
        {
            return Err(SystemError::EINVAL);
        }
        Ok(metadata.size as usize)
    }

    fn admit_bind_locked(&self) -> Result<(), SystemError> {
        if self.inner().state() != LoopState::Unbound {
            return Err(SystemError::EBUSY);
        }
        Ok(())
    }

    /// Called with the device's raw-cache gate exclusive and while Bound.
    /// Terminal block errors have retired their dirty incarnation and remain
    /// in errseq; resources still Dirty or pending payload cannot cross a
    /// mapping generation, even if the first reported error was different.
    pub(super) fn prepare_raw_mapping_change_locked(&self) -> Result<(), SystemError> {
        let manager = block_dev_manager();
        let nodes = manager.snapshot_raw_nodes_locked(self)?;
        let mut error = None;
        for node in &nodes {
            if let Err(failure) = node.sync_raw_cache_locked() {
                error.get_or_insert(failure);
            }
        }
        if nodes
            .iter()
            .any(|node| node.has_raw_writeback_locked() || node.has_dirty_raw_cache_locked())
        {
            return Err(error.unwrap_or(SystemError::EBUSY));
        }
        let mut first_error = None;
        for node in &nodes {
            if let Err(error) = node.invalidate_raw_cache_locked() {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    fn rescan_partitions_locked(&self) {
        let Some(device) = self.self_ref.upgrade() else {
            warn!("loop{}: partition scan lost its device owner", self.minor);
            return;
        };
        let device: Arc<dyn BlockDevice> = device;
        if let Err(error) = block_dev_manager().scan_partitions_locked(&device) {
            // Linux commits the configuration first; partition scanning is
            // best effort and cannot roll that published mapping back.
            warn!("loop{}: partition scan failed: {:?}", self.minor, error);
        }
    }

    pub fn bind_file(&self, backing_file: Arc<File>, read_only: bool) -> Result<(), SystemError> {
        self.configure_file(backing_file, read_only, &LoopConfig::default())
    }

    fn configure_file(
        &self,
        backing_file: Arc<File>,
        forced_read_only: bool,
        config: &LoopConfig,
    ) -> Result<(), SystemError> {
        let _noio = crate::cgroup::memory::io::MemoryIoScope::enter_noio();
        let _raw = self.block_dev_meta.raw_cache_gate.write();
        self.admit_bind_locked()?;
        Self::validate_candidate_type(&backing_file)?;
        {
            let _topology = LOOP_BACKING_TOPOLOGY_LOCK.lock();
            let _config = self.config_mutex.lock();
            self.admit_bind_locked()?;
            self.validate_backing_chain(&backing_file)?;
        }
        // Unknown configure flags are not accepted (unlike status updates).
        if config.info.lo_flags & !LoopFlags::all().bits() != 0 {
            return Err(SystemError::EINVAL);
        }
        if config.block_size != 0 {
            validate_logical_block_size(config.block_size as usize)?;
        }
        Self::validate_loop_status64_params(&config.info)?;
        let total_size = Self::candidate_size(&backing_file)?;
        let logical = configured_logical_block_size(&backing_file, config.block_size)?;
        let offset = config.info.lo_offset as usize;
        let limit = config.info.lo_sizelimit as usize;
        let file_size = Self::calc_effective_size(total_size, offset, limit)?;
        let use_dio = effective_dio(
            &backing_file,
            offset,
            logical,
            backing_file.flags().contains(FileFlags::O_DIRECT)
                || config.info.lo_flags & LoopFlags::DIRECT_IO.bits() != 0,
        )?;
        if use_dio {
            sync_before_mode_change(&backing_file);
        }
        let partscan = config.info.lo_flags & LoopFlags::PARTSCAN.bits() != 0;
        {
            let _topology = LOOP_BACKING_TOPOLOGY_LOCK.lock();
            let _config = self.config_mutex.lock();
            self.admit_bind_locked()?;
            self.validate_backing_chain(&backing_file)?;
            let mut inner = self.inner();
            inner.set_state(LoopState::Bound)?;
            Self::set_file_locked(&mut inner, backing_file, file_size);
            inner.offset = offset;
            inner.size_limit = limit;
            inner.logical_block_size = logical;
            inner.flags = LoopFlags::from_bits_truncate(config.info.lo_flags);
            inner.flags.set(
                LoopFlags::READ_ONLY,
                forced_read_only || config.info.lo_flags & LoopFlags::READ_ONLY.bits() != 0,
            );
            inner.flags.set(LoopFlags::DIRECT_IO, use_dio);
            inner.file_name = config.info.lo_file_name;
            inner.file_name[63] = 0;
        }
        if partscan {
            self.rescan_partitions_locked();
        }
        Ok(())
    }

    pub fn clear_file(&self) -> Result<(), SystemError> {
        self.clear_binding(false)
    }

    /// Frontend exclusion precedes all configuration locks. Flush the old
    /// generation while Bound, then drain real payload and detach atomically.
    fn clear_binding(&self, release: bool) -> Result<(), SystemError> {
        let _noio = crate::cgroup::memory::io::MemoryIoScope::enter_noio();
        let _raw = self.block_dev_meta.raw_cache_gate.write();
        let generation = {
            let _topology = LOOP_BACKING_TOPOLOGY_LOCK.lock();
            let _config = self.config_mutex.lock();
            let mut inner = self.inner();
            if release
                && (inner.state() != LoopState::Bound
                    || !inner.flags.contains(LoopFlags::AUTOCLEAR)
                    || self.open_count.load(Ordering::Acquire) != 0
                    || self.mount_holder_count.load(Ordering::Acquire) != 0)
            {
                return Ok(());
            }
            match inner.state() {
                LoopState::Unbound => return Err(SystemError::ENXIO),
                LoopState::Bound => {}
                _ => return Err(SystemError::EBUSY),
            }
            if self.open_count.load(Ordering::Acquire) > u32::from(!release)
                || self.mount_holder_count.load(Ordering::Acquire) != 0
            {
                if !inner.flags.contains(LoopFlags::AUTOCLEAR) {
                    inner.flags.insert(LoopFlags::AUTOCLEAR);
                    inner.generation = inner.generation.wrapping_add(1);
                }
                return Ok(());
            }
            inner.generation
        };
        let manager = block_dev_manager();
        // Keep removed partition nodes alive until all their old cache pages
        // and error generations are retired, not just the surviving part0.
        let nodes = manager.snapshot_raw_nodes_locked(self)?;
        let mut error = None;
        for node in &nodes {
            if let Err(failure) = node.sync_raw_cache_locked() {
                error.get_or_insert(failure);
            }
        }
        // Final detach is a best-effort flush, as Linux final release is: any
        // remaining dirty pages retire with this binding. A mapping change
        // keeps the device alive and must instead preserve/reject such pages
        // (prepare_raw_mapping_change_locked). Pending payload must always end
        // before either path may release the old backing file.
        if nodes.iter().any(|node| node.has_raw_writeback_locked()) {
            return Err(error.unwrap_or(SystemError::EBUSY));
        }
        let (epoch, generation) = {
            let _topology = LOOP_BACKING_TOPOLOGY_LOCK.lock();
            let _config = self.config_mutex.lock();
            let mut inner = self.inner();
            if inner.state() != LoopState::Bound || inner.generation != generation {
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }
            inner.set_state(LoopState::Draining)?;
            Self::install_quiesce_owner(&mut inner, LoopQuiesceOwner::Clear)
        };
        if let Err(error) = self.wait_for_active_io() {
            self.rollback_bound_quiesce(epoch, generation, LoopQuiesceOwner::Clear);
            return Err(error);
        }
        let Some(device) = self.self_ref.upgrade() else {
            self.rollback_bound_quiesce(epoch, generation, LoopQuiesceOwner::Clear);
            return Err(SystemError::ENODEV);
        };
        let device: Arc<dyn BlockDevice> = device;
        if let Err(error) = manager.remove_partitions_locked(&device) {
            self.rollback_bound_quiesce(epoch, generation, LoopQuiesceOwner::Clear);
            return Err(error);
        }
        for node in &nodes {
            if let Err(error) = node.retire_raw_cache_locked() {
                self.rollback_bound_quiesce(epoch, generation, LoopQuiesceOwner::Clear);
                return Err(error);
            }
        }
        // No fallible namespace or page retirement remains. Reset only this
        // detached binding's errseq, outside all configuration/inner locks.
        for node in &nodes {
            node.reset_raw_cache_writeback_error_locked();
        }
        let old_file = {
            let _topology = LOOP_BACKING_TOPOLOGY_LOCK.lock();
            let _config = self.config_mutex.lock();
            let mut inner = self.inner();
            if !Self::quiesce_still_owned(&inner, epoch, generation, LoopQuiesceOwner::Clear) {
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }
            let old = inner.backing_file.take();
            inner.file_size = 0;
            inner.offset = 0;
            inner.size_limit = 0;
            inner.logical_block_size = LBA_SIZE as u32;
            inner.file_name = [0; 64];
            inner.flags = LoopFlags::empty();
            inner.generation = inner.generation.wrapping_add(1);
            inner.quiesce_owner = None;
            inner.set_state(LoopState::Unbound)?;
            old
        };
        // File release may close a nested block device and enter its owner.
        // The target gate remains exclusive, but no configuration lock does.
        drop(old_file);
        Ok(())
    }

    fn maybe_autoclear(&self) {
        if let Err(error) = self.clear_binding(true) {
            warn!("loop{}: deferred autoclear failed: {:?}", self.minor, error);
        }
    }

    fn validate_loop_status64_params(info: &LoopStatus64) -> Result<(), SystemError> {
        // Linux ignores the legacy crypto fields during normalization, but
        // the wide ABI rejects unsupported transformations and oversized keys.
        if info.lo_encrypt_type != 0 || info.lo_encrypt_key_size > 32 {
            return Err(SystemError::EINVAL);
        }
        if info.lo_offset > i64::MAX as u64
            || info.lo_sizelimit > i64::MAX as u64
            || info.lo_offset > usize::MAX as u64
            || info.lo_sizelimit > usize::MAX as u64
        {
            return Err(SystemError::EOVERFLOW);
        }
        Ok(())
    }

    /// Publish the fields shared by both status ABIs. The caller controls the
    /// mapping generation and has already validated the complete transaction.
    fn update_status_locked(
        inner: &mut LoopDeviceInner,
        status: Option<&LoopStatus64>,
        use_dio: bool,
    ) -> bool {
        let old_flags = inner.flags;
        let old_name = inner.file_name;
        if let Some(status) = status {
            inner.flags.set(
                LoopFlags::AUTOCLEAR,
                status.lo_flags & LoopFlags::AUTOCLEAR.bits() != 0,
            );
            if status.lo_flags & LoopFlags::PARTSCAN.bits() != 0 {
                inner.flags.insert(LoopFlags::PARTSCAN);
            }
            inner.file_name = status.lo_file_name;
            inner.file_name[63] = 0;
        }
        inner.flags.set(LoopFlags::DIRECT_IO, use_dio);
        inner.flags != old_flags || inner.file_name != old_name
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
    fn reconfigure_mapping(&self, status: Option<&LoopStatus64>) -> Result<(), SystemError> {
        let _noio = crate::cgroup::memory::io::MemoryIoScope::enter_noio();
        let _raw = self.block_dev_meta.raw_cache_gate.write();
        for _ in 0..16 {
            let (
                backing_file,
                old_offset,
                old_limit,
                old_size,
                old_dio,
                old_partscan,
                logical,
                generation,
            ) = {
                let inner = self.inner();
                if inner.state() != LoopState::Bound {
                    return Err(SystemError::ENXIO);
                }
                if let Some(status) = status {
                    Self::validate_loop_status64_params(status)?;
                }
                (
                    inner.backing_file.clone().ok_or(SystemError::ENODEV)?,
                    inner.offset,
                    inner.size_limit,
                    inner.file_size,
                    inner.flags.contains(LoopFlags::DIRECT_IO),
                    inner.flags.contains(LoopFlags::PARTSCAN),
                    inner.logical_block_size,
                    inner.generation,
                )
            };
            let new_offset = status.map_or(old_offset, |status| status.lo_offset as usize);
            let new_limit = status.map_or(old_limit, |status| status.lo_sizelimit as usize);
            let use_dio = if status.is_some() {
                effective_dio(&backing_file, new_offset, logical, old_dio)?
            } else {
                old_dio
            };
            let effective =
                if status.is_some() && new_offset == old_offset && new_limit == old_limit {
                    old_size
                } else {
                    Self::compute_effective_size(&backing_file, new_offset, new_limit)?
                };
            let scan = status
                .is_some_and(|status| status.lo_flags & LoopFlags::PARTSCAN.bits() != 0)
                && !old_partscan;
            if new_offset == old_offset
                && new_limit == old_limit
                && effective == old_size
                && use_dio == old_dio
            {
                {
                    let _topology = LOOP_BACKING_TOPOLOGY_LOCK.lock();
                    let _config = self.config_mutex.lock();
                    let mut inner = self.inner();
                    if inner.state() != LoopState::Bound
                        || inner.generation != generation
                        || !inner
                            .backing_file
                            .as_ref()
                            .is_some_and(|file| Arc::ptr_eq(file, &backing_file))
                    {
                        continue;
                    }
                    if Self::update_status_locked(&mut inner, status, use_dio) {
                        inner.generation = inner.generation.wrapping_add(1);
                    }
                }
                // Name/flag-only status does not flush/invalidate raw data.
                // A first PARTSCAN requests its own Linux post-commit scan.
                if scan {
                    self.rescan_partitions_locked();
                }
                return Ok(());
            }
            if self.mount_holder_count.load(Ordering::Acquire) != 0 {
                return Err(SystemError::EBUSY);
            }
            // The cache backend must still see Bound and the old capacity.
            self.prepare_raw_mapping_change_locked()?;
            let owner = {
                let _topology = LOOP_BACKING_TOPOLOGY_LOCK.lock();
                let _config = self.config_mutex.lock();
                let mut inner = self.inner();
                if inner.state() != LoopState::Bound
                    || inner.generation != generation
                    || !inner
                        .backing_file
                        .as_ref()
                        .is_some_and(|file| Arc::ptr_eq(file, &backing_file))
                {
                    continue;
                }
                inner.set_state(LoopState::Draining)?;
                Self::install_quiesce_owner(&mut inner, LoopQuiesceOwner::Reconfigure)
            };
            if let Err(error) = self.wait_for_active_io() {
                self.rollback_bound_quiesce(owner.0, owner.1, LoopQuiesceOwner::Reconfigure);
                return Err(error);
            }
            if use_dio != old_dio {
                sync_before_mode_change(&backing_file);
            }
            {
                let _topology = LOOP_BACKING_TOPOLOGY_LOCK.lock();
                let _config = self.config_mutex.lock();
                let mut inner = self.inner();
                if !Self::quiesce_still_owned(
                    &inner,
                    owner.0,
                    owner.1,
                    LoopQuiesceOwner::Reconfigure,
                ) {
                    return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                }
                inner.offset = new_offset;
                inner.size_limit = new_limit;
                inner.file_size = effective;
                Self::update_status_locked(&mut inner, status, use_dio);
                inner.generation = inner.generation.wrapping_add(1);
                inner.quiesce_owner = None;
                inner.set_state(LoopState::Bound)?;
            }
            if scan {
                self.rescan_partitions_locked();
            }
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
    /// - `Err(SystemError::EINVAL.into())`: 无效的参数或标志位。
    /// - `Err(SystemError::ENXIO.into())`: 设备未绑定或已卸载。
    fn set_status64(&self, user_ptr: usize) -> Result<(), SystemError> {
        let reader = UserBufferReader::new::<LoopStatus64>(
            user_ptr as *const LoopStatus64,
            core::mem::size_of::<LoopStatus64>(),
            true,
        )?;
        let info: LoopStatus64 = reader.buffer_protected(0)?.read_one(0)?;
        self.reconfigure_mapping(Some(&info))
    }

    /// Retain the configuration and its backing File in one generation.
    /// Metadata may block and is queried after releasing the inner lock, from
    /// that retained File rather than a newly observed backing generation.
    fn status_snapshot(&self) -> Result<LoopStatus64, SystemError> {
        let (mut info, backing) = {
            let inner = self.inner();
            if inner.state() != LoopState::Bound {
                return Err(SystemError::ENXIO);
            }
            (
                LoopStatus64 {
                    lo_offset: inner.offset as u64,
                    lo_sizelimit: inner.size_limit as u64,
                    lo_flags: inner.flags.bits(),
                    lo_number: self.minor,
                    lo_file_name: inner.file_name,
                    ..LoopStatus64::default()
                },
                inner.backing_file.clone().ok_or(SystemError::ENODEV)?,
            )
        };
        let metadata = backing.inode().metadata()?;
        info.lo_device = DeviceNumber::from(metadata.dev_id as u32).new_encode_dev() as u64;
        info.lo_inode = metadata.inode_id.data() as u64;
        info.lo_rdevice = metadata.raw_dev.new_encode_dev() as u64;
        Ok(info)
    }

    fn get_status64(&self, user_ptr: usize) -> Result<(), SystemError> {
        if user_ptr == 0 {
            return Err(SystemError::EINVAL);
        }
        let info = self.status_snapshot()?;
        let mut writer = UserBufferWriter::new::<LoopStatus64>(
            user_ptr as *mut LoopStatus64,
            core::mem::size_of::<LoopStatus64>(),
            true,
        )?;
        writer.buffer_protected(0)?.write_one(0, &info)?;
        Ok(())
    }

    fn set_status(&self, user_ptr: usize) -> Result<(), SystemError> {
        let reader = UserBufferReader::new::<LoopStatus>(
            user_ptr as *const LoopStatus,
            core::mem::size_of::<LoopStatus>(),
            true,
        )?;
        let info: LoopStatus = reader.buffer_protected(0)?.read_one(0)?;
        self.reconfigure_mapping(Some(&LoopStatus64::from_legacy(&info)))
    }

    fn get_status(&self, user_ptr: usize) -> Result<(), SystemError> {
        if user_ptr == 0 {
            return Err(SystemError::EINVAL);
        }
        // Checked narrowing precedes all user access, so overflow leaves the
        // output unchanged even if the supplied address would itself fault.
        let info = LoopStatus::try_from(self.status_snapshot()?)?;
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
        let new_file = Self::backing_file_from_fd(new_file_fd)?;
        let _noio = crate::cgroup::memory::io::MemoryIoScope::enter_noio();
        let _raw = self.block_dev_meta.raw_cache_gate.write();
        // Geometry lookup can recurse through a registered backing device.
        // Prepare without configuration/topology locks, then validate the
        // retained mapping generation before claiming the drain transaction.
        for _ in 0..16 {
            let (offset, limit, old_size, old_dio, logical, snapshot_generation) = {
                let inner = self.inner();
                if inner.state() != LoopState::Bound {
                    return Err(SystemError::ENXIO);
                }
                if !inner.is_read_only() {
                    return Err(SystemError::EINVAL);
                }
                if inner.backing_file.is_none() {
                    return Err(SystemError::ENODEV);
                }
                (
                    inner.offset,
                    inner.size_limit,
                    inner.file_size,
                    inner.flags.contains(LoopFlags::DIRECT_IO),
                    inner.logical_block_size,
                    inner.generation,
                )
            };
            Self::validate_candidate_type(&new_file)?;
            {
                let _topology = LOOP_BACKING_TOPOLOGY_LOCK.lock();
                let _config = self.config_mutex.lock();
                {
                    let inner = self.inner();
                    if inner.state() != LoopState::Bound || inner.generation != snapshot_generation
                    {
                        continue;
                    }
                }
                self.validate_backing_chain(&new_file)?;
            }
            let total_size = Self::candidate_size(&new_file)?;
            let effective = Self::calc_effective_size(total_size, offset, limit)?;
            if effective != old_size {
                return Err(SystemError::EINVAL);
            }
            let use_dio = effective_dio(
                &new_file,
                offset,
                logical,
                old_dio || new_file.flags().contains(FileFlags::O_DIRECT),
            )?;
            self.prepare_raw_mapping_change_locked()?;
            let (epoch, generation) = {
                let _topology = LOOP_BACKING_TOPOLOGY_LOCK.lock();
                let _config = self.config_mutex.lock();
                {
                    let inner = self.inner();
                    if inner.state() != LoopState::Bound || inner.generation != snapshot_generation
                    {
                        continue;
                    }
                }
                self.validate_backing_chain(&new_file)?;
                let mut inner = self.inner();
                inner.set_state(LoopState::Draining)?;
                Self::install_quiesce_owner(&mut inner, LoopQuiesceOwner::Change)
            };

            if let Err(error) = self.wait_for_active_io() {
                self.rollback_bound_quiesce(epoch, generation, LoopQuiesceOwner::Change);
                return Err(error);
            }
            if use_dio != old_dio {
                sync_before_mode_change(&new_file);
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
                Self::change_file_locked(&mut inner, new_file.clone(), total_size)?;
                inner.flags.set(LoopFlags::DIRECT_IO, use_dio);
                inner.quiesce_owner = None;
                inner.set_state(LoopState::Bound)?;
                old
            };
            drop(config);
            drop(topology);
            drop(old_file);
            let partscan = self.inner().flags.contains(LoopFlags::PARTSCAN);
            if partscan {
                self.rescan_partitions_locked();
            }
            return Ok(());
        }
        Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
    }

    fn set_capacity(&self, _arg: usize) -> Result<(), SystemError> {
        self.reconfigure_mapping(None)
    }

    /// # 功能
    ///
    /// I/O 操作开始时调用，增加活跃 I/O 计数
    ///
    /// ## 返回值
    /// - `Ok(())`: 成功增加计数
    /// - `Err(SystemError::ENODEV.into())`: 设备正在删除，拒绝新的 I/O
    pub(super) fn io_start(&self) -> Result<bool, SystemError> {
        let inner = self.inner();
        if inner.state() == LoopState::Unbound {
            return Ok(false);
        }
        if !matches!(inner.state(), LoopState::Bound) {
            return Err(SystemError::ENODEV);
        }
        self.active_io_count.fetch_add(1, Ordering::AcqRel);
        // 通过显式 drop 延长锁守卫的生命周期，避免 NLL 提前释放导致 TOCTOU 竞态
        drop(inner);
        Ok(true)
    }

    /// # 功能
    ///
    /// I/O 操作完成时调用，减少活跃 I/O 计数
    pub(super) fn io_end(&self) {
        let prev = self.active_io_count.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(prev > 0, "Loop device I/O count underflow");
    }

    pub(super) fn prepare_delete(&self) -> Result<(), SystemError> {
        let _raw = self.block_dev_meta.raw_cache_gate.write();
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
            inner.logical_block_size = LBA_SIZE as u32;
            inner.file_name = [0; 64];
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
    ) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
        self.try_sync()
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: MutexGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<usize, crate::mm::allocation::MemoryAllocationError> {
        if len > buf.len() {
            return Err(SystemError::ENOBUFS.into());
        }
        self.try_read_bytes_operation(offset, len, buf)
    }

    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        _data: MutexGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<usize, crate::mm::allocation::MemoryAllocationError> {
        if len > buf.len() {
            return Err(SystemError::E2BIG.into());
        }
        self.try_write_bytes_operation(offset, len, &buf[..len], WriteSyncIntent::None)
            .and_then(|result| {
                result.sync_result?;
                Ok(result.written_len)
            })
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
    ) -> Result<usize, crate::mm::allocation::MemoryAllocationError> {
        let result: core::result::Result<_, system_error::SystemError> = (|| {
            let ioctl_cmd = LoopIoctl::from_u32(cmd).ok_or(SystemError::EINVAL)?;
            let control_writable = matches!(
                &*private_data,
                FilePrivateData::Loop(data) if data.control_writable
            );
            let may_reconfigure = control_writable || capable(CAPFlags::CAP_SYS_ADMIN);
            drop(private_data);

            match ioctl_cmd {
                LoopIoctl::LoopSetFd => {
                    let file = Self::backing_file_from_fd(data as i32)?;
                    let read_only = file.flags().is_read_only() || !control_writable;
                    self.bind_file(file, read_only)?;
                    Ok(0)
                }
                LoopIoctl::LoopConfigure => {
                    let reader = UserBufferReader::new::<LoopConfig>(
                        data as *const LoopConfig,
                        core::mem::size_of::<LoopConfig>(),
                        true,
                    )?;
                    let config: LoopConfig = reader.buffer_protected(0)?.read_one(0)?;
                    let file = Self::backing_file_from_fd(config.fd as i32)?;
                    let read_only = file.flags().is_read_only() || !control_writable;
                    self.configure_file(file, read_only, &config)?;
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
                LoopIoctl::LoopSetBlockSize => {
                    if !may_reconfigure {
                        return Err(SystemError::EPERM);
                    }
                    self.set_logical_block_size(data)?;
                    Ok(0)
                }
                LoopIoctl::LoopSetDirectIo => {
                    if !may_reconfigure {
                        return Err(SystemError::EPERM);
                    }
                    self.set_direct_io(data != 0)?;
                    Ok(0)
                }
            }
        })();
        result.map_err(Into::into)
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
        let mut became_last_opener = false;
        if let FilePrivateData::Loop(loop_data) = &mut *data {
            if loop_data.open_counted {
                loop_data.open_counted = false;
                let previous = self.open_count.fetch_sub(1, Ordering::AcqRel);
                debug_assert!(previous > 0, "loop open count underflow");
                became_last_opener = previous == 1;
            }
        }
        drop(data);
        if became_last_opener {
            self.maybe_autoclear();
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
        if previous == 1 {
            self.maybe_autoclear();
        }
    }

    fn dev_name(&self) -> &DevName {
        &self.block_dev_meta.devname
    }

    fn blkdev_meta(&self) -> &BlockDevMeta {
        &self.block_dev_meta
    }

    fn is_read_only(&self) -> bool {
        self.inner().is_read_only()
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
        self.try_read_bytes_operation(block_offset, len, buf)
            .map_err(Self::legacy_io_terminal)
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
        self.try_write_bytes_operation(block_offset, len, buf, WriteSyncIntent::None)
            .and_then(|result| {
                result.sync_result?;
                Ok(result.written_len)
            })
            .map_err(Self::legacy_io_terminal)
    }

    fn read_at_bytes(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
    ) -> Result<usize, SystemError> {
        self.try_read_bytes_operation(offset, len, buf)
            .map_err(Self::legacy_io_terminal)
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
        let mut result = self
            .try_write_bytes_operation(offset, len, buf, sync_intent)
            .map_err(Self::legacy_io_terminal)?;
        // Data already committed: retain its prefix and terminal sync errno;
        // never expose a charge that could replay this legacy request.
        result.sync_result = result
            .sync_result
            .map_err(|error| MemoryAllocationError::System(Self::legacy_io_terminal(error)));
        Ok(result)
    }

    fn sync(&self) -> Result<(), SystemError> {
        self.try_sync().map_err(Self::legacy_io_terminal)
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

    fn io_limits(&self) -> BlockDeviceIoLimits {
        BlockDeviceIoLimits::new(self.inner().logical_block_size)
    }

    fn partitions(&self) -> Vec<Arc<Partition>> {
        Vec::new()
    }
}
