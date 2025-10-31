use alloc::sync::Arc;
use system_error::SystemError;

use crate::{
    exception::{
        irqdata::IrqHandlerData,
        irqdesc::{IrqHandler, IrqReturn},
        IrqNumber,
    },
    process::namespace::net_namespace::INIT_NET_NAMESPACE,
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
        // 这里先暂时唤醒 INIT 网络命名空间的轮询线程
        INIT_NET_NAMESPACE.wakeup_poll_thread();
        Ok(IrqReturn::Handled)
    }
}
