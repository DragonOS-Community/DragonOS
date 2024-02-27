use alloc::sync::Arc;

use crate::arch::{interrupt::TrapFrame, CurrentIrqArch};

use super::{
    irqdesc::{IrqDesc, IrqFlowHandler},
    InterruptArch,
};

/// 获取用于处理错误的中断的处理程序
#[inline(always)]
pub fn bad_irq_handler() -> &'static dyn IrqFlowHandler {
    &HandleBadIrq
}

/// 获取用于处理快速EOI的中断的处理程序
#[inline(always)]
pub fn fast_eoi_irq_handler() -> &'static dyn IrqFlowHandler {
    &FastEOIIrqHandler
}

/// 获取用于处理边沿触发中断的处理程序
#[inline(always)]
pub fn edge_irq_handler() -> &'static dyn IrqFlowHandler {
    &EdgeIrqHandler
}

/// handle spurious and unhandled irqs
#[derive(Debug)]
struct HandleBadIrq;

impl IrqFlowHandler for HandleBadIrq {
    /// 参考: https://code.dragonos.org.cn/xref/linux-6.1.9/kernel/irq/handle.c?fi=handle_bad_irq#33
    fn handle(&self, irq_desc: &Arc<IrqDesc>, _trap_frame: &mut TrapFrame) {
        // todo: print_irq_desc
        // todo: 增加kstat计数
        CurrentIrqArch::ack_bad_irq(irq_desc.irq());
    }
}

#[derive(Debug)]
struct FastEOIIrqHandler;

impl IrqFlowHandler for FastEOIIrqHandler {
    fn handle(&self, irq_desc: &Arc<IrqDesc>, _trap_frame: &mut TrapFrame) {
        // https://code.dragonos.org.cn/xref/linux-6.1.9/kernel/irq/chip.c?r=&mo=17578&fi=689#689
        todo!("FastEOIIrqHandler");
    }
}

#[derive(Debug)]
struct EdgeIrqHandler;

impl IrqFlowHandler for EdgeIrqHandler {
    fn handle(&self, irq_desc: &Arc<IrqDesc>, _trap_frame: &mut TrapFrame) {
        // https://code.dragonos.org.cn/xref/linux-6.1.9/kernel/irq/chip.c?fi=handle_edge_irq#775
        todo!("EdgeIrqHandler");
    }
}
