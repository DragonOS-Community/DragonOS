use crate::driver::base::kset::KSet;
use crate::init::initcall::INITCALL_POSTCORE;
use crate::misc::ksysfs::sys_kernel_kset;
use alloc::string::ToString;
use alloc::sync::Arc;
use system_error::SystemError;
use unified_init::macros::unified_init;

/// `/sys/kernel/debug`的kset
static mut SYS_KERNEL_DEBUG_KSET_INSTANCE: Option<Arc<KSet>> = None;

/// 初始化debug模块在sysfs中的目录
#[unified_init(INITCALL_POSTCORE)]
fn debugfs_init() -> Result<(), SystemError> {
    let debug_kset = KSet::new("debug".to_string());
    debug_kset
        .register(Some(sys_kernel_kset()))
        .expect("register debug kset failed");
    unsafe {
        SYS_KERNEL_DEBUG_KSET_INSTANCE = Some(debug_kset);
    }
    super::tracing::init_debugfs_tracing()?;
    return Ok(());
}

pub fn debugfs_kset() -> Arc<KSet> {
    unsafe { SYS_KERNEL_DEBUG_KSET_INSTANCE.clone().unwrap() }
}
