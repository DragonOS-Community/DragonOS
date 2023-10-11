use crate::{
    driver::{tty::serial::serial8250::serial8250_init_stage1, video::VideoRefreshManager},
    libs::lib_ui::screen_manager::scm_init,
};

pub mod c_adapter;

fn init_intertrait() {
    intertrait::init_caster_map();
}

/// 在内存管理初始化之前，执行的初始化
fn init_before_mem_init() {
    serial8250_init_stage1();
    unsafe { VideoRefreshManager::video_init() };
    scm_init();
}
