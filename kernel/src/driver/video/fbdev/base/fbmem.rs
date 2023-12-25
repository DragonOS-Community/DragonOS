use alloc::{
    string::ToString,
    sync::{Arc, Weak},
};
use system_error::SystemError;
use unified_init::macros::unified_init;

use crate::driver::base::{
    class::{class_manager, Class},
    device::sys_dev_char_kset,
    init::SUBSYSTEM_INITIALIZER_SLICE,
    kobject::KObject,
    subsys::SubSysPrivate,
};

use super::fbcon::fb_console_init;

/// `/sys/class/graphics` 的 class 实例
static mut CLASS_GRAPHICS_INSTANCE: Option<Arc<GraphicsClass>> = None;

/// 获取 `/sys/class/graphics` 的 class 实例
#[inline(always)]
#[allow(dead_code)]
pub fn sys_class_graphics_instance() -> Option<&'static Arc<GraphicsClass>> {
    unsafe { CLASS_GRAPHICS_INSTANCE.as_ref() }
}

/// 初始化帧缓冲区子系统
#[unified_init(SUBSYSTEM_INITIALIZER_SLICE)]
pub fn fbmem_init() -> Result<(), SystemError> {
    let graphics_class = GraphicsClass::new();
    class_manager().class_register(&(graphics_class.clone() as Arc<dyn Class>))?;

    unsafe {
        CLASS_GRAPHICS_INSTANCE = Some(graphics_class);
    }

    fb_console_init()?;
    return Ok(());
}

/// `/sys/class/graphics` 类
#[derive(Debug)]
pub struct GraphicsClass {
    subsystem: SubSysPrivate,
}

impl GraphicsClass {
    const NAME: &'static str = "graphics";
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

impl Class for GraphicsClass {
    fn name(&self) -> &'static str {
        return Self::NAME;
    }

    fn dev_kobj(&self) -> Option<Arc<dyn KObject>> {
        Some(sys_dev_char_kset() as Arc<dyn KObject>)
    }

    fn set_dev_kobj(&self, _kobj: Arc<dyn KObject>) {
        unimplemented!("GraphicsClass::set_dev_kobj");
    }

    fn subsystem(&self) -> &SubSysPrivate {
        return &self.subsystem;
    }
}
