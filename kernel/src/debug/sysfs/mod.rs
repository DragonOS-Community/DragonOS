use crate::driver::base::kobject::{CommonKobj, DynamicKObjKType, KObject, KObjectManager};
use crate::init::initcall::INITCALL_POSTCORE;
use crate::misc::ksysfs::sys_kernel_kobj;
use alloc::string::ToString;
use alloc::sync::Arc;
use system_error::SystemError;
use unified_init::macros::unified_init;

/// `/sys/kernel/debug`的koject
static mut SYS_KERNEL_DEBUG_KOBJECT_INSTANCE: Option<Arc<CommonKobj>> = None;

/// 初始化debug模块在sysfs中的目录
#[unified_init(INITCALL_POSTCORE)]
fn debugfs_init() -> Result<(), SystemError> {
    let debug_kobj = CommonKobj::new("debug".to_string());
    debug_kobj.set_parent(Some(Arc::downgrade(
        &(sys_kernel_kobj() as Arc<dyn KObject>),
    )));
    KObjectManager::init_and_add_kobj(debug_kobj.clone(), Some(&DynamicKObjKType))?;
    unsafe {
        SYS_KERNEL_DEBUG_KOBJECT_INSTANCE = Some(debug_kobj);
    }
    super::tracing::init_debugfs_tracing()?;
    return Ok(());
}

pub fn debugfs_kobj() -> Arc<CommonKobj> {
    unsafe { SYS_KERNEL_DEBUG_KOBJECT_INSTANCE.clone().unwrap() }
}
