use crate::libs::rwlock::RwLock;

use self::boot::BootParams;
pub mod boot;
pub mod cmdline;
#[allow(clippy::module_inception)]
pub mod init;
pub mod initcall;
pub mod initial_kthread;

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
