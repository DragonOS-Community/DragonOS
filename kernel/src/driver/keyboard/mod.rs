use crate::init::initcall::INITCALL_LATE;
use core::ffi::c_void;
use system_error::SystemError;
use unified_init::macros::unified_init;

pub mod ps2_keyboard;
// pub mod ps2_keyboard_inode;

extern "C" {
    fn ps2_keyboard_init() -> c_void;
}

/// 初始化ps2键盘
///
/// todo: 将ps2键盘适配到设备驱动模型后，把初始化时机改为INITCALL_DEVICE
///
/// 当前是LATE的原因是键盘驱动的TypeOneFSM需要在tty设备初始化之后才能工作。
#[unified_init(INITCALL_LATE)]
fn rs_ps2_keyboard_init() -> Result<(), SystemError> {
    unsafe {
        ps2_keyboard_init();
    }
    return Ok(());
}
