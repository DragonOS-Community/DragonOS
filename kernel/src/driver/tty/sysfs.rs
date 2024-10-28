use alloc::{
    string::ToString,
    sync::{Arc, Weak},
};
use system_error::SystemError;
use unified_init::macros::unified_init;

use crate::{
    driver::base::{
        class::{class_manager, Class},
        device::sys_dev_char_kset,
        kobject::KObject,
        subsys::SubSysPrivate,
    },
    init::initcall::INITCALL_SUBSYS,
};

/// `/sys/class/tty` 的 class 实例
static mut CLASS_TTY_INSTANCE: Option<Arc<TtyClass>> = None;

/// 获取 `/sys/class/tty` 的 class 实例
#[inline(always)]
#[allow(dead_code)]
pub fn sys_class_tty_instance() -> Option<&'static Arc<TtyClass>> {
    unsafe { CLASS_TTY_INSTANCE.as_ref() }
}

/// `/sys/class/tty` 类
#[derive(Debug)]
pub struct TtyClass {
    subsystem: SubSysPrivate,
}

impl TtyClass {
    const NAME: &'static str = "tty";
    pub fn new() -> Arc<Self> {
        let r = Self {
            subsystem: SubSysPrivate::new(Self::NAME.to_string(), None, None, &[]),
        };
        let r = Arc::new(r);
        r.subsystem()
            .set_class(Some(Arc::downgrade(&r) as Weak<dyn Class>));

        return r;
    }
}

impl Class for TtyClass {
    fn name(&self) -> &'static str {
        return Self::NAME;
    }

    fn dev_kobj(&self) -> Option<Arc<dyn KObject>> {
        Some(sys_dev_char_kset() as Arc<dyn KObject>)
    }

    fn set_dev_kobj(&self, _kobj: Arc<dyn KObject>) {
        unimplemented!("TtyClass::set_dev_kobj");
    }

    fn subsystem(&self) -> &SubSysPrivate {
        return &self.subsystem;
    }
}

/// 初始化帧缓冲区子系统
#[unified_init(INITCALL_SUBSYS)]
pub fn tty_sysfs_init() -> Result<(), SystemError> {
    let tty_class = TtyClass::new();
    class_manager().class_register(&(tty_class.clone() as Arc<dyn Class>))?;

    unsafe {
        CLASS_TTY_INSTANCE = Some(tty_class);
    }

    return Ok(());
}
