use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
};

use crate::{
    driver::base::{device::bus::Bus, subsys::SubSysPrivate},
    filesystem::sysfs::AttributeGroup,
};
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
        _device: &Arc<dyn crate::driver::base::device::Device>,
        _driver: &Arc<dyn crate::driver::base::device::driver::Driver>,
    ) -> Result<bool, system_error::SystemError> {
        todo!()
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
