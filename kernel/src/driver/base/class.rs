use alloc::{sync::Arc, string::ToString};

use crate::syscall::SystemError;

use super::kset::KSet;

/// `/sys/class`的kset
static mut CLASS_KSET_INSTANCE: Option<Arc<KSet>> = None;

#[inline(always)]
pub fn sys_class_kset() -> Arc<KSet> {
    unsafe { CLASS_KSET_INSTANCE.clone().unwrap() }
}

/// 初始化`/sys/class`的kset
pub(super) fn classes_init() -> Result<(), SystemError> {
    let class_kset = KSet::new("class".to_string());
    class_kset.register(None).expect("register class kset failed");
    unsafe {
        CLASS_KSET_INSTANCE = Some(class_kset);
    }
    return Ok(());
}