use alloc::sync::Arc;
use system_error::SystemError;

use crate::driver::base::device::bus::{bus_register, Bus};

use self::subsys::SerioBus;

pub mod i8042;
pub mod serio_device;
pub mod serio_driver;
pub mod subsys;

static mut SERIO_BUS: Option<Arc<SerioBus>> = None;

#[allow(dead_code)]
#[inline(always)]
pub fn serio_bus() -> Arc<SerioBus> {
    unsafe { SERIO_BUS.clone().unwrap() }
}

/// # 函数的功能
/// 初始化Serio总线
///
/// 参考: https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/input/serio/serio.c#1024
pub fn serio_bus_init() -> Result<(), SystemError> {
    let serio_bus = SerioBus::new();
    let r = bus_register(serio_bus.clone() as Arc<dyn Bus>);
    unsafe { SERIO_BUS = Some(serio_bus) };

    return r;
}
