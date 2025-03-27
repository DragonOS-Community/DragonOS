use crate::debug::traceback::addr_from_symbol;
use alloc::boxed::Box;
use alloc::string::String;
use kprobe::{CallBackFunc, KprobeBuilder, ProbeArgs};
use log::warn;
use system_error::SystemError;

pub struct KprobeInfo {
    pub pre_handler: fn(&dyn ProbeArgs),
    pub post_handler: fn(&dyn ProbeArgs),
    pub fault_handler: Option<fn(&dyn ProbeArgs)>,
    pub event_callback: Option<Box<dyn CallBackFunc>>,
    pub symbol: Option<String>,
    pub addr: Option<usize>,
    pub offset: usize,
    pub enable: bool,
}

impl TryFrom<KprobeInfo> for KprobeBuilder {
    type Error = SystemError;
    fn try_from(kprobe_info: KprobeInfo) -> Result<Self, Self::Error> {
        // 检查参数: symbol和addr必须有一个但不能同时有
        if kprobe_info.symbol.is_none() && kprobe_info.addr.is_none() {
            return Err(SystemError::EINVAL);
        }
        if kprobe_info.symbol.is_some() && kprobe_info.addr.is_some() {
            return Err(SystemError::EINVAL);
        }
        let func_addr = if let Some(symbol) = kprobe_info.symbol.clone() {
            let func_addr = unsafe { addr_from_symbol(symbol.as_str()) };
            if func_addr.is_none() {
                warn!(
                    "register_kprobe: the symbol: {:?} not found",
                    kprobe_info.symbol
                );
                return Err(SystemError::ENXIO);
            }
            func_addr.unwrap() as usize
        } else {
            kprobe_info.addr.unwrap()
        };
        let mut builder = KprobeBuilder::new(
            kprobe_info.symbol,
            func_addr,
            kprobe_info.offset,
            kprobe_info.pre_handler,
            kprobe_info.post_handler,
            kprobe_info.enable,
        );
        if let Some(fault_handler) = kprobe_info.fault_handler {
            builder = builder.with_fault_handler(fault_handler);
        }
        if let Some(event_callback) = kprobe_info.event_callback {
            builder = builder.with_event_callback(event_callback);
        }
        Ok(builder)
    }
}
