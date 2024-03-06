use alloc::sync::Arc;
use system_error::SystemError;

use crate::{
    exception::{
        irqdata::IrqHandlerData,
        irqdesc::{IrqHandler, IrqReturn},
        IrqNumber,
    },
    net::net_core::poll_ifaces_try_lock_onetime,
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
        poll_ifaces_try_lock_onetime().ok();
        Ok(IrqReturn::Handled)
    }
}
