use crate::init::initcall::INITCALL_LATE;
use core::ffi::c_void;
use system_error::SystemError;
use unified_init::macros::unified_init;

pub mod ps2_keyboard;
// pub mod ps2_keyboard_inode;

extern "C" {
    fn ps2_keyboard_init() -> c_void;
}

#[unified_init(INITCALL_LATE)]
pub fn _rs_ps2_keyboard_init() -> Result<(), SystemError> {
    unsafe {
        ps2_keyboard_init();
    }
    return Ok(());
}
