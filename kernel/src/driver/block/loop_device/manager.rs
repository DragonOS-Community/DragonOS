use crate::{
    driver::base::{
        block::{block_device::BlockDevice, manager::block_dev_manager},
        device::DevName,
    },
    libs::spinlock::{SpinLock, SpinLockGuard},
};
use alloc::{format, sync::Arc};
use ida::IdAllocator;
use log::info;
use system_error::SystemError;

use super::{
    constants::{LoopState, LOOP_BASENAME},
    driver::LoopDeviceDriver,
    loop_device::LoopDevice,
};

/// Loop 设备管理器
pub struct LoopManager {
    inner: SpinLock<LoopManagerInner>,
}

pub struct LoopManagerInner {
    devices: [Option<Arc<LoopDevice>>; LoopManager::MAX_DEVICES],
    id_alloc: IdAllocator,
}

impl LoopManager {
    /// 最大设备数量
    const MAX_DEVICES: usize = 256;
    /// 初始化时创建的设备数量
    const MAX_INIT_DEVICES: usize = 8;

    pub fn new() -> Self {
        Self {
            inner: SpinLock::new(LoopManagerInner {
                devices: [const { None }; Self::MAX_DEVICES],
                id_alloc: IdAllocator::new(0, Self::MAX_DEVICES)
                    .expect("create IdAllocator failed"),
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
    fn alloc_specific_id_locked(inner: &mut LoopManagerInner, id: usize) -> Option<usize> {
        inner.id_alloc.alloc_specific(id)
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
        if minor >= Self::MAX_DEVICES as u32 {
            return None;
        }
        inner.devices[minor as usize].clone()
    }

    #[inline]
    fn device_reusable(device: &LoopDevice) -> bool {
        // 只有 Unbound 才是安全的“空闲可复用”状态；
        // Rundown/Draining/Deleting 都表示设备正在删除流程中，不能返回给 loop_add。
        matches!(device.inner().state(), LoopState::Unbound)
    }

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
            if Self::device_reusable(&device) {
                return Ok(device);
            }
            return Err(SystemError::EBUSY);
        }

        let id =
            Self::alloc_specific_id_locked(inner, minor as usize).ok_or(SystemError::ENOSPC)?;
        match self.create_and_register_device_locked(inner, id) {
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
            // 注意：不能用 !is_bound()，否则会把正在删除(Rundown/Draining/Deleting)的设备当成可用设备返回
            .find(|device| Self::device_reusable(device))
        {
            return Ok(device.clone());
        }

        let id = Self::alloc_id_locked(inner).ok_or(SystemError::ENOSPC)?;
        let result = self.create_and_register_device_locked(inner, id);
        if result.is_err() {
            Self::free_id_locked(inner, id);
        }
        result
    }

    fn create_and_register_device_locked(
        &self,
        inner: &mut LoopManagerInner,
        id: usize,
    ) -> Result<Arc<LoopDevice>, SystemError> {
        if id >= Self::MAX_DEVICES {
            return Err(SystemError::EINVAL);
        }
        let minor = id as u32;
        let devname = Self::format_name(id);
        let loop_dev =
            LoopDevice::new_empty_loop_device(devname, id, minor).ok_or(SystemError::ENOMEM)?;

        if let Err(e) = block_dev_manager().register(loop_dev.clone()) {
            if e == SystemError::EEXIST {
                if let Some(existing) = inner.devices[id].clone() {
                    return Ok(existing);
                }
                // 不一致状态：BlockDevManager 认为设备存在但 LoopManager 中没有。
                // 这可能是由于之前的删除操作失败导致的状态不一致。
                log::warn!(
                    "Inconsistent state: BlockDevManager has device at id {} but LoopManager doesn't",
                    id
                );
            }
            return Err(e);
        }

        inner.devices[id] = Some(loop_dev.clone());
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
            !matches!(inner.state(), LoopState::Deleting)
        };

        if needs_drain {
            device.drain_active_io()?;
        }

        device.clear_file()?;

        let block_dev: Arc<dyn BlockDevice> = device.clone();
        // 先尝试从 BlockDevManager 注销（会卸载 devfs gendisk 节点）。
        // 若注销失败，必须保留设备仍可被后续 loop_remove 重试处理；
        // 因此这里不要提前从 sysfs 移除 kobject（否则会扩大失败后的不一致面）。
        if let Err(e) = block_dev_manager().unregister(&block_dev) {
            log::error!(
                "Failed to unregister loop{} from BlockDevManager: {:?} (state={:?})",
                minor,
                e,
                device.inner().state()
            );

            // 回滚状态到 Rundown，允许后续重试删除操作。
            // 这避免了设备卡在 Deleting 状态成为"僵尸"的问题。
            let mut inner = device.inner();
            if matches!(inner.state(), LoopState::Deleting) {
                // Deleting -> Rundown 回滚
                let _ = inner.set_state(LoopState::Rundown);
                log::warn!(
                    "Rolled back loop{} state from Deleting to Rundown for retry",
                    minor
                );
            }
            return Err(e);
        }

        // best-effort：从 sysfs 移除（即使失败也不影响 devfs/manager 一致性）
        let _ = device.remove_from_sysfs();

        {
            let mut inner = self.inner();
            inner.devices[minor as usize] = None;
            Self::free_id_locked(&mut inner, minor as usize);
        }
        info!(
            "Loop device id {} (minor {}) removed successfully.",
            id, minor
        );
        Ok(())
    }

    pub fn find_free_minor(&self) -> Option<u32> {
        let inner = self.inner();
        for minor in 0..Self::MAX_DEVICES as u32 {
            match &inner.devices[minor as usize] {
                // 只有 Unbound 才能视为可复用；删除流程中的设备不应返回给 loop_add
                Some(dev) if Self::device_reusable(dev) => return Some(minor),
                Some(_) => continue,
                None => return Some(minor),
            }
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
            let id =
                Self::alloc_specific_id_locked(&mut inner, minor).ok_or(SystemError::ENOSPC)?;
            if let Err(e) = self.create_and_register_device_locked(&mut inner, id) {
                Self::free_id_locked(&mut inner, id);
                return Err(e);
            }
        }
        log::info!("Loop devices initialized");
        Ok(())
    }
}
