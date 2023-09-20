use super::interrupt::TrapFrame;

use crate::{arch::CurrentIrqArch, exception::InterruptArch};

#[no_mangle]
pub unsafe extern "C" fn do_signal(_frame: &mut TrapFrame) {
    CurrentIrqArch::interrupt_enable();
    // todo: 处理信号
    return;
}
