use alloc::{string::String, sync::Arc};
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
}

int_like!(VirtIODeviceIndex, usize);
