use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
};
use intertrait::cast::CastArc;
use system_error::SystemError;

use crate::{
    driver::base::{device::bus::Bus, subsys::SubSysPrivate},
    filesystem::sysfs::AttributeGroup,
};

use super::{pci_device::PciDevice, pci_driver::PciDriver};
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

    fn probe(
        &self,
        _device: &Arc<dyn crate::driver::base::device::Device>,
    ) -> Result<(), system_error::SystemError> {
        todo!()
        //这里需要实现一个PciDriver的cast，就是说device的driver需要被cast到PciDriver这里
    }

    fn remove(
        &self,
        _device: &Arc<dyn crate::driver::base::device::Device>,
    ) -> Result<(), system_error::SystemError> {
        todo!()
    }

    fn sync_state(&self, _device: &Arc<dyn crate::driver::base::device::Device>) {
        todo!()
    }

    fn shutdown(&self, _device: &Arc<dyn crate::driver::base::device::Device>) {
        todo!()
    }

    fn resume(
        &self,
        device: &Arc<dyn crate::driver::base::device::Device>,
    ) -> Result<(), system_error::SystemError> {
        todo!()
    }

    fn match_device(
        &self,
        device: &Arc<dyn crate::driver::base::device::Device>,
        driver: &Arc<dyn crate::driver::base::device::driver::Driver>,
    ) -> Result<bool, SystemError> {
        let pci_driver=driver.clone().cast::<dyn PciDriver>().map_err(|_|{
            return SystemError::EINVAL;
        })?;
        let pci_dev=device.clone().cast::<dyn PciDevice>().map_err(|_|{
            return SystemError::EINVAL;
        })?;
        for i in pci_driver.locked_dynid_list()?.iter(){
            if i.match_dev(&pci_dev){
                return Ok(true)
            }
        };
//todo:这里似乎需要一个driver_override_only的支持，但是目前不清楚driver_override_only 的用途，故暂时参考platform总线的match方法
//override_only相关代码在 https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/pci/pci-driver.c#159
        if let Some(driver_id_table) = driver.id_table(){
            if driver_id_table.name().eq(&pci_dev.name()){
                return Ok(true);
            }
        };
        return Ok(pci_dev.name().eq(&pci_driver.name()));

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
        kobj: Arc<dyn crate::driver::base::kobject::KObject>,
        attr: &'static dyn crate::filesystem::sysfs::Attribute,
    ) -> Option<crate::filesystem::vfs::syscall::ModeType> {
        return Some(attr.mode());
    }
}
