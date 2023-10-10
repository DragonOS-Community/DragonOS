use alloc::sync::Arc;

use crate::driver::base::device::Device;

/// 参考: https://opengrok.ringotek.cn/xref/linux-6.1.9/drivers/acpi/glue.c#352
pub fn acpi_device_notify(_dev: &Arc<dyn Device>) {
    return;
}
