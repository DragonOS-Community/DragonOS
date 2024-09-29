use alloc::{collections::LinkedList, string::String, sync::Arc};
use system_error::SystemError;

use crate::exception::{irqdesc::IrqReturn, IrqNumber};

use super::base::device::{driver::Driver, Device, DeviceId};

pub(super) mod irq;
pub mod mmio;
pub mod sysfs;
pub mod transport;
pub mod transport_mmio;
pub mod transport_pci;
#[allow(clippy::module_inception)]
pub mod virtio;
pub mod virtio_impl;

/// virtio 设备厂商ID
pub const VIRTIO_VENDOR_ID: u16 = 0x1af4;
// 参考：https://code.dragonos.org.cn/xref/linux-6.6.21/include/linux/mod_devicetable.h?fi=VIRTIO_DEV_ANY_ID#453
pub const VIRTIO_DEV_ANY_ID: u32 = 0xffffffff;

#[allow(dead_code)]
pub trait VirtIODevice: Device {
    fn handle_irq(&self, _irq: IrqNumber) -> Result<IrqReturn, SystemError>;

    fn dev_id(&self) -> &Arc<DeviceId>;

    fn set_device_name(&self, name: String);

    fn device_name(&self) -> String;

    fn set_virtio_device_index(&self, index: VirtIODeviceIndex);

    fn virtio_device_index(&self) -> Option<VirtIODeviceIndex>;

    /// virtio 设备类型
    fn device_type_id(&self) -> u32;

    /// virtio 设备厂商
    fn vendor(&self) -> u32;

    /// virtio设备的中断号
    fn irq(&self) -> Option<IrqNumber>;

    fn set_irq_number(&self, _irq: IrqNumber) -> Result<(), SystemError> {
        Err(SystemError::ENOSYS)
    }
}

pub trait VirtIODriver: Driver {
    fn probe(&self, device: &Arc<dyn VirtIODevice>) -> Result<(), SystemError>;

    fn virtio_id_table(&self) -> LinkedList<VirtioDeviceId>;

    fn add_virtio_id(&self, id: VirtioDeviceId);
}

int_like!(VirtIODeviceIndex, usize);

#[derive(Debug, Default)]
pub struct VirtIODriverCommonData {
    pub id_table: LinkedList<VirtioDeviceId>,
}

/// 参考：https://code.dragonos.org.cn/xref/linux-6.6.21/include/linux/mod_devicetable.h#449
#[derive(Debug, Default, Clone)]
pub struct VirtioDeviceId {
    pub device: u32,
    pub vendor: u32,
}

impl VirtioDeviceId {
    pub fn new(device: u32, vendor: u32) -> Self {
        Self { device, vendor }
    }
}
