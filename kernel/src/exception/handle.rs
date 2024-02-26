use alloc::sync::Arc;

use crate::arch::CurrentIrqArch;

use super::{
    irqdesc::{IrqDesc, IrqFlowHandler},
    InterruptArch,
};

/// 获取用于处理错误的中断的处理程序
#[inline(always)]
pub fn bad_irq_handler() -> &'static dyn IrqFlowHandler {
    &HandleBadIrq
}

/// handle spurious and unhandled irqs
#[derive(Debug)]
struct HandleBadIrq;

impl IrqFlowHandler for HandleBadIrq {
    /// 参考: https://code.dragonos.org.cn/xref/linux-6.1.9/kernel/irq/handle.c?fi=handle_bad_irq#33
    fn handle(&self, irq_desc: &Arc<IrqDesc>) {
        // todo: print_irq_desc
        // todo: 增加kstat计数
        CurrentIrqArch::ack_bad_irq(irq_desc.irq());
    }
}
