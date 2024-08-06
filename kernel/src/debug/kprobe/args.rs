use alloc::string::ToString;
use kprobe::{KprobeBuilder, ProbeArgs};
use log::warn;
use system_error::SystemError;

pub struct KprobeInfo<'a> {
    pub pre_handler: fn(&dyn ProbeArgs),
    pub post_handler: fn(&dyn ProbeArgs),
    pub fault_handler: Option<fn(&dyn ProbeArgs)>,
    pub symbol: Option<&'a str>,
    pub addr: Option<usize>,
    pub offset: usize,
}

extern "C" {
    fn addr_from_symbol(symbol: *const u8) -> usize;
}

impl<'a> TryFrom<KprobeInfo<'a>> for KprobeBuilder {
    type Error = SystemError;
    fn try_from(kprobe_info: KprobeInfo<'a>) -> Result<Self, Self::Error> {
        // 检查参数: symbol和addr必须有一个但不能同时有
        if kprobe_info.symbol.is_none() && kprobe_info.addr.is_none() {
            return Err(SystemError::EINVAL);
        }
        if kprobe_info.symbol.is_some() && kprobe_info.addr.is_some() {
            return Err(SystemError::EINVAL);
        }
        let func_addr = if let Some(symbol) = kprobe_info.symbol {
            let mut symbol_sting = symbol.to_string();
            if !symbol_sting.ends_with("\0") {
                symbol_sting.push('\0');
            }
            let symbol = symbol_sting.as_ptr();
            let func_addr = unsafe { addr_from_symbol(symbol) };
            if func_addr == 0 {
                warn!(
                    "register_kprobe: the symbol: {:?} not found",
                    kprobe_info.symbol
                );
                return Err(SystemError::ENXIO);
            }
            func_addr
        } else {
            kprobe_info.addr.unwrap()
        };
        let mut builder = KprobeBuilder::new(
            kprobe_info.symbol.map(|s| s.to_string()),
            func_addr,
            kprobe_info.offset,
            kprobe_info.pre_handler,
            kprobe_info.post_handler,
        );
        if kprobe_info.fault_handler.is_some() {
            builder = builder.with_fault_handler(kprobe_info.fault_handler.unwrap());
        }
        Ok(builder)
    }
}
