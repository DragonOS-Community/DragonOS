use alloc::sync::Arc;

use crate::{
    driver::base::device::{driver::Driver, Device},
    syscall::SystemError,
};

use super::AcpiManager;

impl AcpiManager {
    /// 通过acpi来匹配驱动
    ///
    /// 参考 https://opengrok.ringotek.cn/xref/linux-6.1.9/drivers/acpi/bus.c#949
    pub fn driver_match_device(
        &self,
        _driver: &Arc<dyn Driver>,
        _device: &Arc<dyn Device>,
    ) -> Result<bool, SystemError> {
        // todo:

        return Ok(false);
    }
}
