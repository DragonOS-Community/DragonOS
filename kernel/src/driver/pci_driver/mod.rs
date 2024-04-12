use alloc::{string::ToString, sync::Arc};
use system_error::SystemError;

use self::{pci_device::PciBusDevice, subsys::PciBus, test::pt_init};

use super::{base::{
    device::{
        bus::{bus_register, Bus, BusState},
        device_register, sys_devices_kset, DevicePrivateData, IdTable,
    },
    kobject::KObject,
}, pci::device::pci_device_search};

pub mod dev_id;
pub mod pci_device;
pub mod pci_driver;
pub mod subsys;
pub mod test;
static mut PCI_BUS_DEVICE: Option<Arc<PciBusDevice>> = None;
static mut PCI_BUS: Option<Arc<PciBus>> = None;

pub fn pci_bus_device() -> Arc<PciBusDevice> {
    unsafe {
        return PCI_BUS_DEVICE.clone().unwrap();
    }
}

pub fn pci_bus() -> Arc<PciBus> {
    unsafe {
        return PCI_BUS.clone().unwrap();
    }
}

pub fn pci_bus_init() -> Result<(), SystemError> {
    let pci_bus_device: Arc<PciBusDevice> = PciBusDevice::new(
        DevicePrivateData::new(
            IdTable::new("pci".to_string(), None),
            BusState::NotInitialized.into(),
        ),
        Some(Arc::downgrade(&(sys_devices_kset() as Arc<dyn KObject>))),
    );
    unsafe {
        PCI_BUS_DEVICE = Some(pci_bus_device.clone());
    }

    device_register(pci_bus_device.clone())?;
    let pci_bus = PciBus::new();
    unsafe { PCI_BUS = Some(pci_bus.clone()) }
    let r = bus_register(pci_bus.clone() as Arc<dyn Bus>);
    pt_init();
    return r;
}
