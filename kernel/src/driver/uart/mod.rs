use self::uart_device::{LockedUart, LockedUartDriver};

use super::base::{
    device::{DeviceNumber, DevicePrivateData, DeviceState, IdTable},
    platform::CompatibleTable,
};

pub mod uart_device;
pub mod uart_driver;
