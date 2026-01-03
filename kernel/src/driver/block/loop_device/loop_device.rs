use crate::{
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
        vfs::{FilePrivateData, FileType, IndexNode, InodeFlags, InodeId, InodeMode, Metadata},
    },
    libs::{
        rwlock::{RwLock, RwLockReadGuard, RwLockWriteGuard},
        spinlock::{SpinLock, SpinLockGuard},
    },
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
use log::{error, info, warn};
use num_traits::FromPrimitive;
use system_error::SystemError;

use super::constants::{
    LoopFlags, LoopIoctl, LoopState, LoopStatus, LoopStatus64, LOOP_BASENAME,
    LOOP_IO_DRAIN_CHECK_INTERVAL_US, LOOP_IO_DRAIN_TIMEOUT_MS,
};

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

/// Loop 设备的私有数据（目前未使用）
#[derive(Debug, Clone, Default)]
pub struct LoopPrivateData;

/// Loop 设备内部状态
pub struct LoopDeviceInner {
    pub device_number: DeviceNumber,
    state: LoopState,
    pub file_inode: Option<Arc<dyn IndexNode>>,
    pub file_size: usize,
    pub offset: usize,
    pub size_limit: usize,
    pub flags: LoopFlags,
    pub kobject_common: KObjectCommonData,
    pub device_common: DeviceCommonData,
    /// drain_active_io 重试计数，用于限制无限重试
    drain_retry_count: u32,
}

