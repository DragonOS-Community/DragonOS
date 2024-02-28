use alloc::sync::Arc;
use system_error::SystemError;
use unified_init::macros::unified_init;

use crate::{
    driver::{
        base::{
            device::{device_manager, Device},
            kobject::KObject,
            platform::{
                platform_device::{platform_device_manager, PlatformDevice},
                platform_driver::{platform_driver_manager, PlatformDriver},
            },
        },
        input::ps2_mouse::ps_mouse_device::rs_ps2_mouse_device_init,
    },
    init::initcall::INITCALL_DEVICE,
};

use self::{
    i8042_device::I8042PlatformDevice, i8042_driver::I8042Driver, i8042_ports::I8042AuxPort,
};

use super::serio_device::{serio_device_manager, SerioDevice};

pub mod i8042_device;
pub mod i8042_driver;
pub mod i8042_ports;

static mut I8042_PLATFORM_DEVICE: Option<Arc<I8042PlatformDevice>> = None;

pub fn i8042_platform_device() -> Arc<I8042PlatformDevice> {
    unsafe { I8042_PLATFORM_DEVICE.clone().unwrap() }
}

// TODO: https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/input/serio/i8042.c#1612
#[unified_init(INITCALL_DEVICE)]
pub fn i8042_init() -> Result<(), SystemError> {
    kdebug!("i8042 initializing...");
    let i8042_device = Arc::new(I8042PlatformDevice::new());
    device_manager().device_default_initialize(&(i8042_device.clone() as Arc<dyn Device>));
    platform_device_manager().device_add(i8042_device.clone() as Arc<dyn PlatformDevice>)?;
    unsafe {
        I8042_PLATFORM_DEVICE = Some(i8042_device);
    }

    let i8042_driver = I8042Driver::new();
    platform_driver_manager().register(i8042_driver.clone() as Arc<dyn PlatformDriver>)?;
    Ok(())
}

// TODO: https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/input/serio/i8042.c#441
pub fn i8042_start(_serio: &Arc<dyn SerioDevice>) -> Result<(), SystemError> {
    todo!()
}

// TODO: https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/input/serio/i8042.c#471
pub fn i8042_stop(_serio: &Arc<dyn SerioDevice>) -> Result<(), SystemError> {
    todo!()
}

/// # 函数的功能
/// 创建i8042 Aux设备
///
/// 参考: https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/input/serio/i8042.c#i8042_setup_aux
pub fn i8042_setup_aux() -> Result<(), SystemError> {
    let aux_port = Arc::new(I8042AuxPort::new());
    aux_port.set_parent(Some(Arc::downgrade(
        &(i8042_platform_device() as Arc<dyn KObject>),
    )));
    serio_device_manager().register_port(aux_port.clone() as Arc<dyn SerioDevice>)?;

    rs_ps2_mouse_device_init(aux_port.clone() as Arc<dyn KObject>)?;
    Ok(())
}
