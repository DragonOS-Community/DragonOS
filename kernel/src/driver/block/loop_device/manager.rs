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
    next_free_minor: u32,
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
            !matches!(inner.state(), LoopState::Deleting)
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
