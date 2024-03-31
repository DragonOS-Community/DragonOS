use alloc::sync::Arc;
use system_error::SystemError;

use crate::driver::base::kobject::KObject;

use super::{global_default_rtc, sysfs::RtcGeneralDevice, utils::kobj2rtc_device, RtcTime};

/// 根据rtc general device, 读取真实时间
pub fn rtc_read_time(general_dev: &Arc<RtcGeneralDevice>) -> Result<RtcTime, SystemError> {
    let class_ops = general_dev.class_ops().ok_or(SystemError::EINVAL)?;

    let real_dev = general_dev
        .parent()
        .and_then(|p| p.upgrade())
        .ok_or(SystemError::ENODEV)?;

    let real_dev = kobj2rtc_device(real_dev).ok_or(SystemError::EINVAL)?;

    let time = class_ops.read_time(&real_dev)?;

    if !time.valid() {
        return Err(SystemError::EINVAL);
    }

    return Ok(time);
}

/// 从全局默认的rtc时钟那里读取时间
pub fn rtc_read_time_default() -> Result<RtcTime, SystemError> {
    rtc_read_time(&global_default_rtc().ok_or(SystemError::ENODEV)?)
}
