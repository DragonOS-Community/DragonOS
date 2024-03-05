use system_error::SystemError;

use crate::arch::CurrentIrqArch;

use super::{
    dummychip::dummy_chip_init, irqdesc::early_irq_init, irqdomain::irq_domain_manager_init,
    InterruptArch,
};

/// 初始化中断
#[inline(never)]
pub fn irq_init() -> Result<(), SystemError> {
    // todo: 通用初始化

    dummy_chip_init();
    irq_domain_manager_init();
    early_irq_init().expect("early_irq_init failed");

    // 初始化架构相关的中断
    unsafe { CurrentIrqArch::arch_irq_init() }?;
    return Ok(());
}
