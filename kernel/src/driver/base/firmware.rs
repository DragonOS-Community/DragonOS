use alloc::{string::ToString, sync::Arc};

use system_error::SystemError;

use super::kobject::{CommonKobj, DynamicKObjKType, KObjectManager};

/// `/sys/firmware`的kobject实例
static mut FIRMWARE_KOBJECT_INSTANCE: Option<Arc<CommonKobj>> = None;

#[inline(always)]
#[allow(dead_code)]
pub fn sys_firmware_kobj() -> Arc<CommonKobj> {
    unsafe { FIRMWARE_KOBJECT_INSTANCE.clone().unwrap() }
}

/// 初始化`/sys/firmware`的kobject
/// 参考：https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/base/firmware.c?fi=firmware_kobj#20
pub(super) fn firmware_init() -> Result<(), SystemError> {
    let firmware_kobj = CommonKobj::new("firmware".to_string());
    KObjectManager::init_and_add_kobj(firmware_kobj.clone(), Some(&DynamicKObjKType))?;
    unsafe {
        FIRMWARE_KOBJECT_INSTANCE = Some(firmware_kobj);
    }
    return Ok(());
}
