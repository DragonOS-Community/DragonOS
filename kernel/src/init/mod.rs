use crate::{
    driver::{tty::init::tty_early_init, video::VideoRefreshManager},
    libs::lib_ui::screen_manager::scm_init,
};

pub mod c_adapter;

fn init_intertrait() {
    intertrait::init_caster_map();
}

/// 在内存管理初始化之前，执行的初始化
pub fn init_before_mem_init() {
    tty_early_init().expect("tty early init failed");
    let video_ok = unsafe { VideoRefreshManager::video_init().is_ok() };
    scm_init(video_ok);
}
