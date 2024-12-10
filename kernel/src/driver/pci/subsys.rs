use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
};
use intertrait::cast::CastArc;
use log::error;
use system_error::SystemError;

use crate::{
    driver::base::{
        device::{
            bus::{bus_register, Bus},
            device_register,
            driver::Driver,
            sys_devices_kset, Device,
        },
        kobject::KObject,
        subsys::SubSysPrivate,
    },
    filesystem::sysfs::AttributeGroup,
};

use super::{
    device::{PciBusDevice, PciDevice},
    driver::PciDriver,
    test::pt_init,
};

static mut PCI_BUS_DEVICE: Option<Arc<PciBusDevice>> = None;
static mut PCI_BUS: Option<Arc<PciBus>> = None;

pub(super) fn set_pci_bus_device(device: Arc<PciBusDevice>) {
    unsafe {
        PCI_BUS_DEVICE = Some(device);
    }
}

pub(super) fn set_pci_bus(bus: Arc<PciBus>) {
    unsafe {
        PCI_BUS = Some(bus);
    }
}

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

/// # 结构功能
/// 该结构为Pci总线，由于总线也属于设备，故设此结构；
/// 此结构对应/sys/bus/pci
#[derive(Debug)]
pub struct PciBus {
    private: SubSysPrivate,
}

impl PciBus {
    pub fn new() -> Arc<Self> {
        let w: Weak<Self> = Weak::new();
        let private = SubSysPrivate::new("pci".to_string(), Some(w), None, &[]);
        let bus = Arc::new(Self { private });
        bus
    }
}

impl Bus for PciBus {
    fn name(&self) -> String {
        return "pci".to_string();
    }

    fn dev_name(&self) -> String {
        return self.name();
    }

    fn dev_groups(&self) -> &'static [&'static dyn AttributeGroup] {
        return &[&PciDeviceAttrGroup];
    }

    fn subsystem(&self) -> &SubSysPrivate {
        return &self.private;
    }

    fn probe(&self, device: &Arc<dyn Device>) -> Result<(), SystemError> {
        let drv = device.driver().ok_or(SystemError::EINVAL)?;
        let pci_drv = drv.cast::<dyn PciDriver>().map_err(|_| {
            error!(
                "PciBus::probe() failed: device.driver() is not a PciDriver. Device: '{:?}'",
                device.name()
            );
            SystemError::EINVAL
        })?;
        let pci_dev = device.clone().cast::<dyn PciDevice>().map_err(|_| {
            error!(
                "PciBus::probe() failed: device is not a PciDevice. Device: '{:?}'",
                device.name()
            );
            SystemError::EINVAL
        })?;
        //见https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/pci/pci-driver.c#324
        let id = pci_drv.match_dev(&pci_dev).ok_or(SystemError::EINVAL)?;
        pci_drv.probe(&pci_dev, &id)
    }

    fn remove(&self, _device: &Arc<dyn Device>) -> Result<(), SystemError> {
        todo!()
    }

    fn sync_state(&self, _device: &Arc<dyn Device>) {
        todo!()
    }

    fn shutdown(&self, _device: &Arc<dyn Device>) {
        todo!()
    }

    fn resume(&self, _device: &Arc<dyn Device>) -> Result<(), SystemError> {
        todo!()
    }

    fn match_device(
        &self,
        device: &Arc<dyn Device>,
        driver: &Arc<dyn Driver>,
    ) -> Result<bool, SystemError> {
        //首先将设备和驱动映射为pci设备和pci驱动
        let pci_driver = driver.clone().cast::<dyn PciDriver>().map_err(|_| {
            return SystemError::EINVAL;
        })?;
        let pci_dev = device.clone().cast::<dyn PciDevice>().map_err(|_| {
            return SystemError::EINVAL;
        })?;
        //pci_driver需要实现一个match_dev函数，即driver需要识别是否支持给定的pci设备
        //这是主要的match方式
        if pci_driver.match_dev(&pci_dev).is_some() {
            return Ok(true);
        }

        //todo:这里似乎需要一个driver_override_only的支持，但是目前不清楚driver_override_only 的用途，故暂时参考platform总线的match方法
        //override_only相关代码在 https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/pci/pci-driver.c#159
        if let Some(driver_id_table) = driver.id_table() {
            if driver_id_table.name().eq(&pci_dev.name()) {
                return Ok(true);
            }
        };
        return Ok(pci_dev.name().eq(&pci_driver.name()));
    }

    fn root_device(&self) -> Option<Weak<dyn Device>> {
        let root_device = pci_bus_device() as Arc<dyn Device>;
        return Some(Arc::downgrade(&root_device));
    }
}

#[derive(Debug)]
pub struct PciDeviceAttrGroup;

impl AttributeGroup for PciDeviceAttrGroup {
    fn name(&self) -> Option<&str> {
        return None;
    }

    fn attrs(&self) -> &[&'static dyn crate::filesystem::sysfs::Attribute] {
        return &[];
    }

    fn is_visible(
        &self,
        _kobj: Arc<dyn crate::driver::base::kobject::KObject>,
        attr: &'static dyn crate::filesystem::sysfs::Attribute,
    ) -> Option<crate::filesystem::vfs::syscall::ModeType> {
        return Some(attr.mode());
    }
}

pub(super) fn pci_bus_subsys_init() -> Result<(), SystemError> {
    let pci_bus_device: Arc<PciBusDevice> = PciBusDevice::new(Some(Arc::downgrade(
        &(sys_devices_kset() as Arc<dyn KObject>),
    )));

    set_pci_bus_device(pci_bus_device.clone());

    device_register(pci_bus_device.clone())?;
    let pci_bus = PciBus::new();

    set_pci_bus(pci_bus.clone());
    let r = bus_register(pci_bus.clone() as Arc<dyn Bus>);
    pt_init()?;
    return r;
}
