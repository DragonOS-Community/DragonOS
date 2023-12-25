use alloc::{string::ToString, sync::Arc};

use system_error::SystemError;

use super::kset::KSet;

/// `/sys/firmware`的kset
static mut FIRMWARE_KSET_INSTANCE: Option<Arc<KSet>> = None;

#[inline(always)]
#[allow(dead_code)]
pub fn sys_firmware_kset() -> Arc<KSet> {
    unsafe { FIRMWARE_KSET_INSTANCE.clone().unwrap() }
}

/// 初始化`/sys/firmware`的kset
pub(super) fn firmware_init() -> Result<(), SystemError> {
    let firmware_kset = KSet::new("firmware".to_string());
    firmware_kset
        .register(None)
        .expect("register firmware kset failed");
    unsafe {
        FIRMWARE_KSET_INSTANCE = Some(firmware_kset);
    }
    return Ok(());
}
