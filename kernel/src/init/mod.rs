use crate::{
    driver::{
        tty::init::tty_early_init,
        video::{fbdev::base::BootTimeScreenInfo, VideoRefreshManager},
    },
    libs::{lib_ui::screen_manager::scm_init, rwlock::RwLock},
};

pub mod c_adapter;

/// 启动参数
static BOOT_PARAMS: RwLock<BootParams> = RwLock::new(BootParams::new());

#[inline(always)]
pub fn boot_params() -> &'static RwLock<BootParams> {
    &BOOT_PARAMS
}

fn init_intertrait() {
    intertrait::init_caster_map();
}

/// 在内存管理初始化之前，执行的初始化
pub fn init_before_mem_init() {
    tty_early_init().expect("tty early init failed");
    let video_ok = unsafe { VideoRefreshManager::video_init().is_ok() };
    scm_init(video_ok);
}

#[derive(Debug)]
pub struct BootParams {
    pub screen_info: BootTimeScreenInfo,
}

impl BootParams {
    const DEFAULT: Self = BootParams {
        screen_info: BootTimeScreenInfo::DEFAULT,
    };

    const fn new() -> Self {
        Self::DEFAULT
    }
}