impl LoopDeviceInner {
    /// 检查状态转换是否有效并执行转换
    ///
    /// 注意：调用者必须持有 LoopDeviceInner 的锁
    pub(super) fn set_state(&mut self, new_state: LoopState) -> Result<(), SystemError> {
        const VALID_TRANSITIONS: &[(LoopState, LoopState)] = &[
            (LoopState::Unbound, LoopState::Bound),
            (LoopState::Bound, LoopState::Unbound),
            (LoopState::Bound, LoopState::Rundown),
            (LoopState::Rundown, LoopState::Draining),
            (LoopState::Rundown, LoopState::Deleting),
            (LoopState::Rundown, LoopState::Unbound),
            // 允许 Draining 回滚到 Rundown：当 I/O 排空超时/失败时保持拒绝新 I/O，
            // 并允许后续重试 drain 或继续删除流程。
            (LoopState::Draining, LoopState::Rundown),
            (LoopState::Draining, LoopState::Deleting),
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

    fn set_file_locked(
        inner: &mut LoopDeviceInner,
        file_inode: Arc<dyn IndexNode>,
        file_size: usize,
    ) {
        inner.file_inode = Some(file_inode);
        inner.file_size = file_size;
        inner.offset = 0;
        inner.size_limit = 0;
    }

    fn change_file_locked(
        inner: &mut LoopDeviceInner,
        file_inode: Arc<dyn IndexNode>,
        total_size: usize,
        read_only: bool,
    ) -> Result<(), SystemError> {
        let effective = Self::calc_effective_size(total_size, inner.offset, inner.size_limit)?;
        inner.file_inode = Some(file_inode);
        inner.flags = if read_only {
            LoopFlags::READ_ONLY
        } else {
            LoopFlags::empty()
        };
        inner.file_size = effective;
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
                file_inode: None,
                file_size: 0,
                device_number: DeviceNumber::new(Major::LOOP_MAJOR, minor),
                offset: 0,
                size_limit: 0,
                flags: LoopFlags::empty(),
                kobject_common: KObjectCommonData::default(),
                device_common: DeviceCommonData::default(),
                state: LoopState::Unbound,
                drain_retry_count: 0,
            }),
            block_dev_meta: BlockDevMeta::new(devname, Major::LOOP_MAJOR),
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

    fn compute_effective_size(
        inode: &Arc<dyn IndexNode>,
        offset: usize,
        size_limit: usize,
    ) -> Result<usize, SystemError> {
        let metadata = inode.metadata()?;
        if metadata.size < 0 {
            return Err(SystemError::EINVAL);
        }
        let total_size = metadata.size as usize;
        Self::calc_effective_size(total_size, offset, size_limit)
    }

    fn recalc_effective_size(&self) -> Result<(), SystemError> {
        // 通过“快照 -> 计算 -> CAS式提交”的方式避免：
        // - 持锁期间调用 metadata() 导致阻塞
        // - offset/limit/inode 并发变化时写入错误的 file_size
        const MAX_RETRY: usize = 8;
        for _ in 0..MAX_RETRY {
            let (file_inode, offset, size_limit) = {
                let inner = self.inner();
                (inner.file_inode.clone(), inner.offset, inner.size_limit)
            };

            let inode = file_inode.ok_or(SystemError::ENODEV)?;
            let effective = Self::compute_effective_size(&inode, offset, size_limit)?;

            let mut inner = self.inner();
            let still_same_inode = inner
                .file_inode
                .as_ref()
                .map(|cur| Arc::ptr_eq(cur, &inode))
                .unwrap_or(false);
            if still_same_inode && inner.offset == offset && inner.size_limit == size_limit {
                inner.file_size = effective;
                return Ok(());
            }
        }
        Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
    }

    pub fn is_bound(&self) -> bool {
        matches!(self.inner().state(), LoopState::Bound)
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
        // 先在锁外拿到 metadata，避免在持锁期间做可能阻塞的操作
        let metadata = file_inode.metadata()?;
        if metadata.size < 0 {
            return Err(SystemError::EINVAL);
        }

        let total_size = metadata.size as usize;

        // 在同一个临界区里完成状态检查 + 状态转换 + 写入 file_inode，
        // 避免 set_file() 先修改数据、再 set_state() 失败造成"半更新"。
        let mut inner = self.inner();
        match inner.state() {
            LoopState::Unbound => {}
            LoopState::Bound => return Err(SystemError::EBUSY),
            LoopState::Rundown | LoopState::Draining | LoopState::Deleting => {
                return Err(SystemError::ENODEV);
            }
        }

        inner.set_state(LoopState::Bound)?;
        Self::set_file_locked(&mut inner, file_inode.clone(), total_size);
        inner.flags = if read_only {
            LoopFlags::READ_ONLY
        } else {
            LoopFlags::empty()
        };
        drop(inner);

        // recalc_effective_size 失败时回滚状态，
        // 避免调用者收到错误但设备实际已处于 Bound 状态的不一致情况。
        if let Err(e) = self.recalc_effective_size() {
            let mut inner = self.inner();
            // 只有当前状态仍是 Bound 且 inode 未被并发修改时才回滚，
            // 避免覆盖其他操作的结果
            let should_rollback = matches!(inner.state(), LoopState::Bound)
                && inner
                    .file_inode
                    .as_ref()
                    .map(|cur| Arc::ptr_eq(cur, &file_inode))
                    .unwrap_or(false);

            if should_rollback {
                inner.file_inode = None;
                inner.file_size = 0;
                inner.offset = 0;
                inner.size_limit = 0;
                inner.flags = LoopFlags::empty();
                // Bound -> Unbound 是有效转换
                let _ = inner.set_state(LoopState::Unbound);
            }
            return Err(e);
        }
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
        match inner.state() {
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
        inner.flags = LoopFlags::empty();
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

        // 关键修复：避免先更新 offset/size_limit 再单独更新 file_size 导致 I/O 看到不一致快照。
        //
        // 回归注释（不要移除）：
        // - 不能在持有 SpinLock 时调用 inode.metadata()（可能阻塞），否则会导致不可接受的延迟/死锁风险；
        // - 同时，I/O 边界检查会在同一把锁下读取 (offset, file_size) 来计算 limit_end = offset + file_size。
        //   若把 offset/size_limit 与 file_size 分两次更新，中间窗口会出现"新 offset + 旧 file_size"的半更新快照，
        //   从而放宽边界检查，造成越界读/写风险。
        // 因此这里必须采用：锁内取 inode 快照 -> 锁外计算新 effective size -> 锁内一次性提交
        // （offset/size_limit/flags/file_size 同一临界区写入，保证对 I/O 原子可见）。

        const MAX_RETRY: usize = 16;
        let new_flags = LoopFlags::from_bits_truncate(info.lo_flags);
        let mut retry_count = 0;
        let mut last_inode_changed = false;

        for _ in 0..MAX_RETRY {
            retry_count += 1;
            let inode = {
                let inner = self.inner();
                if !matches!(inner.state(), LoopState::Bound | LoopState::Rundown) {
                    return Err(SystemError::ENXIO);
                }
                inner.file_inode.clone().ok_or(SystemError::ENODEV)?
            };

            let effective = Self::compute_effective_size(&inode, new_offset, new_limit)?;

            let mut inner = self.inner();
            if !matches!(inner.state(), LoopState::Bound | LoopState::Rundown) {
                return Err(SystemError::ENXIO);
            }
            match inner.file_inode.as_ref() {
                Some(cur_inode) if Arc::ptr_eq(cur_inode, &inode) => {
                    inner.offset = new_offset;
                    inner.size_limit = new_limit;
                    inner.flags = new_flags;
                    inner.file_size = effective;
                    return Ok(());
                }
                _ => {
                    // backing file 在计算期间被切换/解绑，重试以拿到新的 inode 快照
                    last_inode_changed = true;
                    // 在多次重试后让出 CPU，减少竞争
                    if retry_count > 4 {
                        core::hint::spin_loop();
                    }
                    continue;
                }
            }
        }

        // 根据失败原因返回更精确的错误码
        if last_inode_changed {
            // inode 持续变化导致无法完成操作，建议调用者稍后重试
            log::warn!(
                "set_status64: failed after {} retries due to concurrent inode changes",
                retry_count
            );
            Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
        } else {
            Err(SystemError::EBUSY)
        }
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
        let new_flags = LoopFlags::from_bits_truncate(info.lo_flags as u32);

        // legacy loop_info 不携带 sizelimit，这里保持现有的 size_limit 不变，只更新 offset/flags。
        // 同时复用 set_status64 的“快照 -> 计算 -> 原子提交”模式，避免 file_size 与 offset 不一致。
        const MAX_RETRY: usize = 8;
        for _ in 0..MAX_RETRY {
            let (inode, size_limit) = {
                let inner = self.inner();
                if !matches!(inner.state(), LoopState::Bound | LoopState::Rundown) {
                    return Err(SystemError::ENXIO);
                }
                (
                    inner.file_inode.clone().ok_or(SystemError::ENODEV)?,
                    inner.size_limit,
                )
            };

            let effective = Self::compute_effective_size(&inode, new_offset, size_limit)?;

            let mut inner = self.inner();
            if !matches!(inner.state(), LoopState::Bound | LoopState::Rundown) {
                return Err(SystemError::ENXIO);
            }
            match inner.file_inode.as_ref() {
                Some(cur_inode) if Arc::ptr_eq(cur_inode, &inode) => {
                    inner.offset = new_offset;
                    inner.flags = new_flags;
                    inner.file_size = effective;
                    return Ok(());
                }
                _ => continue,
            }
        }

        Err(SystemError::EBUSY)
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
        let file = {
            let guard = fd_table.read();
            guard.get_file_by_fd(new_file_fd)
        }
        .ok_or(SystemError::EBADF)?;

        let read_only = file.flags().is_read_only();

        let inode = file.inode();
        let metadata = inode.metadata()?;
        match metadata.file_type {
            FileType::File | FileType::BlockDevice => {}
            _ => return Err(SystemError::EINVAL),
        }

        if metadata.size < 0 {
            return Err(SystemError::EINVAL);
        }
        let total_size = metadata.size as usize;

        // 单次持锁完成校验+提交，避免“先读快照再提交”期间状态变化导致不一致
        let mut inner = self.inner();
        match inner.state() {
            LoopState::Bound => {}
            _ => return Err(SystemError::ENODEV),
        }
        if inner.file_inode.is_none() {
            return Err(SystemError::ENODEV);
        }
        Self::change_file_locked(&mut inner, inode, total_size, read_only)?;
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
            inner.state(),
            LoopState::Rundown | LoopState::Draining | LoopState::Deleting
        ) {
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

    /// # 功能
    ///
    /// 进入 Rundown 状态，停止接受新的 I/O 请求
    ///
    /// ## 返回值
    /// - `Ok(())`: 成功进入 Rundown 状态
    /// - `Err(SystemError)`: 状态转换失败
    pub(super) fn enter_rundown_state(&self) -> Result<(), SystemError> {
        let mut inner = self.inner();
        match inner.state() {
            LoopState::Bound => {
                // 开始新的删除流程时重置 drain 重试计数
                inner.drain_retry_count = 0;
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
            LoopState::Draining => {
                // 处于 Draining 说明已经进入"停止接收新 I/O + 等待 I/O 排空"的删除流程中。
                // 这里必须幂等返回 Ok：否则一旦 drain_active_io() 超时，设备会永久卡在 Draining，
                // 后续任何删除重试都会被 EBUSY 拒绝，造成资源泄漏。
            }
            LoopState::Deleting => {
                // 关键修复：Deleting 也必须幂等返回 Ok。
                //
                // 背景：loop_remove() 在 drain_active_io() 结束时会把状态推进到 Deleting，
                // 但后续步骤（如 block_dev_manager().unregister()）仍可能失败。
                // 若这里对 Deleting 返回 EBUSY，会导致删除失败后无法再次进入删除流程进行重试，
                // 设备会永久卡死在 Deleting（"zombie"）。
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
    pub(super) fn drain_active_io(&self) -> Result<(), SystemError> {
        use super::constants::LOOP_IO_DRAIN_MAX_RETRIES;

        let mut inner = self.inner();

        // 关键修复：检查重试次数限制，避免无限重试
        if inner.drain_retry_count >= LOOP_IO_DRAIN_MAX_RETRIES {
            error!(
                "Loop device loop{} exceeded max drain retries ({}), forcing cleanup",
                inner.device_number.minor(),
                LOOP_IO_DRAIN_MAX_RETRIES
            );
            // 强制转换到 Deleting 状态，即使还有活跃 I/O
            // 这些 I/O 将在后续访问时收到 ENODEV 错误
            if !matches!(inner.state(), LoopState::Deleting) {
                inner.set_state(LoopState::Deleting)?;
            }
            return Ok(());
        }

        if matches!(inner.state(), LoopState::Rundown) {
            inner.drain_retry_count += 1;
            inner.set_state(LoopState::Draining)?;
            info!(
                "Loop device loop{} entering draining state (attempt {}/{})",
                inner.device_number.minor(),
                inner.drain_retry_count,
                LOOP_IO_DRAIN_MAX_RETRIES
            );
        }
        drop(inner);
        let max_checks = LOOP_IO_DRAIN_TIMEOUT_MS * 1000 / LOOP_IO_DRAIN_CHECK_INTERVAL_US;
        let sleep_ts = PosixTimeSpec::new(
            0,
            (LOOP_IO_DRAIN_CHECK_INTERVAL_US as i64).saturating_mul(1000),
        );

        for _i in 0..max_checks {
            let count = self.active_io_count.load(Ordering::Acquire);
            if count == 0 {
                break;
            }

            let _ = nanosleep(sleep_ts);
        }

        let final_count = self.active_io_count.load(Ordering::Acquire);
        if final_count != 0 {
            error!(
                "Timeout waiting for I/O to drain on loop{}: {} operations still active",
                self.minor(),
                final_count
            );
            // 超时：从 Draining 回滚到 Rundown，保持拒绝新 I/O，并允许后续重试 drain。
            // 注意：这里不能留在 Draining，否则删除流程会永久卡死（enter_rundown_state 返回 EBUSY）。
            let mut inner = self.inner();
            if matches!(inner.state(), LoopState::Draining) {
                let _ = inner.set_state(LoopState::Rundown);
            }
            return Err(SystemError::ETIMEDOUT);
        }

        info!(
            "All I/O operations drained for loop device loop{}",
            self.minor()
        );

        let mut inner = self.inner();
        // 成功排空后重置重试计数
        inner.drain_retry_count = 0;

        // 并发删除幂等性：
        // 两个线程可能同时进入 drain_active_io()，并在 I/O 计数归零后几乎同时推进状态。
        // 第一个线程把状态设置为 Deleting 后，第二个线程若再执行 set_state(Deleting)，
        // 会因 (Deleting, Deleting) 不在 VALID_TRANSITIONS 而返回 EINVAL，导致 loop_remove() 失败。
        // 因此这里需要对 Deleting 做幂等处理。
        if !matches!(inner.state(), LoopState::Deleting) {
            inner.set_state(LoopState::Deleting)?;
        }

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
        self.dev_name().to_string()
    }

    fn set_name(&self, _name: String) {
        // do nothing, loop 设备名称由 devname 字段决定，不支持外部设置
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
        let (inode, file_size, devnum) = {
            let inner = self.inner();
            let inode = inner.file_inode.clone().ok_or(SystemError::EPERM)?;
            (inode, inner.file_size, inner.device_number)
        };

        let file_metadata = inode.metadata()?;
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
        _private_data: &FilePrivateData,
    ) -> Result<usize, SystemError> {
        let ioctl_cmd = LoopIoctl::from_u32(cmd).ok_or(SystemError::ENOSYS)?;

        match ioctl_cmd {
            LoopIoctl::LoopSetFd => {
                let file_fd = data as i32;
                let fd_table = ProcessManager::current_pcb().fd_table();
                let file = {
                    let guard = fd_table.read();
                    guard.get_file_by_fd(file_fd)
                }
                .ok_or(SystemError::EBADF)?;
                let read_only = file.flags().is_read_only();
                let inode = file.inode();
                let metadata = inode.metadata()?;
                match metadata.file_type {
                    FileType::File | FileType::BlockDevice => {}
                    _ => return Err(SystemError::EINVAL),
                }

                self.bind_file(inode, read_only)?;
                Ok(0)
            }
            LoopIoctl::LoopClrFd => {
                self.clear_file()?;
                Ok(0)
            }
            LoopIoctl::LoopSetStatus => {
                self.set_status(data)?;
                Ok(0)
            }
            LoopIoctl::LoopGetStatus => {
                self.get_status(data)?;
                Ok(0)
            }
            LoopIoctl::LoopSetStatus64 => {
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

        let (file_inode, base_offset, limit_end) = {
            let inner = self.inner();
            if inner.is_read_only() {
                return Err(SystemError::EROFS);
            }
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
