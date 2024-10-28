use crate::{
    driver::base::{
        class::{class_manager, Class},
        device::sys_dev_char_kset,
        kobject::KObject,
        subsys::SubSysPrivate,
    },
    filesystem::sysfs::AttributeGroup,
    init::initcall::INITCALL_SUBSYS,
};
use alloc::{
    string::ToString,
    sync::{Arc, Weak},
};
use system_error::SystemError;
use unified_init::macros::unified_init;

use super::sysfs::NetAttrGroup;

/// `/sys/class/net` 的 class 实例
static mut CLASS_NET_INSTANCE: Option<Arc<NetClass>> = None;

/// 获取 `/sys/class/net` 的 class 实例
#[inline(always)]
#[allow(dead_code)]
pub fn sys_class_net_instance() -> Option<&'static Arc<NetClass>> {
    unsafe { CLASS_NET_INSTANCE.as_ref() }
}

/// 初始化net子系统
#[unified_init(INITCALL_SUBSYS)]
pub fn net_init() -> Result<(), SystemError> {
    let net_class: Arc<NetClass> = NetClass::new();
    class_manager().class_register(&(net_class.clone() as Arc<dyn Class>))?;

    unsafe {
        CLASS_NET_INSTANCE = Some(net_class);
    }

    return Ok(());
}

/// '/sys/class/net' 类
#[derive(Debug)]
pub struct NetClass {
    subsystem: SubSysPrivate,
}

impl NetClass {
    const NAME: &'static str = "net";
    pub fn new() -> Arc<Self> {
        let net_class = Arc::new(Self {
            subsystem: SubSysPrivate::new(Self::NAME.to_string(), None, None, &[]),
        });
        net_class
            .subsystem()
            .set_class(Some(Arc::downgrade(&net_class) as Weak<dyn Class>));

        return net_class;
    }
}

impl Class for NetClass {
    fn name(&self) -> &'static str {
        return Self::NAME;
    }

    fn dev_kobj(&self) -> Option<Arc<dyn KObject>> {
        Some(sys_dev_char_kset() as Arc<dyn KObject>)
    }

    fn set_dev_kobj(&self, _kobj: Arc<dyn KObject>) {
        unimplemented!("NetClass::set_dev_kobj");
    }

    fn subsystem(&self) -> &SubSysPrivate {
        return &self.subsystem;
    }

    fn dev_groups(&self) -> &'static [&'static dyn AttributeGroup] {
        return &[&NetAttrGroup];
    }
}
