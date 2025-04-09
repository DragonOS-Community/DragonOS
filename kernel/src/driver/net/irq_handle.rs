use alloc::sync::Arc;
use system_error::SystemError;

use crate::exception::{
    irqdata::IrqHandlerData,
    irqdesc::{IrqHandler, IrqReturn},
    IrqNumber,
};

/// 默认的网卡中断处理函数
#[derive(Debug)]
pub struct DefaultNetIrqHandler;

impl IrqHandler for DefaultNetIrqHandler {
    fn handle(
        &self,
        _irq: IrqNumber,
        _static_data: Option<&dyn IrqHandlerData>,
        _dynamic_data: Option<Arc<dyn IrqHandlerData>>,
    ) -> Result<IrqReturn, SystemError> {
        super::kthread::wakeup_poll_thread();
        Ok(IrqReturn::Handled)
    }
}
