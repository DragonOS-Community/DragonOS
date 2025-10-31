use crate::libs::rwlock::RwLock;

use self::boot::BootParams;
pub mod boot;
pub mod cmdline;
#[allow(clippy::module_inception)]
pub mod init;
pub mod initcall;
pub mod initial_kthread;
pub mod kexec;
pub mod version_info;

#[cfg(feature = "initram")]
pub mod initram;

/// 启动参数
static BOOT_PARAMS: RwLock<BootParams> = RwLock::new(BootParams::new());

#[inline(always)]
pub fn boot_params() -> &'static RwLock<BootParams> {
    &BOOT_PARAMS
}

#[inline(never)]
fn init_intertrait() {
    intertrait::init_caster_map();
}

pub fn enable_initramfs() -> bool {
    #[cfg(feature = "initram")]
    unsafe {
        self::initram::__INIT_ROOT_ENABLED
    }
    #[cfg(not(feature = "initram"))]
    false
}
