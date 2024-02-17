use system_error::SystemError;

use crate::arch::CurrentIrqArch;

use super::InterruptArch;

/// 初始化中断
#[inline(never)]
pub fn irq_init() -> Result<(), SystemError> {
    // todo: 通用初始化

    // 初始化架构相关的中断
    unsafe { CurrentIrqArch::arch_irq_init() }?;
    return Ok(());
}
