/// 系统调用号
pub mod nr;
use system_error::SystemError;

use crate::exception::InterruptArch;

use super::{interrupt::TrapFrame, CurrentIrqArch};

/// 系统调用初始化
pub fn arch_syscall_init() -> Result<(), SystemError> {
    unimplemented!("arch_syscall_init")
}

#[no_mangle]
pub extern "C" fn syscall_handler(frame: &mut TrapFrame) -> () {
    unsafe {
        CurrentIrqArch::interrupt_enable();
    }
    unimplemented!("syscall_handler")
}
