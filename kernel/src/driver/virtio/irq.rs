use alloc::sync::Arc;
use hashbrown::HashMap;
use system_error::SystemError;
use unified_init::macros::unified_init;

use crate::{driver::base::device::DeviceId, init::initcall::INITCALL_CORE, libs::rwlock::RwLock};

use super::VirtIODevice;

static mut VIRTIO_IRQ_MANAGER: Option<VirtIOIrqManager> = None;

#[inline(always)]
pub fn virtio_irq_manager() -> &'static VirtIOIrqManager {
    unsafe { VIRTIO_IRQ_MANAGER.as_ref().unwrap() }
}

pub struct VirtIOIrqManager {
    map: RwLock<HashMap<Arc<DeviceId>, Arc<dyn VirtIODevice>>>,
}

impl VirtIOIrqManager {
    fn new() -> Self {
        VirtIOIrqManager {
            map: RwLock::new(HashMap::new()),
        }
    }

    /// 注册一个新的设备到virtio中断请求（IRQ）映射中。
    ///
    /// # 参数
    ///
    /// - `device` - 实现了 `VirtIODevice` trait 的设备对象，被封装在 `Arc` 智能指针中。
    ///
    /// # 返回值
    ///
    /// - 如果设备成功注册，返回 `Ok(())`。
    /// - 如果设备ID已经存在于映射中，返回 `Err(SystemError::EEXIST)`。
    pub fn register_device(&self, device: Arc<dyn VirtIODevice>) -> Result<(), SystemError> {
        let mut map = self.map.write_irqsave();

        if map.contains_key(device.dev_id()) {
            return Err(SystemError::EEXIST);
        }

        map.insert(device.dev_id().clone(), device);

        return Ok(());
    }

    /// 取消注册设备
    ///
    /// 这个函数会从内部映射中移除指定的设备。设备是通过设备ID来识别的。
    ///
    /// # 参数
    ///
    /// - `device` - 需要被取消注册的设备，它是一个实现了 `VirtIODevice` trait 的智能指针。
    #[allow(dead_code)]
    pub fn unregister_device(&self, dev_id: &Arc<DeviceId>) {
        let mut map = self.map.write_irqsave();
        map.remove(dev_id);
    }

    /// 查找并返回指定设备ID的设备。
    ///
    /// # 参数
    /// - `dev_id` - 我们要查找的设备的设备ID。
    ///
    /// # 返回
    /// - 如果找到了设备，返回一个包含设备的`Option<Arc<dyn VirtIODevice>>`。
    /// - 如果没有找到设备，返回`None`。

    pub fn lookup_device(&self, dev_id: &Arc<DeviceId>) -> Option<Arc<dyn VirtIODevice>> {
        let map = self.map.read_irqsave();
        map.get(dev_id).map(|x| x.clone())
    }
}

#[unified_init(INITCALL_CORE)]
fn init_virtio_irq_manager() -> Result<(), SystemError> {
    let manager = VirtIOIrqManager::new();
    unsafe {
        VIRTIO_IRQ_MANAGER = Some(manager);
    }
    return Ok(());
}
