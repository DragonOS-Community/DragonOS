use self::uart_device::{LockedUart, LockedUartDriver};

use super::base::{device::{DevicePrivateData, IdTable, DeviceState, DeviceNumber}, platform::CompatibleTable};

pub mod uart_device;
pub mod uart_driver;

