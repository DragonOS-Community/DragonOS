use alloc::sync::Arc;
use system_error::SystemError;

use crate::driver::base::device::Device;


/// 参考: https://code.dragonos.org.cn/xref/linux-6.1.9/include/linux/serio.h#20
pub trait SerioDevice: Device {
    fn write(&self, device: & Arc<dyn SerioDevice>, data: u8) -> Result<(), SystemError>;
    fn open(&self, device: & Arc<dyn SerioDevice>) -> Result<(), SystemError>;
    fn close(&self, device: & Arc<dyn SerioDevice>) -> Result<(), SystemError>;
    fn start(&self, device: & Arc<dyn SerioDevice>) -> Result<(), SystemError>;
    fn stop(&self, device: & Arc<dyn SerioDevice>) -> Result<(), SystemError>;
}   