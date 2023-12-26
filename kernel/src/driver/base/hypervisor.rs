use alloc::{string::ToString, sync::Arc};

use system_error::SystemError;

use super::kset::KSet;

/// `/sys/hypervisor`的kset
static mut HYPERVISOR_KSET_INSTANCE: Option<Arc<KSet>> = None;

#[inline(always)]
#[allow(dead_code)]
pub fn sys_hypervisor_kset() -> Arc<KSet> {
    unsafe { HYPERVISOR_KSET_INSTANCE.clone().unwrap() }
}

/// 初始化`/sys/hypervisor`的kset
pub(super) fn hypervisor_init() -> Result<(), SystemError> {
    let hypervisor_kset = KSet::new("hypervisor".to_string());
    hypervisor_kset
        .register(None)
        .expect("register hypervisor kset failed");
    unsafe {
        HYPERVISOR_KSET_INSTANCE = Some(hypervisor_kset);
    }
    return Ok(());
}
