use alloc::sync::Arc;

use super::irqdesc::{IrqDesc, IrqFlowHandler};

/// 获取用于处理错误的中断的处理程序
#[inline(always)]
pub fn bad_irq_handler() -> &'static dyn IrqFlowHandler {
    &HandleBadIrq
}

/// handle spurious and unhandled irqs
#[derive(Debug)]
struct HandleBadIrq;

impl IrqFlowHandler for HandleBadIrq {
    fn handle(&self, _irq_desc: &Arc<IrqDesc>) {
        todo!("handle bad irq");
        // todo: https://code.dragonos.org.cn/xref/linux-6.1.9/kernel/irq/handle.c?fi=handle_bad_irq#33
    }
}
