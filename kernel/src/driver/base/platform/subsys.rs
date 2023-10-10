use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
};

use crate::{
    driver::base::{device::bus::Bus, kobject::KObject, subsys::SubSysPrivate},
    filesystem::{
        sysfs::{Attribute, AttributeGroup},
        vfs::syscall::ModeType,
    },
};

#[derive(Debug)]
pub struct PlatformBus {
    private: SubSysPrivate,
}

impl PlatformBus {
    pub fn new() -> Arc<Self> {
        let w: Weak<Self> = Weak::new();
        let private = SubSysPrivate::new("platform".to_string(), w, &[]);
        let bus = Arc::new(Self { private });
        bus.subsystem()
            .set_bus(Arc::downgrade(&(bus.clone() as Arc<dyn Bus>)));

        return bus;
    }
}

impl Bus for PlatformBus {
    fn name(&self) -> String {
        return "platform".to_string();
    }

    fn dev_name(&self) -> String {
        return self.name();
    }

    fn dev_groups(&self) -> &'static [&'static dyn AttributeGroup] {
        return &[&PlatformDeviceAttrGroup];
    }

    fn subsystem(&self) -> &SubSysPrivate {
        return &self.private;
    }
}

#[derive(Debug)]
pub struct PlatformDeviceAttrGroup;

impl AttributeGroup for PlatformDeviceAttrGroup {
    fn name(&self) -> Option<&str> {
        None
    }

    fn attrs(&self) -> &[&'static dyn Attribute] {
        // todo: https://opengrok.ringotek.cn/xref/linux-6.1.9/drivers/base/platform.c?r=&mo=38425&fi=1511#1311
        return &[];
    }

    fn is_visible(&self, kobj: Arc<dyn KObject>, attr: &dyn Attribute) -> Option<ModeType> {
        return Some(attr.mode());
    }
}
